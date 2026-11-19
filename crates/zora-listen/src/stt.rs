//! Speech to text with sherpa-onnx's streaming Zipformer (English, 20M, int8), one thread, native endpointing.
//! Audio stays in this process; the model is downloaded once by `zora-listen setup`.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sherpa_onnx::{
    OnlineRecognizer, OnlineRecognizerConfig, OnlineStream, OnlineTransducerModelConfig,
};
use std::path::{Path, PathBuf};
use zora_plugin::listen::{RATE, Recognizer, Utterance};

pub const MODEL: &str = "sherpa-onnx-streaming-zipformer-en-20M-2023-02-17";
const URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-20M-2023-02-17.tar.bz2";
const SHA256: &str = "9c559283e8498d3fe95913c79ca1cb454bb26281ac2b102b41306c7d752765d9";
const FILES: [&str; 4] = [
    "encoder-epoch-99-avg-1.int8.onnx",
    "decoder-epoch-99-avg-1.int8.onnx",
    "joiner-epoch-99-avg-1.int8.onnx",
    "tokens.txt",
];

/// `$XDG_DATA_HOME/zora/models/<model>`, or the plugin config's `model` directory.
pub fn model_dir(config: &Value) -> PathBuf {
    if let Some(dir) = config["model"].as_str() {
        return PathBuf::from(expand_tilde(dir));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"));
    data.join("zora/models").join(MODEL)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest).to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

pub fn model_present(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

pub struct Sherpa {
    rec: OnlineRecognizer,
    stream: OnlineStream,
    total: u64,
    seg_start: u64,
}

impl Sherpa {
    pub fn load(config: &Value) -> Result<Sherpa, String> {
        let dir = model_dir(config);
        // Resolve the link once, so every file comes from the same version even if setup swaps it meanwhile.
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !model_present(&dir) {
            return Err(format!(
                "speech model missing in {}; run `zora-listen setup`",
                dir.display()
            ));
        }
        if let Some(locale) = config["locale"].as_str()
            && !locale.to_lowercase().starts_with("en")
        {
            eprintln!("zora-listen: the speech model is English only (locale {locale})");
        }
        let p = |f: &str| Some(dir.join(f).to_string_lossy().into_owned());
        let mut c = OnlineRecognizerConfig::default();
        c.model_config.transducer = OnlineTransducerModelConfig {
            encoder: p(FILES[0]),
            decoder: p(FILES[1]),
            joiner: p(FILES[2]),
        };
        c.model_config.tokens = p(FILES[3]);
        c.model_config.num_threads = config["threads"].as_i64().unwrap_or(1).clamp(1, 8) as i32;
        c.decoding_method = Some("greedy_search".into());
        c.enable_endpoint = true;
        c.rule1_min_trailing_silence = 2.4;
        c.rule2_min_trailing_silence = config["endpointSeconds"].as_f64().unwrap_or(1.0) as f32;
        c.rule3_min_utterance_length = 20.0;
        let rec = OnlineRecognizer::create(&c)
            .ok_or_else(|| format!("cannot load the speech model from {}", dir.display()))?;
        let stream = rec.create_stream();
        Ok(Sherpa {
            rec,
            stream,
            total: 0,
            seg_start: 0,
        })
    }

    fn decode(&mut self) {
        while self.rec.is_ready(&self.stream) {
            self.rec.decode(&self.stream);
        }
    }

    /// The current segment as an utterance; token times are relative to the segment start.
    fn utterance(&self) -> Option<Utterance> {
        let r = self.rec.get_result(&self.stream)?;
        let text = r.text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let at = |secs: f32| self.seg_start + (secs.max(0.0) * RATE as f32) as u64;
        let (start, end) = match r.timestamps.as_deref() {
            Some([first, .., last]) => (at(*first), at(*last + 0.3)),
            Some([only]) => (at(*only), at(*only + 0.3)),
            _ => (self.seg_start, self.total),
        };
        let end = end.min(self.total);
        Some(Utterance {
            text,
            start: start.min(end),
            end,
        })
    }
}

impl Recognizer for Sherpa {
    fn accept(&mut self, samples: &[f32]) {
        self.stream.accept_waveform(RATE as i32, samples);
        self.total += samples.len() as u64;
        self.decode();
    }

    fn partial(&mut self) -> String {
        self.rec
            .get_result(&self.stream)
            .map(|r| r.text)
            .unwrap_or_default()
    }

    fn endpoint(&mut self) -> Option<Utterance> {
        if !self.rec.is_endpoint(&self.stream) {
            return None;
        }
        let u = self.utterance();
        self.rec.reset(&self.stream);
        self.seg_start = self.total;
        u
    }

    fn finish(&mut self) -> Option<Utterance> {
        // Trailing silence lets the model emit the last tokens.
        self.accept(&vec![0.0; RATE as usize / 2]);
        self.stream.input_finished();
        self.decode();
        let u = self.utterance();
        self.reset();
        u
    }

    fn reset(&mut self) {
        self.stream = self.rec.create_stream();
        self.total = 0;
        self.seg_start = 0;
    }
}

/// Downloads and checks the model. Needs `curl`, `tar` and `bzip2`. Everything happens in a staging directory
/// of its own; only a checked, complete model replaces the installed one, in a single rename.
pub fn setup(config: &Value, force: bool) -> Result<PathBuf, String> {
    let dir = model_dir(config);
    if model_present(&dir) && !force {
        return Ok(dir);
    }
    let parent = dir.parent().ok_or("model directory has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let staging = sibling(&dir, "staging");
    std::fs::create_dir(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let result = download_into(&staging).and_then(|model| activate(&model, &dir));
    let _ = std::fs::remove_dir_all(&staging);
    result.map(|()| dir)
}

/// A unique path next to `dir`, so concurrent runs never share files.
fn sibling(dir: &Path, what: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("model");
    dir.with_file_name(format!(".{name}.{what}-{}-{nanos}-{n}", std::process::id()))
}

fn download_into(staging: &Path) -> Result<PathBuf, String> {
    let archive = staging.join("model.tar.bz2");
    eprintln!("downloading {URL}");
    let ok = std::process::Command::new("curl")
        .args(["--fail", "--location", "--proto", "=https", "--output"])
        .arg(&archive)
        .arg(URL)
        .status()
        .map_err(|e| format!("cannot run curl: {e}"))?
        .success();
    if !ok {
        return Err("download failed".into());
    }
    let digest = sha256_file(&archive)?;
    if digest != SHA256 {
        return Err(format!(
            "checksum mismatch (got {digest}); nothing was installed"
        ));
    }
    let model = staging.join("model");
    std::fs::create_dir(&model).map_err(|e| e.to_string())?;
    let members: Vec<String> = FILES.iter().map(|f| format!("{MODEL}/{f}")).collect();
    let ok = std::process::Command::new("tar")
        .arg("-xjf")
        .arg(&archive)
        .arg("-C")
        .arg(&model)
        .arg("--strip-components=1")
        .args(&members)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?
        .success();
    if !ok || !model_present(&model) {
        return Err("cannot unpack the model (is bzip2 installed?)".into());
    }
    Ok(model)
}

/// The prefix of the version directories `setup` creates beside the model path.
fn version_prefix(dir: &Path) -> String {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("model");
    format!(".{name}.v-")
}

/// Whether `path` is one of `setup`'s own version directories for `dir`: a direct child of the same directory
/// named by the version scheme. Nothing else is ever deleted.
fn owned_version(dir: &Path, path: &Path) -> bool {
    let (Some(parent), Some(name)) = (dir.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return false;
    };
    let same_parent = match (
        std::fs::canonicalize(parent),
        path.parent().map(std::fs::canonicalize),
    ) {
        (Ok(a), Some(Ok(b))) => a == b,
        _ => false,
    };
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    same_parent && is_dir && name.starts_with(&version_prefix(dir))
}

/// Swaps `dir` with `other` in one step (Linux `renameat2(RENAME_EXCHANGE)`).
fn exchange(dir: &Path, other: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c =
        |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (a, b) = (c(dir)?, c(other)?);
    // SAFETY: both are valid NUL-terminated paths; AT_FDCWD resolves them like rename(2).
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Installs a complete `staged` model. `dir` is a symlink to a version directory beside it: the new model becomes
/// a new version and the link is replaced in one rename, so `dir` always names a complete model. A model
/// installed as a plain directory is swapped with the link in one exchange and kept as a version. The previous
/// version stays (a running plugin may still read it); older ones of setup's own are removed.
fn activate(staged: &Path, dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    if !model_present(staged) {
        return Err("the new model is incomplete; nothing was installed".into());
    }
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let version = sibling(dir, "v");
    std::fs::rename(staged, &version).map_err(|e| err(&version, e))?;
    let link = sibling(dir, "link");
    let target = version.file_name().ok_or("bad version path")?;
    if let Err(e) = symlink(target, &link) {
        let _ = std::fs::remove_dir_all(&version);
        return Err(err(&link, e));
    }
    let meta = std::fs::symlink_metadata(dir).ok();
    let mut previous = match &meta {
        Some(m) if m.file_type().is_symlink() => {
            std::fs::read_link(dir).ok().map(|t| dir.with_file_name(t))
        }
        _ => None,
    };
    let swapped = if meta.as_ref().is_some_and(|m| m.is_dir()) {
        // The link takes the plain directory's place in one step; the directory then becomes a version.
        exchange(dir, &link).and_then(|()| {
            let kept = sibling(dir, "v");
            std::fs::rename(&link, &kept)?;
            previous = Some(kept);
            Ok(())
        })
    } else {
        std::fs::rename(&link, dir)
    };
    if let Err(e) = swapped {
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&version);
        return Err(err(dir, e));
    }
    if !model_present(dir) {
        return Err(format!(
            "{} does not hold a complete model after the swap",
            dir.display()
        ));
    }
    if let Some(parent) = dir.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        let prefix = version_prefix(dir);
        for e in entries.flatten() {
            let path = e.path();
            let keep = path == version || previous.as_ref() == Some(&path);
            let ours = e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix));
            if !keep && ours && owned_version(dir, &path) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_dir_follows_config_then_xdg() {
        assert_eq!(
            model_dir(&json!({"model": "/opt/m"})),
            PathBuf::from("/opt/m")
        );
        assert!(model_dir(&json!({})).ends_with(format!("zora/models/{MODEL}")));
        assert!(!model_present(Path::new("/nonexistent")));
    }

    fn fake_model(dir: &Path, complete: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let n = if complete { FILES.len() } else { 2 };
        for f in &FILES[..n] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
    }

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("zora-stt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        r
    }

    fn versions(models: &Path, live: &Path) -> usize {
        let prefix = version_prefix(live);
        std::fs::read_dir(models)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().unwrap().starts_with(&prefix))
            .count()
    }

    #[test]
    fn activation_swaps_a_link_and_keeps_the_previous_version() {
        let root = root("swap");
        let models = root.join("models");
        let live = models.join(MODEL);
        // An install from before versions: a plain directory, swapped with the link in one exchange.
        fake_model(&live, true);
        std::fs::write(live.join("marker"), b"legacy").unwrap();
        let partial = root.join("partial");
        fake_model(&partial, false);
        assert!(activate(&partial, &live).is_err());
        assert!(live.join("marker").exists(), "the installed model is kept");
        for (i, tag) in ["a", "b", "c"].iter().enumerate() {
            let m = root.join(tag);
            fake_model(&m, true);
            std::fs::write(m.join("marker"), tag).unwrap();
            activate(&m, &live).unwrap();
            assert!(
                std::fs::symlink_metadata(&live)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read_to_string(live.join("marker")).unwrap(), *tag);
            assert_eq!(
                versions(&models, &live),
                2,
                "current and previous, after run {i}"
            );
        }
        // A run that crashed before its swap leaves a version; the live model is untouched.
        let stray = sibling(&live, "v");
        fake_model(&stray, true);
        assert_eq!(std::fs::read_to_string(live.join("marker")).unwrap(), "c");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn models_not_created_by_setup_are_never_deleted() {
        let root = root("ext");
        let models = root.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let live = models.join(MODEL);
        // A user-managed link to a model elsewhere.
        let external = root.join("shared-model");
        fake_model(&external, true);
        std::os::unix::fs::symlink(&external, &live).unwrap();
        // And one beside it that only looks similar.
        let lookalike = models.join("my-copy");
        fake_model(&lookalike, true);
        let new = root.join("new");
        fake_model(&new, true);
        activate(&new, &live).unwrap();
        let again = root.join("again");
        fake_model(&again, true);
        activate(&again, &live).unwrap();
        assert!(model_present(&external), "the external target is untouched");
        assert!(model_present(&lookalike));
        assert!(!owned_version(&live, &external));
        assert!(!owned_version(&live, &lookalike));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_model_is_reported_not_fatal() {
        let err = Sherpa::load(&json!({"model": "/nonexistent"}))
            .err()
            .unwrap();
        assert!(err.contains("zora-listen setup"));
    }
}

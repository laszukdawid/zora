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

/// Every model file is there and not empty, with the sizes its manifest records. A model without a manifest
/// (installed by hand, or before manifests) is accepted; one with a manifest that can't be read is not.
pub fn model_present(dir: &Path) -> bool {
    model_status(dir).is_ok()
}

/// `model_present`, with the reason when it isn't.
pub fn model_status(dir: &Path) -> Result<(), String> {
    let entries = manifest(dir)?;
    for f in FILES {
        let path = dir.join(f);
        let len = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() > 0 => m.len(),
            _ => return Err(format!("{} is missing or empty", path.display())),
        };
        if let Some(entries) = &entries
            && entries.get(f).map(|(size, _)| *size) != Some(len)
        {
            return Err(format!("{} does not match the manifest", path.display()));
        }
    }
    Ok(())
}

/// Written into each version setup installs: every model file's name, size and SHA-256, taken right after the
/// downloaded archive passed its checksum.
const MANIFEST: &str = "manifest";

/// A parsed manifest: model file name to its size and SHA-256. Load, setup and prune all read it through
/// `manifest`.
type Manifest = std::collections::HashMap<String, (u64, String)>;

fn write_manifest(dir: &Path) -> Result<(), String> {
    let mut text = String::new();
    for f in FILES {
        let path = dir.join(f);
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        text.push_str(&format!("{f} {size} {}\n", sha256_file(&path)?));
    }
    std::fs::write(dir.join(MANIFEST), text).map_err(|e| e.to_string())
}

/// The manifest's entries; `None` when there is no manifest. A manifest that can't be read, doesn't parse, or
/// doesn't list every model file is an error, never "no manifest".
fn manifest(dir: &Path) -> Result<Option<Manifest>, String> {
    let path = dir.join(MANIFEST);
    let bad = || format!("{} is damaged", path.display());
    // Only an absent entry means "no manifest"; a dangling link or anything but a regular file is damaged.
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(m) if !m.is_file() => return Err(bad()),
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut entries = Manifest::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let [name, size, sum] = f[..] else {
            return Err(bad());
        };
        // Every model file has content, so a zero size can only be damage.
        let size: u64 = size.parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
        let valid_sum = sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit());
        // Exactly one entry for each model file, and nothing else.
        if !valid_sum || !FILES.contains(&name) || entries.contains_key(name) {
            return Err(bad());
        }
        entries.insert(name.to_string(), (size, sum.to_string()));
    }
    if entries.len() != FILES.len() {
        return Err(bad());
    }
    Ok(Some(entries))
}

/// Every model file matches the manifest, byte for byte (by size and SHA-256).
fn verify(dir: &Path) -> Result<(), String> {
    let entries = manifest(dir)?.ok_or_else(|| format!("{} has no manifest", dir.display()))?;
    for f in FILES {
        let Some((size, sum)) = entries.get(f) else {
            return Err(format!("{f} is not in the manifest"));
        };
        let path = dir.join(f);
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len != *size || sha256_file(&path)? != *sum {
            return Err(format!("{} does not match the manifest", path.display()));
        }
    }
    Ok(())
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
        if let Err(e) = model_status(&dir) {
            return Err(format!(
                "speech model not usable in {} ({e}); run `zora-listen setup`",
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

/// Downloads, checks and installs the model. Needs `curl`, `tar` and `bzip2`.
pub fn setup(config: &Value, force: bool) -> Result<PathBuf, String> {
    let dir = model_dir(config);
    install(&dir, force, download_into)
}

/// Where setup keeps model versions: `<models>/versions`, marked as setup's own. Setup never deletes a version;
/// `prune` removes all but the current one, and nothing outside that directory, ever.
const VERSIONS: &str = "versions";
const MARKER: &str = ".zora-managed";
const MARKER_TEXT: &[u8] = b"zora-listen setup keeps model versions here\n";

/// Removes every installed version except the one in use. Run it while Zora is stopped: a starting plugin may
/// be about to read an older version. Returns how many were removed.
pub fn prune(config: &Value) -> Result<usize, String> {
    let dir = model_dir(config);
    let root = dir.parent().ok_or("model directory has no parent")?;
    if !root.join(VERSIONS).exists() {
        return Ok(0);
    }
    let _lock = lock(&root.join(".setup.lock"))?;
    let versions = managed_versions(root)?;
    let Some(current) = current_version(&dir, &versions) else {
        return Err(format!(
            "{} does not point at a version setup installed; nothing removed",
            dir.display()
        ));
    };
    // The fallbacks go only when the version kept is whole.
    if let Err(e) = verify(&dir) {
        return Err(format!(
            "{e}; nothing removed (run `zora-listen setup --force`)"
        ));
    }
    let mut removed = 0;
    for (v, path) in list_versions(&versions) {
        if v != current {
            std::fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Installs a model fetched by `fetch` (which fills the directory it is given). Rules:
/// - one setup at a time: an exclusive lock on `<models>/.setup.lock` for the whole run;
/// - versions live only in the marked `<models>/versions`; `dir` becomes a link to one of them;
/// - replacing the link is the commit point; no version is ever deleted here (see `prune`);
/// - a model installed as a plain directory at `dir` is used as it is and never touched; to replace it the user
///   moves it away first.
fn install(
    dir: &Path,
    force: bool,
    fetch: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let plain = |d: &Path| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir());
    if model_present(dir) && !force {
        return Ok(dir.to_path_buf());
    }
    if plain(dir) {
        return Err(format!(
            "{} is a model directory setup didn't create; move it away (or remove it) and run setup again",
            dir.display()
        ));
    }
    let root = dir.parent().ok_or("model directory has no parent")?;
    std::fs::create_dir_all(root).map_err(|e| err(root, e))?;
    let _lock = lock(&root.join(".setup.lock"))?;
    // Another run may have finished while this one waited.
    if model_present(dir) && !force {
        return Ok(dir.to_path_buf());
    }
    let versions = managed_versions(root)?;
    // Staging directories are setup's own and, under the lock, belong to no running setup.
    for e in std::fs::read_dir(&versions)
        .map_err(|e| err(&versions, e))?
        .flatten()
    {
        if e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".staging"))
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let staging = versions.join(".staging");
    std::fs::create_dir(&staging).map_err(|e| err(&staging, e))?;
    let model = staging.join("model");
    let fetched = std::fs::create_dir(&model)
        .map_err(|e| err(&model, e))
        .and_then(|()| fetch(&model))
        .and_then(|()| {
            if model_present(&model) {
                write_manifest(&model)
            } else {
                Err("the downloaded model is incomplete; nothing was installed".into())
            }
        });
    if let Err(e) = fetched {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let n = newest(&versions) + 1;
    let name = format!("{MODEL}-{n}");
    let version = versions.join(&name);
    std::fs::rename(&model, &version).map_err(|e| err(&version, e))?;
    let _ = std::fs::remove_dir_all(&staging);
    let link = root.join(format!(".{MODEL}.link"));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(Path::new(VERSIONS).join(&name), &link)
        .map_err(|e| err(&link, e))?;
    // The commit point.
    std::fs::rename(&link, dir).map_err(|e| err(dir, e))?;
    Ok(dir.to_path_buf())
}

fn lock(path: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: a valid open descriptor; the lock is released when the file is closed.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(f)
}

/// `<models>/versions`, created with its marker. It must be a real directory (not a link) holding setup's
/// marker as a regular file with the expected text; an unmarked one with contents is not setup's and is refused.
fn managed_versions(root: &Path) -> Result<PathBuf, String> {
    let v = root.join(VERSIONS);
    let refuse = || {
        format!(
            "{} was not created by setup; move it away and run setup again",
            v.display()
        )
    };
    match std::fs::symlink_metadata(&v) {
        Err(_) => std::fs::create_dir(&v).map_err(|e| format!("{}: {e}", v.display()))?,
        Ok(m) if !m.is_dir() => return Err(refuse()),
        Ok(_) => {}
    }
    let marker = v.join(MARKER);
    match std::fs::symlink_metadata(&marker) {
        Ok(m) if m.is_file() && std::fs::read(&marker).is_ok_and(|t| t == MARKER_TEXT) => {}
        Ok(_) => return Err(refuse()),
        Err(_) => {
            let empty = std::fs::read_dir(&v)
                .map_err(|e| format!("{}: {e}", v.display()))?
                .next()
                .is_none();
            if !empty {
                return Err(refuse());
            }
            std::fs::write(&marker, MARKER_TEXT)
                .map_err(|e| format!("{}: {e}", marker.display()))?;
        }
    }
    Ok(v)
}

/// Versions in the managed directory, by number.
fn list_versions(versions: &Path) -> Vec<(u64, PathBuf)> {
    let prefix = format!("{MODEL}-");
    std::fs::read_dir(versions)
        .map(|it| {
            it.flatten()
                .filter_map(|e| {
                    let n = e
                        .file_name()
                        .to_str()?
                        .strip_prefix(&prefix)?
                        .parse()
                        .ok()?;
                    e.file_type().ok()?.is_dir().then(|| (n, e.path()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn newest(versions: &Path) -> u64 {
    list_versions(versions)
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(0)
}

/// The version `dir` links to, if it is one of the managed ones.
fn current_version(dir: &Path, versions: &Path) -> Option<u64> {
    let target = std::fs::canonicalize(dir).ok()?;
    let versions = std::fs::canonicalize(versions).ok()?;
    if target.parent()? != versions {
        return None;
    }
    target
        .file_name()?
        .to_str()?
        .strip_prefix(&format!("{MODEL}-"))?
        .parse()
        .ok()
}

fn download_into(model: &Path) -> Result<(), String> {
    let archive = model.with_file_name("model.tar.bz2");
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
    let members: Vec<String> = FILES.iter().map(|f| format!("{MODEL}/{f}")).collect();
    let ok = std::process::Command::new("tar")
        .arg("-xjf")
        .arg(&archive)
        .arg("-C")
        .arg(model)
        .arg("--strip-components=1")
        .args(&members)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?
        .success();
    if !ok {
        return Err("cannot unpack the model (is bzip2 installed?)".into());
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

    #[test]
    fn a_truncated_file_fails_the_manifest() {
        let r = root("manifest");
        let live = r.join(MODEL);
        install(&live, false, |m: &Path| {
            fake_model(m, true);
            std::fs::write(m.join(FILES[1]), b"a longer decoder").unwrap();
            Ok(())
        })
        .unwrap();
        assert!(model_present(&live) && verify(&live).is_ok());
        std::fs::write(live.join(FILES[1]), b"a longer").unwrap();
        assert!(!model_present(&live), "size no longer matches");
        assert!(verify(&live).is_err());
        assert!(prune(&serde_json::json!({"model": live})).is_err());
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_damaged_manifest_is_never_read_as_no_manifest() {
        let r = root("badmanifest");
        let live = r.join(MODEL);
        install(&live, false, fetch_tagged("a")).unwrap();
        assert!(model_present(&live));
        // A truncated file, and a manifest that no longer parses.
        std::fs::write(live.join(FILES[0]), b"x").unwrap();
        std::fs::write(live.join(MANIFEST), b"garbage\n").unwrap();
        assert!(!model_present(&live));
        assert!(model_status(&live).unwrap_err().contains("damaged"));
        let err = Sherpa::load(&serde_json::json!({"model": live}))
            .err()
            .unwrap();
        assert!(err.contains("damaged"), "{err}");
        // Non-forced setup doesn't report the damaged model as ready: it installs a fresh one.
        install(&live, false, fetch_tagged("b")).unwrap();
        assert_eq!(tag(&live), "b");
        assert!(verify(&live).is_ok());
        // A manifest missing an entry is damaged too; no manifest at all is allowed.
        let m = std::fs::read_to_string(live.join(MANIFEST)).unwrap();
        let first = m.lines().next().unwrap().to_string() + "\n";
        std::fs::write(live.join(MANIFEST), first).unwrap();
        assert!(!model_present(&live));
        std::fs::remove_file(live.join(MANIFEST)).unwrap();
        assert!(model_present(&live));
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_manifest_that_is_not_a_regular_file_or_has_duplicates_is_damaged() {
        let r = root("manifest-shape");
        let live = r.join(MODEL);
        install(&live, false, fetch_tagged("a")).unwrap();
        let good = std::fs::read_to_string(live.join(MANIFEST)).unwrap();
        // A dangling link in its place, with a truncated file: damaged, not "no manifest".
        std::fs::remove_file(live.join(MANIFEST)).unwrap();
        std::os::unix::fs::symlink(r.join("nowhere"), live.join(MANIFEST)).unwrap();
        std::fs::write(live.join(FILES[0]), b"t").unwrap();
        assert!(model_status(&live).unwrap_err().contains("damaged"));
        assert!(prune(&serde_json::json!({"model": live})).is_err());
        // Duplicate entries that disagree: damaged, for load and prune alike.
        std::fs::remove_file(live.join(MANIFEST)).unwrap();
        let first = good.lines().next().unwrap();
        let (name, rest) = first.split_once(' ').unwrap();
        let (_, sum) = rest.split_once(' ').unwrap();
        let conflicting = format!("{good}{name} 1 {sum}\n");
        std::fs::write(live.join(MANIFEST), conflicting).unwrap();
        assert!(model_status(&live).unwrap_err().contains("damaged"));
        assert!(verify(&live).is_err());
        // A zero-byte entry is damaged, even if the file on disk is empty to match.
        let zeroed: String = good
            .lines()
            .map(|l| match l.split(' ').collect::<Vec<_>>()[..] {
                [n, _, s] if n == name => format!("{n} 0 {s}\n"),
                _ => format!("{l}\n"),
            })
            .collect();
        std::fs::write(live.join(MANIFEST), zeroed).unwrap();
        std::fs::write(live.join(name), b"").unwrap();
        assert!(model_status(&live).is_err());
        assert!(verify(&live).is_err());
        assert!(prune(&serde_json::json!({"model": live})).is_err());
        // An entry for a file the model doesn't have is damaged too.
        std::fs::write(
            live.join(MANIFEST),
            format!("{good}extra.bin 1 {}\n", "0".repeat(64)),
        )
        .unwrap();
        assert!(model_status(&live).is_err());
        let _ = std::fs::remove_dir_all(&r);
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

    fn fetch_tagged(tag: &'static str) -> impl FnOnce(&Path) -> Result<(), String> {
        move |m: &Path| {
            fake_model(m, true);
            std::fs::write(m.join("marker"), tag).unwrap();
            Ok(())
        }
    }

    fn tag(live: &Path) -> String {
        std::fs::read_to_string(live.join("marker")).unwrap()
    }

    fn version_numbers(root: &Path) -> Vec<u64> {
        let mut v: Vec<u64> = list_versions(&root.join(VERSIONS))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn installs_link_to_versions_and_prunes_only_when_asked() {
        let r = root("versions");
        let live = r.join(MODEL);
        install(&live, false, fetch_tagged("a")).unwrap();
        assert!(
            std::fs::symlink_metadata(&live)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(r.join(VERSIONS).join(MARKER).exists());
        assert_eq!(tag(&live), "a");
        // Present and not forced: nothing is fetched.
        install(&live, false, |_| panic!("no download expected")).unwrap();
        install(&live, true, fetch_tagged("b")).unwrap();
        install(&live, true, fetch_tagged("c")).unwrap();
        assert_eq!(tag(&live), "c");
        assert_eq!(
            version_numbers(&r),
            [1, 2, 3],
            "setup never deletes a version"
        );
        let config = serde_json::json!({"model": live});
        assert_eq!(prune(&config).unwrap(), 2);
        assert_eq!(version_numbers(&r), [3]);
        assert_eq!(tag(&live), "c");
        assert_eq!(prune(&config).unwrap(), 0);
        // A damaged current version keeps its fallbacks: truncated but not empty, or changed in place.
        install(&live, true, fetch_tagged("d")).unwrap();
        assert!(verify(&live).is_ok());
        std::fs::write(live.join(FILES[0]), b"").unwrap();
        assert!(prune(&config).is_err());
        std::fs::write(live.join(FILES[0]), b"y").unwrap();
        assert!(!model_present(&live) || verify(&live).is_err());
        assert!(prune(&config).is_err());
        assert_eq!(version_numbers(&r), [3, 4]);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_failed_fetch_installs_nothing_and_keeps_the_model() {
        let r = root("failed");
        let live = r.join(MODEL);
        install(&live, false, fetch_tagged("a")).unwrap();
        assert!(install(&live, true, |_| Err("download failed".into())).is_err());
        assert!(
            install(&live, true, |m| {
                fake_model(m, false);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(tag(&live), "a");
        assert_eq!(version_numbers(&r), [1]);
        let names: Vec<String> = std::fs::read_dir(r.join(VERSIONS))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with(".staging")),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_plain_model_directory_is_used_and_never_touched() {
        let r = root("plain");
        let live = r.join(MODEL);
        fake_model(&live, true);
        std::fs::write(live.join("marker"), "mine").unwrap();
        assert_eq!(
            install(&live, false, |_| panic!("no download")).unwrap(),
            live
        );
        assert!(install(&live, true, fetch_tagged("new")).is_err());
        assert_eq!(tag(&live), "mine");
        assert!(!r.join(VERSIONS).exists());
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn nothing_outside_the_marked_versions_directory_is_deleted() {
        let r = root("outside");
        let live = r.join(MODEL);
        std::fs::create_dir_all(&r).unwrap();
        // Lookalikes beside the managed directory, and an external link target.
        let lookalike = r.join(format!("{MODEL}-1"));
        fake_model(&lookalike, true);
        let external = r.join("shared");
        fake_model(&external, true);
        std::os::unix::fs::symlink(&external, &live).unwrap();
        install(&live, true, fetch_tagged("a")).unwrap();
        install(&live, true, fetch_tagged("b")).unwrap();
        install(&live, true, fetch_tagged("c")).unwrap();
        assert!(model_present(&lookalike) && model_present(&external));
        // An unmarked `versions` directory with contents is not setup's.
        let r2 = root("unmarked");
        std::fs::create_dir_all(r2.join(VERSIONS).join(format!("{MODEL}-1"))).unwrap();
        assert!(install(&r2.join(MODEL), true, fetch_tagged("x")).is_err());
        assert!(r2.join(VERSIONS).join(format!("{MODEL}-1")).exists());
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&r2);
    }

    #[test]
    fn a_linked_or_falsely_marked_versions_directory_is_refused() {
        let r = root("linked");
        std::fs::create_dir_all(&r).unwrap();
        // A link to someone else's directory that even carries the right marker.
        let elsewhere = r.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join(format!("{MODEL}-1"))).unwrap();
        std::fs::write(elsewhere.join(MARKER), MARKER_TEXT).unwrap();
        std::os::unix::fs::symlink(&elsewhere, r.join(VERSIONS)).unwrap();
        let live = r.join(MODEL);
        assert!(install(&live, true, fetch_tagged("a")).is_err());
        assert!(prune(&serde_json::json!({"model": live})).is_err());
        assert!(elsewhere.join(format!("{MODEL}-1")).exists());
        // A real directory whose marker is a link, or has other contents.
        let r2 = root("marker");
        let v = r2.join(VERSIONS);
        std::fs::create_dir_all(&v).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(MARKER), v.join(MARKER)).unwrap();
        assert!(install(&r2.join(MODEL), true, fetch_tagged("a")).is_err());
        std::fs::remove_file(v.join(MARKER)).unwrap();
        std::fs::write(v.join(MARKER), b"something else").unwrap();
        assert!(install(&r2.join(MODEL), true, fetch_tagged("a")).is_err());
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&r2);
    }

    #[test]
    fn setup_runs_one_at_a_time() {
        let r = root("lock");
        let live = r.join(MODEL);
        std::fs::create_dir_all(&r).unwrap();
        let held = lock(&r.join(".setup.lock")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let l = live.clone();
        let t = std::thread::spawn(move || {
            let res = install(&l, true, fetch_tagged("a"));
            tx.send(()).unwrap();
            res
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "waits for the lock"
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        t.join().unwrap().unwrap();
        // Concurrent forced runs all succeed and leave a consistent install.
        let runs: Vec<_> = (0..4)
            .map(|_| {
                let l = live.clone();
                std::thread::spawn(move || install(&l, true, fetch_tagged("c")))
            })
            .collect();
        for h in runs {
            h.join().unwrap().unwrap();
        }
        assert!(model_present(&live));
        assert_eq!(version_numbers(&r).len(), 5);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn missing_model_is_reported_not_fatal() {
        let err = Sherpa::load(&json!({"model": "/nonexistent"}))
            .err()
            .unwrap();
        assert!(err.contains("zora-listen setup"));
    }
}

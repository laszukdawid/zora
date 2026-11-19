//! zorad with zora-listen in file mode and the scripted recognizer: the whole transcript path (protocol,
//! microphone gate, engine, segment times, pause) without a speech model or a microphone. Needs the `zorad`
//! binary next to this test's (built by `cargo test --workspace`); skipped otherwise. Unix only until the Windows
//! plugins exist (it stops zorad with SIGTERM).
#![cfg(unix)]

use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LISTEN: &str = env!("CARGO_BIN_EXE_zora-listen");

fn zorad() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("zorad");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!(
            "skipped: {} not built (run cargo test --workspace)",
            p.display()
        );
        None
    }
}

struct Env {
    home: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let home = std::env::temp_dir().join(format!(
            "zora-listen-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Env { home }
    }

    fn cmd(&self, zorad: &Path) -> Command {
        let mut c = Command::new(zorad);
        c.env("HOME", &self.home).env_remove("XDG_STATE_HOME");
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn tone(secs: f32) -> Vec<i16> {
    (0..(secs * 16_000.0) as usize)
        .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 9_000.0) as i16)
        .collect()
}

fn silence(secs: f32) -> Vec<i16> {
    vec![0; (secs * 16_000.0) as usize]
}

fn write_fixture(env: &Env) -> (PathBuf, PathBuf) {
    let wav = env.home.join("speech.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    // The core matches speech only once its dictation coverage (here: from startup) reaches back past it.
    for part in [
        silence(3.0),
        tone(0.8),
        silence(0.8),
        tone(1.0),
        silence(0.8),
        tone(0.6),
        silence(0.5),
    ] {
        for s in part {
            w.write_sample(s).unwrap();
        }
    }
    w.finalize().unwrap();
    let script = env.home.join("script.txt");
    std::fs::write(&script, "Hey Zora\nrun the tests\ndo it Zora\n").unwrap();
    (wav, script)
}

fn config(env: &Env, wav: &Path, script: &Path) -> PathBuf {
    let cfg = json!({
        "capture": {"holdBackSeconds": 0.3},
        "dictation": {"monitor": "none"},
        "notifications": {"log": ["*"]},
        "plugins": {"listen": {"command": [LISTEN, "--file", wav, "--fake-stt", script]}}
    });
    let path = env.home.join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    path
}

fn start(env: &Env, zorad: &Path, config: &Path) -> (Child, Arc<Mutex<String>>) {
    let mut child = env
        .cmd(zorad)
        .args(["--config"])
        .arg(config)
        .args(["run", "--dry-run"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let (l, err) = (log.clone(), child.stderr.take().unwrap());
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let mut l = l.lock().unwrap();
            l.push_str(&line);
            l.push('\n');
        }
    });
    (child, log)
}

fn wait_for(log: &Arc<Mutex<String>>, needle: &str, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if log.lock().unwrap().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn stop(mut child: Child) {
    // SAFETY: plain signal to our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
}

fn transcript(env: &Env, zorad: &Path) -> serde_json::Value {
    let out = env
        .cmd(zorad)
        .args(["transcript", "--since", "1h", "--json"])
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn spoken_command_reaches_the_core() {
    let Some(zorad) = zorad() else { return };
    let env = Env::new("cmd");
    let (wav, script) = write_fixture(&env);
    let cfg = config(&env, &wav, &script);
    let (child, log) = start(&env, &zorad, &cfg);
    let queued = wait_for(&log, "commandQueued", Duration::from_secs(30));
    stop(child);
    let log = log.lock().unwrap().clone();
    if !queued {
        eprintln!("transcript: {}", transcript(&env, &zorad));
    }
    assert!(queued, "no command was queued; zorad log:\n{log}");
    assert!(log.contains("captureStarted"), "{log}");
    assert!(
        log.contains("chars=13"),
        "the prompt is \"run the tests\": {log}"
    );
    assert!(!log.contains("protocol error"), "{log}");
    let t = transcript(&env, &zorad);
    let texts: Vec<&str> = t["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["text"].as_str())
        .collect();
    assert_eq!(texts, ["hey zora", "run the tests", "do it zora"]);
}

#[test]
fn paused_core_never_opens_the_input() {
    let Some(zorad) = zorad() else { return };
    let env = Env::new("paused");
    let (wav, script) = write_fixture(&env);
    let cfg = config(&env, &wav, &script);
    let paused = env.cmd(&zorad).arg("pause").output().unwrap();
    assert!(paused.status.success());
    let (child, log) = start(&env, &zorad, &cfg);
    let told = wait_for(&log, "listening=false", Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(6));
    stop(child);
    let log = log.lock().unwrap().clone();
    assert!(told, "the plugin never heard it is paused:\n{log}");
    assert!(!log.contains("listening=true"), "{log}");
    assert!(!log.contains("captureStarted"), "{log}");
    let t = transcript(&env, &zorad);
    assert!(t["segments"].as_array().unwrap().is_empty(), "{t}");
}

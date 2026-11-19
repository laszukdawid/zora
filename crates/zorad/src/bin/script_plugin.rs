//! Reference plugin that replays a script from `hello.config` (docs/protocol.md). Used by the end-to-end tests
//! and as a starting point for real plugins.
//!
//! config: {
//!   "capabilities": ["transcript", "dictation", "notify"],
//!   "heartbeat": true,                      // dictation heartbeats every 2 s (healthy, not capturing)
//!   "steps": [ {"delayMs": 500, "say": "Hey Zora ...", "durMs": 1000},   // "agoMs": ended that long ago
//!              {"delayMs": 0, "raw": "{not json"},           // inject a raw line
//!              {"delayMs": 0, "began": true} / {"ended": true},
//!              {"delayMs": 0, "quietEnd": true}, {"delayMs": 0, "skipSeq": true},
//!              {"delayMs": 0, "exit": 3} ],
//!   "capturingAtStart": false,
//!   "listeningCapability": true,            // declare `listening` with `transcript` (false: an old plugin)
//!   "confirmPause": true,                   // answer each epoch with `mic` (false: never confirm)
//!   "confirmDelayMs": 6000,                 // confirm a pause that late
//!   // {"partial": "hey zora"} carries the current epoch; {"partial": ..., "epoch": 1} a given (stale) one
//! }

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zora_core::clock::{CLOCK_DOMAIN, MILLI, now_ns};

struct Out {
    seq: AtomicU64,
    generation: u64,
    lock: Mutex<std::io::Stdout>,
}

impl Out {
    /// The sequence number is assigned while holding the write lock, so wire order equals sequence order.
    fn send(&self, mut v: Value) {
        let mut o = self.lock.lock().unwrap();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "seq".into(),
                json!(self.seq.fetch_add(1, Ordering::SeqCst) + 1),
            );
            obj.insert("generation".into(), json!(self.generation));
        }
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
    fn raw(&self, line: &str) {
        let mut o = self.lock.lock().unwrap();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }
}

/// A TERM-ignoring child in the plugin's process group (Windows: its job) that writes its pid to `pidfile`.
fn spawn_stubborn(pidfile: &str) {
    #[cfg(unix)]
    let mut cmd = std::process::Command::new("/bin/sh");
    #[cfg(unix)]
    cmd.args([
        "-c",
        &format!("trap '' TERM; echo $$ > {pidfile}; exec sleep 60"),
    ]);
    #[cfg(windows)]
    let mut cmd = std::process::Command::new(
        std::env::current_exe()
            .unwrap()
            .with_file_name("zora-test-harness.exe"),
    );
    #[cfg(windows)]
    cmd.args(["hold", pidfile]);
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn main() {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let Some(Ok(hello)) = lines.next() else {
        return;
    };
    let hello: Value = serde_json::from_str(&hello).unwrap_or_default();
    let generation = hello["generation"].as_u64().unwrap_or(0);
    let config = hello["config"].clone();
    let mut caps: Vec<String> = serde_json::from_value(config["capabilities"].clone())
        .unwrap_or_else(|_| vec!["transcript".into()]);
    if caps.iter().any(|c| c == "transcript")
        && config["listeningCapability"].as_bool().unwrap_or(true)
    {
        caps.push("listening".into());
    }
    let confirm =
        caps.iter().any(|c| c == "listening") && config["confirmPause"].as_bool().unwrap_or(true);
    let epoch = Arc::new(AtomicU64::new(0));
    let out = Arc::new(Out {
        seq: AtomicU64::new(0),
        generation,
        lock: Mutex::new(std::io::stdout()),
    });
    // "spawnBeforeReady" + "withholdReady": a plugin stuck in negotiation that owns a TERM-ignoring child.
    if let Some(pidfile) = config["spawnBeforeReady"].as_str() {
        spawn_stubborn(pidfile);
    }
    if config["withholdReady"].as_bool().unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_secs(120));
        return;
    }
    out.raw(&json!({"t": "ready", "protocol": 1, "generation": generation, "capabilities": caps, "clock": CLOCK_DOMAIN, "now_ns": now_ns()}).to_string());
    // "capturingAtStart": a dictation already running when this connection starts (no `began` is sent).
    let capturing = Arc::new(Mutex::new(
        config["capturingAtStart"].as_bool().unwrap_or(false),
    ));

    let healthy = Arc::new(Mutex::new(true));
    if caps.iter().any(|c| c == "dictation") && config["heartbeat"].as_bool().unwrap_or(true) {
        let (out, capturing, healthy) = (out.clone(), capturing.clone(), healthy.clone());
        std::thread::spawn(move || {
            loop {
                let c = *capturing.lock().unwrap();
                let h = *healthy.lock().unwrap();
                out.send(json!({"t": "health", "healthy": h, "capturing": c, "reason": "ok", "at_ns": now_ns()}));
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    {
        let (out, capturing, healthy, epoch) = (
            out.clone(),
            capturing.clone(),
            healthy.clone(),
            epoch.clone(),
        );
        let steps = config["steps"].as_array().cloned().unwrap_or_default();
        std::thread::spawn(move || {
            for step in steps {
                std::thread::sleep(Duration::from_millis(step["delayMs"].as_u64().unwrap_or(0)));
                if let Some(text) = step["say"].as_str() {
                    let end = now_ns() - step["agoMs"].as_i64().unwrap_or(0) * MILLI;
                    let start = end - step["durMs"].as_i64().unwrap_or(1000) * MILLI;
                    out.send(json!({"t": "segment", "session": 1, "start_ns": start, "end_ns": end, "text": text}));
                } else if let Some(text) = step["partial"].as_str() {
                    let e = step["epoch"]
                        .as_u64()
                        .unwrap_or_else(|| epoch.load(Ordering::SeqCst));
                    out.send(json!({"t": "partial", "session": 1, "text": text, "epoch": e}));
                } else if let Some(raw) = step["raw"].as_str() {
                    out.raw(raw);
                } else if step["began"].as_bool() == Some(true) {
                    *capturing.lock().unwrap() = true;
                    out.send(json!({"t": "dictation", "state": "began", "at_ns": now_ns()}));
                } else if step["ended"].as_bool() == Some(true) {
                    *capturing.lock().unwrap() = false;
                    out.send(json!({"t": "dictation", "state": "ended", "at_ns": now_ns()}));
                } else if step["quietEnd"].as_bool() == Some(true) {
                    // Capture stops but only heartbeats will say so.
                    *capturing.lock().unwrap() = false;
                } else if let Some(h) = step["healthNow"].as_bool() {
                    // Change monitor health and report it immediately (capture state unchanged).
                    *healthy.lock().unwrap() = h;
                    let c = *capturing.lock().unwrap();
                    out.send(json!({"t": "health", "healthy": h, "capturing": c, "reason": "test", "at_ns": now_ns()}));
                } else if let Some(pidfile) = step["spawnStubborn"].as_str() {
                    spawn_stubborn(pidfile);
                } else if step["skipSeq"].as_bool() == Some(true) {
                    out.seq.fetch_add(1, Ordering::SeqCst);
                } else if let Some(code) = step["exit"].as_i64() {
                    std::process::exit(code as i32);
                }
            }
        });
    }

    for line in lines {
        let Ok(line) = line else { break };
        let msg: Value = serde_json::from_str(&line).unwrap_or_default();
        match msg["t"].as_str() {
            Some("sync") => {
                let c = *capturing.lock().unwrap();
                out.send(json!({"t": "synced", "id": msg["id"], "at_ns": now_ns(), "healthy": true, "capturing": c}));
            }
            Some("notify") => {
                eprintln!("notify kind={}", msg["kind"].as_str().unwrap_or(""));
                out.send(json!({"t": "ack", "id": msg["id"], "ok": true}));
            }
            Some("start") | Some("listening") => {
                let on = msg["listening"].as_bool().unwrap_or(false);
                let e = msg["epoch"].as_u64().unwrap_or(0);
                eprintln!("listening={on} epoch={e} confirmed={}", msg["confirmed"]);
                let new = epoch.swap(e, Ordering::SeqCst) != e;
                if confirm && e > 0 && new {
                    // "confirmDelayMs": a pause confirmed late.
                    match config["confirmDelayMs"].as_u64().filter(|_| !on) {
                        Some(ms) => {
                            let out = out.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_millis(ms));
                                out.send(json!({"t": "mic", "epoch": e, "open": on}));
                            });
                        }
                        None => out.send(json!({"t": "mic", "epoch": e, "open": on})),
                    }
                }
            }
            Some("clock") => out.send(json!({"t": "clock", "id": msg["id"], "now_ns": now_ns()})),
            Some("stop") => {
                out.send(json!({"t": "stopped"}));
                // "lingerAfterStop": a plugin that, correctly, keeps running until its input closes.
                if !config["lingerAfterStop"].as_bool().unwrap_or(false) {
                    return;
                }
            }
            _ => {}
        }
    }
}

//! `zora-acdc`: a notify plugin (docs/protocol.md) that puts Zora's voice commands on the ACDC task board.
//!
//! config (`plugins.acdc.config`): {
//!   "url": "http://127.0.0.1:8787/api",   // loopback only
//!   "workspace": null,                     // board workspace id; null = the board's default
//!   "agent": "zora",                       // the name Zora works under on the board
//!   "onSuccess": "review",                 // or "done"
//!   "tags": ["zora"], "claimMinutes": 60
//! }
//! Route job events to it: `"notifications": {"acdc": ["commandQueued", "jobStarted", "jobSucceeded", "jobFailed"]}`.
//! Logs carry metadata only, never command text or results.

mod board;
mod http;

use board::{Board, Notice, Settings};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zora_core::clock::{CLOCK_DOMAIN, now_ns};

struct Out {
    seq: Mutex<u64>,
    generation: u64,
}

impl Out {
    /// Sequence numbers are assigned under the stdout lock, so wire order equals sequence order.
    fn send(&self, mut v: Value) {
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        v["seq"] = json!(*seq);
        v["generation"] = json!(self.generation);
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let Some(Ok(hello)) = lines.next() else {
        return;
    };
    let hello: Value = serde_json::from_str(&hello).unwrap_or_default();
    let generation = hello["generation"].as_u64().unwrap_or(0);
    let config = &hello["config"];
    let url = config["url"]
        .as_str()
        .unwrap_or("http://127.0.0.1:8787/api");
    let setup =
        http::Endpoint::parse(url).and_then(|e| Settings::from_config(config).map(|s| (e, s)));
    let (endpoint, settings) = match setup {
        Ok(v) => v,
        Err(e) => {
            // Exiting before `ready` makes the core report the plugin as failed and retry with backoff.
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };
    let out = Arc::new(Out {
        seq: Mutex::new(0),
        generation,
    });
    {
        let mut o = std::io::stdout().lock();
        let ready = json!({"t": "ready", "protocol": 1, "generation": generation,
                           "capabilities": ["notify"], "clock": CLOCK_DOMAIN, "now_ns": now_ns()});
        let _ = writeln!(o, "{ready}");
        let _ = o.flush();
    }

    // Board calls run on one worker, in order, so a slow board never delays acks to clock probes or `stop`.
    let (tx, rx) = mpsc::channel::<(Value, Notice)>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let worker = {
        let out = out.clone();
        let transport = http::Http {
            endpoint,
            timeout: Duration::from_secs(3),
        };
        std::thread::spawn(move || {
            let mut board = Board::new(transport, settings);
            if let Err(e) = board.hello() {
                eprintln!("board hello failed: {e}");
            }
            while let Ok((id, notice)) = rx.recv() {
                match board.handle(&notice) {
                    Ok(()) => out.send(json!({"t": "ack", "id": id, "ok": true})),
                    Err(e) => {
                        eprintln!("notify kind={} failed: {e}", notice.kind);
                        out.send(json!({"t": "ack", "id": id, "ok": false, "error": e}));
                    }
                }
            }
            board.stop();
            let _ = done_tx.send(());
        })
    };

    for line in lines {
        let Ok(line) = line else { break };
        let msg: Value = serde_json::from_str(&line).unwrap_or_default();
        match msg["t"].as_str() {
            Some("notify") => {
                let notice = Notice {
                    kind: msg["kind"].as_str().unwrap_or("").into(),
                    body: msg["body"].as_str().unwrap_or("").into(),
                    job: msg["job"].as_str().map(String::from),
                    harness: msg["harness"].as_str().map(String::from),
                };
                let _ = tx.send((msg["id"].clone(), notice));
            }
            Some("clock") => out.send(json!({"t": "clock", "id": msg["id"], "now_ns": now_ns()})),
            Some("stop") => break,
            _ => {}
        }
    }
    // Stop or EOF: finish what is queued (bounded; the core waits 5 s for `stopped`), then sign off.
    drop(tx);
    let finished = done_rx.recv_timeout(Duration::from_secs(4)).is_ok();
    if !finished {
        eprintln!("board calls still pending at stop; dropped");
    }
    out.send(json!({"t": "stopped"}));
    if finished {
        let _ = worker.join();
    }
}

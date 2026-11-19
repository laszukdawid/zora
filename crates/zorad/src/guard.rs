//! `zorad __guard` and `zorad __gate`: every harness job runs under a guard that owns its process group.
//!
//! Order (see docs/design.md): guard spawns gate (new pgid = gate pid, blocked on a go-pipe) → guard reports
//! `registered` → core commits ownership durably → core sends `go` → gate execs the harness. If the core goes
//! away at any point (stdin EOF), the guard reaps the whole job group before exiting.
//!
//! Windows has no exec and no process groups: the harness starts suspended inside a named Job Object and is resumed
//! on `go` (os_windows.rs).

use crate::os;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobSpec {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub capture_limit: usize,
    pub kill_grace_ms: u64,
    /// The job id; names the Job Object on Windows.
    #[serde(default)]
    pub name: String,
    /// Unix: the file `zorad jobs cancel` creates to tell the guard (Windows uses a named event).
    #[serde(default)]
    pub cancel_marker: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum GuardMsg {
    Registered {
        guard_pid: i32,
        guard_start: i64,
        pgid: i32,
        gate_start: i64,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        stdout: String,
        stderr: String,
        truncated: bool,
        cancelled: bool,
    },
    Cleaned {
        ok: bool,
    },
    Failed {
        error: String,
    },
    /// A cancel request (`zorad jobs cancel`) reached the guard, which is stopping the job.
    Cancelling,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum CoreMsg {
    Spec(JobSpec),
    Go,
    Cancel,
}

fn send(out: &Mutex<std::io::Stdout>, msg: &GuardMsg) {
    let mut line = serde_json::to_string(msg).unwrap_or_default();
    line.push('\n');
    let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = o.write_all(line.as_bytes());
    let _ = o.flush();
}

type Captured = Arc<Mutex<(Vec<u8>, bool)>>;

/// Drains a pipe into a capped buffer (keeping the tail). The returned receiver fires at EOF; callers wait on it with a deadline
/// because an escaped process can hold the pipe open forever.
fn capped_reader(
    mut r: impl Read + Send + 'static,
    limit: usize,
) -> (Captured, mpsc::Receiver<()>) {
    let buf: Captured = Arc::new(Mutex::new((Vec::new(), false)));
    let (done_tx, done_rx) = mpsc::channel();
    let shared = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Keep the most recent `limit` bytes: agents print their final answer last.
                    let mut b = shared.lock().unwrap_or_else(|e| e.into_inner());
                    b.0.extend_from_slice(&chunk[..n]);
                    if b.0.len() > limit {
                        let excess = b.0.len() - limit;
                        b.0.drain(..excess);
                        b.1 = true;
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });
    (buf, done_rx)
}

fn take_captured(c: &Captured, done: &mpsc::Receiver<()>, wait: Duration) -> (Vec<u8>, bool) {
    let _ = done.recv_timeout(wait);
    let b = c.lock().unwrap_or_else(|e| e.into_inner());
    (b.0.clone(), b.1)
}

/// Encoded (JSON-escaped) budget per output field; the whole frame stays well under the core's 512 KiB limit
/// even when escaping expands every byte (e.g. NUL -> "\u0000").
pub const MAX_FIELD_ENCODED: usize = 128 * 1024;

/// Truncates `s` (keeping its end) until its JSON encoding fits `max` bytes. Returns whether it was cut.
pub fn fit_encoded(s: String, max: usize) -> (String, bool) {
    let encoded = |s: &str| {
        serde_json::to_string(s)
            .map(|e| e.len())
            .unwrap_or(usize::MAX)
    };
    if encoded(&s) <= max {
        return (s, false);
    }
    // Keep the end (where answers are). Worst case each char encodes to 6 bytes ("\\uXXXX").
    let chars: Vec<char> = s.chars().collect();
    let mut keep = (max / 6).min(chars.len());
    loop {
        let cut: String = chars[chars.len() - keep..].iter().collect();
        if encoded(&cut) <= max || keep == 0 {
            return (cut, true);
        }
        keep /= 2;
    }
}

pub fn guard_main() -> i32 {
    os::ignore_termination_signals();
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let (tx, rx) = mpsc::channel::<Option<CoreMsg>>();
    std::thread::spawn(move || {
        for line in BufReader::new(std::io::stdin()).lines() {
            let Ok(line) = line else { break };
            if let Ok(msg) = serde_json::from_str::<CoreMsg>(&line)
                && tx.send(Some(msg)).is_err()
            {
                return;
            }
        }
        let _ = tx.send(None); // liveness lost
    });

    let spec = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Some(CoreMsg::Spec(s))) => s,
        _ => return 2,
    };
    if spec.argv.is_empty() {
        send(
            &out,
            &GuardMsg::Failed {
                error: "empty argv".into(),
            },
        );
        return 2;
    }

    let mut held = match os::spawn_held(&spec) {
        Ok(h) => h,
        Err(error) => {
            send(&out, &GuardMsg::Failed { error });
            return 2;
        }
    };
    let group = held.group.clone();
    let pgid = group.id();
    let grace = Duration::from_millis(spec.kill_grace_ms);
    let (job_out, job_err) = held.take_output();
    let stdout_h = capped_reader(job_out, spec.capture_limit);
    let stderr_h = capped_reader(job_err, spec.capture_limit);
    let me = std::process::id() as i32;
    send(
        &out,
        &GuardMsg::Registered {
            guard_pid: me,
            guard_start: os::start_time(me).unwrap_or(0),
            pgid,
            gate_start: os::start_time(pgid).unwrap_or(0),
        },
    );

    // Fence: nothing executes until the core has committed ownership and says go, and nobody has cancelled. This
    // thread alone moves the job from held to released or cancelled; a cancel that comes after the release is
    // handled below by stopping the running job.
    let fence_end = std::time::Instant::now() + Duration::from_secs(5);
    let released = loop {
        if group.cancel_requested() {
            break Err(Unreleased::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(CoreMsg::Go)) => {
                if group.cancel_requested() {
                    break Err(Unreleased::Cancelled);
                }
                pause_before_release();
                break held.release().map_err(Unreleased::Failed);
            }
            Ok(Some(CoreMsg::Cancel)) => break Err(Unreleased::Cancelled),
            Ok(Some(CoreMsg::Spec(_))) => {}
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => break Err(Unreleased::NoGo),
            Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() >= fence_end => {
                break Err(Unreleased::NoGo);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if let Err(why) = released {
        group.kill();
        // Bounded: a leader that survives the kill leaves the job unverified, not the guard stuck.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(held.wait());
        });
        let _ = rx.recv_timeout(Duration::from_secs(5));
        let ok = group.reap_blocking(Duration::ZERO);
        match why {
            Unreleased::Failed(error) => send(&out, &GuardMsg::Failed { error }),
            Unreleased::Cancelled => send(
                &out,
                &GuardMsg::Exited {
                    code: None,
                    signal: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    cancelled: true,
                },
            ),
            Unreleased::NoGo => {}
        }
        send(&out, &GuardMsg::Cleaned { ok });
        return 1;
    }

    stall_for_tests();

    // Wait for the leader while watching for cancel / liveness loss.
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(held.wait());
    });
    let mut cancelled = false;
    let status = loop {
        if let Ok(s) = done_rx.recv_timeout(Duration::from_millis(100)) {
            break s;
        }
        let from_core = matches!(
            rx.try_recv(),
            Ok(Some(CoreMsg::Cancel)) | Ok(None) | Err(mpsc::TryRecvError::Disconnected)
        );
        if (from_core || group.cancel_requested()) && !cancelled {
            cancelled = true;
            if !from_core {
                // `zorad jobs cancel`: the core arms its cleanup deadline as for its own cancel.
                send(&out, &GuardMsg::Cancelling);
            }
            group.term();
            let (killer, killer_grace) = (group.clone(), grace);
            std::thread::spawn(move || {
                std::thread::sleep(killer_grace);
                killer.kill();
            });
        }
    };
    // The job is over only when its whole group is gone. A cancel accepted at any point until then counts.
    let ok = group.reap_blocking(grace);
    let cancelled = cancelled || group.cancel_requested();
    // Bounded: a detached process that inherited the pipes must not keep the guard (and workspace) busy.
    let (stdout, t1) = take_captured(&stdout_h.0, &stdout_h.1, Duration::from_secs(2));
    let (stderr, t2) = take_captured(&stderr_h.0, &stderr_h.1, Duration::from_secs(1));
    let (out_text, cut1) = fit_encoded(
        String::from_utf8_lossy(&stdout).into_owned(),
        MAX_FIELD_ENCODED,
    );
    let (err_text, cut2) = fit_encoded(
        String::from_utf8_lossy(&stderr).into_owned(),
        MAX_FIELD_ENCODED / 2,
    );
    send(
        &out,
        &GuardMsg::Exited {
            code: status.and_then(|s| s.0),
            signal: status.and_then(|s| s.1),
            stdout: out_text,
            stderr: err_text,
            truncated: t1 || t2 || cut1 || cut2,
            cancelled,
        },
    );
    send(&out, &GuardMsg::Cleaned { ok });
    if ok { 0 } else { 1 }
}

/// Tests only: a guard that stops reacting once its job runs.
fn stall_for_tests() {
    if std::env::var_os("ZORA_TEST_STALL_GUARD").is_some() {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

/// Tests only: widens the window between the guard's last cancel check and the release.
fn pause_before_release() {
    if let Some(ms) = std::env::var("ZORA_TEST_PAUSE_BEFORE_RELEASE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Why the guard never released its job.
enum Unreleased {
    NoGo,
    Cancelled,
    Failed(String),
}

/// Blocks until the guard says go (one byte on fd 3), then execs the harness in place (same pid and pgid).
#[cfg(unix)]
pub fn gate_main(argv: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    let mut b = [0u8; 1];
    // SAFETY: fd 3 was set up by the guard.
    let n = unsafe { libc::read(3, b.as_mut_ptr().cast(), 1) };
    unsafe { libc::close(3) };
    if n != 1 || argv.is_empty() {
        return 125;
    }
    os::reset_signals();
    let err = Command::new(&argv[0]).args(&argv[1..]).exec();
    eprintln!("zora gate: cannot execute {}: {err}", argv[0]);
    127
}

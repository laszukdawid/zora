//! End-to-end tests against the real `zorad` binary with the scripted reference plugin.

use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use zora_core::store::Store;

const ZORAD: &str = env!("CARGO_BIN_EXE_zorad");
const PLUGIN: &str = env!("CARGO_BIN_EXE_zora-script-plugin");

struct Env {
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let home = std::env::temp_dir().join(format!("zora-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("ws")).unwrap();
        Env { home }
    }

    fn db(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home
                .join("Library/Application Support/Zora/zora.sqlite")
        } else {
            self.home.join(".local/state/zora/zora.sqlite")
        }
    }

    fn config(&self, steps: Value, harness_script: &str) -> PathBuf {
        self.config_with(steps, harness_script, json!({}))
    }

    fn config_with(&self, steps: Value, harness_script: &str, extra_plugin: Value) -> PathBuf {
        let mut plugin_cfg =
            json!({"capabilities": ["transcript", "dictation", "notify"], "steps": steps});
        if let (Some(p), Some(e)) = (plugin_cfg.as_object_mut(), extra_plugin.as_object()) {
            p.extend(e.clone());
        }
        let cfg = json!({
            "capture": {"holdBackSeconds": 0.3},
            "dictation": {"monitor": "plugin"},
            "defaultHarness": "sh",
            "harnesses": {"sh": {"type": "argv", "workspace": self.home.join("ws"), "executable": "/bin/sh",
                                 "args": ["-c", harness_script, "zora", "{prompt}"]}},
            "notifications": {"log": ["*"], "script": ["*"]},
            "plugins": {"script": {"command": [PLUGIN], "config": plugin_cfg}}
        });
        let path = self.home.join("config.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        path
    }

    fn start(&self, config: &Path) -> Child {
        Command::new(ZORAD)
            .args(["run", "--config"])
            .arg(config)
            .env("HOME", &self.home)
            .env_remove("XDG_STATE_HOME")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn commands(&self) -> Vec<(String, String, String, Option<String>)> {
        Store::open(&self.db(), true)
            .map(|s| s.commands_since(0.0))
            .unwrap_or_default()
    }

    fn jobs(&self) -> Vec<zora_core::store::JobRecord> {
        Store::open(&self.db(), true)
            .and_then(|s| s.non_final_jobs())
            .unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn wait_for<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn alive(pid: i32) -> bool {
    // SAFETY: existence check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn term_and_collect(mut child: Child) -> (Option<i32>, String) {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_for(Duration::from_secs(15), || child.try_wait().ok().flatten());
    if status.is_none() {
        let _ = child.kill();
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    // Keep the raw outcome in the log so an intermittent failure can be told apart (timeout vs signal vs code).
    use std::os::unix::process::ExitStatusExt;
    match status {
        None => err.push_str("\n[test] zorad did not exit within 15 s after SIGTERM; killed\n"),
        Some(s) => err.push_str(&format!(
            "\n[test] zorad exit: code={:?} signal={:?}\n",
            s.code(),
            s.signal()
        )),
    }
    (status.and_then(|s| s.code()), err)
}

#[test]
fn spoken_command_runs_and_service_stops_cleanly() {
    let env = Env::new();
    // Coverage starts at the first healthy heartbeat; speak after it plus pad + 1 s.
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 800}]),
        "echo \"ran: $1\"",
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (code, err) = term_and_collect(child);
    let (_, text, _, summary) = done.unwrap_or_else(|| {
        panic!(
            "no successful command; commands: {:?}; stderr:\n{err}",
            env.commands()
        )
    });
    assert_eq!(text, "say hi");
    assert_eq!(summary.as_deref(), Some("ran: say hi"));
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert!(
        err.contains("notify kind=jobSucceeded"),
        "notification not delivered to plugin; stderr:\n{err}"
    );
    assert!(
        !err.contains("say hi"),
        "transcript text leaked into logs:\n{err}"
    );
}

#[test]
fn dictated_command_never_runs() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 3500, "began": true}, {"delayMs": 300, "say": "Hey Zora delete it. Do it Zora.", "durMs": 200}, {"delayMs": 300, "ended": true}]),
        "echo ran",
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(7));
    let (_, err) = term_and_collect(child);
    assert!(
        env.commands().is_empty(),
        "a dictated command was recorded; stderr:\n{err}"
    );
    let mut s = Store::open(&env.db(), true).unwrap();
    assert_eq!(
        s.segments_since(0.0)
            .iter()
            .map(|x| x.kind.as_str())
            .collect::<Vec<_>>(),
        ["dictation"]
    );
}

fn stubborn_job(pidfile: &Path) -> String {
    // The leader spawns a TERM-ignoring descendant that records its own pid, then waits.
    format!(
        "(trap '' TERM; exec >/dev/null 2>&1; sh -c 'echo $PPID' > {}; exec sleep 120) & trap '' TERM; sleep 120",
        pidfile.display()
    )
}

#[test]
fn sigkill_of_core_mid_job_kills_the_job_and_restart_recovers() {
    let env = Env::new();
    let pidfile = env.home.join("descendant.pid");
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora long job. Do it Zora.", "durMs": 800}]),
        &stubborn_job(&pidfile),
    );
    let mut child = env.start(&cfg);
    let pid: i32 = wait_for(Duration::from_secs(20), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("job never started");
    assert!(alive(pid));
    child.kill().unwrap(); // SIGKILL the core
    let _ = child.wait();
    // The guard sees liveness EOF and reaps the group (TERM, 5 s grace, KILL).
    assert!(
        wait_for(Duration::from_secs(12), || (!alive(pid)).then_some(())).is_some(),
        "descendant {pid} survived the core"
    );

    let restarted = env.start(&env.config(json!([]), "echo ok"));
    let recovered = wait_for(Duration::from_secs(15), || {
        env.jobs().is_empty().then_some(())
    });
    let (_, err) = term_and_collect(restarted);
    assert!(
        recovered.is_some(),
        "jobs not recovered: {:?}\n{err}",
        env.jobs()
    );
    assert!(
        env.commands().iter().any(|c| c.2 == "interrupted"),
        "{:?}",
        env.commands()
    );
}

#[test]
fn guard_killed_mid_job_is_cleaned_by_the_core() {
    let env = Env::new();
    let pidfile = env.home.join("descendant.pid");
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora long job. Do it Zora.", "durMs": 800}]),
        &stubborn_job(&pidfile),
    );
    let child = env.start(&cfg);
    let pid: i32 = wait_for(Duration::from_secs(20), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("job never started");
    let guard = wait_for(Duration::from_secs(5), || env.jobs().first()?.guard_pid)
        .expect("guard not recorded") as i32;
    unsafe { libc::kill(guard, libc::SIGKILL) };
    assert!(
        wait_for(Duration::from_secs(12), || (!alive(pid)).then_some(())).is_some(),
        "descendant survived its guard"
    );
    let failed = wait_for(Duration::from_secs(5), || {
        env.commands().into_iter().find(|c| c.2 == "failed")
    });
    let (_, err) = term_and_collect(child);
    assert!(failed.is_some(), "{:?}\n{err}", env.commands());
}

#[test]
fn protocol_violation_restarts_the_plugin() {
    let env = Env::new();
    let cfg = env.config(json!([{"delayMs": 300, "raw": "{not json"}]), "echo ok");
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(4));
    let (code, err) = term_and_collect(child);
    assert!(err.contains("malformed message"), "{err}");
    // Restarted at least once with a new generation (1 s backoff).
    assert!(
        err.matches("failed: malformed message").count() >= 2,
        "{err}"
    );
    assert_eq!(code, Some(0));
    assert!(
        !err.contains("not json"),
        "raw frame leaked into logs:\n{err}"
    );
}

#[test]
fn dictation_already_running_at_connect_is_suppressed() {
    let env = Env::new();
    let cfg = env.config_with(
        json!([{"delayMs": 3500, "say": "Hey Zora delete it. Do it Zora.", "durMs": 500}, {"delayMs": 100, "quietEnd": true}]),
        "echo ran",
        json!({"capturingAtStart": true}),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(8));
    let (_, err) = term_and_collect(child);
    assert!(
        env.commands().is_empty(),
        "command ran during a dictation that predates the connection:\n{err}"
    );
}

#[test]
fn sequence_gap_drops_the_connection() {
    let env = Env::new();
    let cfg = env.config(json!([{"delayMs": 3000, "skipSeq": true}, {"delayMs": 500, "say": "Hey Zora x. Do it Zora.", "durMs": 300}]), "echo ran");
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(6));
    let (_, err) = term_and_collect(child);
    assert!(err.contains("sequence violation"), "{err}");
    assert!(env.commands().is_empty(), "{err}");
}

#[test]
fn escaped_pipe_holder_does_not_block_the_job() {
    let env = Env::new();
    let pidfile = env.home.join("escaped.pid");
    // A child in a new session keeps stdout open after the job's group is gone.
    let script = format!(
        "perl -e 'use POSIX; if (fork == 0) {{ POSIX::setsid(); open(F, \">{}\"); print F $$; close F; sleep 60; exit }} print \"hi\\n\"'",
        pidfile.display()
    );
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora go. Do it Zora.", "durMs": 500}]),
        &script,
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    if let Some(pid) = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let (_, err) = term_and_collect(child);
    let done = done.unwrap_or_else(|| panic!("job blocked by an escaped pipe holder:\n{err}"));
    assert_eq!(done.3.as_deref(), Some("hi"));
}

#[test]
fn ambiguous_job_from_an_earlier_boot_is_cleared() {
    let env = Env::new();
    {
        let mut s = Store::open(&env.db(), false).unwrap();
        s.record_command("old", "x", "sh", "queued");
        s.set_job_owner("old", "some-earlier-boot", 1, 1, 999_999, 1)
            .unwrap();
        s.set_status("old", "ambiguous");
    }
    let child = env.start(&env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora go. Do it Zora.", "durMs": 500}]),
        "echo ok",
    ));
    let ran = wait_for(Duration::from_secs(15), || {
        env.commands()
            .into_iter()
            .find(|c| c.0 != "old" && c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert!(ran.is_some(), "admission stayed closed:\n{err}");
    assert!(
        env.commands()
            .iter()
            .any(|c| c.0 == "old" && c.2 == "interrupted")
    );
}

#[test]
fn dictation_survives_a_monitor_health_flap() {
    let env = Env::new();
    let cfg = env.config_with(
        json!([
            {"delayMs": 3000, "healthNow": false},
            {"delayMs": 500, "healthNow": true},
            {"delayMs": 3500, "say": "Hey Zora delete it. Do it Zora.", "durMs": 500},
            {"delayMs": 100, "quietEnd": true}
        ]),
        "echo ran",
        json!({"capturingAtStart": true}),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(10));
    let (_, err) = term_and_collect(child);
    assert!(
        env.commands().is_empty(),
        "dictated command ran after a health flap:\n{err}"
    );
}

#[test]
fn shutdown_reaps_plugin_process_group() {
    let env = Env::new();
    let pidfile = env.home.join("plugin-child.pid");
    let cfg = env.config_with(
        json!([{"delayMs": 200, "spawnStubborn": pidfile.display().to_string()}]),
        "echo ok",
        json!({"lingerAfterStop": true}),
    );
    let child = env.start(&cfg);
    let pid: i32 = wait_for(Duration::from_secs(10), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("plugin child not started");
    let (code, err) = term_and_collect(child);
    let gone = wait_for(Duration::from_secs(2), || (!alive(pid)).then_some(()));
    if gone.is_none() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        gone.is_some(),
        "plugin descendant {pid} outlived zorad:\n{err}"
    );
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn shutdown_during_negotiation_reaps_plugin_group() {
    let env = Env::new();
    let pidfile = env.home.join("negotiating-child.pid");
    let cfg = env.config_with(
        json!([]),
        "echo ok",
        json!({"spawnBeforeReady": pidfile.display().to_string(), "withholdReady": true}),
    );
    let child = env.start(&cfg);
    let pid: i32 = wait_for(Duration::from_secs(10), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("plugin child not started");
    let (_, err) = term_and_collect(child);
    let gone = wait_for(Duration::from_secs(2), || (!alive(pid)).then_some(()));
    if gone.is_none() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        gone.is_some(),
        "negotiating plugin's descendant outlived zorad:\n{err}"
    );
}

#[test]
fn output_that_expands_past_the_frame_limit_still_finishes() {
    let env = Env::new();
    // 64 KiB of NUL on each stream: JSON escaping expands it sixfold.
    let script = "perl -e 'print \"\\0\" x 65536; print STDERR \"\\0\" x 65536'";
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora noisy. Do it Zora.", "durMs": 500}]),
        script,
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands()
            .into_iter()
            .find(|c| c.2 == "succeeded" || c.2 == "failed")
    });
    let (_, err) = term_and_collect(child);
    assert!(
        done.is_some(),
        "job never finished: {:?}\n{err}",
        env.commands()
    );
}

#[test]
fn shutdown_is_prompt_while_the_store_is_stalled() {
    let env = Env::new();
    let steps: Vec<Value> = (0..300).map(|i| json!({"delayMs": if i == 0 { 1500 } else { 5 }, "say": format!("filler sentence number {i}"), "durMs": 100})).collect();
    let cfg = env.config(json!(steps), "echo ok");
    let child = env.start(&cfg);
    // Hold the database write lock while the plugin floods transcript segments.
    let db = env.db();
    let blocker = wait_for(Duration::from_secs(10), || {
        let c = rusqlite::Connection::open(&db).ok()?;
        c.execute_batch("BEGIN EXCLUSIVE").ok()?;
        Some(c)
    })
    .expect("could not lock the store");
    std::thread::sleep(Duration::from_secs(4));
    let started = Instant::now();
    let (code, err) = term_and_collect(child);
    let took = started.elapsed();
    drop(blocker);
    assert!(
        took < Duration::from_secs(14),
        "shutdown took {took:?} behind a stalled store:\n{err}"
    );
    assert_eq!(
        code,
        Some(1),
        "a failed final flush must be a failed exit:\n{err}"
    );
}

#[test]
fn a_name_added_while_running_works_without_restart() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 6000, "say": "Hey Jarvis, say hi. Do it, Jarvis.", "durMs": 800}]),
        "echo \"ran: $1\"",
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(2));
    let out = Command::new(ZORAD)
        .args(["names", "add", "Jarvis", "--config"])
        .arg(&cfg)
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert!(err.contains("config reloaded"), "{err}");
    assert_eq!(done.map(|c| c.1).as_deref(), Some("say hi"), "{err}");
}

#[test]
fn live_text_notifies_listening_before_the_sentence_is_final() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 3500, "partial": "Hey Nova"}, {"delayMs": 1500, "say": "Hey Nova, say hi. Do it, Nova.", "durMs": 800}]),
        "echo ran",
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert!(done.is_some(), "{err}");
    // Exactly one "listening" notification, and it reached the plugin.
    assert_eq!(
        err.matches("notify kind=captureStarted").count(),
        1,
        "{err}"
    );
}

#[test]
fn stop_it_by_voice_cancels_the_running_job() {
    let env = Env::new();
    let cfg = env.config(
        json!([
            {"delayMs": 3500, "say": "Hey Zora, long job. Do it, Zora.", "durMs": 500},
            {"delayMs": 3000, "say": "Stop it, Zora.", "durMs": 500}
        ]),
        "sleep 60; echo finished",
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands()
            .into_iter()
            .find(|c| c.2 == "failed" || c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    let done = done.unwrap_or_else(|| panic!("job not stopped:\n{err}"));
    assert_eq!(
        (done.2.as_str(), done.3.as_deref()),
        ("failed", Some("cancelled")),
        "{err}"
    );
    assert!(err.contains("stop requested by voice"), "{err}");
}

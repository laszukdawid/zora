//! End-to-end tests against the real `zorad` binary with the scripted reference plugin.

use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use zora_core::store::Store;

const ZORAD: &str = env!("CARGO_BIN_EXE_zorad");
const PLUGIN: &str = env!("CARGO_BIN_EXE_zora-script-plugin");
const ACDC: &str = env!("CARGO_BIN_EXE_zora-acdc");

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
        if cfg!(windows) {
            self.home.join("AppData/Local/Zora/zora.sqlite")
        } else if cfg!(target_os = "macos") {
            self.home
                .join("Library/Application Support/Zora/zora.sqlite")
        } else {
            self.home.join(".local/state/zora/zora.sqlite")
        }
    }

    /// `zorad` with this test's home (and on Windows its profile folders).
    fn cmd(&self) -> Command {
        let mut c = Command::new(ZORAD);
        c.env("HOME", &self.home).env_remove("XDG_STATE_HOME");
        #[cfg(windows)]
        c.env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"));
        c
    }

    fn config(&self, steps: Value, harness: Script) -> PathBuf {
        self.config_with(steps, harness, json!({}))
    }

    fn config_with(&self, steps: Value, harness: Script, extra_plugin: Value) -> PathBuf {
        let mut plugin_cfg =
            json!({"capabilities": ["transcript", "dictation", "notify"], "steps": steps});
        if let (Some(p), Some(e)) = (plugin_cfg.as_object_mut(), extra_plugin.as_object()) {
            p.extend(e.clone());
        }
        let (executable, args) = harness.command();
        let cfg = json!({
            "capture": {"holdBackSeconds": 0.3},
            "dictation": {"monitor": "plugin"},
            "defaultHarness": "sh",
            "harnesses": {"sh": {"type": "argv", "workspace": self.home.join("ws"), "executable": executable,
                                 "args": args}},
            "notifications": {"log": ["*"], "script": ["*"]},
            "plugins": {"script": {"command": [PLUGIN], "config": plugin_cfg}}
        });
        let path = self.home.join("config.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        path
    }

    fn start(&self, config: &Path) -> Child {
        self.start_with(config, &[])
    }

    fn start_with(&self, config: &Path, vars: &[(&str, &str)]) -> Child {
        let mut c = self.cmd();
        c.envs(vars.iter().copied());
        c.args(["run", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        sys::own_group(&mut c);
        c.spawn().unwrap()
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

/// A harness job: a shell script on Unix; on Windows the arguments of `zora-test-harness` (src/bin/test_harness.rs).
struct Script {
    sh: String,
    win: Vec<String>,
}

fn sh(script: &str, win: &[&str]) -> Script {
    Script {
        sh: script.into(),
        win: win.iter().map(|s| s.to_string()).collect(),
    }
}

impl Script {
    /// The harness `executable` and `args`; the prompt is the script's `$1`, and the helper's last argument.
    fn command(self) -> (String, Vec<String>) {
        if cfg!(windows) {
            let mut args = self.win;
            args.push("{prompt}".into());
            (env!("CARGO_BIN_EXE_zora-test-harness").into(), args)
        } else {
            let args = ["-c", &self.sh, "zora", "{prompt}"].map(String::from);
            ("/bin/sh".into(), args.to_vec())
        }
    }
}

#[cfg(unix)]
mod sys {
    pub fn alive(pid: i32) -> bool {
        // SAFETY: existence check only.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    pub fn kill(pid: i32) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    pub fn own_group(_cmd: &mut std::process::Command) {}

    /// SIGTERM.
    pub fn stop(pid: u32) {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }

    pub fn describe(s: std::process::ExitStatus) -> String {
        use std::os::unix::process::ExitStatusExt;
        format!("code={:?} signal={:?}", s.code(), s.signal())
    }

    /// What `zorad jobs cancel` does to a guard: creates its cancel marker.
    pub fn request_cancel(_name: &str, marker: &std::path::Path) {
        std::fs::write(marker, b"").unwrap();
    }
}

#[cfg(windows)]
mod sys {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Console::{
        AllocConsole, CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, GetExitCodeProcess, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
    };

    pub fn alive(pid: i32) -> bool {
        // SAFETY: plain query on a handle closed below.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }

    pub fn kill(pid: i32) {
        // SAFETY: terminate a process by pid; the handle is closed below.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    unsafe extern "system" fn ignore_break(event: u32) -> i32 {
        (event == CTRL_BREAK_EVENT) as i32
    }

    /// zorad gets its own console process group, so Ctrl+Break reaches it and not the test runner.
    pub fn own_group(cmd: &mut std::process::Command) {
        static CONSOLE: std::sync::Once = std::sync::Once::new();
        // SAFETY: AllocConsole fails harmlessly when a console exists; the handler only returns a value.
        CONSOLE.call_once(|| unsafe {
            AllocConsole();
            SetConsoleCtrlHandler(Some(ignore_break), 1);
        });
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    /// Ctrl+Break, the Windows counterpart of SIGTERM for a console program.
    pub fn stop(pid: u32) {
        // SAFETY: no pointers involved.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    }

    pub fn describe(s: std::process::ExitStatus) -> String {
        format!("code={:?}", s.code())
    }

    /// What `zorad jobs cancel` does to a guard: sets its named cancel event.
    pub fn request_cancel(name: &str, _marker: &std::path::Path) {
        use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
        let wide: Vec<u16> = format!("Local\\zora-cancel-{name}")
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: NUL-terminated name; the handle is checked and closed.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE, 0, wide.as_ptr());
            assert!(!h.is_null(), "no cancel event for {name}");
            SetEvent(h);
            CloseHandle(h);
        }
    }
}

use sys::alive;

fn term_and_collect(mut child: Child) -> (Option<i32>, String) {
    sys::stop(child.id());
    let status = wait_for(Duration::from_secs(15), || child.try_wait().ok().flatten());
    if status.is_none() {
        let _ = child.kill();
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    // Keep the raw outcome in the log so an intermittent failure can be told apart (timeout vs signal vs code).
    match status {
        None => err.push_str("\n[test] zorad did not exit within 15 s after SIGTERM; killed\n"),
        Some(s) => err.push_str(&format!("\n[test] zorad exit: {}\n", sys::describe(s))),
    }
    (status.and_then(|s| s.code()), err)
}

#[test]
fn spoken_command_runs_and_service_stops_cleanly() {
    let env = Env::new();
    // Coverage starts at the first healthy heartbeat; speak after it plus pad + 1 s.
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 800}]),
        sh("echo \"ran: $1\"", &["ran"]),
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

impl Env {
    fn zorad(&self, args: &[&str]) {
        let ok = self
            .cmd()
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "zorad {args:?} failed");
    }
}

#[test]
fn nothing_is_heard_while_paused_and_resume_listens_again() {
    let env = Env::new();
    env.zorad(&["pause"]);
    let cfg = env.config(
        json!([
            {"delayMs": 3500, "say": "Hey Zora, secret. Do it, Zora.", "durMs": 300},
            {"delayMs": 5500, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 300}
        ]),
        sh("echo \"ran: $1\"", &["ran"]),
    );
    let child = env.start(&cfg);
    // Resume between the two sentences.
    std::thread::sleep(Duration::from_millis(4500));
    env.zorad(&["resume"]);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert!(
        err.contains("listening=false") && err.contains("listening=true"),
        "plugin was not told about the pause; stderr:\n{err}"
    );
    let commands = env.commands();
    assert!(
        done.is_some(),
        "no command after resume; {commands:?}; stderr:\n{err}"
    );
    assert_eq!(
        commands.len(),
        1,
        "a command heard while paused ran: {commands:?}"
    );
    assert_eq!(commands[0].1, "say hi");
    let mut s = Store::open(&env.db(), true).unwrap();
    assert!(
        s.segments_since(0.0)
            .iter()
            .all(|x| !x.text.contains("secret")),
        "speech heard while paused was stored"
    );
}

#[test]
fn dictated_command_never_runs() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 3500, "began": true}, {"delayMs": 300, "say": "Hey Zora delete it. Do it Zora.", "durMs": 200}, {"delayMs": 300, "ended": true}]),
        sh("echo ran", &["print", "ran"]),
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

fn stubborn_job(pidfile: &Path) -> Script {
    // The leader spawns a TERM-ignoring descendant that records its own pid, then waits.
    let path = pidfile.display().to_string();
    sh(
        &format!(
            "(trap '' TERM; exec >/dev/null 2>&1; sh -c 'echo $PPID' > {path}; exec sleep 120) & trap '' TERM; sleep 120"
        ),
        &["stubborn", &path],
    )
}

#[test]
fn sigkill_of_core_mid_job_kills_the_job_and_restart_recovers() {
    let env = Env::new();
    let pidfile = env.home.join("descendant.pid");
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora long job. Do it Zora.", "durMs": 800}]),
        stubborn_job(&pidfile),
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

    let restarted = env.start(&env.config(json!([]), sh("echo ok", &["print", "ok"])));
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
        stubborn_job(&pidfile),
    );
    let child = env.start(&cfg);
    let pid: i32 = wait_for(Duration::from_secs(20), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("job never started");
    let guard = wait_for(Duration::from_secs(5), || env.jobs().first()?.guard_pid)
        .expect("guard not recorded") as i32;
    sys::kill(guard);
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
    let cfg = env.config(
        json!([{"delayMs": 300, "raw": "{not json"}]),
        sh("echo ok", &["print", "ok"]),
    );
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
        sh("echo ran", &["print", "ran"]),
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
    let cfg = env.config(json!([{"delayMs": 3000, "skipSeq": true}, {"delayMs": 500, "say": "Hey Zora x. Do it Zora.", "durMs": 300}]), sh("echo ran", &["print", "ran"]));
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(6));
    let (_, err) = term_and_collect(child);
    assert!(err.contains("sequence violation"), "{err}");
    assert!(env.commands().is_empty(), "{err}");
}

/// Unix only: nothing in a Windows job can leave it (no breakaway), so no process can escape holding the pipe.
#[cfg(unix)]
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
        sh(&script, &[]),
    );
    let child = env.start(&cfg);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    if let Some(pid) = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
    {
        sys::kill(pid);
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
        sh("echo ok", &["print", "ok"]),
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
        sh("echo ran", &["print", "ran"]),
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
        sh("echo ok", &["print", "ok"]),
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
        sys::kill(pid);
    }
    assert!(
        gone.is_some(),
        "plugin descendant {pid} outlived zorad:\n{err}"
    );
    assert_eq!(code, Some(0), "{err}");
}

/// Children are reaped by zorad, not the kernel, even when zorad inherits SIGCHLD ignored: only an unreaped
/// leader keeps a group id from being reused, so the plugin's group is still cleaned up.
#[cfg(unix)]
#[test]
fn shutdown_reaps_plugin_process_group_with_sigchld_ignored() {
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let pidfile = env.home.join("plugin-child.pid");
    let cfg = env.config_with(
        json!([{"delayMs": 200, "spawnStubborn": pidfile.display().to_string()}]),
        sh("echo ok", &["print", "ok"]),
        json!({"lingerAfterStop": true}),
    );
    let mut c = env.cmd();
    c.args(["run", "--config"])
        .arg(&cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: only an async-signal-safe call between fork and exec.
    unsafe {
        c.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = c.spawn().unwrap();
    let pid: i32 = wait_for(Duration::from_secs(10), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    })
    .expect("plugin child not started");
    let (code, err) = term_and_collect(child);
    let gone = wait_for(Duration::from_secs(2), || (!alive(pid)).then_some(()));
    if gone.is_none() {
        sys::kill(pid);
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
        sh("echo ok", &["print", "ok"]),
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
        sys::kill(pid);
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
        sh(script, &["noisy"]),
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
    let cfg = env.config(json!(steps), sh("echo ok", &["print", "ok"]));
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
        sh("echo \"ran: $1\"", &["ran"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(2));
    let out = env
        .cmd()
        .args(["names", "add", "Jarvis", "--config"])
        .arg(&cfg)
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
        sh("echo ran", &["print", "ran"]),
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
        sh("sleep 60; echo finished", &["sleep", "60", "finished"]),
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

#[test]
fn jobs_cancel_stops_the_harness_gracefully_and_records_it() {
    let env = Env::new();
    let marker = env.home.join("stopped");
    let m = marker.display().to_string();
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora, long job. Do it, Zora.", "durMs": 500}]),
        sh(
            &format!("trap 'echo stopped > {m}; exit 0' TERM; sleep 60 & wait"),
            &["graceful", &m],
        ),
    );
    let child = env.start(&cfg);
    let id = wait_for(Duration::from_secs(20), || {
        let j = env.jobs().into_iter().next()?;
        (j.status == "running").then_some(j.id)
    });
    // The harness needs a moment to set up its handler after it starts.
    std::thread::sleep(Duration::from_millis(1500));
    let out = id
        .as_ref()
        .map(|id| env.cmd().args(["jobs", "cancel", id]).output().unwrap());
    let done = wait_for(Duration::from_secs(15), || {
        env.commands()
            .into_iter()
            .find(|c| c.2 == "failed" || c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    let out = out.unwrap_or_else(|| panic!("job never ran:\n{err}"));
    assert!(
        out.status.success(),
        "{}\n{err}",
        String::from_utf8_lossy(&out.stderr)
    );
    let done = done.unwrap_or_else(|| panic!("job not stopped:\n{err}"));
    assert_eq!(
        (done.2.as_str(), done.3.as_deref()),
        ("failed", Some("cancelled")),
        "{err}"
    );
    // TERM (Ctrl+Break on Windows) reached the harness before anything was killed.
    assert!(marker.exists(), "the harness never saw TERM:\n{err}");
}

#[test]
fn a_new_project_folder_applies_without_restart() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 6000, "say": "Hey Zora, where. Do it, Zora.", "durMs": 800}]),
        sh("pwd", &["pwd"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(2));
    let other = env.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let change = json!({"workspace": other}).to_string();
    let out = env
        .cmd()
        .args(["settings", "set", &change, "--config"])
        .arg(&cfg)
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
    let summary = done.and_then(|c| c.3).unwrap_or_default();
    let ran_in = std::fs::canonicalize(summary.trim()).unwrap_or_default();
    assert_eq!(ran_in, std::fs::canonicalize(&other).unwrap(), "{err}");
}

#[test]
fn a_custom_agent_chosen_in_settings_applies_without_restart() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 6000, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 800}]),
        sh("echo old", &["print", "old"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_secs(2));
    let (exe, args) = sh("echo \"ran: $1\"", &["ran"]).command();
    // The settings place the prompt themselves when the command leaves it out.
    let args: Vec<String> = args.into_iter().filter(|a| a != "{prompt}").collect();
    let line = zora_core::config::join_command(&[vec![exe], args].concat());
    let change = json!({"harness": "custom", "customCommand": line}).to_string();
    let out = env
        .cmd()
        .args(["settings", "set", &change, "--config"])
        .arg(&cfg)
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
    let summary = done.and_then(|c| c.3).unwrap_or_default();
    assert!(summary.starts_with("ran: "), "{summary:?}\n{err}");
}

#[test]
fn speech_from_the_pause_that_arrives_after_resume_is_dropped() {
    let env = Env::new();
    env.zorad(&["pause"]);
    let cfg = env.config(
        json!([
            // Spoken at about 2.7-3 s after the plugin starts, while paused (resume comes at 6 s, leaving room for a
            // slow start under load); delivered at 10 s, after the resume.
            {"delayMs": 10000, "say": "Hey Zora, secret. Do it, Zora.", "durMs": 300, "agoMs": 7000},
            {"delayMs": 1000, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 300}
        ]),
        sh("echo \"ran: $1\"", &["ran"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(3500));
    env.zorad(&["resume"]);
    let done = wait_for(Duration::from_secs(20), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    let commands = env.commands();
    assert!(
        done.is_some(),
        "no command after resume; {commands:?}; stderr:\n{err}"
    );
    assert_eq!(commands.len(), 1, "speech from the pause ran: {commands:?}");
    assert_eq!(commands[0].1, "say hi");
}

impl Env {
    /// `zorad settings set <change>` on `cfg`: exit code and stderr.
    fn settings_set(&self, cfg: &Path, change: Value) -> (Option<i32>, String) {
        let out = self
            .cmd()
            .args(["settings", "set", &change.to_string(), "--config"])
            .arg(cfg)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn settings_set_changes_only_the_requested_keys() {
    let env = Env::new();
    let cfg = env.config(json!([]), sh("true", &["print", "ok"]));
    let mut doc = read_json(&cfg);
    doc["futureKey"] = json!({"from": "a newer zorad"});
    std::fs::write(&cfg, serde_json::to_vec(&doc).unwrap()).unwrap();
    let (code, err) = env.settings_set(&cfg, json!({"wakeNames": ["Zora", "Jarvis"]}));
    assert_eq!(code, Some(0), "{err}");
    let after = read_json(&cfg);
    assert_eq!(after["wakeNames"], json!(["zora", "jarvis"]));
    doc["wakeNames"] = after["wakeNames"].clone();
    assert_eq!(after, doc, "keys other than wakeNames changed");
}

#[test]
fn a_settings_change_based_on_stale_values_is_refused() {
    let env = Env::new();
    let cfg = env.config(json!([]), sh("true", &["print", "ok"]));
    let before = std::fs::read(&cfg).unwrap();
    let (code, err) = env.settings_set(
        &cfg,
        json!({"wakeNames": ["zora", "jarvis"], "expect": {"wakeNames": ["zora"]}}),
    );
    assert_eq!(code, Some(3), "{err}");
    assert!(err.contains("wakeNames changed"), "{err}");
    assert_eq!(
        std::fs::read(&cfg).unwrap(),
        before,
        "a refused change wrote"
    );
    // The values actually loaded go through.
    let (code, err) = env.settings_set(
        &cfg,
        json!({"wakeNames": ["zora", "jarvis"], "expect": {"wakeNames": ["zora", "nova"]}}),
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(read_json(&cfg)["wakeNames"], json!(["zora", "jarvis"]));
}

#[test]
fn concurrent_config_writers_do_not_lose_changes() {
    let env = Env::new();
    let cfg = env.config(json!([]), sh("true", &["print", "ok"]));
    let names: Vec<String> = (0..8)
        .map(|i| format!("name{}", "abcdefgh".as_bytes()[i] as char))
        .collect();
    let children: Vec<Child> = names
        .iter()
        .map(|n| {
            env.cmd()
                .args(["names", "add", n, "--config"])
                .arg(&cfg)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let have = read_json(&cfg)["wakeNames"].clone();
    for n in &names {
        assert!(
            have.as_array().unwrap().contains(&json!(n)),
            "{n} lost: {have}"
        );
    }
}

#[test]
fn first_run_default_config_never_replaces_a_concurrent_change() {
    let env = Env::new();
    let cfg = env.home.join("fresh").join("config.json");
    for round in 0..3 {
        let _ = std::fs::remove_file(&cfg);
        // Readers that create the default file race writers that add names.
        let children: Vec<(Option<String>, Child)> = (0..8)
            .map(|i| {
                let name = (i % 2 == 0).then(|| format!("round{round}name{i}"));
                let mut c = env.cmd();
                match &name {
                    Some(n) => c.args(["names", "add", n.as_str()]),
                    None => c.args(["settings"]),
                };
                let child = c
                    .arg("--config")
                    .arg(&cfg)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                (name, child)
            })
            .collect();
        let mut names = vec![];
        for (n, mut c) in children {
            assert!(c.wait().unwrap().success());
            names.extend(n);
        }
        let have = read_json(&cfg)["wakeNames"].clone();
        for n in &names {
            assert!(
                have.as_array().unwrap().contains(&json!(n)),
                "{n} lost: {have}"
            );
        }
    }
}

#[test]
fn a_workspace_edit_for_another_harness_is_refused() {
    let env = Env::new();
    let cfg = env.config(json!([]), sh("true", &["print", "ok"]));
    let before = std::fs::read(&cfg).unwrap();
    let ws = env.home.join("ws").display().to_string();
    // Loaded while "claude" was the default; the file's default is now "sh".
    let (code, err) = env.settings_set(
        &cfg,
        json!({"workspace": ws, "expect": {"harness": "claude", "workspace": ws}}),
    );
    assert_eq!(code, Some(3), "{err}");
    assert!(err.contains("harness"), "{err}");
    assert_eq!(std::fs::read(&cfg).unwrap(), before);
    let (code, err) = env.settings_set(
        &cfg,
        json!({"workspace": ws, "expect": {"harness": "sh", "workspace": ws}}),
    );
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn live_text_from_before_a_pause_does_not_notify_after_resume() {
    let env = Env::new();
    let cfg = env.config(
        // Tagged with epoch 1 (before the pause), delivered after the resume (epoch 3).
        json!([{"delayMs": 7000, "partial": "Hey Nova", "epoch": 1}]),
        sh("echo ran", &["print", "ran"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(1500));
    env.zorad(&["pause"]);
    std::thread::sleep(Duration::from_millis(2000));
    env.zorad(&["resume"]);
    std::thread::sleep(Duration::from_millis(6000));
    let (_, err) = term_and_collect(child);
    assert!(err.contains("listening=true epoch=3"), "{err}");
    assert!(err.contains("pause confirmed=true"), "{err}");
    assert!(!err.contains("notify kind=captureStarted"), "{err}");
}

#[test]
fn a_plugin_without_pause_support_is_stopped_while_paused_and_restarted_on_resume() {
    let env = Env::new();
    let cfg = env.config_with(
        // Each connection replays the script: the first is stopped before speaking, the restarted one speaks.
        json!([{"delayMs": 6000, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 800}]),
        sh("echo \"ran: $1\"", &["ran"]),
        json!({"listeningCapability": false}),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(1000));
    env.zorad(&["pause"]);
    std::thread::sleep(Duration::from_millis(5000));
    env.zorad(&["resume"]);
    let done = wait_for(Duration::from_secs(25), || {
        env.commands().into_iter().find(|c| c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert!(
        err.contains("plugin script can't confirm its microphone is off"),
        "{err}"
    );
    assert_eq!(err.matches("listening=true epoch=1 ").count(), 2, "{err}");
    assert!(!err.contains("listening=false"), "{err}");
    assert_eq!(done.map(|c| c.1).as_deref(), Some("say hi"), "{err}");
}

/// Asserts that the needles appear in `err` in this order.
fn in_order(err: &str, needles: &[&str]) {
    let mut from = 0;
    for n in needles {
        match err[from..].find(n) {
            Some(i) => from += i + n.len(),
            None => panic!("{n:?} missing or out of order in:\n{err}"),
        }
    }
}

#[test]
fn a_late_confirmation_clears_pause_not_confirmed() {
    let env = Env::new();
    // Confirms after the 5 s report but before the 8 s stop deadline.
    let cfg = env.config_with(
        json!([]),
        sh("true", &["print", "ok"]),
        json!({"confirmDelayMs": 6500}),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(1500));
    env.zorad(&["pause"]);
    std::thread::sleep(Duration::from_millis(9000));
    env.zorad(&["resume"]);
    std::thread::sleep(Duration::from_millis(2000));
    let (_, err) = term_and_collect(child);
    in_order(
        &err,
        &[
            "pause confirmed=false",
            "late mic epoch=2",
            "pause confirmed=true",
            // The same connection carries on: a restarted one would start again at epoch 1.
            "listening=true epoch=3 ",
        ],
    );
    assert_eq!(
        err.matches("did not confirm its microphone is off").count(),
        1,
        "{err}"
    );
    assert_eq!(err.matches("listening=true epoch=1 ").count(), 1, "{err}");
}

#[test]
fn a_microphone_reopened_while_paused_unconfirms_the_pause() {
    let env = Env::new();
    let cfg = env.config(
        json!([{"delayMs": 6000, "mic": true}]),
        sh("true", &["print", "ok"]),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(1500));
    env.zorad(&["pause"]);
    std::thread::sleep(Duration::from_millis(7000));
    let (_, err) = term_and_collect(child);
    // Confirmed, reopened (reported at once), then closed again when the plugin is stopped and reaped.
    in_order(
        &err,
        &[
            "pause confirmed=true",
            "plugin script opened its microphone while paused",
            "pause confirmed=false",
            "pause confirmed=true",
        ],
    );
}

#[test]
fn a_plugin_that_never_confirms_a_pause_is_stopped_and_reported() {
    let env = Env::new();
    let cfg = env.config_with(
        json!([]),
        sh("true", &["print", "ok"]),
        json!({"confirmPause": false}),
    );
    let child = env.start(&cfg);
    std::thread::sleep(Duration::from_millis(1500));
    env.zorad(&["pause"]);
    // The 8 s stop deadline, plus up to a second to notice the pause and time to reap.
    std::thread::sleep(Duration::from_millis(11000));
    env.zorad(&["resume"]);
    std::thread::sleep(Duration::from_millis(3000));
    let (_, err) = term_and_collect(child);
    // Reported at 5 s; stopped on the plugin's own deadline, and the verified reap confirms the pause.
    in_order(
        &err,
        &[
            "plugin script did not confirm its microphone is off",
            "pause confirmed=false",
            "pause confirmed=true",
        ],
    );
    // Stopped, started again on resume.
    assert_eq!(err.matches("listening=true epoch=1 ").count(), 2, "{err}");
}

/// Requests the fake board saw: (path, Host header, JSON body).
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

/// A stand-in for the ACDC board that records each request and answers like the board.
fn fake_board() -> (u16, Seen) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let (mut host, mut len) = (String::new(), 0usize);
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("host:") {
                    host = v.trim().into();
                }
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let answer = if path == "/api/tasks" {
                json!({"ok": true, "id": "task-1"})
            } else {
                json!({"ok": true})
            }
            .to_string();
            log.lock().unwrap().push((path, host, body));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (port, seen)
}

#[test]
fn acdc_plugin_puts_the_command_on_the_board() {
    let (port, seen) = fake_board();
    let env = Env::new();
    let path = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora, say hi. Do it, Zora.", "durMs": 800}]),
        sh("echo \"ran: $1\"", &["ran"]),
    );
    let mut cfg: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    cfg["plugins"]["acdc"] = json!({"command": [ACDC],
                                    "config": {"url": format!("http://127.0.0.1:{port}/api"), "workspace": "w-test"}});
    cfg["notifications"]["acdc"] =
        json!(["commandQueued", "jobStarted", "jobSucceeded", "jobFailed"]);
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    let child = env.start(&path);
    let reviewed = wait_for(Duration::from_secs(20), || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|(p, _, b)| p == "/api/tasks/task-1/status" && b["status"] == "review")
            .then_some(())
    });
    let (code, err) = term_and_collect(child);
    let seen = seen.lock().unwrap().clone();
    assert!(
        reviewed.is_some(),
        "no review status; requests: {seen:#?}\nstderr:\n{err}"
    );
    assert_eq!(code, Some(0), "stderr:\n{err}");
    let paths: Vec<&str> = seen
        .iter()
        .map(|(p, _, _)| p.as_str())
        .filter(|p| *p != "/api/sources/poke")
        .collect();
    assert_eq!(
        paths,
        [
            "/api/agents/zora",
            "/api/tasks",
            "/api/tasks/task-1/claim",
            "/api/tasks/task-1/status",
            "/api/agents/zora/bye"
        ],
        "{seen:#?}\nstderr:\n{err}"
    );
    let create = &seen.iter().find(|(p, _, _)| p == "/api/tasks").unwrap().2;
    assert_eq!(create["title"], "say hi");
    assert_eq!(create["workspace"], "w-test");
    let status = &seen
        .iter()
        .find(|(p, _, _)| p.ends_with("/status"))
        .unwrap()
        .2;
    assert!(
        status["note"].as_str().unwrap().contains("ran: say hi"),
        "{status}"
    );
    // The board only accepts its own Host header.
    assert!(
        seen.iter()
            .all(|(_, h, _)| *h == format!("127.0.0.1:{port}"))
    );
    assert!(
        !err.contains("say hi"),
        "command text leaked into logs:\n{err}"
    );
}

#[test]
fn an_invalid_config_exits_with_ex_config_and_settings_lists_the_problem() {
    let env = Env::new();
    let cfg = env.config(json!([]), sh("true", &["print", "ok"]));
    std::fs::remove_dir_all(env.home.join("ws")).unwrap();
    let mut child = env.start(&cfg);
    let status = wait_for(Duration::from_secs(10), || child.try_wait().ok().flatten());
    assert_eq!(status.and_then(|s| s.code()), Some(78));
    let out = env
        .cmd()
        .args(["settings", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["problems"][0].as_str().unwrap().contains("workspace"),
        "{v}"
    );
    assert_eq!(v["locale"], "en-US");
    assert!(v["dictationBundleIDs"].is_array());
    // A dry run needs no project folder, and checking for one doesn't save the flag.
    let before = std::fs::read(&cfg).unwrap();
    let out = env
        .cmd()
        .args(["settings", "--dry-run", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["problems"], json!([]), "{v}");
    assert_eq!(std::fs::read(&cfg).unwrap(), before);
}

/// `zorad __guard` driven directly, as the core drives it.
struct DirectGuard {
    child: Child,
    to: std::process::ChildStdin,
    from: std::sync::mpsc::Receiver<Value>,
    name: String,
    marker: PathBuf,
}

impl DirectGuard {
    /// Starts a guard for a harness that creates `started` and then, with `wait`, keeps running.
    fn start(env: &Env, started: &Path, wait: bool, pause_ms: Option<u64>) -> DirectGuard {
        use std::io::{BufRead, BufReader, Write};
        let argv: Vec<String> = if cfg!(windows) {
            let mode = if wait { "touch-wait" } else { "touch" };
            vec![
                env!("CARGO_BIN_EXE_zora-test-harness").into(),
                mode.into(),
                started.display().to_string(),
            ]
        } else {
            let then = if wait { "; sleep 30" } else { "" };
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("echo started > {}{then}", started.display()),
            ]
        };
        let mut cmd = Command::new(ZORAD);
        cmd.arg("__guard")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(ms) = pause_ms {
            cmd.env("ZORA_TEST_PAUSE_BEFORE_RELEASE_MS", ms.to_string());
        }
        let mut child = cmd.spawn().unwrap();
        let mut to = child.stdin.take().unwrap();
        let (tx, from) = std::sync::mpsc::channel::<Value>();
        let lines = BufReader::new(child.stdout.take().unwrap());
        std::thread::spawn(move || {
            for line in lines.lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str(&line).unwrap_or(Value::Null));
            }
        });
        let name = uuid::Uuid::new_v4().to_string();
        let marker = env.home.join("cancel").join(&name);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let spec = json!({"t": "spec", "argv": argv, "cwd": env.home.join("ws"),
                          "env": std::env::vars().collect::<Vec<_>>(), "capture_limit": 4096,
                          "kill_grace_ms": 1000, "name": name, "cancel_marker": marker});
        writeln!(to, "{spec}").unwrap();
        let registered = from.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(registered["t"], "registered", "{registered}");
        DirectGuard {
            child,
            to,
            from,
            name,
            marker,
        }
    }

    fn go(&mut self) {
        use std::io::Write;
        writeln!(self.to, "{}", json!({"t": "go"})).unwrap();
    }

    fn cancel(&self) {
        sys::request_cancel(&self.name, &self.marker);
    }

    /// Every message until `cleaned`.
    fn finish(mut self) -> Vec<Value> {
        let mut seen = vec![];
        while let Ok(m) = self.from.recv_timeout(Duration::from_secs(20)) {
            let done = m["t"] == "cleaned";
            seen.push(m);
            if done {
                break;
            }
        }
        let _ = self.child.wait();
        seen
    }
}

fn count(seen: &[Value], t: &str) -> usize {
    seen.iter().filter(|m| m["t"] == t).count()
}

#[test]
fn a_job_cancelled_before_go_never_starts() {
    let env = Env::new();
    let started = env.home.join("started");
    let mut g = DirectGuard::start(&env, &started, false, None);
    g.cancel();
    g.go();
    let seen = g.finish();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !started.exists(),
        "the harness ran after a cancel: {seen:?}"
    );
    assert_eq!(count(&seen, "exited"), 1, "{seen:?}");
    assert!(
        seen.iter()
            .any(|m| m["t"] == "exited" && m["cancelled"] == true),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| m["t"] == "cleaned" && m["ok"] == true),
        "{seen:?}"
    );
}

#[test]
fn a_cancel_racing_the_release_has_one_outcome() {
    let env = Env::new();
    let started = env.home.join("started");
    // The guard checks for a cancel, then pauses before releasing; the cancel lands in that pause.
    let mut g = DirectGuard::start(&env, &started, true, Some(1500));
    let t0 = Instant::now();
    g.go();
    std::thread::sleep(Duration::from_millis(500));
    g.cancel();
    let seen = g.finish();
    // Released, then stopped by the guard itself: one exit, recorded cancelled, group gone, well before the
    // harness's 30 s.
    assert_eq!(count(&seen, "exited"), 1, "{seen:?}");
    assert!(
        seen.iter()
            .any(|m| m["t"] == "exited" && m["cancelled"] == true),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| m["t"] == "cleaned" && m["ok"] == true),
        "{seen:?}"
    );
    assert_eq!(count(&seen, "cancelling"), 1, "{seen:?}");
    assert!(t0.elapsed() < Duration::from_secs(15), "{seen:?}");
}

#[test]
fn clear_and_recovery_remove_cancel_markers() {
    let env = Env::new();
    let dir = env.db().parent().unwrap().join("cancel");
    {
        let mut s = Store::open(&env.db(), false).unwrap();
        s.record_command("old", "x", "sh", "queued");
        s.set_job_owner("old", "some-earlier-boot", 1, 1, 999_999, 1)
            .unwrap();
        s.record_command("stuck", "y", "sh", "queued");
        s.set_status("stuck", "ambiguous");
    }
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // As `zorad jobs cancel` makes the folder (os.rs OWNER_FILE).
    std::fs::write(
        dir.join(".zora-cancel-markers"),
        b"zora cancel markers v1\n",
    )
    .unwrap();
    let orphan = uuid::Uuid::new_v4().to_string();
    for id in ["old", "stuck", &orphan, "notes"] {
        std::fs::write(dir.join(id), b"").unwrap();
    }
    env.zorad(&["jobs", "clear", "stuck"]);
    assert!(!dir.join("stuck").exists(), "jobs clear left its marker");
    let child = env.start(&env.config(json!([]), sh("echo ok", &["print", "ok"])));
    // Orphans are pruned on Unix only (Windows keeps no marker files).
    let recovered = wait_for(Duration::from_secs(15), || {
        (!dir.join("old").exists() && (cfg!(windows) || !dir.join(&orphan).exists())).then_some(())
    });
    let (_, err) = term_and_collect(child);
    assert!(recovered.is_some(), "markers left after recovery:\n{err}");
    assert!(dir.join("notes").exists(), "an unrelated file was pruned");
}

#[test]
fn jobs_cancel_ends_a_job_whose_guard_stopped_reacting() {
    let env = Env::new();
    let pidfile = env.home.join("descendant.pid");
    let cfg = env.config(
        json!([{"delayMs": 3500, "say": "Hey Zora, long job. Do it, Zora.", "durMs": 500}]),
        stubborn_job(&pidfile),
    );
    let child = env.start_with(&cfg, &[("ZORA_TEST_STALL_GUARD", "1")]);
    let pid: Option<i32> = wait_for(Duration::from_secs(20), || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    });
    let id = env.jobs().first().map(|j| j.id.clone());
    let cancelled = id.as_ref().map(|id| {
        env.cmd()
            .args(["jobs", "cancel", id])
            .output()
            .unwrap()
            .status
            .success()
    });
    let t0 = Instant::now();
    // The core sees the request itself, gives the guard its bounded time, then kills it and cleans up.
    let done = wait_for(Duration::from_secs(60), || {
        env.commands()
            .into_iter()
            .find(|c| c.2 == "failed" || c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    assert_eq!(cancelled, Some(true), "{err}");
    let pid = pid.unwrap_or_else(|| panic!("job never started:\n{err}"));
    let done = done.unwrap_or_else(|| panic!("job never ended: {:?}\n{err}", env.commands()));
    assert_eq!(
        (done.2.as_str(), done.3.as_deref()),
        ("failed", Some("cancelled")),
        "{err}"
    );
    assert!(t0.elapsed() < Duration::from_secs(60), "{err}");
    assert!(!alive(pid), "descendant {pid} survived:\n{err}");
    assert!(env.jobs().is_empty(), "{:?}\n{err}", env.jobs());
}

#[test]
fn stop_it_during_ownership_registration_never_runs_the_job() {
    let env = Env::new();
    let started = env.home.join("started");
    let s = started.display().to_string();
    let cfg = env.config(
        json!([
            {"delayMs": 3500, "say": "Hey Zora, long job. Do it, Zora.", "durMs": 500},
            {"delayMs": 2000, "say": "Stop it, Zora.", "durMs": 500}
        ]),
        sh(&format!("echo started > {s}"), &["touch", &s]),
    );
    // The core holds the job between its ownership write and Go; "stop it" lands there.
    let child = env.start_with(&cfg, &[("ZORA_TEST_PAUSE_AFTER_OWNER_MS", "5000")]);
    let done = wait_for(Duration::from_secs(25), || {
        env.commands()
            .into_iter()
            .find(|c| c.2 == "failed" || c.2 == "succeeded")
    });
    let (_, err) = term_and_collect(child);
    let done = done.unwrap_or_else(|| panic!("job never ended:\n{err}"));
    assert_eq!(
        (done.2.as_str(), done.3.as_deref()),
        ("failed", Some("cancelled")),
        "{err}"
    );
    assert!(!started.exists(), "the harness ran:\n{err}");
}

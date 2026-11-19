//! Process identity and process-group control (macOS and Linux). Windows: os_windows.rs.

use crate::guard::JobSpec;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Identifier of the current boot; process ids and start times are only comparable within one boot.
pub fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        let name = c"kern.bootsessionuuid";
        let mut buf = [0u8; 128];
        let mut len = buf.len();
        // SAFETY: buffer and length are valid; sysctlbyname writes at most `len` bytes.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return String::new();
        }
        String::from_utf8_lossy(&buf[..len])
            .trim_end_matches('\0')
            .to_string()
    }
}

/// Process start time in an OS-specific unit (macOS: microseconds since epoch; Linux: clock ticks since boot).
pub fn start_time(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesised command name; starttime is field 22 overall.
        let rest = &stat[stat.rfind(')')? + 2..];
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: info is a properly sized, writable proc_bsdinfo.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        Some(info.pbi_start_tvsec as i64 * 1_000_000 + info.pbi_start_tvusec as i64)
    }
}

pub fn group_empty(pgid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    pgid > 0
        && unsafe { libc::kill(-pgid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

pub fn signal_group(pgid: i32, sig: i32) {
    if pgid > 0 {
        // SAFETY: negative pid targets the process group.
        unsafe { libc::kill(-pgid, sig) };
    }
}

/// TERM the group, wait `grace`, KILL, wait up to 2 s. Returns true once the group is empty. Blocking.
pub fn reap_group_blocking(pgid: i32, grace: Duration) -> bool {
    if group_empty(pgid) {
        return true;
    }
    signal_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if group_empty(pgid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal_group(pgid, libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if group_empty(pgid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    group_empty(pgid)
}

/// Resets every catchable signal to its default and unblocks all signals (before exec of a harness).
pub fn reset_signals() {
    // SAFETY: plain libc calls on valid arguments.
    unsafe {
        for sig in 1..32 {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
}

pub fn ignore_termination_signals() {
    // SAFETY: setting dispositions for standard signals.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}

/// A job's process group: the harness and everything it starts.
#[derive(Debug, Clone)]
pub struct Group(i32);

impl Group {
    pub fn id(&self) -> i32 {
        self.0
    }

    pub fn term(&self) {
        signal_group(self.0, libc::SIGTERM);
    }

    pub fn kill(&self) {
        signal_group(self.0, libc::SIGKILL);
    }

    pub fn reap_blocking(&self, grace: Duration) -> bool {
        reap_group_blocking(self.0, grace)
    }
}

/// The gate, started in a new process group and blocked on its go-pipe until `release`.
pub struct Held {
    pub child: Child,
    pub group: Group,
    go: Option<OwnedFd>,
}

impl Held {
    pub fn release(&mut self) {
        if let Some(go) = self.go.take() {
            let _ = std::fs::File::from(go).write_all(b"g");
        }
    }
}

pub fn spawn_held(spec: &JobSpec) -> Result<Held, String> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid 2-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err("pipe failed".into());
    }
    // SAFETY: pipe() returned two new descriptors we now own; CLOEXEC keeps them out of the harness (dup2 onto
    // fd 3 in the gate clears the flag for that copy only).
    let (go_read, go_write) = unsafe {
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    };
    let go_read_raw = go_read.as_raw_fd();
    let go_write_raw = go_write.as_raw_fd();

    let exe = std::env::current_exe().unwrap_or_else(|_| "zorad".into());
    let mut args = vec!["__gate".to_string(), "--".to_string()];
    args.extend(spec.argv.iter().cloned());
    let mut cmd = crate::guard::command(exe.as_os_str(), &args, spec);
    cmd.process_group(0);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::close(go_write_raw);
            if libc::dup2(go_read_raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2(3, 3) is a no-op that keeps CLOEXEC (pipe() often returns fd 3), so clear it explicitly.
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("cannot start gate: {e}"))?;
    drop(go_read);
    let group = Group(child.id() as i32);
    Ok(Held {
        child,
        group,
        go: Some(go_write),
    })
}

/// Starts the job guard in its own process group, out of reach of signals meant for the core's group.
pub fn detach(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

/// Kills a detached process's group (the guard's group holds only the guard).
pub fn kill_detached(pid: i32) {
    signal_group(pid, libc::SIGKILL);
}

/// Starts a plugin as the leader of its own process group.
pub fn spawn_grouped(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<(tokio::process::Child, Group)> {
    let child = cmd.process_group(0).spawn()?;
    let pgid = child.id().unwrap_or(0) as i32;
    Ok((child, Group(pgid)))
}

pub fn job_gone(_id: &str, pgid: i32) -> bool {
    group_empty(pgid)
}

/// Cleans a job group only when its leader is provably ours (same pid and start time). Blocking.
pub fn reap_job(_id: &str, pgid: i32, gate_start: i64, grace: Duration) -> bool {
    if group_empty(pgid) {
        return true;
    }
    if start_time(pgid) == Some(gate_start) {
        return reap_group_blocking(pgid, grace);
    }
    false
}

/// TERMs a job group only when it is provably the job's (leader pid == pgid with the recorded start time); its
/// guard then reaps the group and the core records the outcome.
pub fn cancel_job(_id: &str, pgid: i32, gate_start: i64) -> bool {
    if start_time(pgid) == Some(gate_start) {
        signal_group(pgid, libc::SIGTERM);
        return true;
    }
    false
}

/// SIGTERM and SIGINT stop the service in order.
pub fn on_shutdown(f: impl Fn() + Send + Sync + Clone + 'static) {
    for kind in [
        tokio::signal::unix::SignalKind::terminate(),
        tokio::signal::unix::SignalKind::interrupt(),
    ] {
        let f = f.clone();
        if let Ok(mut sig) = tokio::signal::unix::signal(kind) {
            tokio::spawn(async move {
                while sig.recv().await.is_some() {
                    f();
                }
            });
        }
    }
}

/// macOS inverted launch: the app gave us fd 3 (read) and fd 4 (write).
pub fn attached_plugin() -> Result<
    (
        tokio::net::unix::pipe::Receiver,
        tokio::net::unix::pipe::Sender,
    ),
    String,
> {
    // SAFETY: the parent app passes these descriptors to us; nothing else owns them.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(3), OwnedFd::from_raw_fd(4)) };
    let r = tokio::net::unix::pipe::Receiver::from_owned_fd(r);
    let w = tokio::net::unix::pipe::Sender::from_owned_fd(w);
    match (r, w) {
        (Ok(r), Ok(w)) => Ok((r, w)),
        _ => Err("fds 3/4 are not pipes".into()),
    }
}

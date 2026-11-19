//! Process identity and process-group control (macOS and Linux). Windows: os_windows.rs.

use crate::guard::JobSpec;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
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

pub enum Owner {
    Here,
    OtherBoot,
    /// Recorded somewhere this process can't check (another Windows session); never on Unix.
    #[allow(dead_code)]
    Elsewhere(String),
}

/// Where a job recorded with `recorded` (its `boot_id`) ran. A different boot means none of its processes can
/// still exist.
pub fn owner(recorded: Option<&str>) -> Owner {
    match recorded {
        Some(b) if b != boot_id() => Owner::OtherBoot,
        _ => Owner::Here,
    }
}

/// A job's process group: the harness and everything it starts.
#[derive(Debug, Clone)]
pub struct Group(i32, Option<PathBuf>);

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

    /// Ends the group after its leader exited (see `Held::wait_exit`): the rest of the group is stopped while the
    /// leader still pins the id, then the leader is reaped. True once the group is verifiably empty; also the
    /// leader's (exit code, signal).
    pub fn finish_blocking(&self, grace: Duration, leader: Leader) -> (bool, ExitInfo) {
        // Without a pinned leader the id may already be someone else's: check only.
        if leader.pinned {
            stop_pinned_group_blocking(self.0, grace);
        }
        let mut child = leader.child;
        let status = child.wait().ok().map(|s| (s.code(), s.signal()));
        (wait_group_empty(self.0, Duration::from_secs(2)), status)
    }

    /// For a group that already failed to reap: only checks whether it is gone, never signals it, since its
    /// number may since belong to an unrelated group.
    pub fn leader_waitable(&self) -> bool {
        wait_exit_unreaped(self.0, Some(Duration::ZERO)).is_some()
    }

    pub fn retry_reap(&self) -> bool {
        group_empty(self.0)
    }

    /// Whether `zorad jobs cancel` left this job's cancel marker.
    pub fn cancel_requested(&self) -> bool {
        self.1.as_deref().is_some_and(marker_present)
    }
}

/// The gate, started in a new process group and blocked on its go-pipe until `release`.
pub struct Held {
    child: Child,
    pub group: Group,
    go: Option<OwnedFd>,
}

impl Held {
    pub fn take_output(&mut self) -> (ChildStdout, ChildStderr) {
        (
            self.child.stdout.take().unwrap(),
            self.child.stderr.take().unwrap(),
        )
    }

    pub fn release(&mut self) -> Result<(), String> {
        if let Some(go) = self.go.take() {
            let _ = std::fs::File::from(go).write_all(b"g");
        }
        Ok(())
    }

    /// Blocks until the leader exits, leaving it unreaped so the group id stays ours until `finish_blocking`.
    pub fn wait_exit(self) -> Leader {
        let pinned = wait_exit_unreaped(self.child.id() as i32, None) == Some(true);
        Leader {
            child: self.child,
            pinned,
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
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
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
    let marker = (!spec.cancel_marker.is_empty()).then(|| PathBuf::from(&spec.cancel_marker));
    let group = Group(child.id() as i32, marker);
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

pub type PluginChild = tokio::process::Child;

/// Starts a plugin as the leader of its own process group, with piped stdio.
pub fn spawn_plugin(exe: &str, args: &[String]) -> std::io::Result<(PluginChild, Group)> {
    let child = tokio::process::Command::new(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pgid = child.id().unwrap_or(0) as i32;
    Ok((child, Group(pgid, None)))
}

/// Waits up to `limit` (None: forever) for our child `pid` to exit, leaving it unreaped: the zombie keeps its
/// pid, and so its process group id, from being reused. Some(true): exited; Some(false): still running; None: not
/// waitable (already reaped), so nothing is pinned.
pub fn wait_exit_unreaped(pid: i32, limit: Option<Duration>) -> Option<bool> {
    let end = limit.map(|l| Instant::now() + l);
    loop {
        // SAFETY: a zeroed siginfo_t is valid; WNOWAIT leaves the child waitable.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if r == -1 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if info.si_signo != 0 {
            return Some(true);
        }
        if end.is_some_and(|e| Instant::now() >= e) {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Members of process group `pgid` other than `leader`, or None where they can't be listed.
fn others_in_group(pgid: i32, leader: i32) -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        let mut pids = vec![0i32; 4096];
        // SAFETY: the buffer is valid for its byte size.
        let n = unsafe {
            libc::proc_listpgrppids(
                pgid,
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if n < 0 {
            return None;
        }
        Some(pids.iter().filter(|&&p| p != 0 && p != leader).count())
    }
    #[cfg(target_os = "linux")]
    {
        let mut n = 0;
        for e in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let stat = match std::fs::read(e.path().join("stat")) {
                Ok(b) => b,
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(libc::ESRCH) =>
                {
                    continue; // exited meanwhile
                }
                Err(_) => return None, // can't tell: not proven empty
            };
            let pgrp = stat_pgrp(&stat)?;
            if pgrp == pgid && pid != leader {
                n += 1;
            }
        }
        Some(n)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pgid, leader);
        None
    }
}

/// The process group in a `/proc/<pid>/stat` line ("pid (comm) state ppid pgrp ..."). `comm` may hold spaces,
/// parentheses and bytes that aren't UTF-8, so the fields are read after the last ')'.
#[cfg(any(target_os = "linux", test))]
fn stat_pgrp(stat: &[u8]) -> Option<i32> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    std::str::from_utf8(&stat[close + 1..])
        .ok()?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// Stops the rest of a group whose leader (`pgid`) has exited but is still unreaped, so its id can't have been
/// reused: TERM, wait `grace` for the other members, then KILL. The caller reaps the leader afterwards and from
/// then on only checks `group_empty`.
fn stop_pinned_group_blocking(pgid: i32, grace: Duration) {
    let alone = || others_in_group(pgid, pgid) == Some(0);
    let wait_alone = |limit: Duration| {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if alone() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        alone()
    };
    if alone() {
        return;
    }
    signal_group(pgid, libc::SIGTERM);
    if !wait_alone(grace) {
        signal_group(pgid, libc::SIGKILL);
        wait_alone(Duration::from_secs(2));
    }
}

/// Polls `group_empty` for up to `limit`; for a group whose leader is reaped, so it never signals.
fn wait_group_empty(pgid: i32, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if group_empty(pgid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    group_empty(pgid)
}

/// A leader's (exit code, signal), when known.
pub type ExitInfo = Option<(Option<i32>, Option<i32>)>;

/// A job's group leader that has exited but isn't reaped yet; see `Group::finish_blocking`.
pub struct Leader {
    child: Child,
    /// Seen exited and still unreaped, so it holds the group id.
    pinned: bool,
}

/// Children must stay waitable until we reap them: with SIGCHLD inherited as ignored (or SA_NOCLDWAIT) the kernel
/// reaps them itself, and nothing would pin a group id. Called first thing in every zorad process.
pub fn default_sigchld() {
    // SAFETY: a zeroed sigaction is SIG_DFL with no flags and an empty mask.
    unsafe {
        let sa: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
    }
}

/// Stops a plugin and its process group. Every signal goes out while the leader is unreaped (a live process or a
/// zombie pins the group id); the leader is reaped last and the group then only checked. True once it is
/// verifiably empty.
pub async fn stop_plugin(child: &mut PluginChild, group: &Group) -> bool {
    let pgid = group.0;
    let Some(pid) = child.id() else {
        return group_empty(pgid); // already reaped: nothing is pinned, so only check
    };
    let pid = pid as i32;
    let exited = tokio::task::spawn_blocking(move || {
        // No signal before the leader is known to be waitable: otherwise nothing pins the id.
        wait_exit_unreaped(pid, Some(Duration::ZERO))?;
        signal_group(pgid, libc::SIGTERM);
        let mut exited = wait_exit_unreaped(pid, Some(Duration::from_secs(3)));
        if exited == Some(false) {
            signal_group(pgid, libc::SIGKILL);
            exited = wait_exit_unreaped(pid, Some(Duration::from_secs(2)));
        }
        if exited == Some(true) {
            stop_pinned_group_blocking(pgid, Duration::from_secs(3));
        }
        Some(exited == Some(true))
    })
    .await
    .unwrap_or(Some(false));
    match exited {
        None => {
            return tokio::task::spawn_blocking(move || {
                wait_group_empty(pgid, Duration::from_secs(2))
            })
            .await
            .unwrap_or(false);
        }
        Some(false) => return false,
        Some(true) => {}
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        return false;
    }
    tokio::task::spawn_blocking(move || wait_group_empty(pgid, Duration::from_secs(2)))
        .await
        .unwrap_or(false)
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

enum Guard {
    Alive,
    Gone,
    Unknown,
}

/// Whether the recorded guard still runs. Gone only on proof: no such process (ESRCH), or that pid now has a
/// different start time.
fn guard_state(pid: Option<i64>, start: Option<i64>) -> Guard {
    let (Some(pid), Some(start)) = (pid, start) else {
        return Guard::Unknown;
    };
    // A guard that couldn't read its own start time registered 0: nothing can be proven about it.
    if pid <= 0 || pid > i32::MAX as i64 || start == 0 {
        return Guard::Unknown;
    }
    // SAFETY: signal 0 only checks for existence.
    if unsafe { libc::kill(pid as i32, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return Guard::Gone;
    }
    match start_time(pid as i32) {
        Some(s) if s == start => Guard::Alive,
        Some(_) => Guard::Gone,
        None => Guard::Unknown,
    }
}

/// Requests cancellation of a job: its guard sees the marker and does all the signalling. Only when the guard is
/// proven gone is the job's group TERMed from here, and only when that group is provably the job's (leader pid ==
/// pgid with the recorded start time).
pub fn cancel_job(job: &zora_core::store::JobRecord, marker: &Path) -> Result<(), String> {
    match guard_state(job.guard_pid, job.guard_start) {
        Guard::Alive => return request_cancel(marker),
        Guard::Unknown => {
            return Err(
                "has a guard that can't be identified; nothing was signalled (check `zorad jobs`)"
                    .into(),
            );
        }
        Guard::Gone => {}
    }
    match (job.job_pgid, job.gate_start) {
        (Some(pgid), Some(start)) if pgid > 0 && start_time(pgid as i32) == Some(start) => {
            // SAFETY: negative pid targets the verified process group.
            if unsafe { libc::kill(-(pgid as i32), libc::SIGTERM) } != 0 {
                return Err(format!(
                    "cannot be signalled: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
        _ => Err("has no verifiable running process group".into()),
    }
}

/// Creates the cancel marker: a regular file, made exclusively and without following links, in a folder only the
/// user can enter. An existing marker means cancellation was already requested.
pub fn request_cancel(marker: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let dir = marker.parent().ok_or("no marker folder")?;
    if let Some(store) = dir.parent() {
        zora_core::config::create_private_dir(store)
            .map_err(|e| format!("cannot create {}: {e}", store.display()))?;
    }
    // Only a folder made here gets the ownership file, so pruning never claims a folder Zora didn't create.
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join(OWNER_FILE))
                .map_err(|e| format!("cannot mark {}: {e}", dir.display()))?;
            f.write_all(OWNER_CONTENT)
                .map_err(|e| format!("cannot mark {}: {e}", dir.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
    }
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && m.permissions().mode() & 0o077 == 0 => {}
        _ => {
            return Err(format!(
                "refused: {} is not a private folder",
                dir.display()
            ));
        }
    }
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(marker);
    match created {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && marker_present(marker) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "refused: {} is not a regular file",
            marker.display()
        )),
        Err(e) => Err(format!("cannot create {}: {e}", marker.display())),
    }
}

/// A cancel marker counts only as a regular file (never a link).
fn marker_present(marker: &Path) -> bool {
    std::fs::symlink_metadata(marker).is_ok_and(|m| m.file_type().is_file())
}

/// Whether cancellation of job `_id` was requested (the core's own check, independent of the guard).
pub fn cancel_requested(_id: &str, marker: &Path) -> bool {
    marker_present(marker)
}

/// The file that marks `cancel/` as made by Zora, and its exact contents.
const OWNER_FILE: &str = ".zora-cancel-markers";
const OWNER_CONTENT: &[u8] = b"zora cancel markers v1\n";

/// Removes markers of jobs that are not `live`, from a folder Zora made: opened without following links
/// (`O_DIRECTORY|O_NOFOLLOW`), owned by us with mode 0700, holding the ownership file. Only regular files named as
/// canonical UUIDv4 job ids are removed, relative to that folder's descriptor.
pub fn prune_cancel_markers(dir: &Path, live: &std::collections::HashSet<String>) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: plain syscalls on descriptors owned here; every buffer is valid for its call.
    unsafe {
        let fd = libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return;
        }
        let dirfd = OwnedFd::from_raw_fd(fd);
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(dirfd.as_raw_fd(), &mut st) != 0
            || st.st_uid != libc::geteuid()
            || st.st_mode & 0o777 != 0o700
            || !owner_file_ok(dirfd.as_raw_fd())
        {
            return;
        }
        let listing = libc::dup(dirfd.as_raw_fd());
        if listing < 0 {
            return;
        }
        let d = libc::fdopendir(listing);
        if d.is_null() {
            libc::close(listing);
            return;
        }
        let mut doomed = Vec::new();
        loop {
            let ent = libc::readdir(d);
            if ent.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*ent).d_name.as_ptr());
            if let Ok(s) = name.to_str()
                && job_id(s)
                && !live.contains(s)
            {
                doomed.push(name.to_owned());
            }
        }
        libc::closedir(d);
        for name in doomed {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstatat(
                dirfd.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
                && st.st_mode & libc::S_IFMT == libc::S_IFREG
            {
                libc::unlinkat(dirfd.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
}

/// A canonical (lowercase, hyphenated) UUIDv4, as the core names jobs.
fn job_id(name: &str) -> bool {
    uuid::Uuid::parse_str(name)
        .is_ok_and(|u| u.get_version_num() == 4 && u.hyphenated().to_string() == name)
}

/// Whether the ownership file is a regular file with exactly the expected contents.
fn owner_file_ok(dirfd: i32) -> bool {
    let name = std::ffi::CString::new(OWNER_FILE).unwrap();
    // SAFETY: the descriptor is owned below; the buffer is valid for the read.
    unsafe {
        let fd = libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return false;
        }
        let f = OwnedFd::from_raw_fd(fd);
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(f.as_raw_fd(), &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return false;
        }
        let mut buf = [0u8; 64];
        let n = libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len());
        n >= 0 && buf[..n as usize] == *OWNER_CONTENT
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn stat_lines_with_odd_names_still_give_the_group() {
        assert_eq!(stat_pgrp(b"42 (sh) S 1 4242 4242 0"), Some(4242));
        assert_eq!(stat_pgrp(b"42 (a) b (\xff\xfe) R 1 77 77 0"), Some(77));
        assert_eq!(stat_pgrp(b"42 (truncated"), None);
    }

    #[test]
    fn a_leader_that_was_not_seen_exiting_leaves_check_only_cleanup() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        child.wait().unwrap(); // reaped: nothing pins the id any more
        let group = Group(pgid, None);
        assert!(!group.leader_waitable());
        let started = Instant::now();
        while others_in_group(pgid, pgid) == Some(0) && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let leader = Leader {
            child,
            pinned: false,
        };
        let (ok, _) = group.finish_blocking(Duration::ZERO, leader);
        assert!(!ok, "a member is left");
        assert!(
            others_in_group(pgid, pgid).is_some_and(|n| n >= 1),
            "an unpinned group is never signalled"
        );
        signal_group(pgid, libc::SIGKILL); // test cleanup
    }

    #[test]
    fn a_group_is_stopped_while_its_exited_leader_still_pins_the_id() {
        // The leader leaves a TERM-ignoring member behind and exits.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "(trap '' TERM; sleep 30) & exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        assert_eq!(
            wait_exit_unreaped(pgid, Some(Duration::from_secs(5))),
            Some(true)
        );
        assert!(
            !group_empty(pgid),
            "the zombie leader still holds the group"
        );
        let started = Instant::now();
        while others_in_group(pgid, pgid) == Some(0) && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(others_in_group(pgid, pgid).is_some_and(|n| n >= 1));
        stop_pinned_group_blocking(pgid, Duration::from_millis(200));
        assert_eq!(
            others_in_group(pgid, pgid),
            Some(0),
            "TERM ignored, so KILL"
        );
        child.wait().unwrap();
        assert!(wait_group_empty(pgid, Duration::from_secs(2)));
        assert_eq!(
            wait_exit_unreaped(pgid, Some(Duration::ZERO)),
            None,
            "reaped"
        );
    }
    use zora_core::store::JobRecord;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("zora-os-{}", uuid::Uuid::new_v4()))
    }

    /// A process group of its own: (child, pgid, start time).
    fn group() -> (Child, i32, i64) {
        let c = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = c.id() as i32;
        (c, pid, start_time(pid).unwrap())
    }

    fn job(guard_start: Option<i64>, pgid: i32, gate_start: i64) -> JobRecord {
        JobRecord {
            id: "j".into(),
            status: "running".into(),
            boot_id: None,
            guard_pid: Some(std::process::id() as i64),
            guard_start,
            job_pgid: Some(pgid as i64),
            gate_start: Some(gate_start),
        }
    }

    #[test]
    fn cancel_marker_is_private_and_never_follows_a_link() {
        let dir = tmp().join("cancel");
        let marker = dir.join("a");
        request_cancel(&marker).unwrap();
        request_cancel(&marker).unwrap(); // already requested
        assert!(marker_present(&marker));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let target = dir.parent().unwrap().join("elsewhere");
        let link = dir.join("b");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(request_cancel(&link).is_err());
        assert!(!target.exists());
        assert!(!marker_present(&link));
    }

    #[test]
    fn a_live_guard_only_gets_the_request() {
        let (mut c, pgid, start) = group();
        let me = start_time(std::process::id() as i32);
        let marker = tmp().join("cancel/j");
        cancel_job(&job(me, pgid, start), &marker).unwrap();
        assert!(marker_present(&marker));
        std::thread::sleep(Duration::from_millis(300));
        assert!(c.try_wait().unwrap().is_none(), "the group was signalled");
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn without_its_guard_a_verified_group_is_terminated() {
        let (mut c, pgid, start) = group();
        let gone = start_time(std::process::id() as i32).map(|s| s + 1);
        let marker = tmp().join("cancel/j");
        cancel_job(&job(gone, pgid, start), &marker).unwrap();
        let status = c.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(!marker_present(&marker));
        // Nothing verifiable left: refused.
        assert!(cancel_job(&job(gone, pgid, start), &marker).is_err());
    }

    #[test]
    fn a_guard_that_cant_be_identified_is_not_taken_for_gone() {
        let (mut c, pgid, start) = group();
        let mut j = job(None, pgid, start);
        let marker = tmp().join("cancel/j");
        assert!(cancel_job(&j, &marker).is_err());
        // A live guard that registered start time 0 (it couldn't read its own).
        j.guard_start = Some(0);
        assert!(cancel_job(&j, &marker).is_err());
        j.guard_pid = None;
        assert!(cancel_job(&j, &marker).is_err());
        std::thread::sleep(Duration::from_millis(300));
        assert!(c.try_wait().unwrap().is_none(), "the group was signalled");
        assert!(!marker_present(&marker));
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn pruning_touches_only_orphan_markers_in_a_folder_zora_made() {
        let root = tmp();
        let dir = root.join("cancel");
        let live_id = uuid::Uuid::new_v4().to_string();
        let orphan = uuid::Uuid::new_v4().to_string();
        let linked = uuid::Uuid::new_v4().to_string();
        let upper = uuid::Uuid::new_v4().to_string().to_uppercase();
        let v1 = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        request_cancel(&dir.join(&live_id)).unwrap(); // makes the folder and its ownership file
        for f in [orphan.as_str(), "notes", "notes.txt", &upper, v1] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        std::fs::create_dir(dir.join(uuid::Uuid::new_v4().to_string())).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(&linked)).unwrap();
        let live: std::collections::HashSet<String> = [live_id.clone()].into();
        prune_cancel_markers(&dir, &live);
        assert!(!dir.join(&orphan).exists());
        for kept in [
            live_id.as_str(),
            "notes",
            "notes.txt",
            &upper,
            v1,
            OWNER_FILE,
        ] {
            assert!(dir.join(kept).exists(), "{kept} was removed");
        }
        assert!(std::fs::symlink_metadata(dir.join(&linked)).is_ok() && outside.exists());

        // Without the ownership file (or through a link, or open to others) nothing is touched.
        let other = root.join("other");
        zora_core::config::create_private_dir(&other).unwrap();
        let o = uuid::Uuid::new_v4().to_string();
        for f in [o.as_str(), "notes"] {
            std::fs::write(other.join(f), b"").unwrap();
        }
        prune_cancel_markers(&other, &live);
        assert!(other.join(&o).exists() && other.join("notes").exists());
        std::fs::write(other.join(OWNER_FILE), OWNER_CONTENT).unwrap();
        let link = root.join("cancel-link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        prune_cancel_markers(&link, &live);
        assert!(other.join(&o).exists());
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
        prune_cancel_markers(&other, &live);
        assert!(other.join(&o).exists());
        std::fs::write(other.join(OWNER_FILE), b"something else").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
        prune_cancel_markers(&other, &live);
        assert!(other.join(&o).exists());
    }

    #[test]
    fn a_fifo_in_place_of_the_ownership_file_neither_blocks_nor_counts() {
        let dir = tmp().join("cancel");
        zora_core::config::create_private_dir(&dir).unwrap();
        let fifo =
            std::ffi::CString::new(dir.join(OWNER_FILE).as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let orphan = uuid::Uuid::new_v4().to_string();
        std::fs::write(dir.join(&orphan), b"").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            prune_cancel_markers(&d, &Default::default());
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "pruning blocked on a FIFO"
        );
        assert!(dir.join(&orphan).exists());
    }
}

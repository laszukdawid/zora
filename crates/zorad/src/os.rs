//! Process identity and process-group control (macOS and Linux).

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

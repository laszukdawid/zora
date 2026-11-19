//! Monotonic nanoseconds in the clock domain `zorad` expects on this platform (docs/protocol.md, Clock).

pub type Ns = i64;

pub const SECOND: Ns = 1_000_000_000;
pub const MILLI: Ns = 1_000_000;

#[cfg(target_os = "macos")]
pub const CLOCK_DOMAIN: &str = "darwin-uptime-raw-ns";
#[cfg(target_os = "linux")]
pub const CLOCK_DOMAIN: &str = "linux-monotonic-ns";
#[cfg(windows)]
pub const CLOCK_DOMAIN: &str = "windows-qpc-ns";
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub const CLOCK_DOMAIN: &str = "unsupported";

#[cfg(unix)]
pub fn now_ns() -> Ns {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[cfg(target_os = "macos")]
    let id = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    let id = libc::CLOCK_MONOTONIC;
    // SAFETY: valid pointer to a timespec on the stack.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as Ns * SECOND + ts.tv_nsec as Ns
}

/// QueryPerformanceCounter in nanoseconds, the same clock as the core (zora-core clock.rs).
#[cfg(windows)]
pub fn now_ns() -> Ns {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: both point to valid i64s; these calls cannot fail on Windows XP and later.
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut freq);
    }
    (count as i128 * SECOND as i128 / freq.max(1) as i128) as Ns
}

#[cfg(not(any(unix, windows)))]
pub fn now_ns() -> Ns {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as Ns
}

/// Seconds since the Unix epoch.
pub fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// "HH:MM" in local time for Unix seconds.
#[cfg(unix)]
pub fn local_hhmm(t: f64) -> String {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "?".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(not(unix))]
pub fn local_hhmm(t: f64) -> String {
    let m = (t as i64 / 60) % 1440;
    format!("{:02}:{:02} UTC", m / 60, m % 60)
}

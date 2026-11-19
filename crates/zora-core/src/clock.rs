//! Monotonic nanoseconds in a named clock domain shared with plugins (see docs/protocol.md).

/// Nanoseconds on the platform monotonic clock.
pub type Ns = i64;

pub const SECOND: Ns = 1_000_000_000;
pub const MILLI: Ns = 1_000_000;

pub fn secs(s: f64) -> Ns {
    (s * SECOND as f64).round() as Ns
}

#[cfg(target_os = "macos")]
pub const CLOCK_DOMAIN: &str = "darwin-uptime-raw-ns";
#[cfg(target_os = "linux")]
pub const CLOCK_DOMAIN: &str = "linux-monotonic-ns";
#[cfg(windows)]
pub const CLOCK_DOMAIN: &str = "windows-qpc-ns";

#[cfg(unix)]
pub fn now_ns() -> Ns {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[cfg(target_os = "macos")]
    let id = libc::CLOCK_UPTIME_RAW;
    #[cfg(target_os = "linux")]
    let id = libc::CLOCK_MONOTONIC;
    // SAFETY: valid pointer to a timespec on the stack.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as Ns * SECOND + ts.tv_nsec as Ns
}

/// QueryPerformanceCounter in nanoseconds; the counter is system-wide, so plugins read the same clock.
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

/// Unix seconds for a recent monotonic time (the monotonic clock pauses in sleep on macOS; use near now only).
pub fn wall_secs(t: Ns, now: Ns) -> f64 {
    let wall_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    wall_now + (t - now) as f64 / SECOND as f64
}

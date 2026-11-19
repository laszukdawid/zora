//! Listening on/off. The CLI and the trays write a small state file; the service polls it and tells the plugins,
//! which release the microphone. A paused Zora stays paused across restarts until resumed or the timer runs out (an expired
//! file is simply ignored).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the service tells plugins (`start` and `listening` messages, docs/protocol.md).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Listen {
    pub listening: bool,
    /// Wall-clock seconds since the Unix epoch when a timed pause ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<f64>,
}

impl Listen {
    pub const ON: Listen = Listen {
        listening: true,
        until: None,
    };
}

/// What plugin hosts watch. `turn` counts on/off transitions (each host maps it to its connection's epoch);
/// `confirmed` is set once every transcript plugin confirmed the pause, or the deadline passed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListenState {
    pub listen: Listen,
    pub turn: u64,
    pub confirmed: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct PauseFile {
    paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until: Option<f64>,
}

pub fn path() -> PathBuf {
    crate::service::database_path().with_file_name("pause.json")
}

pub fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The current state. A missing file means listening; a file that can't be read or parsed means paused, so a
/// damaged file never turns the microphone back on.
pub fn read(path: &Path, now: f64) -> Listen {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Listen::ON,
        Err(_) => {
            return Listen {
                listening: false,
                until: None,
            };
        }
    };
    match serde_json::from_slice::<PauseFile>(&data) {
        Ok(PauseFile { paused: false, .. }) => Listen::ON,
        Ok(PauseFile { until: Some(t), .. }) if t <= now => Listen::ON,
        Ok(PauseFile { until, .. }) => Listen {
            listening: false,
            until,
        },
        Err(_) => Listen {
            listening: false,
            until: None,
        },
    }
}

/// Pause until `until` (wall seconds), or until resumed when `None`.
pub fn pause(path: &Path, until: Option<f64>) -> Result<(), String> {
    write_private(
        path,
        &serde_json::to_vec(&PauseFile {
            paused: true,
            until,
        })
        .map_err(|e| e.to_string())?,
    )
}

pub fn resume(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use zora_core::config::{create_private_dir, create_private_file};
    if let Some(dir) = path.parent() {
        create_private_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // A temp file per writer, so two writers never share one inode.
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f =
        create_private_file(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })
}

/// Local time of day for a wall timestamp, e.g. "14:30".
#[cfg(windows)]
pub fn local_hhmm(t: f64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME counts 100 ns intervals since 1601.
    let ticks = ((t + 11_644_473_600.0) * 1e7) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: valid pointers to stack values; a null time zone means the current one.
    unsafe {
        let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = std::mem::zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            return "?".into();
        }
        format!("{:02}:{:02}", local.wHour, local.wMinute)
    }
}

/// Local time of day for a wall timestamp, e.g. "14:30".
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir()
            .join(format!("zora-pause-{}", uuid::Uuid::new_v4()))
            .join("pause.json")
    }

    #[test]
    fn missing_file_is_listening() {
        assert_eq!(read(&tmp(), 0.0), Listen::ON);
    }

    #[test]
    fn pause_resume_and_timer() {
        let p = tmp();
        pause(&p, None).unwrap();
        assert!(!read(&p, 1e12).listening);
        resume(&p).unwrap();
        assert_eq!(read(&p, 0.0), Listen::ON);
        pause(&p, Some(100.0)).unwrap();
        assert_eq!(
            read(&p, 50.0),
            Listen {
                listening: false,
                until: Some(100.0)
            }
        );
        assert_eq!(read(&p, 100.0), Listen::ON); // expired: listening, file left alone
        resume(&p).unwrap();
        resume(&p).unwrap(); // already gone: still fine
    }

    #[test]
    fn damaged_file_fails_closed() {
        let p = tmp();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{nope").unwrap();
        assert!(!read(&p, 0.0).listening);
    }
}

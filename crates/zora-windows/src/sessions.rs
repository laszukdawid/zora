//! Dictation detection from audio capture sessions (WASAPI on Windows): which sessions belong to a configured
//! dictation app (`dictation.windowsApps`), and the fail-closed rules on top of the shared `Monitor`.
//!
//! The OS side only reports complete snapshots of every capture endpoint and its sessions (`SessionSource`). A
//! snapshot that can't be taken makes health unknown; a capture device appearing or going away makes the time
//! since the last snapshot unknown; a stalled poll does the same through the monitor's stall rule. An active
//! session whose process can't be identified counts as dictating, so no command runs while it lasts.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zora_plugin::clock::{Ns, now_ns};
use zora_plugin::dictation::{Monitor, Msg};

/// How often the sessions are read.
pub const POLL: Duration = Duration::from_millis(250);

pub const NO_APPS: &str = "no dictation apps configured (dictation.windowsApps)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The process image path; `None` when the session's process can't be identified.
    pub image: Option<String>,
    /// `AudioSessionStateActive`: the app is capturing.
    pub active: bool,
}

/// Every active capture endpoint (by id) and every session on them, read in one pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub devices: Vec<String>,
    pub sessions: Vec<Session>,
}

pub trait SessionSource {
    /// A complete snapshot, or why there is none (any COM failure).
    fn snapshot(&mut self) -> Result<Snapshot, String>;
}

/// A process image matches a configured app by its file name, with or without `.exe`, ignoring case.
pub fn image_matches(image: &str, apps: &[String]) -> bool {
    let file = stem(image.rsplit(['\\', '/']).next().unwrap_or(image).trim());
    apps.iter().any(|a| {
        let a = stem(a.trim());
        !a.is_empty() && a.eq_ignore_ascii_case(file)
    })
}

/// A file name without a trailing `.exe` (any case).
fn stem(name: &str) -> &str {
    name.len()
        .checked_sub(4)
        .filter(|&i| name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe"))
        .map_or(name, |i| &name[..i])
}

/// A configured app is capturing, or an active session can't be identified.
pub fn dictating(snapshot: &Snapshot, apps: &[String]) -> bool {
    snapshot.sessions.iter().any(|s| {
        s.active
            && s.image
                .as_deref()
                .is_none_or(|image| image_matches(image, apps))
    })
}

pub struct Watcher {
    pub monitor: Monitor,
    apps: Vec<String>,
    devices: Option<Vec<String>>,
    last_ok: Option<Ns>,
}

impl Watcher {
    pub fn new(apps: Vec<String>) -> Watcher {
        Watcher {
            monitor: Monitor::new(!apps.is_empty()),
            apps,
            devices: None,
            last_ok: None,
        }
    }

    /// Reads a snapshot and reports what changed.
    pub fn poll(&mut self, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        if self.apps.is_empty() {
            return self.monitor.lost(NO_APPS, now);
        }
        match src.snapshot() {
            Err(e) => {
                self.devices = None;
                self.monitor
                    .lost(&format!("audio sessions unavailable: {e}"), now)
            }
            Ok(snap) => {
                let mut out = vec![];
                let mut devices = snap.devices.clone();
                devices.sort();
                // A device that came or went between snapshots may have carried a dictation meanwhile.
                if let (Some(before), Some(since)) = (&self.devices, self.last_ok)
                    && *before != devices
                {
                    out.push(Msg::Unknown {
                        from_ns: since,
                        to_ns: now,
                    });
                }
                self.devices = Some(devices);
                self.last_ok = Some(now);
                out.extend(self.monitor.observe(dictating(&snap, &self.apps), now));
                out
            }
        }
    }

    /// The 1 s timer: heartbeat, and health unknown once polls stall.
    pub fn tick(&mut self, now: Ns) -> Vec<Msg> {
        self.monitor.tick(now)
    }

    /// The answer to `sync`: any stalled interval first, then a fresh snapshot, then `synced`.
    pub fn sync(&mut self, id: u64, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        let mut out = self.monitor.stall_check(now);
        out.extend(self.poll(src, now));
        out.push(self.monitor.synced(id, now));
        out
    }
}

/// From the core-reading thread into the monitor loop.
pub enum Req {
    Sync(u64),
    Quit,
}

/// The monitor loop: polls every `POLL`, ticks every second, answers `sync` in order. Returns on `Quit` or when
/// the requests end.
pub fn run(
    mut watcher: Watcher,
    src: &mut impl SessionSource,
    rx: Receiver<Req>,
    mut send: impl FnMut(Vec<Msg>),
) {
    let mut next_poll = Instant::now();
    let mut next_tick = Instant::now();
    loop {
        let wait = next_poll
            .min(next_tick)
            .saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Req::Sync(id)) => send(watcher.sync(id, src, now_ns())),
            Ok(Req::Quit) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if now >= next_poll {
            send(watcher.poll(src, now_ns()));
            next_poll = now + POLL;
        }
        if now >= next_tick {
            send(watcher.tick(now_ns()));
            next_tick = now + Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zora_plugin::clock::{MILLI, SECOND};

    struct Fake(Vec<Result<Snapshot, String>>);

    impl SessionSource for Fake {
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            if self.0.len() > 1 {
                self.0.remove(0)
            } else {
                self.0[0].clone()
            }
        }
    }

    fn snap(devices: &[&str], sessions: &[(Option<&str>, bool)]) -> Result<Snapshot, String> {
        Ok(Snapshot {
            devices: devices.iter().map(|d| d.to_string()).collect(),
            sessions: sessions
                .iter()
                .map(|(i, a)| Session {
                    image: i.map(String::from),
                    active: *a,
                })
                .collect(),
        })
    }

    fn apps() -> Vec<String> {
        vec!["Wispr Flow".into(), "dictate.exe".into()]
    }

    fn health(msgs: &[Msg]) -> Option<(bool, String)> {
        msgs.iter().rev().find_map(|m| match m {
            Msg::Health {
                healthy, reason, ..
            } => Some((*healthy, reason.clone())),
            _ => None,
        })
    }

    #[test]
    fn images_match_by_file_name_with_or_without_exe() {
        let a = apps();
        assert!(image_matches(r"C:\Program Files\Wispr\Wispr Flow.exe", &a));
        assert!(image_matches(r"C:\x\DICTATE.EXE", &a));
        assert!(image_matches("dictate", &a));
        assert!(!image_matches(r"C:\x\zora-listen.exe", &a));
        assert!(!image_matches(r"C:\Wispr Flow\other.exe", &a));
        assert!(!image_matches("anything", &["  ".into()]));
    }

    #[test]
    fn active_matching_or_unidentified_sessions_are_dictation() {
        let a = apps();
        let s = |sessions: &[(Option<&str>, bool)]| snap(&["mic"], sessions).unwrap();
        assert!(dictating(&s(&[(Some(r"C:\a\dictate.exe"), true)]), &a));
        assert!(!dictating(&s(&[(Some(r"C:\a\dictate.exe"), false)]), &a));
        assert!(!dictating(&s(&[(Some(r"C:\a\zora-listen.exe"), true)]), &a));
        assert!(dictating(&s(&[(None, true)]), &a));
        assert!(!dictating(&s(&[(None, false)]), &a));
    }

    #[test]
    fn without_apps_health_stays_unknown() {
        let mut w = Watcher::new(vec![]);
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let out = w.poll(&mut src, SECOND);
        assert_eq!(health(&out), Some((false, NO_APPS.into())));
        assert!(!w.monitor.healthy());
    }

    #[test]
    fn a_snapshot_failure_fails_closed_and_recovery_reports_the_gap() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![
            snap(&["mic"], &[]),
            Err("RPC_E_DISCONNECTED".into()),
            snap(&["mic"], &[]),
        ]);
        assert_eq!(health(&w.poll(&mut src, SECOND)), Some((true, "ok".into())));
        let lost = w.poll(&mut src, SECOND + 250 * MILLI);
        assert!(!w.monitor.healthy(), "{lost:?}");
        assert!(health(&lost).is_some_and(|(_, r)| r.contains("RPC_E_DISCONNECTED")));
        let back = w.poll(&mut src, 5 * SECOND);
        assert!(back.contains(&Msg::Unknown {
            from_ns: SECOND,
            to_ns: 5 * SECOND
        }));
        assert!(w.monitor.healthy());
    }

    #[test]
    fn a_device_change_makes_the_interval_unknown() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![snap(&["mic"], &[]), snap(&["mic", "headset"], &[])]);
        w.poll(&mut src, SECOND);
        let out = w.poll(&mut src, SECOND + 250 * MILLI);
        assert_eq!(
            out[0],
            Msg::Unknown {
                from_ns: SECOND,
                to_ns: SECOND + 250 * MILLI
            }
        );
    }

    #[test]
    fn dictation_edges_follow_sessions() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![
            snap(&["mic"], &[]),
            snap(&["mic"], &[(Some(r"C:\a\dictate.exe"), true)]),
            snap(&["mic"], &[(Some(r"C:\a\dictate.exe"), false)]),
        ]);
        w.poll(&mut src, SECOND);
        let began = w.poll(&mut src, SECOND + 250 * MILLI);
        assert!(began.contains(&Msg::Dictation {
            began: true,
            at_ns: SECOND + 250 * MILLI
        }));
        let ended = w.poll(&mut src, SECOND + 500 * MILLI);
        assert!(ended.contains(&Msg::Dictation {
            began: false,
            at_ns: SECOND + 500 * MILLI
        }));
    }

    #[test]
    fn a_stalled_poll_goes_unknown_and_sync_reports_it_first() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        w.poll(&mut src, SECOND);
        let tick = w.tick(4 * SECOND);
        assert!(!w.monitor.healthy(), "{tick:?}");
        let out = w.sync(7, &mut src, 5 * SECOND);
        assert!(matches!(out[0], Msg::Unknown { .. }), "{out:?}");
        assert_eq!(
            out.last(),
            Some(&Msg::Synced {
                id: 7,
                at_ns: 5 * SECOND,
                healthy: true,
                capturing: false
            })
        );
    }

    #[test]
    fn the_loop_answers_sync_and_stops_on_quit() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Req::Sync(3)).unwrap();
        tx.send(Req::Quit).unwrap();
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let mut sent = vec![];
        run(Watcher::new(apps()), &mut src, rx, |m| sent.extend(m));
        assert!(sent.iter().any(|m| matches!(m, Msg::Synced { id: 3, .. })));
    }
}

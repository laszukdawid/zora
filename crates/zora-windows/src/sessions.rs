//! Dictation detection from audio capture sessions (WASAPI on Windows): which sessions belong to a configured
//! dictation app (`dictation.windowsApps`), and the fail-closed rules on top of the shared `Monitor`.
//!
//! Coverage comes from events: the OS side registers for session creation on every capture endpoint before it
//! enumerates them, and for state changes on every session, and reports each as an `Event` with its time. Edges
//! go out as the events arrive, so a dictation that starts and stops between two polls is still reported. The
//! 250 ms snapshot only reconciles: when it disagrees with what the events said (a session nobody announced, a
//! different state, a device change), the time since the last agreement is unknown. A registration or callback
//! failure, or an event older than the last reconciliation, is unknown too. An active session whose process can't
//! be identified counts as dictating, so no command runs while it lasts.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zora_plugin::clock::{Ns, now_ns};
use zora_plugin::dictation::{Monitor, Msg};

/// How often the sessions are reconciled.
pub const POLL: Duration = Duration::from_millis(250);

pub const NO_APPS: &str = "no dictation apps configured (dictation.windowsApps)";

/// `AUDCLNT_S_NO_SINGLE_PROCESS`: a success code from `GetProcessId` for a session shared by several processes;
/// the pid it returns is only the creator's.
pub const AUDCLNT_S_NO_SINGLE_PROCESS: i32 = 0x0889_000D;

/// The pid that identifies a session's process: none when the session spans several processes, failed, or is
/// the system's (pid 0).
pub fn single_process(hresult: i32, pid: u32) -> Option<u32> {
    (hresult == 0 && pid != 0).then_some(pid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session instance identifier (unique per session).
    pub key: String,
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
    /// Makes sure every endpoint and session is registered for events, then reads a complete snapshot; or why
    /// it can't (any COM failure).
    fn snapshot(&mut self) -> Result<Snapshot, String>;
}

/// What the OS reported between snapshots, with the time it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A session appeared (`OnSessionCreated`) or changed state (`OnStateChanged`).
    Session {
        key: String,
        image: Option<String>,
        active: bool,
        at: Ns,
    },
    /// A session went away (`OnSessionDisconnected`).
    Gone { key: String, at: Ns },
    /// Coverage can't be trusted: a device change, a failed registration or callback.
    Doubt { reason: String, at: Ns },
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

fn dictating_one(image: Option<&str>, active: bool, apps: &[String]) -> bool {
    active && image.is_none_or(|image| image_matches(image, apps))
}

/// A configured app is capturing, or an active session can't be identified.
pub fn dictating(snapshot: &Snapshot, apps: &[String]) -> bool {
    snapshot
        .sessions
        .iter()
        .any(|s| dictating_one(s.image.as_deref(), s.active, apps))
}

pub struct Watcher {
    pub monitor: Monitor,
    apps: Vec<String>,
    devices: Option<Vec<String>>,
    /// When events and a snapshot last agreed.
    last_ok: Option<Ns>,
    /// The sessions as the events since then describe them: key → (image, active).
    live: HashMap<String, (Option<String>, bool)>,
    /// Since when coverage is in doubt; reported as unknown at the next reconciliation.
    blind_since: Option<Ns>,
}

impl Watcher {
    pub fn new(apps: Vec<String>) -> Watcher {
        Watcher {
            monitor: Monitor::new(!apps.is_empty()),
            apps,
            devices: None,
            last_ok: None,
            live: HashMap::new(),
            blind_since: None,
        }
    }

    fn live_dictating(&self) -> bool {
        self.live
            .values()
            .any(|(image, active)| dictating_one(image.as_deref(), *active, &self.apps))
    }

    fn doubt(&mut self, reason: &str, at: Ns) -> Vec<Msg> {
        self.blind_since.get_or_insert(at);
        self.monitor.lost(reason, at)
    }

    /// An OS event. Edges go out as it says; nothing is trusted before the first reconciliation.
    pub fn event(&mut self, event: Event) -> Vec<Msg> {
        if self.apps.is_empty() {
            return vec![];
        }
        let at = match &event {
            Event::Session { at, .. } | Event::Gone { at, .. } | Event::Doubt { at, .. } => *at,
        };
        if let Event::Doubt { reason, .. } = &event {
            return self.doubt(reason, at);
        }
        if self.last_ok.is_some_and(|ok| at < ok) {
            return self.doubt("audio session events out of order", at);
        }
        match event {
            Event::Session {
                key, image, active, ..
            } => {
                self.live.insert(key, (image, active));
            }
            Event::Gone { key, .. } => {
                self.live.remove(&key);
            }
            Event::Doubt { .. } => {}
        }
        if self.last_ok.is_none() {
            return vec![];
        }
        let now = self.live_dictating();
        self.monitor.changed(now, at)
    }

    /// Whether the events since the last reconciliation account for `snap`.
    fn agrees(&self, snap: &Snapshot, devices: &[String]) -> bool {
        if self.devices.as_deref() != Some(devices) {
            return false;
        }
        let seen: HashMap<&str, bool> = snap
            .sessions
            .iter()
            .map(|s| (s.key.as_str(), s.active))
            .collect();
        // Every session was announced and is in the state the events gave it; one that vanished was idle.
        snap.sessions
            .iter()
            .all(|s| self.live.get(&s.key).is_some_and(|(_, a)| *a == s.active))
            && self
                .live
                .iter()
                .all(|(k, (_, a))| seen.contains_key(k.as_str()) || !a)
    }

    /// Reconciles with a fresh snapshot and reports what changed.
    pub fn poll(&mut self, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        if self.apps.is_empty() {
            return self.monitor.lost(NO_APPS, now);
        }
        match src.snapshot() {
            Err(e) => {
                self.devices = None;
                self.doubt(&format!("audio sessions unavailable: {e}"), now)
            }
            Ok(snap) => {
                let mut out = vec![];
                let mut devices = snap.devices.clone();
                devices.sort();
                if let Some(ok) = self.last_ok
                    && !self.agrees(&snap, &devices)
                {
                    self.blind_since = Some(self.blind_since.map_or(ok, |b| b.min(ok)));
                }
                if let Some(since) = self.blind_since.take() {
                    out.push(Msg::Unknown {
                        from_ns: since,
                        to_ns: now,
                    });
                }
                self.devices = Some(devices);
                self.last_ok = Some(now);
                self.live = snap
                    .sessions
                    .iter()
                    .map(|s| (s.key.clone(), (s.image.clone(), s.active)))
                    .collect();
                out.extend(self.monitor.observe(dictating(&snap, &self.apps), now));
                out
            }
        }
    }

    /// The 1 s timer: heartbeat, and health unknown once reconciliation stalls.
    pub fn tick(&mut self, now: Ns) -> Vec<Msg> {
        self.monitor.tick(now)
    }

    /// The answer to `sync`: any stalled interval first, then a fresh reconciliation, then `synced`.
    pub fn sync(&mut self, id: u64, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        let mut out = self.monitor.stall_check(now);
        out.extend(self.poll(src, now));
        out.push(self.monitor.synced(id, now));
        out
    }
}

/// Into the monitor loop: the core's requests and the OS's events, in arrival order.
pub enum Req {
    Sync(u64),
    Event(Event),
    Quit,
}

/// The monitor loop: reconciles every `POLL`, ticks every second, handles events and `sync` in order. Returns on
/// `Quit` or when every sender is gone.
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
            Ok(Req::Event(e)) => send(watcher.event(e)),
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

    type S<'a> = (&'a str, Option<&'a str>, bool);

    fn snap(devices: &[&str], sessions: &[S]) -> Result<Snapshot, String> {
        Ok(Snapshot {
            devices: devices.iter().map(|d| d.to_string()).collect(),
            sessions: sessions
                .iter()
                .map(|(k, i, a)| Session {
                    key: k.to_string(),
                    image: i.map(String::from),
                    active: *a,
                })
                .collect(),
        })
    }

    fn state(key: &str, image: &str, active: bool, at: Ns) -> Event {
        Event::Session {
            key: key.into(),
            image: Some(image.into()),
            active,
            at,
        }
    }

    fn apps() -> Vec<String> {
        vec!["Wispr Flow".into(), "dictate.exe".into()]
    }

    fn ms(n: i64) -> Ns {
        n * MILLI
    }

    fn health(msgs: &[Msg]) -> Option<(bool, String)> {
        msgs.iter().rev().find_map(|m| match m {
            Msg::Health {
                healthy, reason, ..
            } => Some((*healthy, reason.clone())),
            _ => None,
        })
    }

    fn unknowns(msgs: &[Msg]) -> Vec<(Ns, Ns)> {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::Unknown { from_ns, to_ns } => Some((*from_ns, *to_ns)),
                _ => None,
            })
            .collect()
    }

    const DICTATE: &str = r"C:\a\dictate.exe";

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
    fn a_session_of_several_processes_is_unidentified() {
        assert_eq!(single_process(0, 1234), Some(1234));
        assert_eq!(single_process(AUDCLNT_S_NO_SINGLE_PROCESS, 1234), None);
        assert_eq!(single_process(0, 0), None);
        assert_eq!(single_process(0x8000_4005_u32 as i32, 1234), None);
        let s = snap(&["mic"], &[("k", None, true)]).unwrap();
        assert!(dictating(&s, &apps()));
    }

    #[test]
    fn active_matching_or_unidentified_sessions_are_dictation() {
        let a = apps();
        let s = |sessions: &[S]| snap(&["mic"], sessions).unwrap();
        assert!(dictating(&s(&[("k", Some(DICTATE), true)]), &a));
        assert!(!dictating(&s(&[("k", Some(DICTATE), false)]), &a));
        assert!(!dictating(
            &s(&[("k", Some(r"C:\a\zora-listen.exe"), true)]),
            &a
        ));
        assert!(dictating(&s(&[("k", None, true)]), &a));
        assert!(!dictating(&s(&[("k", None, false)]), &a));
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
    fn a_capture_between_two_polls_is_reported_from_its_events() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![
            snap(&["mic"], &[("k", Some(DICTATE), false)]),
            snap(&["mic"], &[("k", Some(DICTATE), false)]),
        ]);
        w.poll(&mut src, ms(1000));
        let began = w.event(state("k", DICTATE, true, ms(1050)));
        let ended = w.event(state("k", DICTATE, false, ms(1150)));
        assert!(began.contains(&Msg::Dictation {
            began: true,
            at_ns: ms(1050)
        }));
        assert!(ended.contains(&Msg::Dictation {
            began: false,
            at_ns: ms(1150)
        }));
        let out = w.poll(&mut src, ms(1250));
        assert!(unknowns(&out).is_empty(), "{out:?}");
        assert!(w.monitor.healthy());
    }

    #[test]
    fn a_new_session_that_captures_is_dictation() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![
            snap(&["mic"], &[]),
            snap(&["mic"], &[("new", Some(DICTATE), true)]),
        ]);
        w.poll(&mut src, ms(1000));
        let out = w.event(state("new", DICTATE, true, ms(1100)));
        assert!(out.contains(&Msg::Dictation {
            began: true,
            at_ns: ms(1100)
        }));
        let out = w.poll(&mut src, ms(1250));
        assert!(unknowns(&out).is_empty(), "{out:?}");
        assert!(w.monitor.capturing());
    }

    #[test]
    fn lost_events_make_the_interval_unknown() {
        let mut w = Watcher::new(apps());
        // A session nobody announced: it may have captured unseen.
        let mut src = Fake(vec![
            snap(&["mic"], &[]),
            snap(&["mic"], &[("missed", Some(DICTATE), false)]),
            snap(&["mic"], &[("missed", Some(DICTATE), true)]),
        ]);
        w.poll(&mut src, ms(1000));
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1000), ms(1250))]);
        // A state change nobody reported.
        let out = w.poll(&mut src, ms(1500));
        assert_eq!(unknowns(&out), [(ms(1250), ms(1500))]);
        assert!(w.monitor.capturing());
    }

    #[test]
    fn a_device_change_makes_the_interval_unknown() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![snap(&["mic"], &[]), snap(&["mic", "headset"], &[])]);
        w.poll(&mut src, ms(1000));
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1000), ms(1250))]);
    }

    #[test]
    fn doubts_and_late_events_fail_closed_until_reconciled() {
        let mut w = Watcher::new(apps());
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        w.poll(&mut src, ms(1000));
        let out = w.event(Event::Doubt {
            reason: "OnSessionCreated failed".into(),
            at: ms(1100),
        });
        assert_eq!(health(&out).map(|h| h.0), Some(false));
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1100), ms(1250))]);
        assert!(w.monitor.healthy());
        // An event from before the last reconciliation arrived late.
        let out = w.event(state("k", DICTATE, true, ms(1200)));
        assert_eq!(health(&out).map(|h| h.0), Some(false));
        let out = w.poll(&mut src, ms(1500));
        assert_eq!(unknowns(&out), [(ms(1200), ms(1500))]);
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
        let lost = w.poll(&mut src, SECOND + ms(250));
        assert!(!w.monitor.healthy(), "{lost:?}");
        assert!(health(&lost).is_some_and(|(_, r)| r.contains("RPC_E_DISCONNECTED")));
        let back = w.poll(&mut src, 5 * SECOND);
        assert!(unknowns(&back).contains(&(SECOND, 5 * SECOND)), "{back:?}");
        assert!(w.monitor.healthy());
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
    fn the_loop_handles_events_and_sync_in_order_and_stops_on_quit() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Req::Sync(3)).unwrap();
        tx.send(Req::Event(Event::Doubt {
            reason: "x".into(),
            at: now_ns(),
        }))
        .unwrap();
        tx.send(Req::Quit).unwrap();
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let mut sent = vec![];
        run(Watcher::new(apps()), &mut src, rx, |m| sent.extend(m));
        let synced = sent
            .iter()
            .position(|m| matches!(m, Msg::Synced { id: 3, .. }));
        let doubt = sent
            .iter()
            .position(|m| matches!(m, Msg::Health { healthy: false, .. }));
        assert!(
            synced.is_some() && doubt.is_some() && synced < doubt,
            "{sent:?}"
        );
    }
}

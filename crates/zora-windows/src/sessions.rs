//! Dictation detection from audio capture sessions (WASAPI on Windows): which sessions belong to a configured
//! dictation app (`dictation.windowsApps`), and the fail-closed rules on top of the shared `Monitor`.
//!
//! Coverage comes from events: the OS side registers for session creation on every capture endpoint before it
//! enumerates them, and for state changes on every session, and reports each as an `Event` with its time. Edges
//! go out as the events arrive, so a dictation that starts and stops between two polls is still reported. The
//! 250 ms snapshot reconciles: when it disagrees with what the events said (a session nobody announced, a
//! different state, a device change), the time since the last agreement is unknown, and stays unknown until a
//! reconciliation agrees. Startup, a registration or callback failure, a handed-off new session, and an event
//! that is out of order or older than the last reconciliation are unknown too; such an event is not applied. An
//! active session whose process can't be identified counts as dictating.
//!
//! None of this can prove that every callback was delivered before a `sync` is answered, so the monitor is off
//! unless `dictation.windowsExperimental` is set: until then it reports health unknown and no command runs.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zora_plugin::clock::{Ns, now_ns};
use zora_plugin::dictation::{Monitor, Msg};

/// How often the sessions are reconciled.
pub const POLL: Duration = Duration::from_millis(250);

pub const NO_APPS: &str = "no dictation apps configured (dictation.windowsApps)";
pub const EXPERIMENTAL: &str = "Windows dictation monitoring is experimental: set dictation.windowsExperimental to true to use it, or dictation.monitor to \"none\" if you use no dictation app";
const RECONCILING: &str = "audio sessions are being reconciled";

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

/// Every active capture endpoint (by id) and the sessions enumerated on them, read in one pass. The enumeration
/// may omit sessions the source learned of from notifications; `gone` lists the sessions it retired since the
/// last snapshot (their endpoint went away).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub devices: Vec<String>,
    pub sessions: Vec<Session>,
    pub gone: Vec<String>,
}

pub trait SessionSource {
    /// Makes sure every endpoint and session is registered for events, then reads a snapshot; or why it can't
    /// (any COM failure).
    fn snapshot(&mut self) -> Result<Snapshot, String>;

    /// Something a callback handed to the owner thread (a new session to subscribe to), as events.
    fn handoff(&mut self, _msg: Box<dyn Any + Send>, _now: Ns) -> Vec<Event> {
        vec![]
    }
}

/// What the OS reported between snapshots, with the time the callback ran.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A session's state, from `OnStateChanged` or from reading a newly subscribed session.
    Session {
        key: String,
        image: Option<String>,
        active: bool,
        at: Ns,
    },
    /// A session ended for good (`OnSessionDisconnected`, or expired).
    Gone { key: String, at: Ns },
    /// Coverage can't be trusted from `at`: a device change, a handed-off session, a failed registration or
    /// callback.
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
    /// Why the monitor reports nothing but health unknown (no apps, or not opted in).
    disabled: Option<&'static str>,
    devices: Option<Vec<String>>,
    /// The last reconciliation, and the one before it.
    last_ok: Option<Ns>,
    prev_ok: Option<Ns>,
    /// Every session known from notifications and snapshots, until it ends for good: key → (image, active).
    live: HashMap<String, (Option<String>, bool)>,
    /// The earliest time coverage is unproven since; unknown until a reconciliation agrees.
    blind_since: Option<Ns>,
    /// The newest event applied.
    last_event: Option<Ns>,
}

impl Watcher {
    pub fn new(apps: Vec<String>, experimental: bool) -> Watcher {
        let disabled = if apps.is_empty() {
            Some(NO_APPS)
        } else if !experimental {
            Some(EXPERIMENTAL)
        } else {
            None
        };
        Watcher {
            monitor: Monitor::new(true),
            apps,
            disabled,
            devices: None,
            last_ok: None,
            prev_ok: None,
            live: HashMap::new(),
            blind_since: None,
            last_event: None,
        }
    }

    /// Whether the OS side should be used at all.
    pub fn enabled(&self) -> bool {
        self.disabled.is_none()
    }

    fn live_dictating(&self) -> bool {
        self.live
            .values()
            .any(|(image, active)| dictating_one(image.as_deref(), *active, &self.apps))
    }

    fn blind(&mut self, at: Ns) {
        self.blind_since = Some(self.blind_since.map_or(at, |b| b.min(at)));
    }

    fn doubt(&mut self, reason: &str, at: Ns) -> Vec<Msg> {
        self.blind(at);
        self.monitor.lost(reason, at)
    }

    /// Whether the sessions already show what `event` says.
    fn reflects(&self, event: &Event) -> bool {
        match event {
            Event::Session { key, active, .. } => self.live.get(key).map(|s| s.1) == Some(*active),
            Event::Gone { key, .. } => !self.live.contains_key(key),
            Event::Doubt { .. } => false,
        }
    }

    /// An OS event. Edges go out as it says. One that is out of order, or no newer than the last
    /// reconciliation, is not applied: unless the sessions already show it, coverage is unknown from the
    /// earliest point it could concern.
    pub fn event(&mut self, event: Event) -> Vec<Msg> {
        if self.disabled.is_some() {
            return vec![];
        }
        let at = match &event {
            Event::Session { at, .. } | Event::Gone { at, .. } | Event::Doubt { at, .. } => *at,
        };
        if let Event::Doubt { reason, .. } = &event {
            return self.doubt(reason, at);
        }
        let stale =
            self.last_ok.is_some_and(|ok| at <= ok) || self.last_event.is_some_and(|l| at < l);
        if stale {
            if self.reflects(&event) {
                return vec![];
            }
            // The callback ran at `at`; the change itself may be older, back to the reconciliation before it.
            let from = self.prev_ok.map_or(at, |p| p.min(at));
            return self.doubt("audio session events out of order", from);
        }
        self.last_event = Some(at);
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

    /// Whether the events account for `snap`: same devices, and every enumerated session known in the state
    /// the events gave it. Sessions the enumeration omitted are kept as they are.
    fn agrees(&self, snap: &Snapshot, devices: &[String]) -> bool {
        self.devices.as_deref() == Some(devices)
            && snap
                .sessions
                .iter()
                .all(|s| self.live.get(&s.key).is_some_and(|(_, a)| *a == s.active))
    }

    /// Reconciles with a fresh snapshot and reports what changed.
    pub fn poll(&mut self, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        if let Some(reason) = self.disabled {
            return self.monitor.lost(reason, now);
        }
        let snap = match src.snapshot() {
            Ok(s) => s,
            Err(e) => {
                self.devices = None;
                return self.doubt(&format!("audio sessions unavailable: {e}"), now);
            }
        };
        let mut devices = snap.devices.clone();
        devices.sort();
        for key in &snap.gone {
            self.live.remove(key);
        }
        let agrees = self.last_ok.is_some() && self.agrees(&snap, &devices);
        if !agrees {
            self.blind(self.last_ok.unwrap_or(now));
        }
        for s in &snap.sessions {
            self.live.insert(s.key.clone(), (s.image.clone(), s.active));
        }
        self.devices = Some(devices);
        self.prev_ok = self.last_ok;
        self.last_ok = Some(now);
        let mut out = vec![];
        if let Some(since) = self.blind_since.take() {
            if since < now {
                out.push(Msg::Unknown {
                    from_ns: since,
                    to_ns: now,
                });
            }
            if !agrees {
                // Unknown until a reconciliation agrees.
                out.extend(self.doubt(RECONCILING, now));
                return out;
            }
        }
        out.extend(self.monitor.observe(self.live_dictating(), now));
        out
    }

    /// The 1 s timer: heartbeat, and health unknown once reconciliation stalls.
    pub fn tick(&mut self, now: Ns) -> Vec<Msg> {
        if let Some(reason) = self.disabled {
            let mut out = self.monitor.lost(reason, now);
            out.extend(self.monitor.tick(now));
            return out;
        }
        self.monitor.tick(now)
    }

    /// The first half of `sync`: any stalled interval, then a fresh reconciliation.
    pub fn sync_start(&mut self, src: &mut impl SessionSource, now: Ns) -> Vec<Msg> {
        let mut out = self.monitor.stall_check(now);
        out.extend(self.poll(src, now));
        out
    }

    /// The second half: the events that arrived while the snapshot was read, then the answer, timestamped after
    /// them. Anything still unproven is reported as unknown up to the answer.
    pub fn sync_finish(&mut self, id: u64, events: Vec<Event>, reply_at: Ns) -> Vec<Msg> {
        let mut out = vec![];
        for e in events {
            out.extend(self.event(e));
        }
        if let Some(since) = self.blind_since
            && since < reply_at
        {
            out.push(Msg::Unknown {
                from_ns: since,
                to_ns: reply_at,
            });
        }
        out.push(self.monitor.synced(id, reply_at));
        out
    }
}

/// Into the monitor loop: the core's requests, the OS's events and callback hand-offs, in arrival order.
pub enum Req {
    Sync(u64),
    Event(Event),
    Handoff(Box<dyn Any + Send>),
    Quit,
}

/// The monitor loop: reconciles every `POLL`, ticks every second, handles events, hand-offs and `sync` in order.
/// A `sync` drains what arrived while its snapshot was read before it answers. Returns on `Quit` or when every
/// sender is gone.
pub fn run(
    mut watcher: Watcher,
    src: &mut impl SessionSource,
    rx: Receiver<Req>,
    mut send: impl FnMut(Vec<Msg>),
) {
    let mut next_poll = Instant::now();
    let mut next_tick = Instant::now();
    let mut backlog: VecDeque<Req> = VecDeque::new();
    loop {
        let wait = next_poll
            .min(next_tick)
            .saturating_duration_since(Instant::now());
        let req = match backlog.pop_front() {
            Some(r) => Ok(r),
            None => rx.recv_timeout(wait),
        };
        match req {
            Ok(Req::Sync(id)) => {
                let mut out = watcher.sync_start(src, now_ns());
                let mut events = vec![];
                while let Ok(r) = rx.try_recv() {
                    match r {
                        Req::Event(e) => events.push(e),
                        Req::Handoff(m) if watcher.enabled() => {
                            events.extend(src.handoff(m, now_ns()))
                        }
                        Req::Handoff(_) => {}
                        other => backlog.push_back(other),
                    }
                }
                out.extend(watcher.sync_finish(id, events, now_ns()));
                send(out);
            }
            Ok(Req::Event(e)) => send(watcher.event(e)),
            Ok(Req::Handoff(m)) => {
                if watcher.enabled() {
                    for e in src.handoff(m, now_ns()) {
                        send(watcher.event(e));
                    }
                }
            }
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
            gone: vec![],
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

    /// A watcher that has reconciled twice (startup is unknown until a second, agreeing snapshot).
    fn settled(
        first: Result<Snapshot, String>,
        rest: Vec<Result<Snapshot, String>>,
    ) -> (Watcher, Fake) {
        let mut w = Watcher::new(apps(), true);
        let mut all = vec![first.clone(), first];
        all.extend(rest);
        let mut src = Fake(all);
        w.poll(&mut src, ms(500));
        w.poll(&mut src, ms(1000));
        assert!(w.monitor.healthy());
        (w, src)
    }

    #[test]
    fn off_by_default_it_reports_health_unknown_and_never_reads_sessions() {
        struct Never;
        impl SessionSource for Never {
            fn snapshot(&mut self) -> Result<Snapshot, String> {
                panic!("read while disabled");
            }
        }
        let mut w = Watcher::new(apps(), false);
        assert!(!w.enabled());
        let out = w.poll(&mut Never, SECOND);
        assert_eq!(health(&out), Some((false, EXPERIMENTAL.into())));
        assert!(w.event(state("k", DICTATE, true, SECOND)).is_empty());
        let mut out = w.sync_start(&mut Never, 2 * SECOND);
        out.extend(w.sync_finish(1, vec![], 2 * SECOND));
        assert!(matches!(
            out.last(),
            Some(Msg::Synced { healthy: false, .. })
        ));
        let tick = w.tick(5 * SECOND);
        assert_eq!(health(&tick), Some((false, EXPERIMENTAL.into())));
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
    fn a_session_of_several_processes_is_unidentified() {
        assert_eq!(single_process(0, 1234), Some(1234));
        assert_eq!(single_process(AUDCLNT_S_NO_SINGLE_PROCESS, 1234), None);
        assert_eq!(single_process(0, 0), None);
        assert_eq!(single_process(0x8000_4005_u32 as i32, 1234), None);
        let s = snap(&["mic"], &[("k", None, true)]).unwrap();
        assert!(dictating(&s, &apps()));
    }

    #[test]
    fn without_apps_health_stays_unknown() {
        let mut w = Watcher::new(vec![], true);
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let out = w.poll(&mut src, SECOND);
        assert_eq!(health(&out), Some((false, NO_APPS.into())));
    }

    #[test]
    fn startup_is_unknown_until_a_second_snapshot_agrees() {
        let mut w = Watcher::new(apps(), true);
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let out = w.poll(&mut src, ms(500));
        assert!(!w.monitor.healthy(), "{out:?}");
        let out = w.poll(&mut src, ms(750));
        assert_eq!(unknowns(&out), [(ms(500), ms(750))]);
        assert!(w.monitor.healthy());
    }

    #[test]
    fn a_capture_between_two_polls_is_reported_from_its_events() {
        let (mut w, mut src) = settled(snap(&["mic"], &[("k", Some(DICTATE), false)]), vec![]);
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
    fn a_short_capture_whose_callbacks_queue_behind_a_sync_is_unknown() {
        let (mut w, mut src) = settled(snap(&["mic"], &[("k", Some(DICTATE), false)]), vec![]);
        // The snapshot at 1250 sees the session idle again; both callbacks are drained after it.
        let mut out = w.sync_start(&mut src, ms(1250));
        out.extend(w.sync_finish(
            9,
            vec![
                state("k", DICTATE, true, ms(1100)),
                state("k", DICTATE, false, ms(1200)),
            ],
            ms(1260),
        ));
        let u = unknowns(&out);
        assert!(
            u.iter().any(|(f, t)| *f <= ms(1100) && *t == ms(1260)),
            "{out:?}"
        );
        let synced = out
            .iter()
            .position(|m| matches!(m, Msg::Synced { id: 9, at_ns, .. } if *at_ns == ms(1260)));
        let unknown = out.iter().position(|m| matches!(m, Msg::Unknown { .. }));
        assert!(synced.is_some() && unknown < synced, "{out:?}");
    }

    #[test]
    fn late_events_in_reverse_order_are_not_applied_and_go_back_to_the_earliest() {
        let (mut w, mut src) = settled(snap(&["mic"], &[("k", Some(DICTATE), false)]), vec![]);
        w.poll(&mut src, ms(1250));
        // Arrive after the 1250 snapshot, newest first.
        w.event(state("k", DICTATE, false, ms(1200)));
        w.event(state("k", DICTATE, true, ms(1100)));
        assert!(!w.monitor.capturing(), "a stale transition was applied");
        let out = w.poll(&mut src, ms(1500));
        assert_eq!(unknowns(&out), [(ms(1000), ms(1500))]);
    }

    #[test]
    fn an_event_older_than_a_newer_snapshot_is_unknown() {
        let (mut w, mut src) = settled(snap(&["mic"], &[]), vec![]);
        w.poll(&mut src, ms(1250));
        let out = w.event(state("new", DICTATE, true, ms(1240)));
        assert_eq!(health(&out).map(|h| h.0), Some(false));
        assert!(!w.monitor.capturing());
        // Out of order with an applied event, too.
        let (mut w, _) = settled(snap(&["mic"], &[]), vec![]);
        w.event(state("a", r"C:\x\other.exe", true, ms(1200)));
        let out = w.event(state("b", DICTATE, true, ms(1100)));
        assert_eq!(health(&out).map(|h| h.0), Some(false));
    }

    #[test]
    fn a_new_session_that_captures_is_dictation() {
        let (mut w, mut src) = settled(
            snap(&["mic"], &[]),
            vec![snap(&["mic"], &[("new", Some(DICTATE), true)])],
        );
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
    fn a_session_the_enumeration_omits_is_kept_until_it_ends() {
        let (mut w, mut src) = settled(snap(&["mic"], &[]), vec![]);
        w.event(state("cb", DICTATE, true, ms(1100)));
        let out = w.poll(&mut src, ms(1250));
        assert!(unknowns(&out).is_empty(), "{out:?}");
        assert!(w.monitor.capturing(), "an omitted session was dropped");
        w.event(Event::Gone {
            key: "cb".into(),
            at: ms(1300),
        });
        assert!(!w.monitor.capturing());
    }

    #[test]
    fn lost_events_stay_unknown_until_a_reconciliation_agrees() {
        let (mut w, mut src) = settled(
            snap(&["mic"], &[]),
            vec![
                snap(&["mic"], &[("missed", Some(DICTATE), false)]),
                snap(&["mic"], &[("missed", Some(DICTATE), false)]),
            ],
        );
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1000), ms(1250))]);
        assert!(!w.monitor.healthy());
        let out = w.poll(&mut src, ms(1500));
        assert_eq!(unknowns(&out), [(ms(1250), ms(1500))]);
        assert!(w.monitor.healthy());
    }

    #[test]
    fn a_device_change_or_doubt_is_unknown() {
        let (mut w, mut src) = settled(snap(&["mic"], &[]), vec![snap(&["mic", "headset"], &[])]);
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1000), ms(1250))]);
        let (mut w, mut src) = settled(snap(&["mic"], &[]), vec![]);
        let out = w.event(Event::Doubt {
            reason: "new session handed off".into(),
            at: ms(1100),
        });
        assert_eq!(health(&out).map(|h| h.0), Some(false));
        let out = w.poll(&mut src, ms(1250));
        assert_eq!(unknowns(&out), [(ms(1100), ms(1250))]);
        assert!(w.monitor.healthy());
    }

    #[test]
    fn retired_sessions_leave_with_their_endpoint() {
        let (mut w, mut src) = settled(
            snap(&["mic"], &[("k", Some(DICTATE), true)]),
            vec![Ok(Snapshot {
                devices: vec![],
                sessions: vec![],
                gone: vec!["k".into()],
            })],
        );
        assert!(w.monitor.capturing());
        w.poll(&mut src, ms(1250));
        assert!(!w.live_dictating());
    }

    #[test]
    fn the_loop_drains_events_before_answering_sync() {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = now_ns();
        tx.send(Req::Sync(3)).unwrap();
        tx.send(Req::Event(state("k", DICTATE, true, t))).unwrap();
        tx.send(Req::Quit).unwrap();
        let mut src = Fake(vec![snap(&["mic"], &[])]);
        let mut sent = vec![];
        run(Watcher::new(apps(), true), &mut src, rx, |m| sent.extend(m));
        assert!(
            sent.iter().any(|m| matches!(m, Msg::Synced { id: 3, .. })),
            "{sent:?}"
        );
    }
}

//! The `dictation` capability: which capture streams belong to a configured dictation app, and the health,
//! heartbeat, stall and `sync` rules of docs/protocol.md. The OS side only reports complete observations.

use crate::clock::{MILLI, Ns, SECOND};
use serde_json::{Value, json};

/// Stream properties that name the application, checked in this order.
pub const APP_KEYS: [&str; 3] = [
    "application.process.binary",
    "application.name",
    "application.id",
];

/// The media class of an application's audio capture stream (PipeWire).
pub const CAPTURE_CLASS: &str = "Stream/Input/Audio";

/// A capture stream belongs to a dictation app when one of its name properties equals a configured name
/// (case-insensitive; a binary also matches by its file name).
pub fn matches_app<'a>(prop: impl Fn(&str) -> Option<&'a str>, apps: &[String]) -> bool {
    APP_KEYS.iter().any(|k| {
        prop(k).is_some_and(|v| {
            let v = v.trim();
            let base = v.rsplit('/').next().unwrap_or(v);
            apps.iter().any(|a| {
                let a = a.trim();
                !a.is_empty() && (a.eq_ignore_ascii_case(v) || a.eq_ignore_ascii_case(base))
            })
        })
    })
}

/// What a node is, from everything known about it. Capture detection, snapshot completeness, edges and
/// `zora-dictation watch` all use this one classification (`StreamNode::effective`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Proven not to be an application's capture stream.
    Irrelevant,
    /// A capture stream that names its app; `matches` when that app is a configured dictation tool.
    Capture { matches: bool, running: bool },
    /// Not proven either way: no properties yet, conflicting classes, or a capture stream without an app.
    Unclassified,
}

/// One audio node: the media class its registry entry advertised, and its latest info.
#[derive(Debug, Default, Clone)]
pub struct StreamNode {
    props: std::collections::HashMap<String, String>,
    global_class: Option<String>,
    pub running: bool,
    seen_info: bool,
}

impl StreamNode {
    /// Replaces the properties. PipeWire sends the full set when they change (its change mask says so) and an
    /// empty set otherwise; the caller passes only the former.
    pub fn set_props<'a>(&mut self, props: impl IntoIterator<Item = (&'a str, &'a str)>) {
        self.props = props
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    }

    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    /// The one classification: registry class and info properties together.
    pub fn effective(&self, apps: &[String]) -> Class {
        let class = match (self.prop("media.class"), self.global_class.as_deref()) {
            (Some(a), Some(b)) if a != b => return Class::Unclassified,
            (a, b) => a.or(b),
        };
        match class {
            Some(c) if c != CAPTURE_CLASS => Class::Irrelevant,
            Some(_) if self.identified() => Class::Capture {
                matches: matches_app(|k| self.prop(k), apps),
                running: self.running,
            },
            Some(_) => Class::Unclassified,
            None if self.is_driver() => Class::Irrelevant,
            None => Class::Unclassified,
        }
    }

    fn identified(&self) -> bool {
        APP_KEYS
            .iter()
            .any(|k| self.prop(k).is_some_and(|v| !v.trim().is_empty()))
    }

    /// PipeWire's graph drivers (Dummy-Driver, Freewheel-Driver) carry no media class but say what they are.
    pub fn is_driver(&self) -> bool {
        self.prop("factory.name") == Some("support.node.driver")
            || self.prop("node.driver") == Some("true")
    }
}

/// Configured app names from `hello.config.apps`.
pub fn apps_from_config(config: &Value) -> Vec<String> {
    config["apps"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Dictation {
        began: bool,
        at_ns: Ns,
    },
    Health {
        healthy: bool,
        capturing: bool,
        reason: String,
        at_ns: Ns,
    },
    Unknown {
        from_ns: Ns,
        to_ns: Ns,
    },
    Synced {
        id: u64,
        at_ns: Ns,
        healthy: bool,
        capturing: bool,
    },
}

impl Msg {
    pub fn to_json(&self) -> Value {
        match self {
            Msg::Dictation { began, at_ns } => {
                json!({"t": "dictation", "state": if *began { "began" } else { "ended" }, "at_ns": at_ns})
            }
            Msg::Health {
                healthy,
                capturing,
                reason,
                at_ns,
            } => {
                json!({"t": "health", "healthy": healthy, "capturing": capturing, "reason": reason, "at_ns": at_ns})
            }
            Msg::Unknown { from_ns, to_ns } => {
                json!({"t": "unknown", "from_ns": from_ns, "to_ns": to_ns})
            }
            Msg::Synced {
                id,
                at_ns,
                healthy,
                capturing,
            } => {
                json!({"t": "synced", "id": id, "at_ns": at_ns, "healthy": healthy, "capturing": capturing})
            }
        }
    }
}

pub const STALL: Ns = 1500 * MILLI;
pub const HEARTBEAT: Ns = 2 * SECOND;

/// Health and capture state. Starts unknown (unhealthy) and becomes healthy only after a complete snapshot.
#[derive(Debug, Clone)]
pub struct Monitor {
    healthy: bool,
    capturing: bool,
    reason: String,
    last_confirmed: Option<Ns>,
    last_heartbeat: Option<Ns>,
    has_apps: bool,
}

impl Monitor {
    pub fn new(has_apps: bool) -> Self {
        Monitor {
            healthy: false,
            capturing: false,
            reason: "starting".into(),
            last_confirmed: None,
            last_heartbeat: None,
            has_apps,
        }
    }

    pub fn healthy(&self) -> bool {
        self.healthy
    }

    pub fn capturing(&self) -> bool {
        self.capturing
    }

    /// `unknown{last_confirmed, now}` when observations stopped for longer than the stall limit. Sent before any
    /// further state or `synced` message.
    pub fn stall_check(&mut self, now: Ns) -> Vec<Msg> {
        match self.last_confirmed {
            Some(l) if now - l > STALL => vec![Msg::Unknown {
                from_ns: l,
                to_ns: now,
            }],
            _ => vec![],
        }
    }

    /// A capture stream changed between snapshots. Edges go out as they happen, so a dictation that starts and
    /// stops between two snapshots is still reported.
    pub fn changed(&mut self, capturing: bool, now: Ns) -> Vec<Msg> {
        if capturing == self.capturing {
            return vec![];
        }
        let mut out = self.stall_check(now);
        self.capturing = capturing;
        out.push(Msg::Dictation {
            began: capturing,
            at_ns: now,
        });
        out
    }

    /// A complete, fresh snapshot of every capture stream. `capturing`: a matching stream is running.
    pub fn observe(&mut self, capturing: bool, now: Ns) -> Vec<Msg> {
        let mut out = self.stall_check(now);
        self.last_confirmed = Some(now);
        if capturing != self.capturing {
            self.capturing = capturing;
            out.push(Msg::Dictation {
                began: capturing,
                at_ns: now,
            });
        }
        let (healthy, reason) = if self.has_apps {
            (true, "ok")
        } else {
            (false, "no dictation apps configured (dictation.linuxApps)")
        };
        if healthy != self.healthy || reason != self.reason {
            self.healthy = healthy;
            self.reason = reason.into();
            out.push(self.health(now));
            self.last_heartbeat = Some(now);
        }
        out
    }

    /// Observation is impossible (server gone, not running, unsupported audio system).
    pub fn lost(&mut self, reason: &str, now: Ns) -> Vec<Msg> {
        // Once unhealthy the core treats everything as unknown; the interval is reported when observation resumes.
        let mut out = if self.healthy {
            self.stall_check(now)
        } else {
            vec![]
        };
        if self.healthy || self.reason != reason {
            self.healthy = false;
            self.reason = reason.into();
            out.push(self.health(now));
            self.last_heartbeat = Some(now);
        }
        out
    }

    /// The 1 s timer. Health goes unknown when observations stall; a heartbeat goes out every 2 s.
    pub fn tick(&mut self, now: Ns) -> Vec<Msg> {
        let mut out = vec![];
        let stalled = self.last_confirmed.is_some_and(|l| now - l > STALL);
        if stalled && self.healthy {
            out.extend(self.stall_check(now));
            self.healthy = false;
            self.reason = "observations stalled".into();
            out.push(self.health(now));
            self.last_heartbeat = Some(now);
        }
        if self.last_heartbeat.is_none_or(|h| now - h >= HEARTBEAT) {
            if self.healthy {
                out.extend(self.stall_check(now));
            }
            out.push(self.health(now));
            self.last_heartbeat = Some(now);
        }
        out
    }

    /// The answer to `sync`, after `stall_check`, then a fresh `observe` (or `lost`).
    pub fn synced(&self, id: u64, now: Ns) -> Msg {
        Msg::Synced {
            id,
            at_ns: now,
            healthy: self.healthy,
            capturing: self.capturing,
        }
    }

    fn health(&self, now: Ns) -> Msg {
        Msg::Health {
            healthy: self.healthy,
            capturing: self.capturing,
            reason: self.reason.clone(),
            at_ns: now,
        }
    }
}

/// The audio nodes seen on one connection. A node is a blind spot while it is unclassified: from its discovery,
/// or from the update that took away what classified it. Every blind interval is reported as unknown when it
/// closes, whatever the node turns out to be, because its final identity doesn't prove what it did meanwhile.
/// The only exceptions: a node the registry already proved irrelevant at discovery never becomes a blind spot,
/// and a driver-marked node closes its interval silently.
#[derive(Debug)]
pub struct Graph {
    pub monitor: Monitor,
    apps: Vec<String>,
    streams: std::collections::HashMap<u32, StreamNode>,
    /// Blind spots and since when.
    blind: std::collections::HashMap<u32, Ns>,
    /// Blind spots that were dictating when they went blind: they still count as capturing.
    held: std::collections::HashSet<u32>,
}

impl Graph {
    pub fn new(apps: Vec<String>) -> Graph {
        Graph {
            monitor: Monitor::new(!apps.is_empty()),
            apps,
            streams: Default::default(),
            blind: Default::default(),
            held: Default::default(),
        }
    }

    pub fn class(&self, id: u32) -> Option<Class> {
        self.streams.get(&id).map(|n| n.effective(&self.apps))
    }

    /// A node appeared. `media_class` is from its registry entry, when it carries one.
    pub fn discovered(&mut self, id: u32, media_class: Option<&str>, now: Ns) {
        let node = self.streams.entry(id).or_default();
        node.global_class = media_class.map(String::from);
        if node.effective(&self.apps) != Class::Irrelevant {
            self.blind.insert(id, now);
        }
    }

    /// Whether the node has delivered info before (its first info carries every property).
    pub fn known(&self, id: u32) -> bool {
        self.streams.get(&id).is_some_and(|n| n.seen_info)
    }

    /// A node's info. `props` is the full property set when it changed (or on the first info), else None.
    pub fn info<'a>(
        &mut self,
        id: u32,
        props: Option<impl IntoIterator<Item = (&'a str, &'a str)>>,
        running: bool,
        now: Ns,
    ) -> Vec<Msg> {
        let was_dictating = self.dictating(id);
        let node = self.streams.entry(id).or_default();
        node.seen_info = true;
        if let Some(p) = props {
            node.set_props(p);
        }
        node.running = running;
        let class = node.effective(&self.apps);
        let driver = node.is_driver();
        let mut out = vec![];
        match (class, self.blind.contains_key(&id)) {
            (Class::Unclassified, false) => {
                self.blind.insert(id, now);
                if was_dictating {
                    self.held.insert(id);
                }
            }
            (Class::Unclassified, true) => {}
            (_, true) => out.extend(self.close_blind(id, driver, now)),
            (_, false) => {}
        }
        out.extend(self.edges(now));
        out
    }

    pub fn removed(&mut self, id: u32, now: Ns) -> Vec<Msg> {
        let driver = self.streams.get(&id).is_some_and(StreamNode::is_driver);
        let mut out = self.close_blind(id, driver, now);
        self.streams.remove(&id);
        out.extend(self.edges(now));
        out
    }

    fn close_blind(&mut self, id: u32, driver: bool, now: Ns) -> Vec<Msg> {
        self.held.remove(&id);
        match self.blind.remove(&id) {
            Some(since) if !driver => vec![Msg::Unknown {
                from_ns: since,
                to_ns: now,
            }],
            _ => vec![],
        }
    }

    pub fn complete(&self) -> bool {
        self.blind.is_empty()
    }

    /// A configured dictation app is capturing through this node now, or may be (held while blind).
    fn dictating(&self, id: u32) -> bool {
        self.held.contains(&id)
            || matches!(
                self.class(id),
                Some(Class::Capture {
                    matches: true,
                    running: true
                })
            )
    }

    pub fn capturing(&self) -> bool {
        self.streams.keys().any(|id| self.dictating(*id))
    }

    pub fn node(&self, id: u32) -> Option<&StreamNode> {
        self.streams.get(&id)
    }

    /// The connection is gone; everything learned on it is forgotten.
    pub fn clear(&mut self) {
        self.streams.clear();
        self.blind.clear();
        self.held.clear();
    }

    fn edges(&mut self, now: Ns) -> Vec<Msg> {
        let capturing = self.capturing();
        self.monitor.changed(capturing, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn props(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn m(p: &HashMap<String, String>, apps: &[&str]) -> bool {
        let apps: Vec<String> = apps.iter().map(|s| s.to_string()).collect();
        matches_app(|k| p.get(k).map(String::as_str), &apps)
    }

    #[test]
    fn matches_by_binary_name_or_id() {
        let p = props(&[
            ("application.process.binary", "/usr/bin/nerd-dictation"),
            ("application.name", "Nerd Dictation"),
        ]);
        assert!(m(&p, &["nerd-dictation"]));
        assert!(m(&p, &["NERD DICTATION"]));
        assert!(!m(&p, &["dictation"]), "no substring matches");
        assert!(!m(&p, &[""]), "an empty name matches nothing");
        assert!(!m(&p, &[]));
        let p = props(&[("application.id", "com.example.Talk")]);
        assert!(m(&p, &["com.example.talk"]));
        let p = props(&[("node.name", "nerd-dictation")]);
        assert!(
            !m(&p, &["nerd-dictation"]),
            "only application properties count"
        );
    }

    #[test]
    fn dictation_between_two_snapshots_is_still_reported() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        mon.observe(false, t);
        let began = mon.changed(true, t + 300 * MILLI);
        let ended = mon.changed(false, t + 600 * MILLI);
        assert_eq!(
            began,
            [Msg::Dictation {
                began: true,
                at_ns: t + 300 * MILLI
            }]
        );
        assert_eq!(
            ended,
            [Msg::Dictation {
                began: false,
                at_ns: t + 600 * MILLI
            }]
        );
        assert!(mon.changed(false, t + 700 * MILLI).is_empty());
        // The next snapshot (or a command's sync) sees nothing running, and repeats no edge.
        assert!(mon.observe(false, t + SECOND).is_empty());
        assert_eq!(
            mon.synced(1, t + SECOND),
            Msg::Synced {
                id: 1,
                at_ns: t + SECOND,
                healthy: true,
                capturing: false
            }
        );
    }

    const REC: &str = "rec";

    fn graph(t: Ns) -> Graph {
        let mut g = Graph::new(vec![REC.into()]);
        g.monitor.observe(false, t);
        g
    }

    fn unknown(from_ns: Ns, to_ns: Ns) -> Msg {
        Msg::Unknown { from_ns, to_ns }
    }

    const NONE: Option<[(&str, &str); 0]> = None;

    #[test]
    fn registry_class_and_info_identity_together_detect_a_recorder() {
        let t = 10 * SECOND;
        let mut g = graph(t);
        g.discovered(1, Some(CAPTURE_CLASS), t);
        // The info names the app but not the class; the registry supplied that.
        let out = g.info(1, Some([("application.name", REC)]), true, t + 100 * MILLI);
        assert_eq!(
            g.class(1),
            Some(Class::Capture {
                matches: true,
                running: true
            })
        );
        assert!(g.capturing());
        assert!(matches!(
            out.last(),
            Some(Msg::Dictation { began: true, .. })
        ));
        assert!(matches!(
            g.monitor.synced(1, t + 200 * MILLI),
            Msg::Synced {
                capturing: true,
                ..
            }
        ));
    }

    #[test]
    fn every_blind_interval_is_unknown_when_it_closes() {
        let t = 10 * SECOND;
        let mut g = graph(t);
        g.discovered(1, Some(CAPTURE_CLASS), t);
        let rec = [("media.class", CAPTURE_CLASS), ("application.name", REC)];
        assert_eq!(g.info(1, Some(rec), false, t), [unknown(t, t)]);
        // Idle, then it loses its identity, runs while unclassified, and comes back as another app.
        assert!(
            g.info(
                1,
                Some([("media.class", CAPTURE_CLASS)]),
                false,
                t + 100 * MILLI
            )
            .is_empty()
        );
        assert!(!g.complete());
        assert!(g.info(1, NONE, true, t + 200 * MILLI).is_empty());
        let other = [
            ("media.class", CAPTURE_CLASS),
            ("application.name", "browser"),
        ];
        let out = g.info(1, Some(other), false, t + 400 * MILLI);
        assert_eq!(out, [unknown(t + 100 * MILLI, t + 400 * MILLI)]);
        assert!(g.complete());
        // A node seen only through its registry entry and removed unseen: also unknown.
        g.discovered(2, None, t + 500 * MILLI);
        assert_eq!(
            g.removed(2, t + 600 * MILLI),
            [unknown(t + 500 * MILLI, t + 600 * MILLI)]
        );
        // Even when the first info proves it irrelevant (its registry entry had no class).
        g.discovered(3, None, t + 600 * MILLI);
        let out = g.info(
            3,
            Some([("media.class", "Audio/Sink")]),
            false,
            t + 700 * MILLI,
        );
        assert_eq!(out, [unknown(t + 600 * MILLI, t + 700 * MILLI)]);
    }

    #[test]
    fn only_registry_proof_or_a_driver_marker_avoid_blind_spots() {
        let t = 10 * SECOND;
        let mut g = graph(t);
        g.discovered(1, Some("Audio/Sink"), t);
        assert!(g.complete(), "proved irrelevant at discovery");
        assert!(
            g.info(1, Some([("node.name", "speakers")]), false, t)
                .is_empty()
        );
        g.discovered(2, None, t);
        let driver = [
            ("node.name", "Dummy-Driver"),
            ("factory.name", "support.node.driver"),
            ("node.driver", "true"),
        ];
        assert!(g.info(2, Some(driver), true, t + 100 * MILLI).is_empty());
        assert!(g.complete());
        // Proving nothing: empty properties, a name only, a capture stream without an app, conflicting classes.
        type Case<'a> = (Option<&'a str>, Vec<(&'a str, &'a str)>);
        let cases: [Case; 4] = [
            (None, vec![]),
            (None, vec![("node.name", "x")]),
            (Some(CAPTURE_CLASS), vec![("node.name", "x")]),
            (
                Some(CAPTURE_CLASS),
                vec![("media.class", "Audio/Sink"), ("application.name", "x")],
            ),
        ];
        for (i, (global, props)) in cases.into_iter().enumerate() {
            let id = 10 + i as u32;
            g.discovered(id, global, t);
            g.info(id, Some(props), false, t);
            assert_eq!(g.class(id), Some(Class::Unclassified), "case {i}");
            assert!(!g.complete());
            g.removed(id, t);
        }
        assert!(g.complete());
    }

    #[test]
    fn a_running_recorder_that_goes_blind_still_counts_as_capturing() {
        let t = 10 * SECOND;
        let mut g = graph(t);
        g.discovered(1, Some(CAPTURE_CLASS), t);
        let rec = [("media.class", CAPTURE_CLASS), ("application.name", REC)];
        g.info(1, Some(rec), true, t);
        assert!(g.capturing());
        let out = g.info(
            1,
            Some([("media.class", CAPTURE_CLASS)]),
            true,
            t + 200 * MILLI,
        );
        assert!(out.is_empty(), "no ended: {out:?}");
        assert!(g.capturing() && !g.complete());
        let other = [
            ("media.class", CAPTURE_CLASS),
            ("application.name", "browser"),
        ];
        let out = g.info(1, Some(other), true, t + 500 * MILLI);
        assert_eq!(
            out,
            [
                unknown(t + 200 * MILLI, t + 500 * MILLI),
                Msg::Dictation {
                    began: false,
                    at_ns: t + 500 * MILLI
                }
            ]
        );
    }

    #[test]
    fn graph_reports_edges_from_node_info() {
        let t = 10 * SECOND;
        let mut g = graph(t);
        g.discovered(3, Some(CAPTURE_CLASS), t);
        let props = [("media.class", CAPTURE_CLASS), ("application.name", REC)];
        assert_eq!(g.info(3, Some(props), false, t), [unknown(t, t)]);
        assert!(g.complete() && g.known(3));
        let out = g.info(3, NONE, true, t + SECOND);
        assert!(matches!(out[0], Msg::Dictation { began: true, .. }));
        let out = g.removed(3, t + 1200 * MILLI);
        assert_eq!(
            out,
            [Msg::Dictation {
                began: false,
                at_ns: t + 1200 * MILLI
            }]
        );
    }

    #[test]
    fn apps_come_from_hello_config() {
        let c = json!({"apps": [" talk ", "", 3, "x"]});
        assert_eq!(apps_from_config(&c), ["talk", "x"]);
        assert!(apps_from_config(&json!({})).is_empty());
    }

    #[test]
    fn unhealthy_until_first_snapshot_then_reports_capture_edges() {
        let mut mon = Monitor::new(true);
        let t = 100 * SECOND;
        let first = mon.tick(t);
        assert!(matches!(first[0], Msg::Health { healthy: false, .. }));
        let out = mon.observe(false, t);
        assert_eq!(
            out,
            [Msg::Health {
                healthy: true,
                capturing: false,
                reason: "ok".into(),
                at_ns: t
            }]
        );
        let out = mon.observe(true, t + SECOND);
        assert_eq!(
            out,
            [Msg::Dictation {
                began: true,
                at_ns: t + SECOND
            }]
        );
        assert!(mon.observe(true, t + 2 * SECOND).is_empty());
        let out = mon.observe(false, t + 3 * SECOND);
        assert_eq!(
            out,
            [Msg::Dictation {
                began: false,
                at_ns: t + 3 * SECOND
            }]
        );
    }

    #[test]
    fn fails_closed_without_configured_apps() {
        let mut mon = Monitor::new(false);
        let out = mon.observe(false, SECOND);
        assert!(
            matches!(&out[0], Msg::Health { healthy: false, reason, .. } if reason.contains("linuxApps"))
        );
        assert!(!mon.healthy());
    }

    #[test]
    fn heartbeat_every_two_seconds() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        mon.observe(false, t);
        let mut beats = 0;
        for i in 1..=6 {
            let now = t + i * SECOND;
            mon.observe(false, now);
            beats += mon
                .tick(now)
                .iter()
                .filter(|m| matches!(m, Msg::Health { .. }))
                .count();
        }
        assert_eq!(beats, 3);
    }

    #[test]
    fn stall_sends_unknown_before_further_state() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        mon.observe(false, t);
        // Observations stop: the timer marks health unknown, starting with the uncovered interval.
        let out = mon.tick(t + 2 * SECOND);
        assert_eq!(
            out[0],
            Msg::Unknown {
                from_ns: t,
                to_ns: t + 2 * SECOND
            }
        );
        assert!(matches!(out[1], Msg::Health { healthy: false, .. }));
        // The next completed observation still reports the whole stalled interval first.
        let out = mon.observe(true, t + 3 * SECOND);
        assert_eq!(
            out[0],
            Msg::Unknown {
                from_ns: t,
                to_ns: t + 3 * SECOND
            }
        );
        assert!(matches!(out[1], Msg::Dictation { began: true, .. }));
        assert!(matches!(out[2], Msg::Health { healthy: true, .. }));
    }

    #[test]
    fn sync_reply_reflects_the_fresh_snapshot() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        mon.observe(false, t);
        assert!(mon.stall_check(t + SECOND).is_empty());
        let mut out = mon.observe(true, t + SECOND);
        out.push(mon.synced(4, t + SECOND));
        assert!(matches!(out[0], Msg::Dictation { began: true, .. }));
        assert_eq!(
            out[1],
            Msg::Synced {
                id: 4,
                at_ns: t + SECOND,
                healthy: true,
                capturing: true
            }
        );
        let mut mon = Monitor::new(true);
        mon.observe(false, t);
        mon.lost("PipeWire is not running", t + SECOND);
        assert!(matches!(
            mon.synced(5, t + SECOND),
            Msg::Synced { healthy: false, .. }
        ));
    }

    #[test]
    fn lost_reports_once_until_reason_changes() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        assert_eq!(mon.lost("no server", t).len(), 1);
        assert!(mon.lost("no server", t).is_empty());
        assert_eq!(mon.lost("other", t).len(), 1);
    }

    #[test]
    fn losing_the_server_reports_the_stall_once_then_again_on_recovery() {
        let mut mon = Monitor::new(true);
        let t = 10 * SECOND;
        mon.observe(false, t);
        let out = mon.lost("gone", t + 2 * SECOND);
        assert!(matches!(out[0], Msg::Unknown { .. }));
        assert!(matches!(out[1], Msg::Health { healthy: false, .. }));
        assert!(mon.lost("gone", t + 3 * SECOND).is_empty());
        assert!(
            !mon.tick(t + 5 * SECOND)
                .iter()
                .any(|m| matches!(m, Msg::Unknown { .. }))
        );
        let out = mon.observe(false, t + 6 * SECOND);
        assert_eq!(
            out[0],
            Msg::Unknown {
                from_ns: t,
                to_ns: t + 6 * SECOND
            }
        );
        assert!(matches!(out[1], Msg::Health { healthy: true, .. }));
    }
}

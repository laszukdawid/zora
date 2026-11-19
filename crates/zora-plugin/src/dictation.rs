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

/// One audio node as seen through its info updates.
#[derive(Debug, Default, Clone)]
pub struct StreamNode {
    props: std::collections::HashMap<String, String>,
    capture: bool,
    matches: bool,
    pub running: bool,
}

impl StreamNode {
    /// Replaces the properties. PipeWire sends the full set when they change (its change mask says so) and an
    /// empty set otherwise; the caller passes only the former.
    pub fn set_props<'a>(
        &mut self,
        props: impl IntoIterator<Item = (&'a str, &'a str)>,
        apps: &[String],
    ) {
        self.props = props
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.capture = self.props.get("media.class").map(String::as_str) == Some(CAPTURE_CLASS);
        self.matches = matches_app(|k| self.props.get(k).map(String::as_str), apps);
    }

    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    pub fn is_capture(&self) -> bool {
        self.capture
    }

    /// A configured dictation app is capturing through this node right now.
    pub fn dictating(&self) -> bool {
        self.capture && self.matches && self.running
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

/// The audio nodes seen on one connection. A node whose kind or state is not known yet is a blind spot: it may
/// be a dictation stream capturing right now, so a snapshot is incomplete until its first info arrives, and one
/// that disappears before that leaves an unknown interval.
#[derive(Debug)]
pub struct Graph {
    pub monitor: Monitor,
    apps: Vec<String>,
    streams: std::collections::HashMap<u32, StreamNode>,
    /// Possibly relevant nodes without their first info, and since when.
    awaiting: std::collections::HashMap<u32, Ns>,
}

impl Graph {
    pub fn new(apps: Vec<String>) -> Graph {
        Graph {
            monitor: Monitor::new(!apps.is_empty()),
            apps,
            streams: Default::default(),
            awaiting: Default::default(),
        }
    }

    /// A node appeared. `media_class` is from its global properties, when they carry it.
    pub fn discovered(&mut self, id: u32, media_class: Option<&str>, now: Ns) {
        if media_class.is_none_or(|c| c == CAPTURE_CLASS) {
            self.awaiting.insert(id, now);
        }
    }

    /// Whether the node has delivered info before (its first info carries every property).
    pub fn known(&self, id: u32) -> bool {
        self.streams.contains_key(&id)
    }

    /// A node's info. `props` is the full property set when it changed (or on the first info), else None.
    pub fn info<'a>(
        &mut self,
        id: u32,
        props: Option<impl IntoIterator<Item = (&'a str, &'a str)>>,
        running: bool,
        now: Ns,
    ) -> Vec<Msg> {
        let classified = props.is_some();
        let entry = self.streams.entry(id).or_default();
        if let Some(p) = props {
            entry.set_props(p, &self.apps);
        }
        entry.running = running;
        let mut out = vec![];
        // Until its first info with properties, a node can't be classified and stays a blind spot. A dictation
        // stream may have run and stopped between its discovery and that info, so that time is unknown.
        if classified && let Some(since) = self.awaiting.remove(&id) {
            let e = &self.streams[&id];
            if e.capture && e.matches {
                out.push(Msg::Unknown {
                    from_ns: since,
                    to_ns: now,
                });
            }
        }
        out.extend(self.edges(now));
        out
    }

    pub fn removed(&mut self, id: u32, now: Ns) -> Vec<Msg> {
        let mut out = vec![];
        if let Some(since) = self.awaiting.remove(&id) {
            out.push(Msg::Unknown {
                from_ns: since,
                to_ns: now,
            });
        }
        self.streams.remove(&id);
        out.extend(self.edges(now));
        out
    }

    pub fn complete(&self) -> bool {
        self.awaiting.is_empty()
    }

    pub fn capturing(&self) -> bool {
        self.streams.values().any(StreamNode::dictating)
    }

    pub fn node(&self, id: u32) -> Option<&StreamNode> {
        self.streams.get(&id)
    }

    /// The connection is gone; everything learned on it is forgotten.
    pub fn clear(&mut self) {
        self.streams.clear();
        self.awaiting.clear();
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
    fn property_changes_replace_the_set() {
        let apps = vec!["pw-record".to_string()];
        let mut n = StreamNode::default();
        n.set_props(
            [
                ("media.class", CAPTURE_CLASS),
                ("application.name", "pw-record"),
            ],
            &apps,
        );
        n.running = true;
        assert!(n.dictating());
        n.set_props(
            [
                ("media.class", CAPTURE_CLASS),
                ("node.latency", "1024/48000"),
            ],
            &apps,
        );
        assert!(!n.dictating(), "a removed app name no longer matches");
        let mut other = StreamNode::default();
        other.set_props(
            [
                ("media.class", "Audio/Source"),
                ("application.name", "pw-record"),
            ],
            &apps,
        );
        other.running = true;
        assert!(!other.dictating(), "only capture streams count");
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

    #[test]
    fn a_node_gone_before_its_first_info_leaves_an_unknown_interval() {
        let t = 10 * SECOND;
        let mut g = Graph::new(vec!["rec".into()]);
        g.monitor.observe(false, t);
        g.discovered(7, Some(CAPTURE_CLASS), t + SECOND);
        g.discovered(8, None, t + SECOND);
        g.discovered(9, Some("Audio/Sink"), t + SECOND);
        assert!(!g.complete());
        let out = g.removed(7, t + 2 * SECOND);
        assert_eq!(
            out,
            [Msg::Unknown {
                from_ns: t + SECOND,
                to_ns: t + 2 * SECOND
            }]
        );
        assert!(!g.complete(), "an unclassified node is still a blind spot");
        assert_eq!(g.removed(8, t + 2 * SECOND).len(), 1);
        assert!(g.complete(), "a sink never blocks a snapshot");
        assert!(g.removed(9, t + 2 * SECOND).is_empty());
    }

    #[test]
    fn time_before_a_dictation_streams_first_info_is_unknown() {
        let t = 10 * SECOND;
        let mut g = Graph::new(vec!["rec".into()]);
        g.monitor.observe(false, t);
        g.discovered(4, Some(CAPTURE_CLASS), t + 100 * MILLI);
        g.discovered(5, Some(CAPTURE_CLASS), t + 100 * MILLI);
        // Info without properties can't classify the node: still a blind spot.
        assert!(
            g.info(4, None::<[(&str, &str); 0]>, false, t + 200 * MILLI)
                .is_empty()
        );
        assert!(!g.complete());
        let rec = [("media.class", CAPTURE_CLASS), ("application.name", "rec")];
        let out = g.info(4, Some(rec), false, t + 500 * MILLI);
        assert_eq!(
            out,
            [Msg::Unknown {
                from_ns: t + 100 * MILLI,
                to_ns: t + 500 * MILLI
            }],
            "it may have recorded before it was seen idle"
        );
        // A capture stream of another app is proven irrelevant by its first info.
        let other = [
            ("media.class", CAPTURE_CLASS),
            ("application.name", "browser"),
        ];
        assert!(g.info(5, Some(other), false, t + 500 * MILLI).is_empty());
        assert!(g.complete());
        // The command's sync now reports idle, and the core already holds the unknown interval.
        assert!(g.monitor.observe(false, t + 600 * MILLI).is_empty());
        assert!(matches!(
            g.monitor.synced(1, t + 600 * MILLI),
            Msg::Synced {
                healthy: true,
                capturing: false,
                ..
            }
        ));
    }

    #[test]
    fn graph_reports_edges_from_node_info() {
        let t = 10 * SECOND;
        let mut g = Graph::new(vec!["rec".into()]);
        g.monitor.observe(false, t);
        g.discovered(3, Some(CAPTURE_CLASS), t);
        let props = [("media.class", CAPTURE_CLASS), ("application.name", "rec")];
        assert_eq!(
            g.info(3, Some(props), false, t),
            [Msg::Unknown {
                from_ns: t,
                to_ns: t
            }]
        );
        assert!(g.complete() && g.known(3));
        let out = g.info(3, None::<[(&str, &str); 0]>, true, t + SECOND);
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

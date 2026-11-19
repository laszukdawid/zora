//! What a tray icon shows, as plain state: the same states, labels and menu as the macOS menu bar
//! (Sources/zora/StatusIndicator.swift). A platform tray renders `view()` and calls `zorad` for changes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Listening,
    Running,
    Done,
    Failed,
    Degraded,
}

/// The icon to draw; `Paused` replaces `Idle` while listening is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Idle,
    Paused,
    Listening,
    Running,
    Done,
    Failed,
    Degraded,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Idle => "Zora: listening for your wake phrase",
            State::Listening => "Zora: heard the wake phrase, waiting for “Do it …”",
            State::Running => "Zora: running your command…",
            State::Done => "Zora: done",
            State::Failed => "Zora: command failed or was discarded",
            State::Degraded => "Zora: needs attention",
        }
    }

    fn icon(self) -> Icon {
        match self {
            State::Idle => Icon::Idle,
            State::Listening => Icon::Listening,
            State::Running => Icon::Running,
            State::Done => Icon::Done,
            State::Failed => Icon::Failed,
            State::Degraded => Icon::Degraded,
        }
    }

    /// Finished states fall back to idle after a while; listening and running stay until the next event.
    fn expires_after(self) -> Option<Duration> {
        match self {
            State::Done | State::Failed => Some(Duration::from_secs(20)),
            State::Degraded => Some(Duration::from_secs(60)),
            // A capture ends within its 3-minute limit, so a "heard you" nothing followed up on expires too.
            State::Listening => Some(Duration::from_secs(200)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub icon: Icon,
    pub tooltip: String,
    pub state_line: String,
    pub pause_label: &'static str,
    pub show_pause_hour: bool,
    pub pause_enabled: bool,
    /// Newest first, already shortened.
    pub history: Vec<String>,
}

/// The pause action the tray asks `zorad` for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Pause,
    PauseHour,
    Resume,
}

impl Request {
    pub fn args(&self) -> Vec<String> {
        match self {
            Request::Pause => vec!["pause".into()],
            Request::PauseHour => vec!["pause".into(), "1h".into()],
            Request::Resume => vec!["resume".into()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrayModel {
    state: State,
    /// As last reported by the core; None until it has said.
    listening: Option<bool>,
    paused_until: Option<f64>,
    /// Shown instead of the state line until the core next reports its listening state.
    notice: Option<String>,
    history: VecDeque<String>,
    reset_at: Option<Instant>,
    in_flight: bool,
    /// After a saved change, when to stop waiting for the core to confirm it.
    pending_until: Option<Instant>,
}

impl Default for TrayModel {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayModel {
    pub fn new() -> Self {
        TrayModel {
            state: State::Idle,
            listening: None,
            paused_until: None,
            notice: None,
            history: VecDeque::new(),
            reset_at: None,
            in_flight: false,
            pending_until: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn listening(&self) -> Option<bool> {
        self.listening
    }

    /// `start` / `listening` from the core.
    pub fn set_listening(&mut self, on: bool, until: Option<f64>) {
        self.listening = Some(on);
        self.paused_until = if on { None } else { until };
        self.pending_until = None;
        self.notice = None;
        if !on {
            self.reset_at = None;
            self.state = State::Idle;
        }
    }

    /// A `notify` from the core. Returns whether it should also be shown as a desktop notification.
    pub fn handle(&mut self, kind: &str, title: &str, body: &str, now: Instant) -> bool {
        let next = match kind {
            "captureStarted" => State::Listening,
            "commandQueued" | "jobStarted" => State::Running,
            "jobSucceeded" => State::Done,
            "jobFailed" | "captureDiscarded" => State::Failed,
            "degraded" => State::Degraded,
            _ => return true,
        };
        if kind == "captureDiscarded" && (body == "cancelled" || self.listening == Some(false)) {
            // Cancelled by voice, or cut off by a pause: back to idle, silently.
            self.state = State::Idle;
            self.reset_at = None;
            return false;
        }
        self.state = next;
        if next != State::Listening {
            self.remember(title, body);
        }
        self.reset_at = next.expires_after().map(|d| now + d);
        // jobStarted only moves the icon; the "queued" banner already said it is running.
        kind != "jobStarted"
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self.reset_at.is_some_and(|t| now >= t) {
            self.reset_at = None;
            self.state = State::Idle;
            changed = true;
        }
        if self.pending_until.is_some_and(|t| now >= t) {
            self.pending_until = None;
            self.notice = Some("Zora: change saved; it applies when Zora's core is running".into());
            changed = true;
        }
        changed
    }

    /// The pause item's action, or None while a request is still running.
    pub fn toggle_request(&self) -> Option<Request> {
        if self.in_flight {
            return None;
        }
        Some(if self.listening == Some(false) {
            Request::Resume
        } else {
            Request::Pause
        })
    }

    pub fn begin(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// `zorad` answered. On success the icon still waits for the core to confirm, so it never claims the
    /// microphone is off while it is on; a core that doesn't answer gets a notice after 4 s.
    pub fn finish(&mut self, req: &Request, result: Result<(), String>, now: Instant) {
        self.in_flight = false;
        let want = *req == Request::Resume;
        match result {
            Ok(()) if self.listening != Some(want) => {
                self.pending_until = Some(now + Duration::from_secs(4))
            }
            Ok(()) => {}
            Err(e) => {
                let verb = if want { "resume" } else { "pause" };
                self.notice = Some(format!("Zora couldn't {verb} listening: {e}"));
            }
        }
    }

    pub fn view(&self) -> View {
        let paused = self.listening == Some(false);
        let (icon, label) = if paused && self.state == State::Idle {
            let label = match self.paused_until {
                Some(t) => format!("Zora: paused until {}", crate::clock::local_hhmm(t)),
                None => "Zora: paused (not listening)".into(),
            };
            (Icon::Paused, label)
        } else if self.listening.is_none() && self.state == State::Idle {
            (Icon::Paused, "Zora: waiting for the core".into())
        } else {
            (self.state.icon(), self.state.label().into())
        };
        View {
            icon,
            tooltip: label.clone(),
            state_line: self.notice.clone().unwrap_or(label),
            pause_label: if paused {
                "Resume Listening"
            } else {
                "Pause Listening"
            },
            show_pause_hour: !paused,
            pause_enabled: !self.in_flight,
            history: self.history.iter().cloned().collect(),
        }
    }

    fn remember(&mut self, title: &str, body: &str) {
        let text = format!("{title}: {}", body.replace('\n', " "));
        let short: String = text.chars().take(90).collect();
        let entry = if text.chars().count() > 90 {
            short + "…"
        } else {
            short
        };
        self.history.push_front(entry);
        self.history.truncate(5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_the_core_before_claiming_anything() {
        let m = TrayModel::new();
        assert_eq!(m.view().icon, Icon::Paused);
        assert_eq!(m.view().state_line, "Zora: waiting for the core");
    }

    #[test]
    fn follows_a_command_through_its_life() {
        let mut m = TrayModel::new();
        let t = Instant::now();
        m.set_listening(true, None);
        assert_eq!(m.view().icon, Icon::Idle);
        assert!(m.handle("captureStarted", "Zora", "listening", t));
        assert_eq!(m.view().icon, Icon::Listening);
        assert!(m.handle("commandQueued", "Zora: got it", "fix the bug", t));
        assert!(!m.handle("jobStarted", "Zora", "running", t), "no banner");
        assert_eq!(m.view().icon, Icon::Running);
        assert!(
            !m.tick(t + Duration::from_secs(600)),
            "running never expires"
        );
        m.handle("jobSucceeded", "Zora: done", "fixed", t);
        assert_eq!(m.view().icon, Icon::Done);
        assert!(m.tick(t + Duration::from_secs(21)));
        assert_eq!(m.state(), State::Idle);
        assert_eq!(
            m.view().history,
            [
                "Zora: done: fixed",
                "Zora: running",
                "Zora: got it: fix the bug"
            ]
        );
    }

    #[test]
    fn cancel_and_pause_return_to_idle_silently() {
        let mut m = TrayModel::new();
        let t = Instant::now();
        m.set_listening(true, None);
        m.handle("captureStarted", "Zora", "listening", t);
        assert!(!m.handle("captureDiscarded", "Zora", "cancelled", t));
        assert_eq!(m.state(), State::Idle);
        m.handle("captureStarted", "Zora", "listening", t);
        m.set_listening(false, None);
        assert_eq!(m.view().icon, Icon::Paused);
        assert!(!m.handle("captureDiscarded", "Zora", "paused", t));
        assert!(m.view().history.is_empty());
    }

    #[test]
    fn pause_menu_matches_macos() {
        let mut m = TrayModel::new();
        m.set_listening(true, None);
        let v = m.view();
        assert_eq!(
            (v.pause_label, v.show_pause_hour),
            ("Pause Listening", true)
        );
        assert_eq!(m.toggle_request(), Some(Request::Pause));
        m.set_listening(false, Some(4_000_000_000.0));
        let v = m.view();
        assert_eq!(
            (v.pause_label, v.show_pause_hour),
            ("Resume Listening", false)
        );
        assert!(v.state_line.starts_with("Zora: paused until "));
        assert_eq!(m.toggle_request(), Some(Request::Resume));
        assert_eq!(Request::PauseHour.args(), ["pause", "1h"]);
    }

    #[test]
    fn one_request_at_a_time_and_unconfirmed_changes_get_a_notice() {
        let mut m = TrayModel::new();
        let t = Instant::now();
        m.set_listening(true, None);
        assert!(m.begin());
        assert!(!m.begin());
        assert_eq!(m.toggle_request(), None);
        assert!(!m.view().pause_enabled);
        m.finish(&Request::Pause, Ok(()), t);
        assert!(m.view().pause_enabled);
        assert_eq!(
            m.view().icon,
            Icon::Idle,
            "not paused until the core says so"
        );
        assert!(m.tick(t + Duration::from_secs(5)));
        assert!(m.view().state_line.contains("change saved"));
        m.set_listening(false, None);
        assert_eq!(m.view().state_line, "Zora: paused (not listening)");
    }

    #[test]
    fn confirmed_in_time_shows_no_notice() {
        let mut m = TrayModel::new();
        let t = Instant::now();
        m.set_listening(true, None);
        m.begin();
        m.finish(&Request::Pause, Ok(()), t);
        m.set_listening(false, None);
        assert!(!m.tick(t + Duration::from_secs(5)));
    }

    #[test]
    fn failed_request_is_shown() {
        let mut m = TrayModel::new();
        m.set_listening(true, None);
        m.begin();
        m.finish(&Request::Pause, Err("disk full".into()), Instant::now());
        assert!(
            m.view()
                .state_line
                .contains("couldn't pause listening: disk full")
        );
    }

    #[test]
    fn history_keeps_five_shortened_entries() {
        let mut m = TrayModel::new();
        let t = Instant::now();
        for i in 0..7 {
            m.handle("jobFailed", "Zora", &format!("{i} {}", "x".repeat(100)), t);
        }
        let h = m.view().history;
        assert_eq!(h.len(), 5);
        assert!(h[0].starts_with("Zora: 6 ") && h[0].ends_with('…'));
        assert_eq!(h[0].chars().count(), 91);
    }
}

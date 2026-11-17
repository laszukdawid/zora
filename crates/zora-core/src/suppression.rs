//! Push-to-talk dictation suppression. Fails closed: anything not positively covered by a healthy monitor is
//! treated as possibly dictated.

use crate::clock::{Ns, SECOND};

#[derive(Debug, Clone, PartialEq)]
pub enum DictationSignal {
    Began(Ns),
    Ended(Ns),
    Health {
        healthy: bool,
        at: Ns,
    },
    /// The monitor could not observe anything between these times.
    Unknown {
        from: Ns,
        to: Ns,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Dictation,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Suppression {
    pub pad: Ns,
    healthy: bool,
    dictating_since: Option<Ns>,
    unknown_since: Option<Ns>,
    dictation: Vec<(Ns, Ns)>,
    unknown: Vec<(Ns, Ns)>,
    /// First healthy complete snapshot of the current monitoring period.
    coverage_start: Option<Ns>,
    /// History before this point may have been pruned.
    retained_from: Ns,
}

impl Suppression {
    pub fn new(pad: Ns, now: Ns) -> Self {
        Suppression {
            pad,
            healthy: false,
            dictating_since: None,
            unknown_since: Some(now),
            dictation: Vec::new(),
            unknown: Vec::new(),
            coverage_start: None,
            retained_from: Ns::MIN / 2,
        }
    }

    pub fn healthy(&self) -> bool {
        self.healthy
    }

    pub fn is_dictating(&self) -> bool {
        self.dictating_since.is_some()
    }

    pub fn coverage_start(&self) -> Option<Ns> {
        self.coverage_start
    }

    /// Returns true when a dictation interval began.
    pub fn apply(&mut self, signal: &DictationSignal) -> bool {
        match *signal {
            DictationSignal::Began(t) => {
                if self.dictating_since.is_none() {
                    self.dictating_since = Some(t);
                    return true;
                }
            }
            DictationSignal::Ended(t) => {
                if let Some(start) = self.dictating_since.take() {
                    self.dictation.push((start, t.max(start)));
                }
            }
            DictationSignal::Health { healthy, at } => {
                if healthy && !self.healthy {
                    self.healthy = true;
                    if let Some(u) = self.unknown_since.take() {
                        self.unknown.push((u, at.max(u)));
                    }
                    self.coverage_start = Some(at);
                } else if !healthy && self.healthy {
                    self.healthy = false;
                    self.unknown_since = Some(at);
                    self.coverage_start = None;
                    // A dictation that was open when we lost sight of it is now open-ended unknown.
                    if let Some(start) = self.dictating_since.take() {
                        self.dictation.push((start, at.max(start)));
                    }
                }
            }
            DictationSignal::Unknown { from, to } => {
                self.unknown.push((from, to.max(from)));
            }
        }
        false
    }

    fn overlaps(list: &[(Ns, Ns)], open: Option<Ns>, start: Ns, end: Ns, pad: Ns) -> bool {
        // Saturating arithmetic: invalid input must never overflow policy math.
        if let Some(o) = open
            && end >= o.saturating_sub(pad)
        {
            return true;
        }
        list.iter()
            .any(|&(a, b)| start <= b.saturating_add(pad) && end >= a.saturating_sub(pad))
    }

    pub fn overlaps_dictation(&self, start: Ns, end: Ns) -> bool {
        Self::overlaps(&self.dictation, self.dictating_since, start, end, self.pad)
    }

    pub fn overlaps_unknown(&self, start: Ns, end: Ns) -> bool {
        Self::overlaps(&self.unknown, self.unknown_since, start, end, self.pad)
    }

    /// Whether suppression history fully covers speech starting at `start` (with pad and 1 s lookback).
    pub fn covers(&self, start: Ns) -> bool {
        match self.coverage_start {
            Some(c) => {
                start.saturating_sub(self.pad).saturating_sub(SECOND) >= c.max(self.retained_from)
            }
            None => false,
        }
    }

    /// Final check for a held-back command, given a fresh `synced` observation at `at`.
    pub fn verdict(&self, capture_start: Ns, at: Ns) -> Verdict {
        let from = capture_start.saturating_sub(SECOND);
        if !self.healthy || !self.covers(capture_start) || self.overlaps_unknown(from, at) {
            Verdict::Unknown
        } else if self.overlaps_dictation(from, at) {
            Verdict::Dictation
        } else {
            Verdict::Allow
        }
    }

    /// Drops history older than the horizon, but never anything an open or pending capture still needs.
    pub fn prune(&mut self, now: Ns, horizon: Ns, protect_from: Option<Ns>) {
        let mut cut = now.saturating_sub(horizon);
        if let Some(p) = protect_from {
            cut = cut.min(p.saturating_sub(self.pad).saturating_sub(SECOND));
        }
        let pad = self.pad;
        self.dictation
            .retain(|&(_, b)| b.saturating_add(pad) >= cut);
        self.unknown.retain(|&(_, b)| b.saturating_add(pad) >= cut);
        // History before the cut is no longer guaranteed, whether or not anything was removed this time.
        self.retained_from = self.retained_from.max(cut);
    }
}

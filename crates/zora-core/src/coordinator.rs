//! The single owner of policy state. A pure state machine: inputs in, effects out. The daemon delivers every
//! input through one ordered queue and executes the effects.

use crate::clock::{Ns, SECOND, secs};
use crate::config::Config;
use crate::detector::{CommandDetector, DetectorOutput};
use crate::model::{Command, DiscardReason, Segment, SegmentKind, ZoraEvent};
use crate::suppression::{DictationSignal, Suppression, Verdict};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Segment(Segment),
    /// Transcript continuity was lost (plugin gap, queue loss marker, plugin restart).
    Gap,
    Dictation(DictationSignal),
    /// A new transcript plugin connection; older generations become store-only.
    TranscriptGeneration(u64),
    Synced {
        id: u64,
        at: Ns,
        healthy: bool,
        capturing: bool,
    },
    Tick,
    /// Live, unfinished transcript text. Only drives the early "listening" notification; never the commands.
    Partial {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Store {
        segment: Segment,
        kind: SegmentKind,
    },
    Retag {
        from: Ns,
        to: Ns,
    },
    Notify(ZoraEvent),
    /// Deliver a `Tick` through the ordered queue after this delay.
    Wake(Ns),
    /// Ask the dictation monitor for a fresh observation (the barrier before dispatch).
    Sync {
        id: u64,
        cutoff: Ns,
    },
    Dispatch(Command),
    /// Stop the most recently started running job (voice "stop it, Zora" outside a capture).
    StopRunning,
}

pub struct Coordinator {
    detector: CommandDetector,
    suppression: Suppression,
    hold_back: Ns,
    sync_timeout: Ns,
    horizon: Ns,
    pending: Vec<(Command, Ns)>,
    awaiting: BTreeMap<u64, (Command, Ns, Ns)>,
    accepting: bool,
    transcript_generation: u64,
    next_sync: u64,
    last_prune: Ns,
    reported_unhealthy: bool,
    dictation_began: Option<Ns>,
    next_detector: Option<crate::detector::DetectorConfig>,
    /// When the early (live-text) "listening" notification was last sent.
    early_listening: Option<Ns>,
}

impl Coordinator {
    pub fn new(config: &Config, now: Ns) -> Coordinator {
        let pad = secs(config.dictation.pad_seconds);
        let hold_back = secs(config.capture.hold_back_seconds);
        let horizon = (secs(config.capture.timeout_seconds) + hold_back + 2 * pad + 60 * SECOND)
            .max(600 * SECOND);
        Coordinator {
            detector: CommandDetector::new(config.detector_config()),
            suppression: Suppression::new(pad, now),
            hold_back,
            sync_timeout: SECOND,
            horizon,
            pending: Vec::new(),
            awaiting: BTreeMap::new(),
            accepting: true,
            transcript_generation: 0,
            next_sync: 0,
            last_prune: now,
            reported_unhealthy: false,
            dictation_began: None,
            next_detector: None,
            early_listening: None,
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.detector.is_capturing()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len() + self.awaiting.len()
    }

    pub fn suppression(&self) -> &Suppression {
        &self.suppression
    }

    pub fn handle(&mut self, input: Input, now: Ns) -> Vec<Effect> {
        let mut fx = Vec::new();
        match input {
            Input::Segment(seg) => self.segment(seg, now, &mut fx),
            Input::Gap => {
                let out = self.detector.abort(DiscardReason::Gap);
                self.apply(out, now, &mut fx);
            }
            Input::TranscriptGeneration(g) => {
                self.transcript_generation = self.transcript_generation.max(g);
                let out = self.detector.abort(DiscardReason::Gap);
                self.apply(out, now, &mut fx);
            }
            Input::Dictation(signal) => self.dictation(signal, now, &mut fx),
            Input::Synced {
                id,
                at,
                healthy,
                capturing,
            } => self.synced(id, at, healthy, capturing, now, &mut fx),
            Input::Tick => self.tick(now, &mut fx),
            Input::Partial { text } => {
                let quiet = self.early_listening.is_none_or(|t| now - t > 8 * SECOND);
                if quiet
                    && self.accepting
                    && !self.detector.is_capturing()
                    && !self.suppression.is_dictating()
                    && self.suppression.healthy()
                    && self.detector.config.mentions_start(&text)
                {
                    self.early_listening = Some(now);
                    fx.push(Effect::Notify(ZoraEvent::CaptureStarted));
                }
            }
        }
        fx
    }

    /// Applies new phrases/names/limits. An open capture keeps the old rules until it ends.
    pub fn reload_detector(&mut self, config: crate::detector::DetectorConfig) {
        if self.detector.is_capturing() {
            self.next_detector = Some(config);
        } else {
            self.detector = CommandDetector::new(config);
        }
    }

    /// Closes admission: pending and awaiting commands are dropped and no further Dispatch is emitted.
    pub fn shutdown(&mut self) {
        self.accepting = false;
        self.pending.clear();
        self.awaiting.clear();
        let _ = self.detector.abort(DiscardReason::Cancelled);
    }

    fn segment(&mut self, seg: Segment, now: Ns, fx: &mut Vec<Effect>) {
        let s = &self.suppression;
        // Too old for the retained suppression history, or from an older plugin generation: store only.
        let stale = seg.generation < self.transcript_generation
            || seg.end < now.saturating_sub(self.horizon);
        let dictated = s.overlaps_dictation(seg.start, seg.end);
        let uncovered = s.overlaps_unknown(seg.start, seg.end) || !s.covers(seg.start);
        // State changes happen before the store effect is emitted, so order of effects mirrors decisions.
        let outputs = if stale {
            self.detector.abort(DiscardReason::SuppressionUnknown)
        } else if dictated {
            self.detector.abort(DiscardReason::Dictation)
        } else if uncovered {
            self.detector.abort(DiscardReason::SuppressionUnknown)
        } else {
            self.detector.feed(&seg)
        };
        self.apply(outputs, now, fx);
        fx.push(Effect::Store {
            segment: seg,
            kind: if dictated {
                SegmentKind::Dictation
            } else {
                SegmentKind::Speech
            },
        });
    }

    fn dictation(&mut self, signal: DictationSignal, now: Ns, fx: &mut Vec<Effect>) {
        let pad = self.suppression.pad;
        let was_healthy = self.suppression.healthy();
        let began = self.suppression.apply(&signal);
        match signal {
            DictationSignal::Began(t) => {
                if began {
                    self.dictation_began = Some(t);
                    let out = self.detector.abort(DiscardReason::Dictation);
                    self.apply(out, now, fx);
                    fx.push(Effect::Retag {
                        from: t - pad,
                        to: now + pad,
                    });
                }
            }
            DictationSignal::Ended(t) => {
                let from = self.dictation_began.take().unwrap_or(t);
                fx.push(Effect::Retag {
                    from: from - pad,
                    to: t + pad,
                });
            }
            DictationSignal::Health { healthy, .. } => {
                if was_healthy && !healthy {
                    let out = self.detector.abort(DiscardReason::SuppressionUnknown);
                    self.apply(out, now, fx);
                }
                if !healthy && !self.reported_unhealthy {
                    self.reported_unhealthy = true;
                    fx.push(Effect::Notify(ZoraEvent::Degraded {
                        reason: "dictation monitor unavailable; commands paused".into(),
                    }));
                } else if healthy {
                    self.reported_unhealthy = false;
                }
            }
            DictationSignal::Unknown { to, .. } => {
                if let Some(start) = self.detector.capture_started_at()
                    && start - pad - SECOND <= to + pad
                {
                    let out = self.detector.abort(DiscardReason::SuppressionUnknown);
                    self.apply(out, now, fx);
                }
            }
        }
    }

    fn tick(&mut self, now: Ns, fx: &mut Vec<Effect>) {
        if !self.accepting {
            return;
        }
        let out = self.detector.tick(now);
        self.apply(out, now, fx);
        if !self.detector.is_capturing()
            && let Some(cfg) = self.next_detector.take()
        {
            self.detector = CommandDetector::new(cfg);
        }

        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, r)| *r <= now);
        self.pending = waiting;
        for (command, _) in ready {
            self.next_sync += 1;
            let id = self.next_sync;
            self.awaiting
                .insert(id, (command, now + self.sync_timeout, now));
            fx.push(Effect::Sync { id, cutoff: now });
            fx.push(Effect::Wake(self.sync_timeout + 10 * crate::clock::MILLI));
        }
        let expired: Vec<u64> = self
            .awaiting
            .iter()
            .filter(|(_, (_, deadline, _))| *deadline < now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.awaiting.remove(&id);
            fx.push(Effect::Notify(ZoraEvent::CaptureDiscarded {
                reason: DiscardReason::SuppressionUnknown,
            }));
        }

        if now - self.last_prune >= 60 * SECOND {
            self.last_prune = now;
            let protect = self
                .detector
                .capture_started_at()
                .into_iter()
                .chain(self.pending.iter().map(|(c, _)| c.capture_start))
                .chain(self.awaiting.values().map(|(c, _, _)| c.capture_start))
                .min();
            self.suppression.prune(now, self.horizon, protect);
        }
    }

    fn synced(
        &mut self,
        id: u64,
        at: Ns,
        healthy: bool,
        capturing: bool,
        now: Ns,
        fx: &mut Vec<Effect>,
    ) {
        let Some((command, deadline, cutoff)) = self.awaiting.remove(&id) else {
            return;
        };
        if !self.accepting {
            return;
        }
        // The deadline is checked here, not only by a later tick: a late reply never releases a command.
        let verdict = if now > deadline || !healthy || at < cutoff || at > now {
            Verdict::Unknown
        } else if capturing {
            Verdict::Dictation
        } else {
            self.suppression.verdict(command.capture_start, at)
        };
        match verdict {
            Verdict::Allow => fx.push(Effect::Dispatch(command)),
            Verdict::Dictation => fx.push(Effect::Notify(ZoraEvent::CaptureDiscarded {
                reason: DiscardReason::Dictation,
            })),
            Verdict::Unknown => fx.push(Effect::Notify(ZoraEvent::CaptureDiscarded {
                reason: DiscardReason::SuppressionUnknown,
            })),
        }
    }

    fn apply(&mut self, outputs: Vec<DetectorOutput>, now: Ns, fx: &mut Vec<Effect>) {
        for o in outputs {
            match o {
                DetectorOutput::CaptureStarted { .. } => {
                    // Already announced from live text a moment ago: don't repeat it.
                    if self
                        .early_listening
                        .take()
                        .is_none_or(|t| now - t > 20 * SECOND)
                    {
                        fx.push(Effect::Notify(ZoraEvent::CaptureStarted));
                    }
                }
                DetectorOutput::StopRunning => {
                    if self.accepting {
                        fx.push(Effect::StopRunning);
                    }
                }
                DetectorOutput::Discarded(reason) => {
                    fx.push(Effect::Notify(ZoraEvent::CaptureDiscarded { reason }))
                }
                DetectorOutput::Command(command) => {
                    if !self.accepting {
                        continue;
                    }
                    self.pending.push((command, now + self.hold_back));
                    fx.push(Effect::Wake(self.hold_back));
                }
            }
        }
    }
}

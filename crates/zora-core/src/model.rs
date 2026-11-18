use crate::clock::Ns;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub generation: u64,
    pub session: u64,
    pub start: Ns,
    pub end: Ns,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Speech,
    Dictation,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Speech => "speech",
            SegmentKind::Dictation => "dictation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub id: String,
    pub text: String,
    pub capture_start: Ns,
    pub capture_end: Ns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardReason {
    Cancelled,
    Timeout,
    Gap,
    TooLong,
    Empty,
    Dictation,
    SuppressionUnknown,
    QueueFull,
    Expired,
}

impl DiscardReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscardReason::Cancelled => "cancelled",
            DiscardReason::Timeout => "timeout",
            DiscardReason::Gap => "gap",
            DiscardReason::TooLong => "tooLong",
            DiscardReason::Empty => "empty",
            DiscardReason::Dictation => "dictation",
            DiscardReason::SuppressionUnknown => "suppressionUnknown",
            DiscardReason::QueueFull => "queueFull",
            DiscardReason::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Started,
    CaptureStarted,
    CommandQueued,
    JobStarted,
    JobSucceeded,
    JobFailed,
    CaptureDiscarded,
    Degraded,
}

impl EventKind {
    pub const ALL: [EventKind; 8] = [
        EventKind::Started,
        EventKind::CaptureStarted,
        EventKind::CommandQueued,
        EventKind::JobStarted,
        EventKind::JobSucceeded,
        EventKind::JobFailed,
        EventKind::CaptureDiscarded,
        EventKind::Degraded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Started => "started",
            EventKind::CaptureStarted => "captureStarted",
            EventKind::CommandQueued => "commandQueued",
            EventKind::JobStarted => "jobStarted",
            EventKind::JobSucceeded => "jobSucceeded",
            EventKind::JobFailed => "jobFailed",
            EventKind::CaptureDiscarded => "captureDiscarded",
            EventKind::Degraded => "degraded",
        }
    }

    pub fn parse(s: &str) -> Option<EventKind> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoraEvent {
    Started {
        dry_run: bool,
    },
    CaptureStarted,
    CommandQueued {
        id: String,
        text: String,
        harness: String,
    },
    JobStarted {
        id: String,
        harness: String,
    },
    JobSucceeded {
        id: String,
        summary: String,
    },
    JobFailed {
        id: String,
        reason: String,
    },
    CaptureDiscarded {
        reason: DiscardReason,
    },
    Degraded {
        reason: String,
    },
}

impl ZoraEvent {
    pub fn kind(&self) -> EventKind {
        match self {
            ZoraEvent::Started { .. } => EventKind::Started,
            ZoraEvent::CaptureStarted => EventKind::CaptureStarted,
            ZoraEvent::CommandQueued { .. } => EventKind::CommandQueued,
            ZoraEvent::JobStarted { .. } => EventKind::JobStarted,
            ZoraEvent::JobSucceeded { .. } => EventKind::JobSucceeded,
            ZoraEvent::JobFailed { .. } => EventKind::JobFailed,
            ZoraEvent::CaptureDiscarded { .. } => EventKind::CaptureDiscarded,
            ZoraEvent::Degraded { .. } => EventKind::Degraded,
        }
    }

    /// The job this event is about, for notifiers that track a command through its life.
    pub fn job_id(&self) -> Option<&str> {
        match self {
            ZoraEvent::CommandQueued { id, .. }
            | ZoraEvent::JobStarted { id, .. }
            | ZoraEvent::JobSucceeded { id, .. }
            | ZoraEvent::JobFailed { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn harness(&self) -> Option<&str> {
        match self {
            ZoraEvent::CommandQueued { harness, .. } | ZoraEvent::JobStarted { harness, .. } => {
                Some(harness)
            }
            _ => None,
        }
    }

    /// Human-facing title and body (may contain prompt or summary text; never logged).
    pub fn display(&self) -> (String, String) {
        match self {
            ZoraEvent::Started { dry_run } => (
                "Zora".into(),
                if *dry_run {
                    "Listening (dry run)"
                } else {
                    "Listening"
                }
                .into(),
            ),
            ZoraEvent::CaptureStarted => (
                "Zora is listening".into(),
                "Say \u{201c}Do it\u{201d} when you're done.".into(),
            ),
            ZoraEvent::CommandQueued { text, .. } => ("Zora got it".into(), text.clone()),
            ZoraEvent::JobStarted { harness, .. } => {
                ("Zora".into(), format!("Running on {harness}"))
            }
            ZoraEvent::JobSucceeded { summary, .. } => ("Zora: done".into(), summary.clone()),
            ZoraEvent::JobFailed { reason, .. } if reason == "cancelled" => (
                "Zora: stopped".into(),
                "The running command was stopped.".into(),
            ),
            ZoraEvent::JobFailed { reason, .. } => ("Zora: failed".into(), reason.clone()),
            ZoraEvent::CaptureDiscarded { reason } => {
                ("Zora: command discarded".into(), reason.as_str().into())
            }
            ZoraEvent::Degraded { reason } => ("Zora: degraded".into(), reason.clone()),
        }
    }

    /// Metadata-only description for logs.
    pub fn metadata(&self) -> String {
        match self {
            ZoraEvent::Started { dry_run } => format!("started dryRun={dry_run}"),
            ZoraEvent::CaptureStarted => "captureStarted".into(),
            ZoraEvent::CommandQueued { id, text, harness } => format!(
                "commandQueued id={id} harness={harness} chars={}",
                text.chars().count()
            ),
            ZoraEvent::JobStarted { id, harness } => {
                format!("jobStarted id={id} harness={harness}")
            }
            ZoraEvent::JobSucceeded { id, summary } => format!(
                "jobSucceeded id={id} summaryChars={}",
                summary.chars().count()
            ),
            ZoraEvent::JobFailed { id, .. } => format!("jobFailed id={id}"),
            ZoraEvent::CaptureDiscarded { reason } => {
                format!("captureDiscarded reason={}", reason.as_str())
            }
            ZoraEvent::Degraded { reason } => format!("degraded reason={reason}"),
        }
    }
}

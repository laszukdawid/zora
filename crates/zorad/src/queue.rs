//! Ordered input queue for the service loop. Bulk items (transcript, ticks) are capped and a dropped
//! transcript item leaves a loss marker at its exact position. Control items are never silently dropped:
//! past their own cap `send` reports overflow so the producer can fail closed.

use std::sync::Mutex;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Transcript segments: may be dropped (replaced by a loss marker).
    Transcript,
    /// Ticks: may be dropped silently.
    Tick,
    /// Everything else: dictation signals, sync replies, job events, lifecycle.
    Control,
}

pub trait Classify {
    fn class(&self) -> Class;
    /// Approximate payload bytes, counted against the byte budget for bulk items.
    fn cost(&self) -> usize {
        0
    }
    fn loss_marker() -> Self;
    fn is_loss_marker(&self) -> bool;
}

struct Counts {
    bulk: usize,
    bytes: usize,
    control: usize,
    last_was_marker: bool,
}

pub struct InputQueue<T> {
    tx: mpsc::UnboundedSender<T>,
    counts: Mutex<Counts>,
    bulk_limit: usize,
    byte_limit: usize,
    control_limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ControlOverflow;

impl<T: Classify> InputQueue<T> {
    pub fn new(
        bulk_limit: usize,
        control_limit: usize,
    ) -> (InputQueue<T>, mpsc::UnboundedReceiver<T>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            InputQueue {
                tx,
                counts: Mutex::new(Counts {
                    bulk: 0,
                    bytes: 0,
                    control: 0,
                    last_was_marker: false,
                }),
                bulk_limit,
                byte_limit: 1024 * 1024,
                control_limit,
            },
            rx,
        )
    }

    pub fn send(&self, item: T) -> Result<(), ControlOverflow> {
        // Yield under the lock so a loss marker lands exactly where the dropped item would have been.
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        match item.class() {
            Class::Control => {
                // Control items are never dropped silently, but they do count against the byte budget.
                if c.control >= self.control_limit || c.bytes + item.cost() > self.byte_limit {
                    return Err(ControlOverflow);
                }
                c.control += 1;
                c.bytes += item.cost();
                c.last_was_marker = false;
                let _ = self.tx.send(item);
            }
            class => {
                if c.bulk < self.bulk_limit && c.bytes + item.cost() <= self.byte_limit {
                    c.bulk += 1;
                    c.bytes += item.cost();
                    c.last_was_marker = false;
                    let _ = self.tx.send(item);
                } else if class == Class::Transcript && !c.last_was_marker {
                    // Uncounted, but at most one per accepted item, so the stream stays bounded.
                    c.last_was_marker = true;
                    let _ = self.tx.send(T::loss_marker());
                }
            }
        }
        Ok(())
    }

    /// Enqueues a fail-closed signal regardless of caps (rare: plugin teardown).
    pub fn force(&self, item: T) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        match item.class() {
            Class::Control => {
                c.control += 1;
                c.bytes += item.cost();
            }
            _ => {
                c.bulk += 1;
                c.bytes += item.cost();
            }
        }
        c.last_was_marker = false;
        let _ = self.tx.send(item);
    }

    /// The consumer calls this for every item it takes.
    pub fn consumed(&self, item: &T) {
        if item.is_loss_marker() {
            return;
        }
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        match item.class() {
            Class::Control => {
                c.control = c.control.saturating_sub(1);
                c.bytes = c.bytes.saturating_sub(item.cost());
            }
            _ => {
                c.bulk = c.bulk.saturating_sub(1);
                c.bytes = c.bytes.saturating_sub(item.cost());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum I {
        T(&'static str),
        C(&'static str),
        Loss,
    }

    impl Classify for I {
        fn class(&self) -> Class {
            match self {
                I::T(_) | I::Loss => Class::Transcript,
                I::C(_) => Class::Control,
            }
        }
        fn loss_marker() -> Self {
            I::Loss
        }
        fn is_loss_marker(&self) -> bool {
            matches!(self, I::Loss)
        }
    }

    #[test]
    fn loss_marker_sits_where_the_item_was_dropped() {
        let (q, mut rx) = InputQueue::<I>::new(2, 2);
        q.send(I::T("hey zora")).unwrap();
        q.send(I::T("filler")).unwrap();
        q.send(I::T("zora cancel")).unwrap(); // dropped
        q.send(I::T("also dropped")).unwrap(); // no second marker
        q.send(I::C("dictation")).unwrap();
        let mut got = vec![];
        for _ in 0..4 {
            let i = rx.try_recv().unwrap();
            q.consumed(&i);
            got.push(i);
        }
        assert_eq!(
            got,
            [I::T("hey zora"), I::T("filler"), I::Loss, I::C("dictation")]
        );
        q.send(I::T("do it zora")).unwrap();
        assert_eq!(rx.try_recv().unwrap(), I::T("do it zora"));
    }

    #[derive(Debug)]
    struct Big(usize);
    impl Classify for Big {
        fn class(&self) -> Class {
            if self.0 == 0 {
                Class::Control
            } else {
                Class::Transcript
            }
        }
        fn cost(&self) -> usize {
            self.0
        }
        fn loss_marker() -> Self {
            Big(usize::MAX)
        }
        fn is_loss_marker(&self) -> bool {
            self.0 == usize::MAX
        }
    }

    #[test]
    fn byte_budget_bounds_transcript_payload() {
        let (q, mut rx) = InputQueue::<Big>::new(1000, 10);
        for _ in 0..10 {
            q.send(Big(300 * 1024)).unwrap();
        }
        let mut accepted = 0;
        let mut markers = 0;
        while let Ok(b) = rx.try_recv() {
            if b.is_loss_marker() {
                markers += 1
            } else {
                accepted += 1
            }
        }
        assert_eq!((accepted, markers), (3, 1));
    }

    #[test]
    fn control_payload_counts_against_byte_budget() {
        // Big(0) is control with zero cost; use a control-classified big item via a wrapper.
        #[derive(Debug)]
        struct Ctl(usize);
        impl Classify for Ctl {
            fn class(&self) -> Class {
                Class::Control
            }
            fn cost(&self) -> usize {
                self.0
            }
            fn loss_marker() -> Self {
                Ctl(0)
            }
            fn is_loss_marker(&self) -> bool {
                false
            }
        }
        let (q, _rx) = InputQueue::<Ctl>::new(10, 4096);
        let mut ok = 0;
        while q.send(Ctl(60 * 1024)).is_ok() {
            ok += 1;
            assert!(ok < 100, "control bytes unbounded");
        }
        assert_eq!(ok, 17);
    }

    #[test]
    fn control_overflow_is_reported_not_dropped() {
        let (q, _rx) = InputQueue::<I>::new(2, 1);
        assert!(q.send(I::C("a")).is_ok());
        assert_eq!(q.send(I::C("b")), Err(ControlOverflow));
    }
}

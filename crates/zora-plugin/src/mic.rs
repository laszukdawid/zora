//! Microphone on/off. Every open, close and stop runs in one thread, in message order, so a late "listen" can
//! never reopen the microphone after a pause or a stop. The microphone is open only while the speech engine is
//! ready and the core has said `listening: true` (docs/protocol.md, Listening on/off).

use crate::clock::{Ns, now_ns};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// From the capture callback to the speech engine.
pub enum Audio {
    Samples {
        epoch: u64,
        /// Interleaved frames.
        data: Vec<f32>,
        channels: u16,
        rate: u32,
        /// Monotonic time of the last frame.
        end_ns: Ns,
        /// Audio was dropped just before this chunk.
        lost_before: bool,
    },
    Failed {
        epoch: u64,
        reason: String,
        at_ns: Ns,
    },
    /// Transcribe what is buffered for `epoch`, then answer on `done`.
    Finish { epoch: u64, done: mpsc::Sender<()> },
}

/// Handed to an `AudioInput` when it opens; tags every chunk with the epoch it was opened in.
#[derive(Clone)]
pub struct AudioSink {
    tx: SyncSender<Audio>,
    epoch: u64,
    lost: Arc<AtomicBool>,
    failed: mpsc::Sender<MicEvent>,
}

impl AudioSink {
    /// Never blocks (it may run on a real-time audio thread). A full queue drops the chunk and marks a gap.
    pub fn push(&self, data: &[f32], channels: u16, rate: u32, end_ns: Ns) {
        let lost_before = self.lost.swap(false, Ordering::SeqCst);
        let msg = Audio::Samples {
            epoch: self.epoch,
            data: data.to_vec(),
            channels,
            rate,
            end_ns,
            lost_before,
        };
        if let Err(TrySendError::Full(_)) = self.tx.try_send(msg) {
            self.lost.store(true, Ordering::SeqCst);
        }
    }

    /// The device dropped frames (overrun); the next chunk starts a new speech session.
    pub fn mark_lost(&self) {
        self.lost.store(true, Ordering::SeqCst);
    }

    /// The stream broke (device gone, server restarted). The mic thread closes it and retries.
    pub fn fail(&self, reason: &str) {
        let _ = self.tx.try_send(Audio::Failed {
            epoch: self.epoch,
            reason: reason.into(),
            at_ns: now_ns(),
        });
        let _ = self.failed.send(MicEvent::Failed { epoch: self.epoch });
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A capture device. Implementations must stop delivering audio once `close` returns.
pub trait AudioInput {
    fn open(&mut self, sink: AudioSink) -> Result<(), String>;
    fn close(&mut self);
}

#[derive(Debug, Clone, PartialEq)]
pub enum MicEvent {
    /// `start` or `listening` from the core.
    Listening(bool),
    /// The speech engine can take audio.
    Ready,
    /// `stop`, or the core went away. The microphone stays closed for good.
    Stop,
    Failed {
        epoch: u64,
    },
}

/// The decision part, kept apart from the device so it can be tested on its own.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MicGate {
    /// None until the core has said; the microphone stays closed meanwhile.
    pub core_listening: Option<bool>,
    pub ready: bool,
    pub stopped: bool,
}

impl MicGate {
    pub fn wanted(&self) -> bool {
        !self.stopped && self.ready && self.core_listening == Some(true)
    }

    pub fn apply(&mut self, ev: &MicEvent) {
        match ev {
            MicEvent::Listening(on) => self.core_listening = Some(*on),
            MicEvent::Ready => self.ready = true,
            MicEvent::Stop => self.stopped = true,
            MicEvent::Failed { .. } => {}
        }
    }
}

const RETRY: Duration = Duration::from_secs(2);

pub struct MicController<I: AudioInput> {
    gate: MicGate,
    input: I,
    running: bool,
    /// Bumped on every open and close; the engine drops audio from any other epoch.
    epoch: Arc<AtomicU64>,
    tx: SyncSender<Audio>,
    events: mpsc::Sender<MicEvent>,
    retry_at: Option<Instant>,
    failing: bool,
    status: Box<dyn FnMut(bool, String) + Send>,
}

impl<I: AudioInput> MicController<I> {
    /// `status(ready, reason)` reports open failures and recoveries; `events` is where a failing stream reports.
    pub fn new(
        input: I,
        epoch: Arc<AtomicU64>,
        tx: SyncSender<Audio>,
        events: mpsc::Sender<MicEvent>,
        status: Box<dyn FnMut(bool, String) + Send>,
    ) -> Self {
        MicController {
            gate: MicGate::default(),
            input,
            running: false,
            epoch,
            tx,
            events,
            retry_at: None,
            failing: false,
            status,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn input(&self) -> &I {
        &self.input
    }

    pub fn handle(&mut self, ev: MicEvent) {
        self.gate.apply(&ev);
        match ev {
            MicEvent::Failed { epoch } => {
                if self.running && epoch == self.epoch.load(Ordering::SeqCst) {
                    self.close(true);
                    self.retry_at = Some(Instant::now() + RETRY);
                }
            }
            MicEvent::Stop => {
                // Keep the epoch: what was heard before `stop` is still transcribed.
                if self.running {
                    self.close(false);
                }
                let (done, wait) = mpsc::channel();
                let epoch = self.epoch.load(Ordering::SeqCst);
                if self.tx.send(Audio::Finish { epoch, done }).is_ok() {
                    let _ = wait.recv_timeout(Duration::from_secs(4));
                }
            }
            _ => self.apply(),
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|t| now >= t) {
            self.retry_at = None;
            self.apply();
        }
    }

    fn apply(&mut self) {
        let wanted = self.gate.wanted();
        if wanted && !self.running && self.retry_at.is_none() {
            let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let sink = AudioSink {
                tx: self.tx.clone(),
                epoch,
                lost: Arc::new(AtomicBool::new(false)),
                failed: self.events.clone(),
            };
            match self.input.open(sink) {
                Ok(()) => {
                    self.running = true;
                    if self.failing {
                        self.failing = false;
                        (self.status)(true, "microphone open".into());
                    }
                }
                Err(e) => {
                    self.failing = true;
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                    (self.status)(false, format!("cannot open the microphone: {e}"));
                    self.retry_at = Some(Instant::now() + RETRY);
                }
            }
        } else if !wanted && self.running {
            self.close(true);
        }
    }

    fn close(&mut self, drop_audio: bool) {
        self.input.close();
        self.running = false;
        if drop_audio {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Runs the controller until `Stop`. `make` builds the input on this thread (some audio streams can't move
/// between threads).
pub fn run_mic<I: AudioInput>(
    make: impl FnOnce() -> I,
    rx: Receiver<MicEvent>,
    epoch: Arc<AtomicU64>,
    tx: SyncSender<Audio>,
    events: mpsc::Sender<MicEvent>,
    status: Box<dyn FnMut(bool, String) + Send>,
) {
    let mut ctl = MicController::new(make(), epoch, tx, events, status);
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let stop = ev == MicEvent::Stop;
                ctl.handle(ev);
                if stop {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => ctl.tick(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => {
                ctl.handle(MicEvent::Stop);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default, Clone)]
    struct FakeInput {
        log: Arc<Mutex<Vec<String>>>,
        fail_next: Arc<AtomicBool>,
    }

    impl AudioInput for FakeInput {
        fn open(&mut self, sink: AudioSink) -> Result<(), String> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                self.log.lock().unwrap().push("open failed".into());
                return Err("no device".into());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("open {}", sink.epoch()));
            Ok(())
        }
        fn close(&mut self) {
            self.log.lock().unwrap().push("close".into());
        }
    }

    type Statuses = Arc<Mutex<Vec<(bool, String)>>>;

    fn controller() -> (
        MicController<FakeInput>,
        FakeInput,
        Receiver<Audio>,
        Statuses,
    ) {
        let input = FakeInput::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let (etx, _erx) = mpsc::channel();
        let statuses = Arc::new(Mutex::new(vec![]));
        let s = statuses.clone();
        let ctl = MicController::new(
            input.clone(),
            Arc::new(AtomicU64::new(0)),
            tx,
            etx,
            Box::new(move |ok, r| s.lock().unwrap().push((ok, r))),
        );
        (ctl, input, rx, statuses)
    }

    fn log(i: &FakeInput) -> Vec<String> {
        i.log.lock().unwrap().clone()
    }

    #[test]
    fn closed_until_ready_and_listening() {
        let mut g = MicGate::default();
        assert!(!g.wanted());
        g.apply(&MicEvent::Ready);
        assert!(!g.wanted(), "the core has not said yet");
        g.apply(&MicEvent::Listening(false));
        assert!(!g.wanted());
        g.apply(&MicEvent::Listening(true));
        assert!(g.wanted());
        g.apply(&MicEvent::Stop);
        assert!(!g.wanted());
        g.apply(&MicEvent::Listening(true));
        assert!(!g.wanted(), "nothing reopens after stop");
    }

    #[test]
    fn start_paused_never_opens() {
        let (mut c, input, _rx, _) = controller();
        c.handle(MicEvent::Listening(false));
        c.handle(MicEvent::Ready);
        assert!(log(&input).is_empty());
        assert!(!c.is_running());
    }

    #[test]
    fn pause_closes_and_bumps_epoch_resume_reopens() {
        let (mut c, input, _rx, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        c.handle(MicEvent::Listening(false));
        c.handle(MicEvent::Listening(true));
        assert_eq!(log(&input), ["open 1", "close", "open 3"]);
        assert!(c.is_running());
    }

    #[test]
    fn repeated_listening_does_not_reopen() {
        let (mut c, input, _rx, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        c.handle(MicEvent::Listening(true));
        assert_eq!(log(&input), ["open 1"]);
    }

    #[test]
    fn stop_closes_keeps_epoch_and_asks_engine_to_finish() {
        let (mut c, input, rx, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        let engine = std::thread::spawn(move || match rx.recv().unwrap() {
            Audio::Finish { epoch, done } => {
                done.send(()).unwrap();
                epoch
            }
            _ => panic!("expected finish"),
        });
        c.handle(MicEvent::Stop);
        assert_eq!(
            engine.join().unwrap(),
            1,
            "audio from the open epoch is finished"
        );
        c.handle(MicEvent::Listening(true));
        assert_eq!(log(&input), ["open 1", "close"]);
    }

    #[test]
    fn failed_open_reports_and_retries_while_wanted() {
        let (mut c, input, _rx, statuses) = controller();
        input.fail_next.store(true, Ordering::SeqCst);
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        assert!(!c.is_running());
        assert!(!statuses.lock().unwrap()[0].0);
        c.tick(Instant::now());
        assert!(!c.is_running(), "waits before retrying");
        c.tick(Instant::now() + RETRY);
        assert!(c.is_running());
        assert!(statuses.lock().unwrap()[1].0);
    }

    #[test]
    fn pause_during_retry_wait_stays_closed() {
        let (mut c, input, _rx, _) = controller();
        input.fail_next.store(true, Ordering::SeqCst);
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        c.handle(MicEvent::Listening(false));
        c.tick(Instant::now() + RETRY);
        assert!(!c.is_running());
        assert_eq!(log(&input), ["open failed"]);
    }

    #[test]
    fn stream_failure_closes_then_reopens() {
        let (mut c, input, _rx, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Listening(true));
        c.handle(MicEvent::Failed { epoch: 99 });
        assert!(c.is_running(), "a stale failure is ignored");
        c.handle(MicEvent::Failed { epoch: 1 });
        assert!(!c.is_running());
        c.tick(Instant::now() + RETRY);
        assert_eq!(log(&input), ["open 1", "close", "open 3"]);
    }

    #[test]
    fn full_queue_marks_the_next_chunk() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (etx, _e) = mpsc::channel();
        let sink = AudioSink {
            tx,
            epoch: 1,
            lost: Arc::new(AtomicBool::new(false)),
            failed: etx,
        };
        sink.push(&[0.0], 1, 16000, 1);
        sink.push(&[0.0], 1, 16000, 2); // dropped
        let first = rx.recv().unwrap();
        assert!(matches!(
            first,
            Audio::Samples {
                lost_before: false,
                ..
            }
        ));
        sink.push(&[0.0], 1, 16000, 3);
        assert!(matches!(
            rx.recv().unwrap(),
            Audio::Samples {
                lost_before: true,
                end_ns: 3,
                ..
            }
        ));
    }
}

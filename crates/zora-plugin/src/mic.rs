//! Microphone on/off. Every open, close and stop runs in one thread, in message order, so a late "listen" can
//! never reopen the microphone after a pause or a stop. The microphone is open only while the speech engine is
//! ready and the core has said `listening: true` in `start` or later (docs/protocol.md, Listening on/off).
//! A pause takes effect at once through `Live`, before the device has finished closing.

use crate::clock::{Ns, SECOND, now_ns};
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

/// What every thread checks before taking or publishing audio: whether listening is allowed right now, and the
/// current epoch (bumped on every open and close; audio from any other epoch is dropped).
#[derive(Debug, Default)]
pub struct Live {
    epoch: AtomicU64,
    allowed: AtomicBool,
}

impl Live {
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn allowed(&self) -> bool {
        self.allowed.load(Ordering::SeqCst)
    }

    /// A pause or a lost core: refuse audio and invalidate what is buffered, before the device is closed.
    pub fn revoke(&self) {
        self.allowed.store(false, Ordering::SeqCst);
        self.bump();
    }

    pub fn allow(&self) {
        self.allowed.store(true, Ordering::SeqCst);
    }

    /// True while audio tagged `epoch` may still be used.
    pub fn current(&self, epoch: u64) -> bool {
        self.allowed() && self.epoch() == epoch
    }

    pub(crate) fn bump(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// The monotonic time of a buffer's last frame, from when the callback ran and how long before that the device
/// captured the buffer. None when the backend gave no usable capture time (the caller marks a gap).
pub fn buffer_end_ns(
    callback_ns: Ns,
    capture_lag: Option<Duration>,
    frames: usize,
    rate: u32,
) -> Option<Ns> {
    let lag = capture_lag?.as_nanos() as Ns;
    if lag > 2 * SECOND {
        return None;
    }
    let dur = frames as Ns * SECOND / rate.max(1) as Ns;
    Some((callback_ns - lag + dur).min(callback_ns))
}

/// Handed to an `AudioInput` when it opens; tags every chunk with the epoch it was opened in.
#[derive(Clone)]
pub struct AudioSink {
    tx: SyncSender<Audio>,
    epoch: u64,
    live: Arc<Live>,
    lost: Arc<AtomicBool>,
    /// A timed buffer went out on this stream; untimed ones after it break continuity.
    timed: Arc<AtomicBool>,
    failed: mpsc::Sender<MicEvent>,
}

impl AudioSink {
    /// Never blocks (it may run on a real-time audio thread). A full queue drops the chunk and marks a gap;
    /// audio arriving after a pause is dropped here.
    pub fn push(&self, data: &[f32], channels: u16, rate: u32, end_ns: Ns) {
        if !self.live.current(self.epoch) {
            return;
        }
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

    /// A buffer the device captured `capture_lag` before the callback ran at `callback_ns`. Without a usable
    /// capture time its place in time is unknown, so it is dropped and the next timed buffer starts a new speech
    /// session. Returns whether the buffer was timed.
    pub fn push_captured(
        &self,
        data: &[f32],
        channels: u16,
        rate: u32,
        callback_ns: Ns,
        capture_lag: Option<Duration>,
    ) -> bool {
        let frames = data.len() / channels.max(1) as usize;
        match buffer_end_ns(callback_ns, capture_lag, frames, rate) {
            Some(end) => {
                self.timed.store(true, Ordering::SeqCst);
                self.push(data, channels, rate, end);
                true
            }
            None => {
                // At the start of a stream nothing came before, and a new stream is a new session anyway.
                if self.timed.load(Ordering::SeqCst) {
                    self.mark_lost();
                }
                false
            }
        }
    }

    /// The device dropped frames or its timing is unusable; the next chunk starts a new speech session.
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

    /// Whether the open that made this sink is still wanted: no pause, stop or lost core since.
    pub fn current(&self) -> bool {
        self.live.current(self.epoch)
    }
}

/// A capture device. `open` builds the stream stopped and starts it only if `sink.current()` still holds right
/// before starting; otherwise it returns `Ok` without starting (the controller closes it). Implementations must
/// stop delivering audio once `close` returns.
pub trait AudioInput {
    fn open(&mut self, sink: AudioSink) -> Result<(), String>;
    fn close(&mut self);
}

#[derive(Debug, Clone, PartialEq)]
pub enum MicEvent {
    /// `start` from the core; nothing opens before it.
    Start(bool),
    /// `listening` from the core.
    Listening(bool),
    /// The speech engine can take audio.
    Ready,
    /// `stop`: close, transcribe what was heard, and stay closed.
    Stop,
    /// The core went away: close and drop untranscribed audio.
    Disconnect,
    Failed {
        epoch: u64,
    },
}

/// The decision part, kept apart from the device so it can be tested on its own.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MicGate {
    pub started: bool,
    /// None until the core has said; the microphone stays closed meanwhile.
    pub core_listening: Option<bool>,
    pub ready: bool,
    pub stopped: bool,
}

impl MicGate {
    pub fn wanted(&self) -> bool {
        !self.stopped && self.ready && self.started && self.core_listening == Some(true)
    }

    pub fn apply(&mut self, ev: &MicEvent) {
        match ev {
            MicEvent::Start(on) => {
                self.started = true;
                self.core_listening = Some(*on);
            }
            MicEvent::Listening(on) => self.core_listening = Some(*on),
            MicEvent::Ready => self.ready = true,
            MicEvent::Stop | MicEvent::Disconnect => self.stopped = true,
            MicEvent::Failed { .. } => {}
        }
    }
}

const RETRY: Duration = Duration::from_secs(2);

pub struct MicController<I: AudioInput> {
    gate: MicGate,
    input: I,
    running: bool,
    live: Arc<Live>,
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
        live: Arc<Live>,
        tx: SyncSender<Audio>,
        events: mpsc::Sender<MicEvent>,
        status: Box<dyn FnMut(bool, String) + Send>,
    ) -> Self {
        MicController {
            gate: MicGate::default(),
            input,
            running: false,
            live,
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
                if self.running && epoch == self.live.epoch() {
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
                let epoch = self.live.epoch();
                if self.tx.send(Audio::Finish { epoch, done }).is_ok() {
                    let _ = wait.recv_timeout(Duration::from_secs(4));
                }
            }
            MicEvent::Disconnect => {
                if self.running {
                    self.close(true);
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
        // The gate reflects the events handled so far; `Live` reflects the latest one the core sent, which may still
        // be queued behind them. Open only when both agree.
        let wanted = self.gate.wanted() && self.live.allowed();
        if wanted && !self.running && self.retry_at.is_none() {
            let epoch = self.live.bump();
            let sink = AudioSink {
                tx: self.tx.clone(),
                epoch,
                live: self.live.clone(),
                lost: Arc::new(AtomicBool::new(false)),
                timed: Arc::new(AtomicBool::new(false)),
                failed: self.events.clone(),
            };
            match self.input.open(sink) {
                Ok(()) => {
                    self.running = true;
                    // Opening can take a while; a pause or lost core meanwhile closes it again at once.
                    if !self.live.current(epoch) {
                        self.close(false);
                        return;
                    }
                    if self.failing {
                        self.failing = false;
                        (self.status)(true, "microphone open".into());
                    }
                }
                Err(e) => {
                    self.failing = true;
                    self.live.bump();
                    (self.status)(false, format!("cannot open the microphone: {e}"));
                    self.retry_at = Some(Instant::now() + RETRY);
                }
            }
        } else if !wanted && self.running {
            self.close(true);
        }
    }

    fn close(&mut self, drop_audio: bool) {
        // Invalidate first: the device may take a while to close, and nothing it still delivers may be used.
        if drop_audio {
            self.live.bump();
        }
        self.input.close();
        self.running = false;
    }
}

/// Runs the controller until `Stop` or `Disconnect`. `make` builds the input on this thread (some audio
/// streams can't move between threads).
pub fn run_mic<I: AudioInput>(
    make: impl FnOnce() -> I,
    rx: Receiver<MicEvent>,
    live: Arc<Live>,
    tx: SyncSender<Audio>,
    events: mpsc::Sender<MicEvent>,
    status: Box<dyn FnMut(bool, String) + Send>,
) {
    let mut ctl = MicController::new(make(), live, tx, events, status);
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let last = matches!(ev, MicEvent::Stop | MicEvent::Disconnect);
                ctl.handle(ev);
                if last {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => ctl.tick(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => {
                ctl.handle(MicEvent::Disconnect);
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
        revoke_during_open: Arc<AtomicBool>,
        live: Arc<Mutex<Option<Arc<Live>>>>,
    }

    impl AudioInput for FakeInput {
        fn open(&mut self, sink: AudioSink) -> Result<(), String> {
            if self.revoke_during_open.swap(false, Ordering::SeqCst) {
                self.live.lock().unwrap().clone().unwrap().revoke();
            }
            if !sink.current() {
                self.log.lock().unwrap().push("built, not started".into());
                return Ok(());
            }
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
            // Records whether audio was already refused when the device started closing.
            let live = self.live.lock().unwrap().clone().unwrap();
            self.log
                .lock()
                .unwrap()
                .push(format!("close at epoch {}", live.epoch()));
        }
    }

    type Statuses = Arc<Mutex<Vec<(bool, String)>>>;

    fn controller() -> (
        MicController<FakeInput>,
        FakeInput,
        Receiver<Audio>,
        Statuses,
        Arc<Live>,
    ) {
        let input = FakeInput::default();
        let live = Arc::new(Live::default());
        live.allow();
        *input.live.lock().unwrap() = Some(live.clone());
        let (tx, rx) = mpsc::sync_channel(8);
        let (etx, _erx) = mpsc::channel();
        let statuses = Arc::new(Mutex::new(vec![]));
        let s = statuses.clone();
        let ctl = MicController::new(
            input.clone(),
            live.clone(),
            tx,
            etx,
            Box::new(move |ok, r| s.lock().unwrap().push((ok, r))),
        );
        (ctl, input, rx, statuses, live)
    }

    fn log(i: &FakeInput) -> Vec<String> {
        i.log.lock().unwrap().clone()
    }

    #[test]
    fn closed_until_ready_started_and_listening() {
        let mut g = MicGate::default();
        assert!(!g.wanted());
        g.apply(&MicEvent::Ready);
        assert!(!g.wanted(), "the core has not said yet");
        g.apply(&MicEvent::Listening(true));
        assert!(!g.wanted(), "`listening` before `start` opens nothing");
        g.apply(&MicEvent::Start(false));
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
        let (mut c, input, _rx, _, _) = controller();
        c.handle(MicEvent::Start(false));
        c.handle(MicEvent::Ready);
        assert!(log(&input).is_empty());
        assert!(!c.is_running());
    }

    #[test]
    fn pause_invalidates_audio_before_closing_then_resume_reopens() {
        let (mut c, input, _rx, _, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        c.handle(MicEvent::Listening(false));
        c.handle(MicEvent::Listening(true));
        assert_eq!(log(&input), ["open 1", "close at epoch 2", "open 3"]);
        assert!(c.is_running());
    }

    #[test]
    fn a_queued_start_does_not_open_after_a_pause_or_lost_core() {
        // The protocol thread already revoked (pause, or EOF) while Ready and Start(true) were still queued.
        for last in [MicEvent::Listening(false), MicEvent::Disconnect] {
            let (mut c, input, _rx, _, live) = controller();
            live.revoke();
            c.handle(MicEvent::Ready);
            c.handle(MicEvent::Start(true));
            c.handle(last);
            assert!(log(&input).is_empty(), "{:?}", log(&input));
            assert!(!c.is_running());
        }
    }

    #[test]
    fn a_pause_while_opening_closes_again_at_once() {
        let (mut c, input, _rx, _, _) = controller();
        input.revoke_during_open.store(true, Ordering::SeqCst);
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        assert!(!c.is_running());
        assert_eq!(log(&input), ["built, not started", "close at epoch 2"]);
    }

    #[test]
    fn repeated_listening_does_not_reopen() {
        let (mut c, input, _rx, _, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        c.handle(MicEvent::Listening(true));
        assert_eq!(log(&input), ["open 1"]);
    }

    #[test]
    fn stop_closes_keeps_epoch_and_asks_engine_to_finish() {
        let (mut c, input, rx, _, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
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
        assert_eq!(log(&input), ["open 1", "close at epoch 1"]);
    }

    #[test]
    fn disconnect_drops_audio_without_finishing() {
        let (mut c, input, rx, _, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        c.handle(MicEvent::Disconnect);
        assert_eq!(log(&input), ["open 1", "close at epoch 2"]);
        assert!(rx.try_recv().is_err(), "no finish request");
        c.handle(MicEvent::Listening(true));
        assert!(!c.is_running());
    }

    #[test]
    fn failed_open_reports_and_retries_while_wanted() {
        let (mut c, input, _rx, statuses, _) = controller();
        input.fail_next.store(true, Ordering::SeqCst);
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
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
        let (mut c, input, _rx, _, _) = controller();
        input.fail_next.store(true, Ordering::SeqCst);
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        c.handle(MicEvent::Listening(false));
        c.tick(Instant::now() + RETRY);
        assert!(!c.is_running());
        assert_eq!(log(&input), ["open failed"]);
    }

    #[test]
    fn stream_failure_closes_then_reopens() {
        let (mut c, input, _rx, _, _) = controller();
        c.handle(MicEvent::Ready);
        c.handle(MicEvent::Start(true));
        c.handle(MicEvent::Failed { epoch: 99 });
        assert!(c.is_running(), "a stale failure is ignored");
        c.handle(MicEvent::Failed { epoch: 1 });
        assert!(!c.is_running());
        c.tick(Instant::now() + RETRY);
        assert_eq!(log(&input), ["open 1", "close at epoch 2", "open 3"]);
    }

    fn sink(cap: usize, live: Arc<Live>, epoch: u64) -> (AudioSink, Receiver<Audio>) {
        let (tx, rx) = mpsc::sync_channel(cap);
        let (etx, _e) = mpsc::channel();
        let s = AudioSink {
            tx,
            epoch,
            live,
            lost: Arc::new(AtomicBool::new(false)),
            timed: Arc::new(AtomicBool::new(false)),
            failed: etx,
        };
        (s, rx)
    }

    #[test]
    fn full_queue_marks_the_next_chunk() {
        let live = Arc::new(Live::default());
        live.allow();
        let (sink, rx) = sink(1, live, 0);
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

    #[test]
    fn audio_after_a_revoke_is_refused_at_once() {
        let live = Arc::new(Live::default());
        live.allow();
        let (sink, rx) = sink(4, live.clone(), 0);
        sink.push(&[0.0], 1, 16000, 1);
        live.revoke();
        sink.push(&[0.0], 1, 16000, 2);
        assert!(rx.recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "the callback's late chunk never queues"
        );
    }

    #[test]
    fn buffers_without_capture_times_are_dropped() {
        let live = Arc::new(Live::default());
        live.allow();
        let lag = Some(Duration::from_millis(30));
        // A stream that starts without timing (ALSA's first buffer): nothing is queued until timing appears.
        let (s1, rx) = sink(8, live.clone(), 0);
        assert!(!s1.push_captured(&[0.0; 320], 1, 16_000, SECOND, None));
        assert!(!s1.push_captured(&[0.0; 320], 1, 16_000, SECOND, Some(Duration::from_secs(9))));
        assert!(rx.try_recv().is_err());
        assert!(s1.push_captured(&[0.0; 320], 1, 16_000, 2 * SECOND, lag));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Audio::Samples {
                lost_before: false,
                ..
            }
        ));
        // Timing lost after a resume (a new sink for the new epoch): dropped again.
        let (s2, rx2) = sink(8, live, 0);
        assert!(s2.push_captured(&[0.0; 320], 1, 16_000, 3 * SECOND, lag));
        assert!(!s2.push_captured(&[0.0; 320], 1, 16_000, 3 * SECOND, None));
        assert!(s2.push_captured(&[0.0; 320], 1, 16_000, 4 * SECOND, lag));
        let flags: Vec<bool> = rx2
            .try_iter()
            .map(|a| {
                matches!(
                    a,
                    Audio::Samples {
                        lost_before: true,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(flags, [false, true]);
    }

    #[test]
    fn buffer_end_uses_the_capture_time() {
        let now = 100 * SECOND;
        // A 20 ms buffer captured 120 ms before its callback ran.
        let end = buffer_end_ns(now, Some(Duration::from_millis(120)), 320, 16_000).unwrap();
        assert_eq!(end, now - 100_000_000);
        assert_eq!(
            buffer_end_ns(now, Some(Duration::ZERO), 320, 16_000),
            Some(now),
            "never later than the callback"
        );
        assert_eq!(buffer_end_ns(now, None, 320, 16_000), None);
        assert_eq!(
            buffer_end_ns(now, Some(Duration::from_secs(5)), 320, 16_000),
            None
        );
    }
}

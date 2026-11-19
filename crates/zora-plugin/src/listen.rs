//! The `transcript` capability: audio chunks in, `segment`/`partial`/`gap` out. Speech recognition sits behind
//! `Recognizer` so tests (and the e2e file mode) run without a model.

use crate::clock::{MILLI, Ns, SECOND, now_ns};
use crate::mic::{Audio, AudioInput, AudioSink, Live, MicEvent, run_mic};
use crate::proto::{CoreMsg, Out, clock_reply, handshake, parse, read_line};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const RATE: u32 = 16_000;
const NS_PER_SAMPLE: Ns = SECOND / RATE as Ns;
/// Chunks queued between the capture callback and the engine (about 4 s of audio at 20 ms chunks).
const QUEUE: usize = 200;

/// One finished utterance; offsets count 16 kHz samples since the recognizer's last `reset`.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

/// A streaming recognizer fed 16 kHz mono.
pub trait Recognizer {
    fn accept(&mut self, samples: &[f32]);
    /// The unfinished text of the current utterance.
    fn partial(&mut self) -> String;
    /// An utterance that just ended (endpoint), after which recognition continues.
    fn endpoint(&mut self) -> Option<Utterance>;
    /// No more audio is coming: finish what is buffered.
    fn finish(&mut self) -> Option<Utterance>;
    /// Forget everything buffered and start counting samples from zero.
    fn reset(&mut self);
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Segment {
        session: u64,
        start_ns: Ns,
        end_ns: Ns,
        text: String,
    },
    Partial {
        session: u64,
        text: String,
    },
    Gap {
        at_ns: Ns,
        reason: String,
    },
}

impl Event {
    pub fn to_json(&self) -> Value {
        match self {
            Event::Segment {
                session,
                start_ns,
                end_ns,
                text,
            } => {
                json!({"t": "segment", "session": session, "start_ns": start_ns, "end_ns": end_ns, "text": text})
            }
            Event::Partial { session, text } => {
                json!({"t": "partial", "session": session, "text": text})
            }
            Event::Gap { at_ns, reason } => json!({"t": "gap", "at_ns": at_ns, "reason": reason}),
        }
    }
}

/// Downmix to mono and resample to 16 kHz: box averaging when downsampling, linear interpolation when upsampling.
#[derive(Debug, Clone)]
pub struct Resampler {
    rate: u32,
    acc: f64,
    sum: f32,
    count: u32,
    prev: f32,
    pos: f64,
}

impl Resampler {
    pub fn new(rate: u32) -> Self {
        Resampler {
            rate: rate.max(1),
            acc: 0.0,
            sum: 0.0,
            count: 0,
            prev: 0.0,
            pos: 0.0,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn process(&mut self, data: &[f32], channels: u16, out: &mut Vec<f32>) {
        let ch = channels.max(1) as usize;
        let step = self.rate as f64 / RATE as f64;
        for frame in data.chunks(ch) {
            let x = frame.iter().sum::<f32>() / frame.len() as f32;
            if self.rate == RATE {
                out.push(x);
            } else if step > 1.0 {
                self.sum += x;
                self.count += 1;
                self.acc += 1.0;
                if self.acc >= step {
                    out.push(self.sum / self.count as f32);
                    self.acc -= step;
                    self.sum = 0.0;
                    self.count = 0;
                }
            } else {
                // Outputs between the previous input sample and this one.
                while self.pos < 1.0 {
                    out.push(self.prev + (x - self.prev) * self.pos as f32);
                    self.pos += step;
                }
                self.pos -= 1.0;
                self.prev = x;
            }
        }
    }
}

/// Turns chunks into protocol events. Owns the recognizer; runs on one thread.
pub struct Engine<R: Recognizer> {
    rec: R,
    live: Arc<Live>,
    seen_epoch: u64,
    session: u64,
    resampler: Option<Resampler>,
    /// Samples fed since the recognizer's last reset, and the time of the last one.
    fed: u64,
    anchor: Option<(u64, Ns)>,
    last_end_ns: Option<Ns>,
    last_partial: String,
    last_partial_at: Ns,
    pub partials: bool,
    buf: Vec<f32>,
}

/// Longest segment text sent; the protocol's line limit is 64 KiB and escaping can double the size.
const MAX_TEXT: usize = 16 * 1024;

/// More than this between two chunks is a discontinuity (suspend, stalled device).
const MAX_JUMP: Ns = 500 * MILLI;

impl<R: Recognizer> Engine<R> {
    pub fn new(rec: R, live: Arc<Live>) -> Self {
        let seen_epoch = live.epoch();
        Engine {
            rec,
            live,
            seen_epoch,
            session: 1,
            resampler: None,
            fed: 0,
            anchor: None,
            last_end_ns: None,
            last_partial: String::new(),
            last_partial_at: 0,
            partials: true,
            buf: Vec::new(),
        }
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    /// Drops untranscribed audio when the microphone was closed or reopened since the last chunk.
    pub fn check_epoch(&mut self) {
        let e = self.live.epoch();
        if e != self.seen_epoch {
            self.seen_epoch = e;
            self.restart();
        }
    }

    fn restart(&mut self) {
        self.rec.reset();
        self.session += 1;
        self.fed = 0;
        self.anchor = None;
        self.last_end_ns = None;
        self.resampler = None;
        self.last_partial.clear();
    }

    pub fn handle(&mut self, audio: Audio, emit: &mut dyn FnMut(Event)) {
        self.check_epoch();
        // Rechecked as each result goes out: a pause during decoding must not let it through.
        let (live, epoch) = (self.live.clone(), self.seen_epoch);
        let emit = &mut |ev: Event| {
            if live.current(epoch) {
                emit(ev)
            }
        };
        match audio {
            Audio::Samples {
                epoch,
                data,
                channels,
                rate,
                end_ns,
                lost_before,
            } => {
                if epoch != self.seen_epoch {
                    return;
                }
                let frames = data.len() / channels.max(1) as usize;
                let dur = frames as Ns * SECOND / rate.max(1) as Ns;
                let begin = end_ns - dur;
                let jumped = self.last_end_ns.is_some_and(|l| begin - l > MAX_JUMP);
                if lost_before || jumped {
                    let reason = if lost_before {
                        "audio dropped"
                    } else {
                        "audio discontinuity"
                    };
                    emit(Event::Gap {
                        at_ns: self.last_end_ns.unwrap_or(begin),
                        reason: reason.into(),
                    });
                    self.restart();
                }
                if self.resampler.as_ref().map(|r| r.rate()) != Some(rate) {
                    self.resampler = Some(Resampler::new(rate));
                }
                self.buf.clear();
                if let Some(r) = self.resampler.as_mut() {
                    r.process(&data, channels, &mut self.buf);
                }
                self.rec.accept(&self.buf);
                self.fed += self.buf.len() as u64;
                self.anchor = Some((self.fed, end_ns));
                self.last_end_ns = Some(end_ns);
                while let Some(u) = self.rec.endpoint() {
                    self.emit_utterance(u, emit);
                }
                self.maybe_partial(end_ns, emit);
            }
            Audio::Failed {
                epoch,
                reason,
                at_ns,
            } => {
                if epoch == self.seen_epoch {
                    emit(Event::Gap { at_ns, reason });
                    self.restart();
                }
            }
            Audio::Finish { epoch, done } => {
                if epoch == self.seen_epoch
                    && let Some(u) = self.rec.finish()
                {
                    self.emit_utterance(u, emit);
                }
                let _ = done.send(());
            }
        }
    }

    fn maybe_partial(&mut self, now: Ns, emit: &mut dyn FnMut(Event)) {
        // At most 4 per second, as the protocol asks.
        if !self.partials || now - self.last_partial_at < 250 * MILLI {
            return;
        }
        let text = self.rec.partial();
        let text = text.trim();
        if text.is_empty() || text == self.last_partial {
            return;
        }
        self.last_partial = text.to_string();
        self.last_partial_at = now;
        let tail: String = {
            let n = text.chars().count();
            text.chars().skip(n.saturating_sub(512)).collect()
        };
        emit(Event::Partial {
            session: self.session,
            text: tail,
        });
    }

    fn emit_utterance(&mut self, u: Utterance, emit: &mut dyn FnMut(Event)) {
        self.last_partial.clear();
        let text = u.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some((fed, at)) = self.anchor else { return };
        if text.is_empty() {
            return;
        }
        let time = |off: u64| at - (fed.saturating_sub(off.min(fed))) as Ns * NS_PER_SAMPLE;
        let now = now_ns();
        let end_ns = time(u.end).min(now);
        let start_ns = time(u.start.min(u.end)).min(end_ns);
        // The core rejects anything older than 15 minutes.
        if start_ns < now - 15 * 60 * SECOND + SECOND {
            return;
        }
        if text.len() > MAX_TEXT {
            emit(Event::Gap {
                at_ns: start_ns,
                reason: "utterance too long to send".into(),
            });
            return;
        }
        emit(Event::Segment {
            session: self.session,
            start_ns,
            end_ns,
            text: text.to_lowercase(),
        });
    }
}

/// Runs the engine until the audio channel closes. `emit` gets each event with the epoch it was made in.
pub fn run_engine<R: Recognizer>(
    mut engine: Engine<R>,
    rx: Receiver<Audio>,
    mut emit: impl FnMut(Event, u64),
) {
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(a) => {
                engine.check_epoch();
                // Results carry the epoch they belong to, so the writer can drop them if it went stale.
                let epoch = engine.seen_epoch;
                engine.handle(a, &mut |ev| emit(ev, epoch));
            }
            // A pause drops buffered audio even when no further chunk arrives.
            Err(RecvTimeoutError::Timeout) => engine.check_epoch(),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Plays a WAV file as if it were the microphone, in real time, then a little silence so the last utterance
/// ends. Closing pauses it; reopening continues where it stopped.
pub struct FileInput {
    frames: Arc<Vec<f32>>,
    channels: u16,
    rate: u32,
    pos: Arc<AtomicU64>,
    running: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl FileInput {
    pub fn open_wav(path: &std::path::Path) -> Result<FileInput, String> {
        let r = hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let spec = r.spec();
        let data: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => r.into_samples::<f32>().filter_map(Result::ok).collect(),
            hound::SampleFormat::Int => {
                let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
                r.into_samples::<i32>()
                    .filter_map(Result::ok)
                    .map(|s| s as f32 / scale)
                    .collect()
            }
        };
        Ok(FileInput {
            frames: Arc::new(data),
            channels: spec.channels,
            rate: spec.sample_rate,
            pos: Arc::new(AtomicU64::new(0)),
            running: None,
        })
    }

    /// All samples, mono 16 kHz.
    pub fn mono16k(&self) -> Vec<f32> {
        let mut out = Vec::new();
        Resampler::new(self.rate).process(&self.frames, self.channels, &mut out);
        out
    }
}

impl AudioInput for FileInput {
    fn open(&mut self, sink: AudioSink) -> Result<(), String> {
        let flag = Arc::new(AtomicBool::new(true));
        let (f, frames, pos) = (flag.clone(), self.frames.clone(), self.pos.clone());
        let (ch, rate) = (self.channels as usize, self.rate);
        let chunk = (rate as usize / 50).max(1) * ch; // 20 ms
        let silence = vec![0.0f32; chunk];
        let tail = 75; // 1.5 s
        let handle = std::thread::spawn(move || {
            let total = frames.len() as u64 + (tail * chunk) as u64;
            while f.load(Ordering::SeqCst) {
                let p = pos.load(Ordering::SeqCst);
                if p >= total {
                    return;
                }
                let s = p as usize;
                let piece = if s < frames.len() {
                    &frames[s..(s + chunk).min(frames.len())]
                } else {
                    &silence[..]
                };
                std::thread::sleep(Duration::from_millis(20));
                if !f.load(Ordering::SeqCst) {
                    return;
                }
                sink.push(piece, ch as u16, rate, now_ns());
                pos.store(p + piece.len() as u64, Ordering::SeqCst);
            }
        });
        self.running = Some((flag, handle));
        Ok(())
    }

    fn close(&mut self) {
        if let Some((flag, h)) = self.running.take() {
            flag.store(false, Ordering::SeqCst);
            let _ = h.join();
        }
    }
}

/// Test recognizer: an utterance is a stretch of sound above a level, ended by 0.5 s of quiet; each one is
/// "recognized" as the next line of a script. Lets the whole pipeline run without a speech model.
pub struct ScriptedRecognizer {
    lines: std::collections::VecDeque<String>,
    total: u64,
    start: Option<u64>,
    last_voice: u64,
    frame: Vec<f32>,
}

impl ScriptedRecognizer {
    pub fn new(lines: impl IntoIterator<Item = String>) -> Self {
        ScriptedRecognizer {
            lines: lines.into_iter().filter(|l| !l.trim().is_empty()).collect(),
            total: 0,
            start: None,
            last_voice: 0,
            frame: Vec::new(),
        }
    }

    fn take(&mut self, end: u64) -> Option<Utterance> {
        let start = self.start.take()?;
        let text = self.lines.pop_front()?;
        Some(Utterance { text, start, end })
    }
}

impl Recognizer for ScriptedRecognizer {
    fn accept(&mut self, samples: &[f32]) {
        for &s in samples {
            self.frame.push(s);
            self.total += 1;
            if self.frame.len() == 160 {
                let rms = (self.frame.iter().map(|x| x * x).sum::<f32>() / 160.0).sqrt();
                if rms > 0.02 {
                    if self.start.is_none() {
                        self.start = Some(self.total - 160);
                    }
                    self.last_voice = self.total;
                }
                self.frame.clear();
            }
        }
    }

    fn partial(&mut self) -> String {
        match self.start {
            Some(_) => self.lines.front().cloned().unwrap_or_default(),
            None => String::new(),
        }
    }

    fn endpoint(&mut self) -> Option<Utterance> {
        if self.start.is_some() && self.total - self.last_voice >= RATE as u64 / 2 {
            let end = self.last_voice;
            return self.take(end);
        }
        None
    }

    fn finish(&mut self) -> Option<Utterance> {
        let end = self.last_voice;
        self.take(end)
    }

    fn reset(&mut self) {
        self.total = 0;
        self.start = None;
        self.last_voice = 0;
        self.frame.clear();
    }
}

/// What a core message means for the microphone: the event for the mic thread, and whether audio is allowed
/// from now on. Nothing is allowed before `start`; a malformed `listening` arrives as a pause.
pub fn mic_control(started: &mut bool, msg: &CoreMsg) -> Option<(MicEvent, bool)> {
    match msg {
        CoreMsg::Start { listening, .. } => {
            *started = true;
            Some((MicEvent::Start(*listening), *listening))
        }
        CoreMsg::Listening { listening, .. } => {
            Some((MicEvent::Listening(*listening), *listening && *started))
        }
        _ => None,
    }
}

/// The whole `transcript` plugin over stdin/stdout: handshake, microphone on/off, the engine, and `stop`.
/// `make_recognizer` runs on the engine thread after `ready` (loading a model can take a while); until it
/// succeeds the microphone stays closed and `status{ready:false}` says why.
pub fn run_listen_plugin<I, R>(
    make_input: impl FnOnce() -> I + Send + 'static,
    make_recognizer: impl FnOnce(&Value) -> Result<R, String> + Send + 'static,
) -> i32
where
    I: AudioInput,
    R: Recognizer + 'static,
{
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let out = Arc::new(Out::stdout());
    let session = match handshake(&mut input, &out, &["transcript"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zora-listen: {e}");
            return 1;
        }
    };
    let config = session.config.clone();
    let live = Arc::new(Live::default());
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Audio>(QUEUE);
    let (mic_tx, mic_rx) = mpsc::channel::<MicEvent>();

    let engine = {
        let (out, live, mic_tx) = (out.clone(), live.clone(), mic_tx.clone());
        let partials = config["partials"].as_bool().unwrap_or(true);
        std::thread::spawn(move || match make_recognizer(&config) {
            Ok(rec) => {
                out.send(json!({"t": "status", "ready": true, "reason": "speech ready"}));
                let _ = mic_tx.send(MicEvent::Ready);
                let mut engine = Engine::new(rec, live.clone());
                engine.partials = partials;
                run_engine(engine, audio_rx, |ev, epoch| {
                    out.send_if(ev.to_json(), || live.current(epoch));
                });
            }
            Err(reason) => {
                eprintln!("zora-listen: {reason}");
                out.send(json!({"t": "status", "ready": false, "reason": reason}));
                // Answer `stop` so the core doesn't wait for its deadline.
                while let Ok(a) = audio_rx.recv() {
                    if let Audio::Finish { done, .. } = a {
                        let _ = done.send(());
                    }
                }
            }
        })
    };
    let mic = {
        let (out, live, mic_tx) = (out.clone(), live.clone(), mic_tx.clone());
        let status_out = out.clone();
        std::thread::spawn(move || {
            run_mic(
                make_input,
                mic_rx,
                live,
                audio_tx,
                mic_tx,
                Box::new(move |ready, reason| {
                    eprintln!("zora-listen: microphone ready={ready}");
                    status_out.send(json!({"t": "status", "ready": ready, "reason": reason}));
                }),
            )
        })
    };
    let mic = Arc::new(Mutex::new(Some(mic)));
    let join_mic = |timeout: Duration| {
        let h = mic.lock().unwrap().take();
        if let Some(h) = h {
            let end = std::time::Instant::now() + timeout;
            while !h.is_finished() && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let mut stopped = false;
    let mut started = false;
    while let Ok(Some(line)) = read_line(&mut input) {
        let msg = parse(&line);
        if let Some((ev, allow)) = mic_control(&mut started, &msg) {
            eprintln!("zora-listen: {ev:?}, listening={allow}");
            // Takes effect here, before the mic thread gets to it.
            if allow {
                live.allow();
            } else {
                live.revoke();
            }
            let _ = mic_tx.send(ev);
            continue;
        }
        match msg {
            CoreMsg::Clock { id } => {
                out.send(clock_reply(id));
            }
            CoreMsg::Stop if !stopped => {
                stopped = true;
                let _ = mic_tx.send(MicEvent::Stop);
                join_mic(Duration::from_secs(5));
                out.send(json!({"t": "stopped"}));
            }
            _ => {}
        }
        if out.is_broken() {
            break;
        }
    }
    // The core went away: drop what wasn't transcribed and close the microphone before exiting.
    live.revoke();
    let _ = mic_tx.send(MicEvent::Disconnect);
    join_mic(Duration::from_secs(2));
    drop(engine);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, rate: u32, amp: f32) -> Vec<f32> {
        (0..(secs * rate as f32) as usize)
            .map(|i| amp * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin())
            .collect()
    }

    fn engine(lines: &[&str]) -> (Engine<ScriptedRecognizer>, Arc<Live>) {
        let live = Arc::new(Live::default());
        live.bump();
        live.allow();
        let rec = ScriptedRecognizer::new(lines.iter().map(|s| s.to_string()));
        (Engine::new(rec, live.clone()), live)
    }

    /// Feeds `samples` (16 kHz mono) in 20 ms chunks ending at `t0 + offset`.
    fn feed(e: &mut Engine<ScriptedRecognizer>, samples: &[f32], t0: Ns, epoch: u64) -> Vec<Event> {
        let mut evs = vec![];
        let mut fed = 0usize;
        for c in samples.chunks(320) {
            fed += c.len();
            let end_ns = t0 + fed as Ns * NS_PER_SAMPLE;
            e.handle(
                Audio::Samples {
                    epoch,
                    data: c.to_vec(),
                    channels: 1,
                    rate: RATE,
                    end_ns,
                    lost_before: false,
                },
                &mut |ev| evs.push(ev),
            );
        }
        evs
    }

    fn segments(evs: &[Event]) -> Vec<(Ns, Ns, String)> {
        evs.iter()
            .filter_map(|e| match e {
                Event::Segment {
                    start_ns,
                    end_ns,
                    text,
                    ..
                } => Some((*start_ns, *end_ns, text.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resampler_downmixes_and_converts_rates() {
        let mut out = vec![];
        Resampler::new(48_000).process(&vec![0.5; 48_000 * 2], 2, &mut out);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|x| (x - 0.5).abs() < 1e-6));
        let mut out = vec![];
        let mut r = Resampler::new(44_100);
        for _ in 0..10 {
            r.process(&vec![0.1; 4410], 1, &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() <= 2, "{}", out.len());
        let mut out = vec![];
        Resampler::new(8_000).process(&vec![0.2; 8_000], 1, &mut out);
        assert!((out.len() as i64 - 16_000).abs() <= 2);
    }

    #[test]
    fn segments_are_anchored_to_captured_samples() {
        let (mut e, _) = engine(&["Hey Zora", "do it zora"]);
        let t0 = now_ns() - 10 * SECOND;
        let mut audio = vec![0.0; 16_000];
        audio.extend(tone(1.0, RATE, 0.3));
        audio.extend(vec![0.0; 16_000]);
        audio.extend(tone(0.5, RATE, 0.3));
        audio.extend(vec![0.0; 16_000]);
        let evs = feed(&mut e, &audio, t0, 1);
        let segs = segments(&evs);
        assert_eq!(segs.len(), 2, "{evs:?}");
        assert_eq!(segs[0].2, "hey zora");
        let near = |a: Ns, b: Ns| (a - b).abs() < 30 * MILLI;
        assert!(near(segs[0].0, t0 + SECOND), "{}", segs[0].0 - t0);
        assert!(near(segs[0].1, t0 + 2 * SECOND));
        assert!(near(segs[1].0, t0 + 3 * SECOND));
        assert!(segs.iter().all(|(s, e, _)| s <= e));
        assert!(evs.iter().any(|e| matches!(e, Event::Partial { .. })));
    }

    #[test]
    fn closing_the_mic_drops_untranscribed_audio() {
        let (mut e, epoch) = engine(&["secret", "after"]);
        let t0 = now_ns() - 10 * SECOND;
        let evs = feed(&mut e, &tone(1.0, RATE, 0.3), t0, 1);
        assert!(segments(&evs).is_empty());
        let session = e.session();
        // Paused mid-utterance; no further audio arrives.
        epoch.revoke();
        e.check_epoch();
        assert_eq!(e.session(), session + 1);
        // A chunk still queued from the old epoch is ignored.
        let evs = feed(&mut e, &vec![0.0; 16_000], t0 + SECOND, 1);
        assert!(segments(&evs).is_empty());
        // Resumed: only new speech is transcribed; "secret" was never finished.
        epoch.bump();
        epoch.allow();
        let mut a = tone(0.5, RATE, 0.3);
        a.extend(vec![0.0; 16_000]);
        let evs = feed(&mut e, &a, t0 + 5 * SECOND, 3);
        assert_eq!(segments(&evs)[0].2, "secret", "the script just moves on");
        let (mut e2, epoch2) = engine(&["first"]);
        feed(&mut e2, &tone(1.0, RATE, 0.3), t0, 1);
        epoch2.revoke();
        let (tx, rx) = mpsc::channel();
        let mut evs = vec![];
        e2.handle(Audio::Finish { epoch: 1, done: tx }, &mut |ev| evs.push(ev));
        rx.recv().unwrap();
        assert!(
            segments(&evs).is_empty(),
            "finish of a dropped epoch emits nothing"
        );
    }

    #[test]
    fn finish_flushes_the_open_utterance() {
        let (mut e, _) = engine(&["last words"]);
        let t0 = now_ns() - 10 * SECOND;
        feed(&mut e, &tone(1.0, RATE, 0.3), t0, 1);
        let (tx, rx) = mpsc::channel();
        let mut evs = vec![];
        e.handle(Audio::Finish { epoch: 1, done: tx }, &mut |ev| evs.push(ev));
        rx.recv().unwrap();
        assert_eq!(segments(&evs)[0].2, "last words");
    }

    #[test]
    fn lost_audio_and_time_jumps_become_gaps_and_new_sessions() {
        let (mut e, _) = engine(&["a"]);
        let t0 = now_ns() - 10 * SECOND;
        feed(&mut e, &vec![0.0; 3200], t0, 1);
        let s = e.session();
        let evs = feed(&mut e, &vec![0.0; 3200], t0 + 3 * SECOND, 1);
        assert!(matches!(&evs[0], Event::Gap { reason, .. } if reason == "audio discontinuity"));
        assert_eq!(e.session(), s + 1);
        let mut evs = vec![];
        e.handle(
            Audio::Samples {
                epoch: 1,
                data: vec![0.0; 320],
                channels: 1,
                rate: RATE,
                end_ns: t0 + 4 * SECOND,
                lost_before: true,
            },
            &mut |ev| evs.push(ev),
        );
        assert!(matches!(&evs[0], Event::Gap { reason, .. } if reason == "audio dropped"));
        let mut evs = vec![];
        e.handle(
            Audio::Failed {
                epoch: 1,
                reason: "device gone".into(),
                at_ns: t0,
            },
            &mut |ev| evs.push(ev),
        );
        assert!(matches!(&evs[0], Event::Gap { reason, .. } if reason == "device gone"));
    }

    #[test]
    fn segment_times_never_lie_in_the_future() {
        let (mut e, _) = engine(&["x"]);
        // Chunk timestamps a little ahead of the clock (a skewed device clock).
        let t0 = now_ns() + 2 * SECOND;
        let mut a = tone(0.5, RATE, 0.3);
        a.extend(vec![0.0; 16_000]);
        let evs = feed(&mut e, &a, t0, 1);
        let segs = segments(&evs);
        let now = now_ns();
        assert!(segs[0].1 <= now && segs[0].0 <= segs[0].1);
    }

    #[test]
    fn partials_are_rate_limited_and_optional() {
        let (mut e, _) = engine(&["x"]);
        e.partials = false;
        let evs = feed(&mut e, &tone(1.0, RATE, 0.3), now_ns() - SECOND, 1);
        assert!(!evs.iter().any(|e| matches!(e, Event::Partial { .. })));
    }

    /// Pauses (as the protocol thread would) while the recognizer is still decoding.
    struct PausedMidDecode(ScriptedRecognizer, Arc<Live>);

    impl Recognizer for PausedMidDecode {
        fn accept(&mut self, s: &[f32]) {
            self.0.accept(s)
        }
        fn partial(&mut self) -> String {
            self.0.partial()
        }
        fn endpoint(&mut self) -> Option<Utterance> {
            let u = self.0.endpoint();
            if u.is_some() {
                self.1.revoke();
            }
            u
        }
        fn finish(&mut self) -> Option<Utterance> {
            self.0.finish()
        }
        fn reset(&mut self) {
            self.0.reset()
        }
    }

    #[test]
    fn a_result_finished_after_a_pause_is_not_sent() {
        let live = Arc::new(Live::default());
        live.bump();
        live.allow();
        let rec = PausedMidDecode(
            ScriptedRecognizer::new(["secret".to_string()]),
            live.clone(),
        );
        let mut e = Engine::new(rec, live);
        let mut a = tone(0.5, RATE, 0.3);
        a.extend(vec![0.0; 16_000]);
        let mut evs = vec![];
        for c in a.chunks(320) {
            e.handle(
                Audio::Samples {
                    epoch: 1,
                    data: c.to_vec(),
                    channels: 1,
                    rate: RATE,
                    end_ns: now_ns(),
                    lost_before: false,
                },
                &mut |ev| evs.push(ev),
            );
        }
        assert!(segments(&evs).is_empty(), "{evs:?}");
    }

    #[test]
    fn an_utterance_too_long_to_send_becomes_a_gap() {
        let (mut e, _) = engine(&[&"word ".repeat(5000)]);
        let mut a = tone(0.5, RATE, 0.3);
        a.extend(vec![0.0; 16_000]);
        let evs = feed(&mut e, &a, now_ns() - 10 * SECOND, 1);
        assert!(segments(&evs).is_empty());
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::Gap { reason, .. } if reason.contains("too long")))
        );
    }

    #[test]
    fn microphone_control_needs_start_first() {
        let mut started = false;
        let l = |on| CoreMsg::Listening {
            listening: on,
            until: None,
        };
        assert_eq!(
            mic_control(&mut started, &l(true)),
            Some((MicEvent::Listening(true), false)),
            "no audio before start"
        );
        let start = CoreMsg::Start {
            listening: true,
            until: None,
        };
        assert_eq!(
            mic_control(&mut started, &start),
            Some((MicEvent::Start(true), true))
        );
        assert_eq!(
            mic_control(&mut started, &l(false)),
            Some((MicEvent::Listening(false), false))
        );
        assert_eq!(mic_control(&mut started, &CoreMsg::Stop), None);
    }

    #[test]
    fn event_json_matches_protocol() {
        let v = Event::Segment {
            session: 2,
            start_ns: 1,
            end_ns: 2,
            text: "hi".into(),
        }
        .to_json();
        assert_eq!(v["t"], "segment");
        assert_eq!(v["start_ns"], 1);
        assert_eq!(
            Event::Gap {
                at_ns: 5,
                reason: "r".into()
            }
            .to_json()["t"],
            "gap"
        );
    }
}

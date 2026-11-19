//! Plugin host: implements docs/protocol.md for one plugin (a spawned child, or the attached app that started us).

use crate::pause::ListenState;
use crate::queue::{Class, Classify, InputQueue};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};
use zora_core::clock::{CLOCK_DOMAIN, MILLI, Ns, SECOND, now_ns};
use zora_core::coordinator::Input;
use zora_core::model::Segment;
use zora_core::suppression::DictationSignal;

pub const PROTOCOL: u32 = 1;
const MAX_LINE: usize = 64 * 1024;
const HEARTBEAT_LEASE: Ns = 6 * SECOND;
/// Ends a session whose plugin must stay off until listening resumes.
const PAUSED_OUT: &str = "stopped while paused";
/// A spawned transcript plugin that hasn't confirmed a pause by then is stopped, however its connection is doing.
/// Longer than the service's 5 s, so the service reports it first.
const PAUSE_STOP: Duration = Duration::from_secs(7);
/// A plugin that doesn't take a message within this long isn't reading its input.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages from a plugin. Some fields are part of the protocol but unused by the core.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum FromPlugin {
    Ready {
        protocol: u32,
        generation: u64,
        capabilities: Vec<String>,
        clock: String,
        now_ns: Ns,
    },
    Segment {
        seq: u64,
        generation: u64,
        session: u64,
        start_ns: Ns,
        end_ns: Ns,
        text: String,
    },
    Gap {
        seq: u64,
        generation: u64,
        at_ns: Ns,
        reason: String,
    },
    Partial {
        seq: u64,
        generation: u64,
        session: u64,
        text: String,
        #[serde(default)]
        epoch: Option<u64>,
    },
    Mic {
        seq: u64,
        generation: u64,
        epoch: u64,
        open: bool,
    },
    Status {
        seq: u64,
        generation: u64,
        ready: bool,
        reason: String,
    },
    Dictation {
        seq: u64,
        generation: u64,
        state: String,
        at_ns: Ns,
    },
    Health {
        seq: u64,
        generation: u64,
        healthy: bool,
        capturing: bool,
        reason: String,
        at_ns: Ns,
    },
    Unknown {
        seq: u64,
        generation: u64,
        from_ns: Ns,
        to_ns: Ns,
    },
    Synced {
        seq: u64,
        generation: u64,
        id: u64,
        at_ns: Ns,
        healthy: bool,
        capturing: bool,
    },
    Ack {
        seq: u64,
        generation: u64,
        id: u64,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    Clock {
        seq: u64,
        generation: u64,
        id: u64,
        now_ns: Ns,
    },
    Stopped {
        seq: u64,
        generation: u64,
    },
}

/// Messages to a plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ToPlugin {
    Hello {
        protocol: u32,
        generation: u64,
        clock: &'static str,
        now_ns: Ns,
        config: Value,
    },
    Start {
        listening: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<f64>,
        epoch: u64,
    },
    /// Listening was paused or resumed. Plugins with `transcript` release or reopen the microphone.
    Listening {
        listening: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<f64>,
        epoch: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        confirmed: Option<bool>,
    },
    Sync {
        id: u64,
        cutoff_ns: Ns,
    },
    Notify {
        id: u64,
        kind: String,
        title: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        job: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        harness: Option<String>,
    },
    Clock {
        id: u64,
    },
    Stop,
}

impl ToPlugin {
    pub fn line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default() + "\n"
    }
}

/// Everything the service loop consumes, in one ordered queue.
#[derive(Debug)]
pub enum Item {
    Core(Input),
    PluginUp {
        id: String,
        generation: u64,
        capabilities: Vec<String>,
        tx: mpsc::Sender<String>,
    },
    PluginDown {
        id: String,
        generation: u64,
        /// Its processes are verifiably gone (a spawned plugin's group was reaped).
        reaped: bool,
        /// It may hold a microphone: it declared `transcript`, or never got as far as saying.
        may_record: bool,
    },
    /// A plugin group that failed to reap at `PluginDown` is gone after all.
    Reaped {
        id: String,
        generation: u64,
    },
    PluginStopped {
        id: String,
    },
    Ack {
        id: String,
    },
    /// Live text of the current epoch; `turn` is the listening turn it was heard in.
    Partial {
        text: String,
        turn: u64,
    },
    /// A plugin's microphone state for the current epoch of `turn`.
    Mic {
        id: String,
        turn: u64,
        open: bool,
    },
    Notice(String),
    Job(crate::jobs::JobEvent),
    Shutdown,
    /// The attached plugin connection failed: stop and exit non-zero so the app restarts the core.
    Restart,
    Loss,
}

impl Classify for Item {
    fn class(&self) -> Class {
        match self {
            Item::Core(Input::Segment(_)) | Item::Loss => Class::Transcript,
            Item::Core(Input::Tick) | Item::Partial { .. } => Class::Tick,
            _ => Class::Control,
        }
    }
    fn cost(&self) -> usize {
        match self {
            Item::Core(Input::Segment(s)) => s.text.len() + 64,
            Item::Notice(t) | Item::Partial { text: t, .. } => t.len() + 64,
            _ => 64,
        }
    }
    fn loss_marker() -> Self {
        Item::Loss
    }
    fn is_loss_marker(&self) -> bool {
        matches!(self, Item::Loss)
    }
}

#[derive(Debug, Clone)]
pub enum Connection {
    Spawn(Vec<String>),
    /// Inverted launch: the app started us and talks over fds 3/4 (Windows: stdin/stdout).
    Attached,
}

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub id: String,
    pub connection: Connection,
    pub config: Value,
    /// Listening on/off, forwarded to the plugin as `start` and `listening` messages.
    pub listen: tokio::sync::watch::Receiver<ListenState>,
    /// Capabilities this plugin may declare; `None` allows all. A plugin declaring others is rejected.
    pub allowed: Option<Vec<&'static str>>,
}

#[derive(Debug)]
struct ProtocolError(String);

/// Runs (and on Linux restarts) one plugin until the process exits. Attached mode ends the service on failure.
pub async fn run_plugin(
    spec: PluginSpec,
    queue: Arc<InputQueue<Item>>,
    generation_base: u64,
    stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut generation = generation_base;
    let mut backoff = Duration::from_secs(1);
    let mut wait_for_resume = false;
    loop {
        if std::mem::take(&mut wait_for_resume) {
            let (mut listen, mut stop_wait) = (spec.listen.clone(), stop.clone());
            tokio::select! {
                r = listen.wait_for(|l| l.listen.listening) => if r.is_err() { return },
                _ = stop_wait.wait_for(|s| *s) => return,
            }
            backoff = Duration::from_secs(1);
        }
        generation += 1;
        let started = tokio::time::Instant::now();
        let mut reaped = false;
        let mut may_record = true;
        let result = match &spec.connection {
            Connection::Attached => match crate::os::attached_plugin() {
                Ok((r, w)) => {
                    until_stop(
                        session(
                            &spec,
                            generation,
                            r,
                            w,
                            &queue,
                            stop.clone(),
                            &mut may_record,
                        ),
                        stop.clone(),
                    )
                    .await
                }
                Err(e) => Err(ProtocolError(e)),
            },
            Connection::Spawn(cmd) => match spawn(cmd) {
                Ok((mut child, group)) => {
                    let (r, w) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
                    if let Some(err) = child.stderr.take() {
                        tokio::spawn(forward_stderr(spec.id.clone(), err));
                    }
                    // Shutdown is observed in every phase (negotiation, clock probes, running); cancelling the
                    // session drops it and falls through to the group cleanup below.
                    let res = until_stop(
                        session(
                            &spec,
                            generation,
                            r,
                            w,
                            &queue,
                            stop.clone(),
                            &mut may_record,
                        ),
                        stop.clone(),
                    )
                    .await;
                    // Reap the whole plugin group (TERM, grace, KILL, verify) before any restart.
                    {
                        let pgid = group.id();
                        // Wait for the leader first: an unreaped (zombie) leader still counts as a group member.
                        group.term();
                        if tokio::time::timeout(Duration::from_secs(3), child.wait())
                            .await
                            .is_err()
                        {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                        }
                        let (group, ok) = match tokio::task::spawn_blocking(move || {
                            let ok = group.reap_blocking(Duration::from_secs(3));
                            (group, ok)
                        })
                        .await
                        {
                            Ok((g, ok)) => (Some(g), ok),
                            Err(_) => (None, false),
                        };
                        reaped = ok;
                        if !reaped {
                            eprintln!("[zorad] plugin {} left processes in group {pgid}", spec.id);
                            if let Some(group) = group {
                                tokio::spawn(reap_later(
                                    group,
                                    spec.id.clone(),
                                    generation,
                                    queue.clone(),
                                    stop.clone(),
                                ));
                            }
                        }
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    res
                }
                Err(e) => {
                    reaped = true; // nothing was started
                    Err(ProtocolError(format!("cannot start: {e}")))
                }
            },
        };
        let _ = queue.send(Item::PluginDown {
            id: spec.id.clone(),
            generation,
            reaped,
            may_record,
        });
        if *stop.borrow() {
            return; // service shutdown: the group above is already reaped
        }
        match result {
            Ok(()) => return, // orderly stop
            Err(ProtocolError(reason)) if reason == PAUSED_OUT => {
                wait_for_resume = true;
                continue;
            }
            Err(ProtocolError(reason)) => {
                eprintln!(
                    "[zorad] plugin {} generation {generation} failed: {reason}",
                    spec.id
                );
                let _ = queue.send(Item::Notice(format!(
                    "plugin {} failed; restarting",
                    spec.id
                )));
            }
        }
        if matches!(spec.connection, Connection::Attached) {
            queue.force(Item::Restart);
            return;
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        let mut stop_wait = stop.clone();
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop_wait.wait_for(|s| *s) => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Keeps trying to reap a group that outlived its plugin; the pause counts it until this succeeds.
async fn reap_later(
    group: crate::os::Group,
    id: String,
    generation: u64,
    queue: Arc<InputQueue<Item>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut group = Some(group);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
        let Some(g) = group.take() else { return };
        let Ok((g, ok)) = tokio::task::spawn_blocking(move || {
            let ok = g.reap_blocking(Duration::ZERO);
            (g, ok)
        })
        .await
        else {
            return;
        };
        if ok {
            eprintln!("[zorad] plugin {id} group reaped late");
            queue.force(Item::Reaped { id, generation });
            return;
        }
        group = Some(g);
    }
}

async fn until_stop(
    session: impl std::future::Future<Output = Result<(), ProtocolError>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), ProtocolError> {
    tokio::select! {
        r = session => r,
        _ = stop.wait_for(|s| *s) => Err(ProtocolError("service shutdown".into())),
    }
}

fn spawn(cmd: &[String]) -> std::io::Result<(crate::os::PluginChild, crate::os::Group)> {
    let (exe, args) = cmd
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty plugin command"))?;
    crate::os::spawn_plugin(&zora_core::config::expand_tilde(exe), args)
}

/// Forwards plugin stderr (metadata-only by contract) in whole lines, at roughly 4 KiB/s at most.
async fn forward_stderr(id: String, mut err: impl AsyncRead + Unpin) {
    let mut buf = [0u8; 4096];
    let mut lines = StderrLines::default();
    loop {
        let n = match err.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for line in lines.push(&buf[..n], tokio::time::Instant::now().into_std()) {
            eprintln!("[{id}] {line}");
        }
    }
    if let Some(line) = lines.finish() {
        eprintln!("[{id}] {line}");
    }
}

/// Splits a byte stream into lines, however the writes were cut. A line over `MAX_STDERR_LINE` is cut there and the
/// rest of it dropped; lines beyond the per-second budget are dropped whole.
#[derive(Default)]
struct StderrLines {
    pending: Vec<u8>,
    /// Inside an overlong line, dropping bytes until its newline.
    skipping: bool,
    budget: usize,
    window: Option<std::time::Instant>,
}

const MAX_STDERR_LINE: usize = 1024;
const BUDGET_PER_SECOND: usize = 4096;
const LINE_OVERHEAD: usize = 16;

impl StderrLines {
    fn push(&mut self, bytes: &[u8], now: std::time::Instant) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if !std::mem::take(&mut self.skipping) {
                    self.emit(now, &mut out);
                }
            } else if !self.skipping {
                self.pending.push(b);
                if self.pending.len() >= MAX_STDERR_LINE {
                    self.emit(now, &mut out);
                    self.skipping = true;
                }
            }
        }
        out
    }

    /// A last line without a newline.
    fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            self.emit(std::time::Instant::now(), &mut out);
        }
        out.pop()
    }

    fn emit(&mut self, now: std::time::Instant, out: &mut Vec<String>) {
        let line = std::mem::take(&mut self.pending);
        if self
            .window
            .is_none_or(|w| now.duration_since(w) >= Duration::from_secs(1))
        {
            self.window = Some(now);
            self.budget = BUDGET_PER_SECOND;
        }
        // Every line costs at least its prefix and newline, so a flood of empty lines is limited too.
        let cost = line.len() + LINE_OVERHEAD;
        if cost <= self.budget {
            self.budget -= cost;
            out.push(String::from_utf8_lossy(&line).into_owned());
        }
    }
}

async fn read_message(
    lines: &mut FramedRead<impl AsyncRead + Unpin, LinesCodec>,
    wait: Duration,
) -> Result<FromPlugin, ProtocolError> {
    match tokio::time::timeout(wait, lines.next()).await {
        Err(_) => Err(ProtocolError("timed out waiting for plugin".into())),
        Ok(None) => Err(ProtocolError("plugin closed its output".into())),
        Ok(Some(Err(e))) => Err(ProtocolError(format!("bad frame: {e}"))),
        Ok(Some(Ok(l))) => {
            serde_json::from_str(&l).map_err(|_| ProtocolError("malformed message".into()))
        }
    }
}

struct Writer(mpsc::Sender<String>);

impl Writer {
    /// Bounded, so a plugin that stops reading can't stall the session (and with it pause enforcement).
    async fn send(&self, msg: &ToPlugin) -> Result<(), ProtocolError> {
        match tokio::time::timeout(SEND_TIMEOUT, self.0.send(msg.line())).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ProtocolError("plugin input closed".into())),
            Err(_) => Err(ProtocolError("plugin is not reading its input".into())),
        }
    }
}

/// The epoch and message for a new listening state. Transitions the plugin never saw are skipped, not replayed:
/// the epoch jumps past them, which invalidates everything before, and only the latest state is sent.
fn listening_update(sent: &ListenState, next: &ListenState, epoch: u64) -> (u64, ToPlugin) {
    let epoch = epoch + next.turn.saturating_sub(sent.turn);
    let msg = ToPlugin::Listening {
        listening: next.listen.listening,
        until: next.listen.until,
        epoch,
        confirmed: next.confirmed,
    };
    (epoch, msg)
}

async fn session(
    spec: &PluginSpec,
    generation: u64,
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: &InputQueue<Item>,
    stop: tokio::sync::watch::Receiver<bool>,
    may_record: &mut bool,
) -> Result<(), ProtocolError> {
    let (tx, mut rx) = mpsc::channel::<String>(64);
    // A failed write is a broken connection: the reader side must not stay half-open.
    let (broken_tx, broken_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                let _ = broken_tx.send(());
                return;
            }
        }
    });
    let out = Writer(tx.clone());
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_LINE));
    // Handshake and clock check.
    let t_send = now_ns();
    out.send(&ToPlugin::Hello {
        protocol: PROTOCOL,
        generation,
        clock: CLOCK_DOMAIN,
        now_ns: t_send,
        config: spec.config.clone(),
    })
    .await?;
    let FromPlugin::Ready {
        protocol,
        generation: g,
        capabilities,
        clock,
        now_ns: plugin_now,
    } = read_message(&mut lines, Duration::from_secs(15)).await?
    else {
        return Err(ProtocolError("expected ready".into()));
    };
    let t_recv = now_ns();
    if protocol != PROTOCOL || g != generation || clock != CLOCK_DOMAIN {
        return Err(ProtocolError(format!(
            "incompatible plugin (protocol {protocol}, clock {clock})"
        )));
    }
    if let Some(allowed) = &spec.allowed
        && let Some(c) = capabilities.iter().find(|c| !allowed.contains(&c.as_str()))
    {
        return Err(ProtocolError(format!(
            "capability {c} is not allowed for this plugin"
        )));
    }
    let mut last_seq = 0u64;
    let in_window = |p: Ns, s: Ns, r: Ns| p >= s - 5 * MILLI && p <= r + 5 * MILLI;
    if !in_window(plugin_now, t_send, t_recv) {
        let mut ok = false;
        for id in 1..=3u64 {
            let s = now_ns();
            out.send(&ToPlugin::Clock { id }).await?;
            if let FromPlugin::Clock {
                seq,
                id: rid,
                now_ns: p,
                ..
            } = read_message(&mut lines, Duration::from_secs(2)).await?
            {
                if seq != last_seq + 1 {
                    return Err(ProtocolError("sequence violation".into()));
                }
                last_seq = seq;
                if rid == id && in_window(p, s, now_ns()) {
                    ok = true;
                    break;
                }
            }
        }
        if !ok {
            return Err(ProtocolError("clock skew check failed".into()));
        }
    }

    let has = |c: &str| capabilities.iter().any(|x| x == c);
    let (transcript, dictation) = (has("transcript"), has("dictation"));
    *may_record = transcript;
    let mut state = Conn {
        healthy: false,
        capturing: false,
        stopped: false,
        last_health_at: now_ns(),
    };
    let result = connected(
        spec,
        generation,
        out,
        capabilities.clone(),
        tx,
        transcript,
        dictation,
        last_seq,
        &mut lines,
        queue,
        &mut state,
        stop,
        broken_rx,
    )
    .await;

    // Every post-handshake exit, including queue overflow, fails closed here. These signals bypass the caps
    // and are queued before PluginDown, so suppression loses coverage before the replacement connects.
    if transcript {
        queue.force(Item::Core(Input::Gap));
    }
    if dictation {
        if state.capturing {
            queue.force(Item::Core(Input::Dictation(DictationSignal::Ended(
                now_ns(),
            ))));
        }
        queue.force(Item::Core(Input::Dictation(DictationSignal::Health {
            healthy: false,
            at: state.last_health_at,
        })));
    }
    // EOF after an orderly stop is the normal end; anything else is a failure to restart from.
    let eof = matches!(&result, Err(ProtocolError(r)) if r == "plugin closed its output");
    if eof && state.stopped { Ok(()) } else { result }
}

struct Conn {
    healthy: bool,
    /// The plugin's last reported capture state; reconciled into began/ended so a dictation already running
    /// when this connection started is still suppressed.
    capturing: bool,
    stopped: bool,
    last_health_at: Ns,
}

fn ts_ok(t: Ns, now: Ns) -> bool {
    t >= now.saturating_sub(15 * 60 * SECOND) && t <= now.saturating_add(SECOND)
}

#[allow(clippy::too_many_arguments)]
async fn connected(
    spec: &PluginSpec,
    generation: u64,
    out: Writer,
    capabilities: Vec<String>,
    tx: mpsc::Sender<String>,
    transcript: bool,
    dictation: bool,
    mut last_seq: u64,
    lines: &mut FramedRead<impl AsyncRead + Unpin, LinesCodec>,
    queue: &InputQueue<Item>,
    st: &mut Conn,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut broken: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), ProtocolError> {
    let attached = matches!(spec.connection, Connection::Attached);
    let listening_cap = capabilities.iter().any(|c| c == "listening");
    // A transcript plugin that can't confirm a pause doesn't run while paused (an attached one is only reported).
    let stop_when_paused = transcript && !listening_cap && !attached;
    let control = |item: Item| {
        queue
            .send(item)
            .map_err(|_| ProtocolError("control queue overflow".into()))
    };
    let paused_out = || {
        control(Item::Notice(format!(
            "plugin {} can't confirm its microphone is off, so it is stopped while paused",
            spec.id
        )))?;
        Err(ProtocolError(PAUSED_OUT.into()))
    };
    let mut listen = spec.listen.clone();
    let mut sent = *listen.borrow_and_update();
    if stop_when_paused && !sent.listen.listening {
        return paused_out();
    }
    // The listening epoch of this connection (docs/protocol.md): 1 in `start`, +1 per on/off transition.
    let mut epoch = 1u64;
    // The plugin reported its microphone closed in the current epoch.
    let mut mic_closed = false;
    out.send(&ToPlugin::Start {
        listening: sent.listen.listening,
        until: sent.listen.until,
        epoch,
    })
    .await?;
    if sent.confirmed.is_some() {
        out.send(&ToPlugin::Listening {
            listening: sent.listen.listening,
            until: sent.listen.until,
            epoch,
            confirmed: sent.confirmed,
        })
        .await?;
    }
    let mut listen_open = true;
    let mut pending_listen: Option<ListenState> = None;
    // When a spawned transcript plugin is stopped unless it has confirmed the current pause.
    let enforce = transcript && !attached;
    let mut stop_at =
        (enforce && !sent.listen.listening).then(|| tokio::time::Instant::now() + PAUSE_STOP);
    control(Item::PluginUp {
        id: spec.id.clone(),
        generation,
        capabilities,
        tx,
    })?;
    if transcript {
        control(Item::Core(Input::TranscriptGeneration(generation)))?;
    }
    let mut lease = tokio::time::Instant::now() + Duration::from_nanos(HEARTBEAT_LEASE as u64);
    loop {
        if let Some(next) = pending_listen.take() {
            let (new_epoch, msg) = listening_update(&sent, &next, epoch);
            if new_epoch != epoch {
                mic_closed = false;
                // The deadline runs from here, whether or not the plugin takes the message.
                stop_at = (enforce && !next.listen.listening)
                    .then(|| tokio::time::Instant::now() + PAUSE_STOP);
            }
            epoch = new_epoch;
            sent = next;
            if let Some(at) = stop_at {
                match tokio::time::timeout_at(at, out.send(&msg)).await {
                    Ok(r) => r?,
                    Err(_) => return Err(ProtocolError(PAUSED_OUT.into())),
                }
            } else {
                out.send(&msg).await?;
            }
            if !sent.listen.listening {
                if stop_when_paused {
                    return paused_out();
                }
                // The core gave up waiting (and notified the user); a spawned plugin is stopped.
                if transcript && sent.confirmed == Some(false) && !mic_closed && !attached {
                    return Err(ProtocolError(PAUSED_OUT.into()));
                }
            }
        }
        let msg = tokio::select! {
            m = read_message(lines, Duration::from_secs(24 * 3600)) => m?,
            _ = stop.wait_for(|s| *s) => return Err(ProtocolError("service shutdown".into())),
            _ = &mut broken => return Err(ProtocolError("plugin input closed".into())),
            _ = tokio::time::sleep_until(stop_at.unwrap_or_else(tokio::time::Instant::now)), if stop_at.is_some() => {
                // Not confirmed in time; the service has reported it.
                return Err(ProtocolError(PAUSED_OUT.into()));
            }
            changed = listen.changed(), if listen_open => {
                if changed.is_err() {
                    listen_open = false;
                    continue;
                }
                // Sent at the top of the loop: nothing may be awaited while the select output is alive.
                pending_listen = Some(*listen.borrow_and_update());
                continue;
            }
            _ = tokio::time::sleep_until(lease), if dictation && st.healthy => {
                // Missed heartbeats: suppression state is unknown from the last confirmed observation, and the
                // core closes any open dictation, so a later healthy report must reopen it.
                st.healthy = false;
                st.capturing = false;
                control(Item::Core(Input::Dictation(DictationSignal::Health { healthy: false, at: st.last_health_at })))?;
                continue;
            }
        };
        let (seq, g) = match &msg {
            FromPlugin::Ready { .. } => return Err(ProtocolError("duplicate ready".into())),
            FromPlugin::Segment {
                seq, generation, ..
            }
            | FromPlugin::Gap {
                seq, generation, ..
            }
            | FromPlugin::Partial {
                seq, generation, ..
            }
            | FromPlugin::Mic {
                seq, generation, ..
            }
            | FromPlugin::Status {
                seq, generation, ..
            }
            | FromPlugin::Dictation {
                seq, generation, ..
            }
            | FromPlugin::Health {
                seq, generation, ..
            }
            | FromPlugin::Unknown {
                seq, generation, ..
            }
            | FromPlugin::Synced {
                seq, generation, ..
            }
            | FromPlugin::Ack {
                seq, generation, ..
            }
            | FromPlugin::Clock {
                seq, generation, ..
            }
            | FromPlugin::Stopped { seq, generation } => (*seq, *generation),
        };
        if g != generation {
            return Err(ProtocolError("wrong generation".into()));
        }
        // Exactly the next number: a missing message could have been a dictation event.
        if Some(seq) != last_seq.checked_add(1) {
            return Err(ProtocolError("sequence violation".into()));
        }
        last_seq = seq;
        let now = now_ns();
        match msg {
            FromPlugin::Segment {
                session,
                start_ns,
                end_ns,
                text,
                ..
            } if transcript => {
                if end_ns < start_ns || !ts_ok(start_ns, now) || !ts_ok(end_ns, now) {
                    return Err(ProtocolError("segment timestamps out of range".into()));
                }
                // Segments may be dropped under load; the queue leaves a loss marker in their place.
                let _ = queue.send(Item::Core(Input::Segment(Segment {
                    generation,
                    session,
                    start: start_ns,
                    end: end_ns,
                    text,
                })));
            }
            FromPlugin::Partial { text, epoch: e, .. } if transcript => {
                // UI-only early wake hint; droppable under load, never part of command logic. Text heard in an
                // earlier epoch (or not tagged with one) may be from before a pause.
                if e != Some(epoch) || !sent.listen.listening {
                    continue;
                }
                let tail: String = text
                    .chars()
                    .rev()
                    .take(512)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let _ = queue.send(Item::Partial {
                    text: tail,
                    turn: sent.turn,
                });
            }
            FromPlugin::Mic { epoch: e, open, .. } if listening_cap => {
                if e == 0 || e > epoch {
                    return Err(ProtocolError("mic epoch out of range".into()));
                }
                if e == epoch {
                    control(Item::Mic {
                        id: spec.id.clone(),
                        turn: sent.turn,
                        open,
                    })?;
                    if !sent.listen.listening {
                        mic_closed = !open;
                        if mic_closed {
                            stop_at = None;
                        }
                        if open {
                            control(Item::Notice(format!(
                                "plugin {} opened its microphone while paused",
                                spec.id
                            )))?;
                            if !attached {
                                return Err(ProtocolError(PAUSED_OUT.into()));
                            }
                        }
                    }
                }
            }
            FromPlugin::Gap { at_ns, .. } if transcript => {
                if !ts_ok(at_ns, now) {
                    return Err(ProtocolError("gap timestamp out of range".into()));
                }
                control(Item::Core(Input::Gap))?;
            }
            FromPlugin::Status { ready, reason, .. } => {
                if !ready {
                    let reason: String = reason.chars().take(256).collect();
                    control(Item::Notice(format!("plugin {}: {reason}", spec.id)))?;
                }
            }
            FromPlugin::Dictation { state, at_ns, .. } if dictation => {
                if !ts_ok(at_ns, now) {
                    return Err(ProtocolError("dictation timestamp out of range".into()));
                }
                match state.as_str() {
                    "began" => {
                        st.capturing = true;
                        control(Item::Core(Input::Dictation(DictationSignal::Began(at_ns))))?;
                    }
                    "ended" => {
                        st.capturing = false;
                        control(Item::Core(Input::Dictation(DictationSignal::Ended(at_ns))))?;
                    }
                    _ => return Err(ProtocolError("unknown dictation state".into())),
                }
            }
            FromPlugin::Health {
                healthy: h,
                capturing,
                at_ns,
                ..
            } if dictation => {
                if !ts_ok(at_ns, now) {
                    return Err(ProtocolError("health timestamp out of range".into()));
                }
                lease = tokio::time::Instant::now() + Duration::from_nanos(HEARTBEAT_LEASE as u64);
                st.last_health_at = at_ns;
                if !h {
                    if st.healthy {
                        // The core closes any open dictation on health loss; forget ours so recovery reopens it.
                        st.healthy = false;
                        st.capturing = false;
                        control(Item::Core(Input::Dictation(DictationSignal::Health {
                            healthy: false,
                            at: at_ns,
                        })))?;
                    }
                } else {
                    // Reconcile the capture state first, so coverage never starts during a dictation.
                    if capturing != st.capturing {
                        st.capturing = capturing;
                        let signal = if capturing {
                            DictationSignal::Began(at_ns)
                        } else {
                            DictationSignal::Ended(at_ns)
                        };
                        control(Item::Core(Input::Dictation(signal)))?;
                    }
                    if !st.healthy {
                        st.healthy = true;
                        control(Item::Core(Input::Dictation(DictationSignal::Health {
                            healthy: true,
                            at: at_ns,
                        })))?;
                    }
                }
            }
            FromPlugin::Unknown { from_ns, to_ns, .. } if dictation => {
                if to_ns < from_ns || !ts_ok(from_ns, now) || !ts_ok(to_ns, now) {
                    return Err(ProtocolError("unknown interval out of range".into()));
                }
                control(Item::Core(Input::Dictation(DictationSignal::Unknown {
                    from: from_ns,
                    to: to_ns,
                })))?;
            }
            FromPlugin::Synced {
                id,
                at_ns,
                healthy: h,
                capturing,
                ..
            } if dictation => {
                if !ts_ok(at_ns, now) {
                    return Err(ProtocolError("sync timestamp out of range".into()));
                }
                if h && capturing != st.capturing {
                    st.capturing = capturing;
                    let signal = if capturing {
                        DictationSignal::Began(at_ns)
                    } else {
                        DictationSignal::Ended(at_ns)
                    };
                    control(Item::Core(Input::Dictation(signal)))?;
                }
                control(Item::Core(Input::Synced {
                    id,
                    at: at_ns,
                    healthy: h && st.healthy,
                    capturing,
                }))?;
            }
            FromPlugin::Ack { .. } => control(Item::Ack {
                id: spec.id.clone(),
            })?,
            FromPlugin::Clock { .. } => {}
            FromPlugin::Stopped { .. } => {
                st.stopped = true;
                control(Item::PluginStopped {
                    id: spec.id.clone(),
                })?;
                // Keep reading until EOF so late final segments still arrive.
            }
            _ => {
                return Err(ProtocolError(
                    "message for a capability the plugin did not declare".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod listening_tests {
    use super::*;
    use crate::pause::Listen;

    fn state(listening: bool, turn: u64) -> ListenState {
        ListenState {
            listen: Listen {
                listening,
                until: None,
            },
            turn,
            confirmed: None,
        }
    }

    fn sent(msg: ToPlugin) -> (bool, u64) {
        match msg {
            ToPlugin::Listening {
                listening, epoch, ..
            } => (listening, epoch),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missed_resume_is_never_replayed() {
        // Paused at turn 1 (epoch 2); the host missed the resume (turn 2) and sees the next pause (turn 3).
        let (epoch, msg) = listening_update(&state(false, 1), &state(false, 3), 2);
        assert_eq!(epoch, 4);
        assert_eq!(sent(msg), (false, 4));
        // Listening, missed a pause: the plugin gets a new epoch, which invalidates what it heard before.
        let (_, msg) = listening_update(&state(true, 0), &state(true, 2), 1);
        assert_eq!(sent(msg), (true, 3));
        // A new `until` or a confirmation keeps the epoch.
        let mut confirmed = state(false, 1);
        confirmed.confirmed = Some(true);
        let (epoch, _) = listening_update(&state(false, 1), &confirmed, 2);
        assert_eq!(epoch, 2);
    }
}

#[cfg(test)]
mod stderr_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_line_written_in_pieces_comes_out_whole() {
        let mut l = StderrLines::default();
        let t = Instant::now();
        assert!(l.push(b"notify kind=", t).is_empty());
        assert_eq!(
            l.push(b"jobSucceeded\nnext", t),
            vec!["notify kind=jobSucceeded"]
        );
        assert_eq!(l.finish().as_deref(), Some("next"));
    }

    #[test]
    fn utf8_split_across_writes_survives() {
        let mut l = StderrLines::default();
        let t = Instant::now();
        let s = "zażółć\n".as_bytes();
        assert!(l.push(&s[..3], t).is_empty());
        assert_eq!(l.push(&s[3..], t), vec!["zażółć"]);
    }

    #[test]
    fn an_overlong_line_is_cut_once_and_its_rest_dropped() {
        let mut l = StderrLines::default();
        let t = Instant::now();
        let long = vec![b'a'; MAX_STDERR_LINE * 3];
        let out = l.push(&long, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_STDERR_LINE);
        assert_eq!(l.push(b"aaa\nok\n", t), vec!["ok"]);
    }

    #[test]
    fn lines_over_the_budget_are_dropped_whole_until_the_next_second() {
        let mut l = StderrLines::default();
        let t = Instant::now();
        let line = [vec![b'x'; 1008], vec![b'\n']].concat();
        let out = l.push(&line.repeat(5), t);
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|s| s.len() == 1008));
        assert!(l.push(&[b'\n'; 100], t).is_empty());
        assert_eq!(l.push(&line, t + Duration::from_secs(1)).len(), 1);
    }

    #[test]
    fn empty_lines_are_limited_too() {
        let mut l = StderrLines::default();
        let out = l.push(&[b'\n'; 10_000], Instant::now());
        assert_eq!(out.len(), BUDGET_PER_SECOND / LINE_OVERHEAD);
    }
}

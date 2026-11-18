//! Plugin host: implements docs/protocol.md for one plugin (spawned child on Linux, attached fds on macOS).

use crate::pause::Listen;
use crate::queue::{Class, Classify, InputQueue};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Stdio;
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
    },
    /// Listening was paused or resumed. Plugins with `transcript` release or reopen the microphone.
    Listening {
        listening: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<f64>,
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
    },
    PluginStopped {
        id: String,
    },
    Ack {
        id: String,
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
            Item::Core(Input::Tick) | Item::Core(Input::Partial { .. }) => Class::Tick,
            _ => Class::Control,
        }
    }
    fn cost(&self) -> usize {
        match self {
            Item::Core(Input::Segment(s)) => s.text.len() + 64,
            Item::Notice(t) => t.len() + 64,
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
    /// macOS inverted launch: the app gave us fd 3 (read) and fd 4 (write).
    Attached,
}

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub id: String,
    pub connection: Connection,
    pub config: Value,
    /// Listening on/off, forwarded to the plugin as `start` and `listening` messages.
    pub listen: tokio::sync::watch::Receiver<Listen>,
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
    loop {
        generation += 1;
        let started = tokio::time::Instant::now();
        let result = match &spec.connection {
            Connection::Attached => {
                // SAFETY: the parent app passes these descriptors to us; nothing else owns them.
                let (r, w) = unsafe { (OwnedFd::from_raw_fd(3), OwnedFd::from_raw_fd(4)) };
                let r = tokio::net::unix::pipe::Receiver::from_owned_fd(r);
                let w = tokio::net::unix::pipe::Sender::from_owned_fd(w);
                match (r, w) {
                    (Ok(r), Ok(w)) => {
                        until_stop(
                            session(&spec, generation, r, w, &queue, stop.clone()),
                            stop.clone(),
                        )
                        .await
                    }
                    _ => Err(ProtocolError("fds 3/4 are not pipes".into())),
                }
            }
            Connection::Spawn(cmd) => match spawn(cmd) {
                Ok(mut child) => {
                    let (r, w) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
                    if let Some(err) = child.stderr.take() {
                        tokio::spawn(forward_stderr(spec.id.clone(), err));
                    }
                    // Shutdown is observed in every phase (negotiation, clock probes, running); cancelling the
                    // session drops it and falls through to the group cleanup below.
                    let res = until_stop(
                        session(&spec, generation, r, w, &queue, stop.clone()),
                        stop.clone(),
                    )
                    .await;
                    // Reap the whole plugin group (TERM, grace, KILL, verify) before any restart.
                    if let Some(pid) = child.id() {
                        let pgid = pid as i32;
                        // Wait for the leader first: an unreaped (zombie) leader still counts as a group member.
                        crate::os::signal_group(pgid, libc::SIGTERM);
                        if tokio::time::timeout(Duration::from_secs(3), child.wait())
                            .await
                            .is_err()
                        {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                        }
                        let reaped = tokio::task::spawn_blocking(move || {
                            crate::os::reap_group_blocking(pgid, Duration::from_secs(3))
                        })
                        .await
                        .unwrap_or(false);
                        if !reaped {
                            eprintln!("[zorad] plugin {} left processes in group {pgid}", spec.id);
                        }
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    res
                }
                Err(e) => Err(ProtocolError(format!("cannot start: {e}"))),
            },
        };
        let _ = queue.send(Item::PluginDown {
            id: spec.id.clone(),
            generation,
        });
        if *stop.borrow() {
            return; // service shutdown: the group above is already reaped
        }
        match result {
            Ok(()) => return, // orderly stop
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

async fn until_stop(
    session: impl std::future::Future<Output = Result<(), ProtocolError>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), ProtocolError> {
    tokio::select! {
        r = session => r,
        _ = stop.wait_for(|s| *s) => Err(ProtocolError("service shutdown".into())),
    }
}

fn spawn(cmd: &[String]) -> std::io::Result<tokio::process::Child> {
    let (exe, args) = cmd
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty plugin command"))?;
    tokio::process::Command::new(zora_core::config::expand_tilde(exe))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
}

/// Forwards plugin stderr (metadata-only by contract) at no more than 4 KiB/s.
async fn forward_stderr(id: String, mut err: impl AsyncRead + Unpin) {
    let mut buf = [0u8; 4096];
    let mut budget = 4096usize;
    let mut window = tokio::time::Instant::now();
    loop {
        let n = match err.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if window.elapsed() >= Duration::from_secs(1) {
            window = tokio::time::Instant::now();
            budget = 4096;
        }
        let take = n.min(budget);
        budget -= take;
        if take > 0 {
            let text = String::from_utf8_lossy(&buf[..take]);
            for line in text.lines() {
                eprintln!("[{id}] {line}");
            }
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
    async fn send(&self, msg: &ToPlugin) -> Result<(), ProtocolError> {
        self.0
            .send(msg.line())
            .await
            .map_err(|_| ProtocolError("plugin input closed".into()))
    }
}

async fn session(
    spec: &PluginSpec,
    generation: u64,
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: &InputQueue<Item>,
    stop: tokio::sync::watch::Receiver<bool>,
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
    let mut listen = spec.listen.clone();
    let initial = *listen.borrow_and_update();
    out.send(&ToPlugin::Start {
        listening: initial.listening,
        until: initial.until,
    })
    .await?;
    let mut listen_open = true;
    let mut pending_listen: Option<Listen> = None;
    let control = |item: Item| {
        queue
            .send(item)
            .map_err(|_| ProtocolError("control queue overflow".into()))
    };
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
        if let Some(l) = pending_listen.take() {
            out.send(&ToPlugin::Listening {
                listening: l.listening,
                until: l.until,
            })
            .await?;
        }
        let msg = tokio::select! {
            m = read_message(lines, Duration::from_secs(24 * 3600)) => m?,
            _ = stop.wait_for(|s| *s) => return Err(ProtocolError("service shutdown".into())),
            _ = &mut broken => return Err(ProtocolError("plugin input closed".into())),
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
            FromPlugin::Partial { text, .. } if transcript => {
                // UI-only early wake hint; droppable under load, never part of command logic.
                let tail: String = text
                    .chars()
                    .rev()
                    .take(512)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let _ = queue.send(Item::Core(Input::Partial { text: tail }));
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

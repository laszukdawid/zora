//! Dictation monitor on PipeWire: binds every node, tracks the capture streams (`Stream/Input/Audio`) whose
//! application properties name a configured dictation app, and reports when one starts or stops running.
//!
//! Capture edges are sent as node updates arrive. A snapshot is complete when a core `sync` round trip returns
//! and every bound node has delivered its first info; until then a round trip is repeated. The first snapshot,
//! the 1 s heartbeat and every `sync` from the core use that rule. Everything runs on one PipeWire main loop.

use pipewire as pw;
use pw::node::{Node, NodeChangeMask, NodeListener, NodeState};
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use zora_plugin::clock::{Ns, SECOND, now_ns};
use zora_plugin::dictation::{APP_KEYS, Monitor, Msg, StreamNode};
use zora_plugin::proto::Out;

/// From the stdin thread into the loop.
pub enum Req {
    Sync(u64),
    Quit,
}

#[derive(Clone, Copy)]
enum Purpose {
    Tick,
    Sync(u64),
}

/// A round trip unanswered this long means the server is stuck: reconnect.
const ROUND_TRIP_LIMIT: Ns = 3 * SECOND;
/// How long an incomplete snapshot is retried before it counts as no observation.
const RETRY_WINDOW: Ns = SECOND / 2;

struct Shared {
    monitor: Monitor,
    apps: Vec<String>,
    streams: HashMap<u32, StreamNode>,
    /// Nodes bound (or that failed to bind) whose first info has not arrived; a snapshot is incomplete meanwhile.
    awaiting: HashSet<u32>,
    pending: HashMap<i32, (Purpose, Ns)>,
    tick_outstanding: bool,
    core: Option<pw::core::CoreRc>,
    /// Set the moment the connection fails; nothing it still delivers counts.
    broken: Option<String>,
    out: Arc<Out>,
    /// `zora-dictation watch`: print capture streams instead of judging them.
    watch: bool,
}

impl Shared {
    fn send(&self, msgs: Vec<Msg>) {
        for m in msgs {
            self.out.send(m.to_json());
        }
    }

    fn capturing(&self) -> bool {
        self.streams.values().any(StreamNode::dictating)
    }

    fn request(&mut self, purpose: Purpose, issued: Ns) -> Result<(), String> {
        let core = self.core.as_ref().ok_or("not connected")?;
        let seq = core.sync(0).map_err(|e| e.to_string())?;
        if matches!(purpose, Purpose::Tick) {
            self.tick_outstanding = true;
        }
        self.pending.insert(seq.seq(), (purpose, issued));
        Ok(())
    }

    /// The connection can't be trusted any more: health unknown now, and every waiting `sync` answered so.
    fn fail(&mut self, reason: &str, now: Ns) {
        if self.broken.is_some() {
            return;
        }
        eprintln!("zora-dictation: {reason}");
        self.broken = Some(reason.to_string());
        let mut msgs = self.monitor.lost(reason, now);
        for (purpose, _) in self.pending.drain().map(|(_, p)| p) {
            if let Purpose::Sync(id) = purpose {
                msgs.push(self.monitor.synced(id, now));
            }
        }
        self.tick_outstanding = false;
        self.send(msgs);
    }

    fn capture_changed(&mut self, now: Ns) {
        let capturing = self.capturing();
        let msgs = self.monitor.changed(capturing, now);
        self.send(msgs);
    }

    fn done(&mut self, seq: i32, now: Ns) {
        if self.broken.is_some() {
            return;
        }
        let Some((purpose, issued)) = self.pending.remove(&seq) else {
            return;
        };
        if !self.awaiting.is_empty() {
            // Some node's first info is still on its way: ask again for a short while, then count it as no
            // observation (the stall rule turns that into unknown) and answer a waiting `sync` unhealthy.
            if now - issued < RETRY_WINDOW {
                if let Err(e) = self.request(purpose, issued) {
                    self.fail(&format!("PipeWire round trip failed: {e}"), now);
                }
                return;
            }
            match purpose {
                Purpose::Tick => self.tick_outstanding = false,
                Purpose::Sync(id) => {
                    let mut msgs = self.monitor.lost("PipeWire nodes without state", now);
                    msgs.push(self.monitor.synced(id, now));
                    self.send(msgs);
                }
            }
            return;
        }
        let capturing = self.capturing();
        let mut msgs = self.monitor.observe(capturing, now);
        match purpose {
            Purpose::Tick => self.tick_outstanding = false,
            Purpose::Sync(id) => msgs.push(self.monitor.synced(id, now)),
        }
        self.send(msgs);
    }
}

struct Conn {
    _core: pw::core::CoreRc,
    _registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    _registry_listener: pw::registry::Listener,
    _nodes: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>>,
}

const NOT_RUNNING: &str = "cannot connect to PipeWire (PulseAudio or ALSA alone is not supported)";

fn describe(id: u32, n: &StreamNode) -> String {
    let props: Vec<String> = APP_KEYS
        .iter()
        .filter_map(|k| n.prop(k).map(|v| format!("{k}={v:?}")))
        .collect();
    let state = if n.running { "running" } else { "not running" };
    format!("stream {id} {state}: {}", props.join(" "))
}

fn connect(context: &pw::context::ContextRc, shared: &Rc<RefCell<Shared>>) -> Result<Conn, String> {
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let nodes: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>> = Rc::default();

    let core_listener = {
        let (s1, s2) = (Rc::downgrade(shared), Rc::downgrade(shared));
        core.add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE
                    && let Some(shared) = s1.upgrade()
                {
                    shared.borrow_mut().done(seq.seq(), now_ns());
                }
            })
            .error(move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    if let Some(shared) = s2.upgrade() {
                        shared.borrow_mut().fail(
                            &format!("PipeWire connection lost ({res} {message})"),
                            now_ns(),
                        );
                    }
                } else {
                    // A node that vanished while being bound; it stays incomplete until its removal arrives.
                    eprintln!("zora-dictation: PipeWire error on proxy {id}: {res} {message}");
                }
            })
            .register()
    };

    let registry_listener = {
        let reg = registry.downgrade();
        let (n1, n2) = (Rc::downgrade(&nodes), Rc::downgrade(&nodes));
        let (s1, s2) = (Rc::downgrade(shared), Rc::downgrade(shared));
        registry
            .add_listener_local()
            .global(move |obj| {
                if obj.type_ != ObjectType::Node {
                    return;
                }
                let (Some(reg), Some(nodes), Some(shared)) =
                    (reg.upgrade(), n1.upgrade(), s1.upgrade())
                else {
                    return;
                };
                let id = obj.id;
                shared.borrow_mut().awaiting.insert(id);
                let Ok(node) = reg.bind::<Node, _>(obj) else {
                    eprintln!("zora-dictation: cannot bind node {id}");
                    return;
                };
                let s = s1.clone();
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        let Some(shared) = s.upgrade() else { return };
                        let sh = &mut *shared.borrow_mut();
                        if sh.broken.is_some() {
                            return;
                        }
                        let first = sh.awaiting.remove(&id);
                        let apps = sh.apps.clone();
                        let entry = sh.streams.entry(id).or_default();
                        let before = (entry.running, entry.prop(APP_KEYS[0]).map(String::from));
                        if (first || info.change_mask().contains(NodeChangeMask::PROPS))
                            && let Some(props) = info.props()
                        {
                            entry.set_props(props.iter(), &apps);
                        }
                        entry.running = matches!(info.state(), NodeState::Running);
                        if sh.watch && entry.is_capture() {
                            let after = (entry.running, entry.prop(APP_KEYS[0]).map(String::from));
                            if first || before != after {
                                println!("{}", describe(id, entry));
                            }
                        }
                        sh.capture_changed(now_ns());
                    })
                    .register();
                nodes.borrow_mut().insert(id, (node, listener));
            })
            .global_remove(move |id| {
                if let Some(nodes) = n2.upgrade() {
                    nodes.borrow_mut().remove(&id);
                }
                if let Some(shared) = s2.upgrade() {
                    let sh = &mut *shared.borrow_mut();
                    sh.awaiting.remove(&id);
                    if let Some(n) = sh.streams.remove(&id)
                        && sh.watch
                        && n.is_capture()
                    {
                        println!("stream {id} removed");
                    }
                    if sh.broken.is_none() {
                        sh.capture_changed(now_ns());
                    }
                }
            })
            .register()
    };

    shared.borrow_mut().core = Some(core.clone());
    let conn = Conn {
        _core: core,
        _registry: registry,
        _core_listener: core_listener,
        _registry_listener: registry_listener,
        _nodes: nodes,
    };
    shared.borrow_mut().request(Purpose::Tick, now_ns())?;
    Ok(conn)
}

/// Runs until `Req::Quit`. With `watch`, prints every capture stream's identity and state as it changes.
pub fn run(
    apps: Vec<String>,
    out: Arc<Out>,
    rx: pw::channel::Receiver<Req>,
    watch: bool,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let shared = Rc::new(RefCell::new(Shared {
        monitor: Monitor::new(!apps.is_empty()),
        apps,
        streams: HashMap::new(),
        awaiting: HashSet::new(),
        pending: HashMap::new(),
        tick_outstanding: false,
        core: None,
        broken: None,
        out,
        watch,
    }));
    let conn: Rc<RefCell<Option<Conn>>> = Rc::default();

    let reconnect = {
        let (shared, conn, context) = (shared.clone(), conn.clone(), context.clone());
        move || {
            let now = now_ns();
            let mut c = conn.borrow_mut();
            if c.is_some() && shared.borrow().broken.is_some() {
                *c = None;
                let mut sh = shared.borrow_mut();
                sh.core = None;
                sh.streams.clear();
                sh.awaiting.clear();
                sh.pending.clear();
                sh.tick_outstanding = false;
            }
            if c.is_none() {
                shared.borrow_mut().broken = None;
                match connect(&context, &shared) {
                    Ok(new) => {
                        eprintln!("zora-dictation: connected to PipeWire");
                        *c = Some(new);
                    }
                    Err(e) => {
                        let sh = &mut *shared.borrow_mut();
                        sh.core = None;
                        let msgs = sh.monitor.lost(NOT_RUNNING, now);
                        if !msgs.is_empty() {
                            eprintln!("zora-dictation: {e}");
                        }
                        sh.send(msgs);
                    }
                }
            }
        }
    };
    reconnect();

    let timer = {
        let shared = shared.clone();
        let reconnect = reconnect.clone();
        mainloop.loop_().add_timer(move |_| {
            reconnect();
            let now = now_ns();
            let sh = &mut *shared.borrow_mut();
            if sh
                .pending
                .values()
                .any(|(_, at)| now - at > ROUND_TRIP_LIMIT)
            {
                sh.fail("PipeWire stopped answering", now);
            } else if sh.core.is_some()
                && sh.broken.is_none()
                && !sh.tick_outstanding
                && let Err(e) = sh.request(Purpose::Tick, now)
            {
                sh.fail(&format!("PipeWire round trip failed: {e}"), now);
            }
            let msgs = sh.monitor.tick(now);
            sh.send(msgs);
        })
    };
    timer
        .update_timer(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
        .into_result()
        .map_err(|e| e.to_string())?;

    let _rx = {
        let (shared, weak_loop) = (shared.clone(), mainloop.downgrade());
        rx.attach(mainloop.loop_(), move |req| match req {
            Req::Sync(id) => {
                let now = now_ns();
                let sh = &mut *shared.borrow_mut();
                let msgs = sh.monitor.stall_check(now);
                sh.send(msgs);
                let ok = sh.broken.is_none() && sh.request(Purpose::Sync(id), now).is_ok();
                if !ok {
                    let reason = sh.broken.clone().unwrap_or_else(|| NOT_RUNNING.into());
                    let mut msgs = sh.monitor.lost(&reason, now);
                    msgs.push(sh.monitor.synced(id, now));
                    sh.send(msgs);
                }
            }
            Req::Quit => {
                if let Some(l) = weak_loop.upgrade() {
                    l.quit();
                }
            }
        })
    };

    mainloop.run();
    Ok(())
}

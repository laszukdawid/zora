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
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use zora_plugin::clock::{Ns, SECOND, now_ns};
use zora_plugin::dictation::{APP_KEYS, Class, Graph, Msg, StreamNode};
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
    graph: Graph,
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
        // Recorders running now are held from this moment until the next connection shows them again.
        self.graph.clear(now);
        let mut msgs = self.graph.monitor.lost(reason, now);
        for (purpose, _) in self.pending.drain().map(|(_, p)| p) {
            if let Purpose::Sync(id) = purpose {
                msgs.push(self.graph.monitor.synced(id, now));
            }
        }
        self.tick_outstanding = false;
        self.send(msgs);
    }

    fn done(&mut self, seq: i32, now: Ns) {
        if self.broken.is_some() {
            return;
        }
        let Some((purpose, issued)) = self.pending.remove(&seq) else {
            return;
        };
        if !self.graph.complete() {
            // Some node's first info is still on its way: ask again for a short while, then count it as no
            // observation (the stall rule turns that into unknown) and answer a waiting `sync` unhealthy.
            if now - issued < RETRY_WINDOW {
                if let Err(e) = self.request(purpose, issued) {
                    // `fail` answers the syncs still pending; this one was already taken out.
                    self.fail(&format!("PipeWire round trip failed: {e}"), now);
                    if let Purpose::Sync(id) = purpose {
                        let reply = self.graph.monitor.synced(id, now);
                        self.send(vec![reply]);
                    }
                }
                return;
            }
            match purpose {
                Purpose::Tick => self.tick_outstanding = false,
                Purpose::Sync(id) => {
                    let mut msgs = self.graph.monitor.lost("PipeWire nodes without state", now);
                    msgs.push(self.graph.monitor.synced(id, now));
                    self.send(msgs);
                }
            }
            return;
        }
        let capturing = self.graph.capturing();
        let mut msgs = self.graph.monitor.observe(capturing, now);
        match purpose {
            Purpose::Tick => self.tick_outstanding = false,
            Purpose::Sync(id) => msgs.push(self.graph.monitor.synced(id, now)),
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

fn describe(id: u32, n: &StreamNode, class: Class) -> String {
    let props: Vec<String> = APP_KEYS
        .iter()
        .filter_map(|k| n.prop(k).map(|v| format!("{k}={v:?}")))
        .collect();
    let state = if n.running { "running" } else { "not running" };
    let kind = if class == Class::Unclassified {
        " (names no app)"
    } else {
        ""
    };
    format!("stream {id} {state}{kind}: {}", props.join(" "))
}

fn connect(context: &pw::context::ContextRc, shared: &Rc<RefCell<Shared>>) -> Result<Conn, String> {
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let nodes: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>> = Rc::default();

    let core_listener = {
        let (s1, s2, s3) = (
            Rc::downgrade(shared),
            Rc::downgrade(shared),
            Rc::downgrade(shared),
        );
        core.add_listener_local()
            .info(move |info| {
                // The cookie is random per server start, so it tells a restarted server from the same one.
                if let Some(shared) = s3.upgrade() {
                    let identity = format!("{}:{}", info.name(), info.cookie());
                    let sh = &mut *shared.borrow_mut();
                    let msgs = sh.graph.server(&identity, now_ns());
                    sh.send(msgs);
                }
            })
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE
                    && let Some(shared) = s1.upgrade()
                {
                    shared.borrow_mut().done(seq.seq(), now_ns());
                }
            })
            .error(move |id, _seq, res, message| {
                // On the core the connection is gone; on a node proxy its cached state can't be trusted. Either
                // way start again from a fresh connection and a complete snapshot.
                let what = if id == pw::core::PW_ID_CORE {
                    "PipeWire connection lost".to_string()
                } else {
                    format!("PipeWire error on proxy {id}")
                };
                if let Some(shared) = s2.upgrade() {
                    shared
                        .borrow_mut()
                        .fail(&format!("{what} ({res} {message})"), now_ns());
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
                let class = obj.props.as_ref().and_then(|p| p.get("media.class"));
                let serial = obj
                    .props
                    .as_ref()
                    .and_then(|p| p.get("object.serial"))
                    .and_then(|v| v.parse().ok());
                shared
                    .borrow_mut()
                    .graph
                    .discovered(id, class, serial, now_ns());
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
                        let first = !sh.graph.known(id);
                        let seen = |g: &Graph| {
                            g.node(id).map(|n| {
                                (
                                    n.running,
                                    g.class(id),
                                    n.prop(APP_KEYS[0]).map(String::from),
                                )
                            })
                        };
                        let before = seen(&sh.graph);
                        let props = if first || info.change_mask().contains(NodeChangeMask::PROPS) {
                            info.props().map(|p| p.iter())
                        } else {
                            None
                        };
                        let running = matches!(info.state(), NodeState::Running);
                        let msgs = sh.graph.info(id, props, running, now_ns());
                        sh.send(msgs);
                        if sh.watch
                            && let (Some(n), Some(class)) = (sh.graph.node(id), sh.graph.class(id))
                            && class != Class::Irrelevant
                            && (first || before != seen(&sh.graph))
                        {
                            println!("{}", describe(id, n, class));
                        }
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
                    if sh.watch && sh.graph.class(id).is_some_and(|c| c != Class::Irrelevant) {
                        println!("stream {id} removed");
                    }
                    let msgs = sh.graph.removed(id, now_ns());
                    if sh.broken.is_none() {
                        sh.send(msgs);
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
        graph: Graph::new(apps),
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
                sh.graph.clear(now);
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
                        let msgs = sh.graph.monitor.lost(NOT_RUNNING, now);
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
            let mut msgs = sh.graph.expire(now);
            msgs.extend(sh.graph.monitor.tick(now));
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
                let msgs = sh.graph.monitor.stall_check(now);
                sh.send(msgs);
                let ok = sh.broken.is_none() && sh.request(Purpose::Sync(id), now).is_ok();
                if !ok {
                    let reason = sh.broken.clone().unwrap_or_else(|| NOT_RUNNING.into());
                    let mut msgs = sh.graph.monitor.lost(&reason, now);
                    msgs.push(sh.graph.monitor.synced(id, now));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_retry_answers_the_sync_once_unhealthy() {
        let buf = Buf::default();
        let mut sh = Shared {
            graph: Graph::new(vec!["rec".into()]),
            pending: HashMap::new(),
            tick_outstanding: false,
            // No connection, so the retry's round trip can't be sent.
            core: None,
            broken: None,
            out: Arc::new(Out::new(Box::new(buf.clone()))),
            watch: false,
        };
        let now = 100 * SECOND;
        sh.graph.monitor.observe(false, now);
        // A node without state keeps the snapshot incomplete, so the sync's round trip is retried.
        sh.graph.discovered(1, None, None, now);
        sh.pending.insert(7, (Purpose::Sync(42), now));
        sh.done(7, now);
        let lines: Vec<serde_json::Value> = String::from_utf8(buf.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let synced: Vec<_> = lines.iter().filter(|l| l["t"] == "synced").collect();
        assert_eq!(synced.len(), 1, "{lines:?}");
        assert_eq!(synced[0]["id"], 42);
        assert_eq!(synced[0]["healthy"], false);
        assert!(sh.broken.is_some());
    }
}

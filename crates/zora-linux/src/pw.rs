//! Dictation monitor on PipeWire: binds every node, tracks the capture streams (`Stream/Input/Audio`) whose
//! application properties name a configured dictation app, and reports whether one is running. A core `sync`
//! round trip is the "complete observation": it confirms every event the server sent before it has arrived.
//! Everything runs on one PipeWire main loop, including the 1 s timer and the core's `sync` requests.

use pipewire as pw;
use pw::node::{Node, NodeListener, NodeState};
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use zora_plugin::clock::now_ns;
use zora_plugin::dictation::{Monitor, Msg, StreamNode};
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

struct Shared {
    monitor: Monitor,
    apps: Vec<String>,
    streams: HashMap<u32, StreamNode>,
    pending: HashMap<i32, Purpose>,
    tick_outstanding: bool,
    broken: bool,
    out: Arc<Out>,
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
}

struct Conn {
    core: pw::core::CoreRc,
    _registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    _registry_listener: pw::registry::Listener,
    _nodes: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>>,
}

const NOT_RUNNING: &str = "cannot connect to PipeWire (PulseAudio or ALSA alone is not supported)";

fn connect(context: &pw::context::ContextRc, shared: &Rc<RefCell<Shared>>) -> Result<Conn, String> {
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let nodes: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>> = Rc::default();

    let core_listener = {
        let (s1, s2) = (Rc::downgrade(shared), Rc::downgrade(shared));
        core.add_listener_local()
            .done(move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                let Some(shared) = s1.upgrade() else { return };
                let mut sh = shared.borrow_mut();
                let Some(purpose) = sh.pending.remove(&seq.seq()) else {
                    return;
                };
                let now = now_ns();
                let capturing = sh.capturing();
                let mut msgs = sh.monitor.observe(capturing, now);
                match purpose {
                    Purpose::Tick => sh.tick_outstanding = false,
                    Purpose::Sync(id) => msgs.push(sh.monitor.synced(id, now)),
                }
                sh.send(msgs);
            })
            .error(move |id, _seq, res, message| {
                eprintln!("zora-dictation: PipeWire error on {id}: {res} {message}");
                if id == pw::core::PW_ID_CORE
                    && let Some(shared) = s2.upgrade()
                {
                    shared.borrow_mut().broken = true;
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
                let (Some(reg), Some(nodes)) = (reg.upgrade(), n1.upgrade()) else {
                    return;
                };
                let Ok(node) = reg.bind::<Node, _>(obj) else {
                    return;
                };
                let id = obj.id;
                let s = s1.clone();
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        let Some(shared) = s.upgrade() else { return };
                        let mut sh = shared.borrow_mut();
                        let apps = sh.apps.clone();
                        let entry = sh.streams.entry(id).or_default();
                        if let Some(props) = info.props() {
                            entry.update_props(props.iter(), &apps);
                        }
                        entry.running = matches!(info.state(), NodeState::Running);
                    })
                    .register();
                nodes.borrow_mut().insert(id, (node, listener));
            })
            .global_remove(move |id| {
                if let Some(nodes) = n2.upgrade() {
                    nodes.borrow_mut().remove(&id);
                }
                if let Some(shared) = s2.upgrade() {
                    shared.borrow_mut().streams.remove(&id);
                }
            })
            .register()
    };

    let conn = Conn {
        core,
        _registry: registry,
        _core_listener: core_listener,
        _registry_listener: registry_listener,
        _nodes: nodes,
    };
    // The first `done` arrives after every global and the info of every node bound meanwhile.
    request(&conn, shared, Purpose::Tick)?;
    Ok(conn)
}

fn request(conn: &Conn, shared: &Rc<RefCell<Shared>>, purpose: Purpose) -> Result<(), String> {
    let seq = conn.core.sync(0).map_err(|e| e.to_string())?;
    let mut sh = shared.borrow_mut();
    if matches!(purpose, Purpose::Tick) {
        sh.tick_outstanding = true;
    }
    sh.pending.insert(seq.seq(), purpose);
    Ok(())
}

/// Runs until `Req::Quit`.
pub fn run(apps: Vec<String>, out: Arc<Out>, rx: pw::channel::Receiver<Req>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let shared = Rc::new(RefCell::new(Shared {
        monitor: Monitor::new(!apps.is_empty()),
        apps,
        streams: HashMap::new(),
        pending: HashMap::new(),
        tick_outstanding: false,
        broken: false,
        out,
    }));
    let conn: Rc<RefCell<Option<Conn>>> = Rc::default();

    let reconnect = {
        let (shared, conn, context) = (shared.clone(), conn.clone(), context.clone());
        move || {
            let now = now_ns();
            let mut c = conn.borrow_mut();
            if c.is_some() && shared.borrow().broken {
                *c = None;
                let mut sh = shared.borrow_mut();
                sh.streams.clear();
                sh.pending.clear();
                sh.tick_outstanding = false;
                sh.broken = false;
                let msgs = sh.monitor.lost("PipeWire connection lost", now);
                sh.send(msgs);
            }
            if c.is_none() {
                match connect(&context, &shared) {
                    Ok(new) => {
                        eprintln!("zora-dictation: connected to PipeWire");
                        *c = Some(new);
                    }
                    Err(e) => {
                        let sh = &mut *shared.borrow_mut();
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
        let (shared, conn) = (shared.clone(), conn.clone());
        let reconnect = reconnect.clone();
        mainloop.loop_().add_timer(move |_| {
            reconnect();
            let outstanding = shared.borrow().tick_outstanding;
            let failed = match conn.borrow().as_ref() {
                Some(c) if !outstanding => request(c, &shared, Purpose::Tick).is_err(),
                _ => false,
            };
            if failed {
                shared.borrow_mut().broken = true;
            }
            let mut sh = shared.borrow_mut();
            let msgs = sh.monitor.tick(now_ns());
            sh.send(msgs);
        })
    };
    timer
        .update_timer(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
        .into_result()
        .map_err(|e| e.to_string())?;

    let _rx = {
        let (shared, conn, weak_loop) = (shared.clone(), conn.clone(), mainloop.downgrade());
        rx.attach(mainloop.loop_(), move |req| match req {
            Req::Sync(id) => {
                let now = now_ns();
                {
                    let sh = &mut *shared.borrow_mut();
                    let msgs = sh.monitor.stall_check(now);
                    sh.send(msgs);
                }
                let ok = conn
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| request(c, &shared, Purpose::Sync(id)).is_ok());
                if !ok {
                    let sh = &mut *shared.borrow_mut();
                    let mut msgs = sh.monitor.lost(NOT_RUNNING, now);
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

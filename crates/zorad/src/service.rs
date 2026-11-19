//! The always-on service: plugins → ordered queue → coordinator → effects (store, notify, sync, dispatch).

use crate::host::{Connection, Item, PluginSpec, ToPlugin, run_plugin};
use crate::jobs::{JobEvent, JobManager, Submit};
use crate::queue::InputQueue;
use crate::store_thread::StoreHandle;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zora_core::clock::{Ns, now_ns, wall_secs};
use zora_core::config::Config;
use zora_core::coordinator::{Coordinator, Effect, Input};
use zora_core::harness::HarnessSpec;
use zora_core::model::{DiscardReason, EventKind, ZoraEvent};
use zora_core::suppression::DictationSignal;

/// `%LOCALAPPDATA%\Zora\zora.sqlite`; the pause file sits beside it.
#[cfg(windows)]
pub fn database_path() -> PathBuf {
    zora_core::config::windows_dir("LOCALAPPDATA", &["AppData", "Local"])
        .join("Zora")
        .join("zora.sqlite")
}

#[cfg(unix)]
pub fn database_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Zora/zora.sqlite")
    } else {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        state.join("zora/zora.sqlite")
    }
}

struct Plugin {
    generation: u64,
    capabilities: Vec<String>,
    tx: mpsc::Sender<String>,
    outstanding: usize,
}

struct Notifier {
    routes: HashMap<String, HashSet<EventKind>>,
    next_id: u64,
}

impl Notifier {
    fn new(config: &Config) -> Notifier {
        let routes = config
            .notifications
            .iter()
            .map(|(id, kinds)| {
                let set = if kinds.iter().any(|k| k == "*") {
                    EventKind::ALL.into_iter().collect()
                } else {
                    kinds.iter().filter_map(|k| EventKind::parse(k)).collect()
                };
                (id.clone(), set)
            })
            .collect();
        Notifier { routes, next_id: 0 }
    }

    fn notify(&mut self, event: &ZoraEvent, plugins: &mut HashMap<String, Plugin>) {
        for (id, kinds) in &self.routes {
            if !kinds.contains(&event.kind()) {
                continue;
            }
            if id == "log" {
                eprintln!("[zorad] {}", event.metadata());
                continue;
            }
            let Some(p) = plugins.get_mut(id) else {
                continue;
            };
            if !p.capabilities.iter().any(|c| c == "notify") || p.outstanding >= 16 {
                continue;
            }
            self.next_id += 1;
            let (title, body) = event.display();
            // Bound the encoded frame well under the protocol's 64 KiB line limit (JSON escaping can double it).
            let title: String = title.chars().take(200).collect();
            let body: String = body.chars().take(1000).collect();
            let line = ToPlugin::Notify {
                id: self.next_id,
                kind: event.kind().as_str().into(),
                title,
                body,
                job: event.job_id().map(String::from),
                harness: event.harness().map(String::from),
            }
            .line();
            if p.tx.try_send(line).is_ok() {
                p.outstanding += 1;
            }
        }
    }
}

pub struct Options {
    pub attached: bool,
    /// Watched for live changes to phrases, wake names and capture limits.
    pub config_path: PathBuf,
}

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub async fn run(config: Config, opts: Options) -> i32 {
    let store = match StoreHandle::open(&database_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zorad: cannot open store: {e}");
            return 1;
        }
    };
    let (queue, mut rx) = InputQueue::<Item>::new(1024, 4096);
    let queue = Arc::new(queue);
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<JobEvent>();
    let jobs = JobManager::new(
        store.clone(),
        config.search_path(),
        Duration::from_secs_f64(config.jobs.timeout_seconds),
        config.jobs.max_queue,
        Duration::from_secs_f64(config.jobs.max_age_seconds),
        job_tx,
    );
    {
        let q = queue.clone();
        tokio::spawn(async move {
            while let Some(ev) = job_rx.recv().await {
                q.force(Item::Job(ev));
            }
        });
    }

    let mut plugins: HashMap<String, Plugin> = HashMap::new();
    let mut notifier = Notifier::new(&config);

    // Recovery happens before admission opens.
    if let Err(reason) = crate::jobs::recover(&store).await {
        jobs.block(reason.clone());
        notifier.notify(&ZoraEvent::Degraded { reason }, &mut plugins);
    }

    let mut harness = HarnessSpec::from_config(
        &config.default_harness,
        &config.harnesses[&config.default_harness],
        &config,
    );
    let mut coordinator = Coordinator::new(&config, now_ns());
    let no_monitor = config.dictation.monitor == "none";

    // Listening on/off (paused state survives restarts).
    let pause_path = crate::pause::path();
    let mut listen = crate::pause::read(&pause_path, crate::pause::wall_now());
    // Speech that started before the last resume was heard while paused, however late it arrives.
    let mut listening_since: Ns = Ns::MIN;
    let (listen_tx, listen_rx) = tokio::sync::watch::channel(listen);
    if !listen.listening {
        eprintln!("[zorad] starting paused");
    }

    // Plugins.
    // On macOS the app is the "macos" plugin (attached); any other configured plugin is spawned beside it but may
    // only notify, so the app stays the one source of transcript and dictation.
    let mut specs: Vec<PluginSpec> = Vec::new();
    if opts.attached {
        let mut cfg = config
            .plugins
            .get("macos")
            .map(|p| p.config.clone())
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = cfg.as_object_mut() {
            obj.entry("locale").or_insert(json!(config.locale));
            obj.entry("speechDetector")
                .or_insert(json!(config.speech_detector));
            obj.entry("bundleIDs")
                .or_insert(json!(config.dictation.bundle_ids));
        }
        specs.push(PluginSpec {
            id: "macos".into(),
            connection: Connection::Attached,
            config: cfg,
            listen: listen_rx.clone(),
            allowed: None,
        });
    }
    specs.extend(
        config
            .plugins
            .iter()
            .filter(|(id, p)| !p.command.is_empty() && !(opts.attached && id.as_str() == "macos"))
            .map(|(id, p)| {
                let mut cfg = p.config.clone();
                if let Some(obj) = cfg.as_object_mut() {
                    obj.entry("locale").or_insert(json!(config.locale));
                    let apps = if cfg!(windows) {
                        &config.dictation.windows_apps
                    } else {
                        &config.dictation.linux_apps
                    };
                    obj.entry("apps").or_insert(json!(apps));
                }
                PluginSpec {
                    id: id.clone(),
                    connection: Connection::Spawn(p.command.clone()),
                    config: cfg,
                    listen: listen_rx.clone(),
                    allowed: opts.attached.then(|| vec!["notify"]),
                }
            }),
    );
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut plugin_tasks = Vec::new();
    for (i, spec) in specs.into_iter().enumerate() {
        plugin_tasks.push(tokio::spawn(run_plugin(
            spec,
            queue.clone(),
            (i as u64 + 1) * 1_000_000,
            stop_rx.clone(),
        )));
    }
    if no_monitor {
        queue.force(Item::Core(Input::Dictation(DictationSignal::Health {
            healthy: true,
            at: now_ns(),
        })));
    }

    // Ticker and signals.
    {
        let q = queue.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(1));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                let _ = q.send(Item::Core(Input::Tick));
            }
        });
    }
    {
        let q = queue.clone();
        crate::os::on_shutdown(move || q.force(Item::Shutdown));
    }

    notifier.notify(
        &ZoraEvent::Started {
            dry_run: config.dry_run,
        },
        &mut plugins,
    );
    let retention = config.retention_hours * 3600.0;
    let mut last_maintenance = Instant::now();
    // None: the first check always reloads, so a save between the startup read and here is never missed.
    let mut config_mtime = None;
    let mut last_config_check = Instant::now();
    let mut last_pause_check = Instant::now();
    let failure_check = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut notices: HashMap<String, Instant> = HashMap::new();
    let mut shutting_down: Option<(Instant, HashSet<String>)> = None;
    let mut exit_code = 0;

    loop {
        let item = if let Some((deadline, waiting)) = &shutting_down {
            if waiting.is_empty() || Instant::now() >= *deadline {
                break;
            }
            match tokio::time::timeout_at((*deadline).into(), rx.recv()).await {
                Ok(Some(i)) => i,
                _ => break,
            }
        } else {
            match rx.recv().await {
                Some(i) => i,
                None => break,
            }
        };
        queue.consumed(&item);
        let now = now_ns();
        // Pause is applied before the item, so speech arriving with a due pause is already dropped.
        if last_pause_check.elapsed() >= Duration::from_secs(1) {
            last_pause_check = Instant::now();
            let next = crate::pause::read(&pause_path, crate::pause::wall_now());
            if next != listen && shutting_down.is_none() {
                if !next.listening {
                    // Anything half-heard is forgotten: an open capture ends here.
                    for fx in coordinator.handle(Input::Gap, now) {
                        execute(
                            fx,
                            now,
                            no_monitor,
                            &store,
                            &jobs,
                            &harness,
                            &queue,
                            &mut notifier,
                            &mut plugins,
                        );
                    }
                }
                eprintln!(
                    "[zorad] listening {}",
                    if next.listening { "resumed" } else { "paused" }
                );
                if next.listening {
                    listening_since = now_ns();
                }
                listen = next;
                let _ = listen_tx.send(listen);
            }
        }
        let effects = match item {
            // While paused nothing heard is kept or matched, including results the plugin finalizes late.
            Item::Core(Input::Segment(_) | Input::Partial { .. }) if !listen.listening => vec![],
            Item::Core(Input::Segment(s)) if s.start < listening_since => vec![],
            Item::Core(input) => coordinator.handle(input, now),
            Item::Loss => coordinator.handle(Input::Gap, now),
            Item::PluginUp {
                id,
                generation,
                capabilities,
                tx,
            } => {
                plugins.insert(
                    id,
                    Plugin {
                        generation,
                        capabilities,
                        tx,
                        outstanding: 0,
                    },
                );
                vec![]
            }
            Item::PluginDown { id, generation } => {
                if plugins.get(&id).is_some_and(|p| p.generation == generation) {
                    plugins.remove(&id);
                }
                if let Some((_, waiting)) = &mut shutting_down {
                    waiting.remove(&id);
                }
                vec![]
            }
            Item::PluginStopped { id } => {
                if let Some((_, waiting)) = &mut shutting_down {
                    waiting.remove(&id);
                }
                vec![]
            }
            Item::Ack { id } => {
                if let Some(p) = plugins.get_mut(&id) {
                    p.outstanding = p.outstanding.saturating_sub(1);
                }
                vec![]
            }
            Item::Notice(text) => {
                let due = notices
                    .get(&text)
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(600));
                if due {
                    notices.insert(text.clone(), Instant::now());
                    notifier.notify(&ZoraEvent::Degraded { reason: text }, &mut plugins);
                }
                vec![]
            }
            Item::Job(ev) => {
                job_event(ev, &store, &mut notifier, &mut plugins);
                vec![]
            }
            stop @ (Item::Shutdown | Item::Restart) => {
                if matches!(stop, Item::Restart) {
                    exit_code = 3;
                }
                if shutting_down.is_none() {
                    // Close admission first, while dictation monitoring still runs.
                    coordinator.shutdown();
                    jobs.close();
                    let mut waiting = HashSet::new();
                    for (id, p) in &plugins {
                        let _ = p.tx.try_send(ToPlugin::Stop.line());
                        waiting.insert(id.clone());
                    }
                    shutting_down = Some((Instant::now() + Duration::from_secs(5), waiting));
                }
                vec![]
            }
        };
        for fx in effects {
            execute(
                fx,
                now,
                no_monitor,
                &store,
                &jobs,
                &harness,
                &queue,
                &mut notifier,
                &mut plugins,
            );
        }
        // Live reload of phrases, wake names and capture limits (a stat once a second; nothing else reloads).
        if last_config_check.elapsed() >= Duration::from_secs(1) {
            last_config_check = Instant::now();
            let m = modified(&opts.config_path);
            if m != config_mtime {
                config_mtime = m;
                match Config::load(&opts.config_path) {
                    Ok(mut new) => {
                        new.dry_run = config.dry_run;
                        let problems = new.validate();
                        if problems.is_empty() {
                            coordinator.reload_detector(new.detector_config());
                            // Jobs already queued keep the harness they were submitted with.
                            harness = HarnessSpec::from_config(
                                &new.default_harness,
                                &new.harnesses[&new.default_harness],
                                &new,
                            );
                            eprintln!("[zorad] config reloaded: wake names {:?}", new.wake_names);
                        } else {
                            notifier.notify(
                                &ZoraEvent::Degraded {
                                    reason: format!("config not applied: {}", problems.join("; ")),
                                },
                                &mut plugins,
                            );
                        }
                    }
                    Err(e) => notifier.notify(
                        &ZoraEvent::Degraded {
                            reason: format!("config not applied: {e}"),
                        },
                        &mut plugins,
                    ),
                }
            }
        }
        if last_maintenance.elapsed() >= Duration::from_secs(60) {
            last_maintenance = Instant::now();
            store.send(move |s| s.maintenance(retention));
            let dropped = store.take_dropped();
            if dropped > 0 {
                notifier.notify(
                    &ZoraEvent::Degraded {
                        reason: format!("transcript store fell behind; {dropped} writes dropped"),
                    },
                    &mut plugins,
                );
            }
            // Never await the store in the policy loop: a stalled disk must not delay shutdown. The result comes
            // back through the ordered queue; at most one check is outstanding.
            if !failure_check.swap(true, std::sync::atomic::Ordering::AcqRel) {
                let (st, q, flag) = (store.clone(), queue.clone(), failure_check.clone());
                tokio::spawn(async move {
                    if let Some(Some(failure)) = st.call(|s| s.take_failure()).await {
                        q.force(Item::Notice(failure));
                    }
                    flag.store(false, std::sync::atomic::Ordering::Release);
                });
            }
        } else {
            store.send(|s| s.flush_if_due());
        }
    }

    // Plugins: end their sessions and reap their process groups before exiting.
    let _ = stop_tx.send(true);
    for task in plugin_tasks {
        if tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .is_err()
        {
            // Plugin cleanup is bounded (TERM, grace, KILL, verify); exceeding this is a failure, not success.
            eprintln!("[zorad] plugin cleanup did not finish");
            exit_code = 1;
        }
    }
    // Drain: jobs cancelled and reaped, then final flush.
    jobs.close();
    jobs.drain(Duration::from_secs(8)).await;
    while let Ok(item) = rx.try_recv() {
        if let Item::Job(ev) = item {
            job_event(ev, &store, &mut notifier, &mut plugins);
        }
    }
    // Bounded: a stalled disk must not hold the process hostage; it is reported as a failed exit.
    match tokio::time::timeout(Duration::from_secs(10), store.call(|s| s.flush())).await {
        Ok(Some(Ok(()))) => {}
        Ok(Some(Err(e))) => {
            eprintln!("[zorad] final flush failed: {e}");
            return 1;
        }
        _ => {
            eprintln!("[zorad] final flush did not complete");
            return 1;
        }
    }
    exit_code
}

#[allow(clippy::too_many_arguments)]
fn execute(
    fx: Effect,
    now: Ns,
    no_monitor: bool,
    store: &StoreHandle,
    jobs: &JobManager,
    harness: &HarnessSpec,
    queue: &Arc<InputQueue<Item>>,
    notifier: &mut Notifier,
    plugins: &mut HashMap<String, Plugin>,
) {
    match fx {
        Effect::Store { segment, kind } => {
            let (s, e) = (wall_secs(segment.start, now), wall_secs(segment.end, now));
            store.send(move |st| st.append(segment.session, s, e, &segment.text, kind.as_str()));
        }
        Effect::Retag { from, to } => {
            let (f, t) = (wall_secs(from, now), wall_secs(to, now));
            store.send(move |st| st.retag_dictation(f, t));
        }
        Effect::Notify(ev) => notifier.notify(&ev, plugins),
        Effect::Wake(delay) => {
            let q = queue.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_nanos(delay.max(0) as u64)).await;
                let _ = q.send(Item::Core(Input::Tick));
            });
        }
        Effect::Sync { id, cutoff } => {
            if no_monitor {
                queue.force(Item::Core(Input::Synced {
                    id,
                    at: now_ns().max(cutoff),
                    healthy: true,
                    capturing: false,
                }));
            } else if let Some(p) = plugins
                .values()
                .find(|p| p.capabilities.iter().any(|c| c == "dictation"))
            {
                // If this fails the coordinator's sync deadline discards the command.
                let _ = p.tx.try_send(
                    ToPlugin::Sync {
                        id,
                        cutoff_ns: cutoff,
                    }
                    .line(),
                );
            }
        }
        Effect::StopRunning => {
            // "Stop it, Zora" with nothing to forget: stop the latest running job. Its guard reaps the group and
            // the finish notification reports "stopped". With nothing running it is ordinary speech.
            if let Some(id) = jobs.cancel_latest() {
                eprintln!("[zorad] stop requested by voice id={id}");
            }
        }
        Effect::Dispatch(cmd) => {
            // Capacity first: only admitted commands are written to the store, so critical store traffic stays
            // bounded by the job queue. Rejections are notifications only.
            let admission = match jobs.admission() {
                // Each admitted command holds a store reservation until its terminal write is stored.
                Submit::Accepted if !store.try_reserve_record(jobs.max_queue() * 2) => {
                    Submit::QueueFull
                }
                other => other,
            };
            let rejected = match admission {
                Submit::Accepted => {
                    let (id, text, h) = (cmd.id.clone(), cmd.text.clone(), harness.id.clone());
                    store.send_critical(move |s| s.record_command(&id, &text, &h, "queued"));
                    match jobs.submit(cmd.id.clone(), cmd.text.clone(), harness.clone()) {
                        Submit::Accepted => None,
                        other => {
                            let id = cmd.id.clone();
                            store.send_terminal(move |s| {
                                s.finish_command(&id, "rejected", None, "not admitted")
                            });
                            Some(other)
                        }
                    }
                }
                other => Some(other),
            };
            match rejected {
                None => notifier.notify(
                    &ZoraEvent::CommandQueued {
                        id: cmd.id,
                        text: cmd.text,
                        harness: harness.id.clone(),
                    },
                    plugins,
                ),
                Some(Submit::QueueFull) => notifier.notify(
                    &ZoraEvent::CaptureDiscarded {
                        reason: DiscardReason::QueueFull,
                    },
                    plugins,
                ),
                Some(_) => {
                    let reason = jobs.blocked().unwrap_or_else(|| "shutting down".into());
                    notifier.notify(
                        &ZoraEvent::Degraded {
                            reason: format!("command not run: {reason}"),
                        },
                        plugins,
                    );
                }
            }
        }
    }
}

fn job_event(
    ev: JobEvent,
    store: &StoreHandle,
    notifier: &mut Notifier,
    plugins: &mut HashMap<String, Plugin>,
) {
    match ev {
        JobEvent::Started { id, harness } => {
            notifier.notify(&ZoraEvent::JobStarted { id, harness }, plugins)
        }
        JobEvent::Finished {
            id, result, clean, ..
        } => {
            let (sid, summary, code, status) = (
                id.clone(),
                result.summary.clone(),
                result.exit_code,
                if result.success {
                    "succeeded"
                } else {
                    "failed"
                },
            );
            // An unverified cleanup is carried explicitly: the job stays non-final for recovery and the manual
            // check, whether or not the earlier "ambiguous" write has landed yet.
            if clean {
                store.send_terminal(move |s| s.finish_command(&sid, status, code, &summary));
            } else {
                store.send_terminal(move |s| s.set_status(&sid, "ambiguous"));
            }
            let event = if result.success {
                ZoraEvent::JobSucceeded {
                    id,
                    summary: result.summary,
                }
            } else {
                ZoraEvent::JobFailed {
                    id,
                    reason: result.summary,
                }
            };
            notifier.notify(&event, plugins);
        }
        JobEvent::Expired { id } => {
            store.send_terminal(move |s| {
                s.finish_command(&id, "expired", None, "waited too long in queue")
            });
            notifier.notify(
                &ZoraEvent::CaptureDiscarded {
                    reason: DiscardReason::Expired,
                },
                plugins,
            );
        }
        JobEvent::AdmissionClosed { reason } => {
            notifier.notify(&ZoraEvent::Degraded { reason }, plugins)
        }
    }
}

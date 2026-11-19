//! Core side of job execution: bounded per-workspace queue, guard supervision, durable ownership, recovery.

use crate::guard::{CoreMsg, GuardMsg, JobSpec};
use crate::os;
use crate::store_thread::StoreHandle;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};
use zora_core::config::PATH_SEPARATOR;
use zora_core::harness::{HarnessResult, HarnessSpec, resolve_command};

#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    Started {
        id: String,
        harness: String,
    },
    Finished {
        id: String,
        harness: String,
        result: HarnessResult,
        /// False when the job's process group could not be verified empty; the job must stay non-final.
        clean: bool,
    },
    Expired {
        id: String,
    },
    /// Cleanup could not be verified; admission is closed until a manual check.
    AdmissionClosed {
        reason: String,
    },
}

#[derive(Clone)]
pub struct Job {
    pub id: String,
    pub prompt: String,
    pub spec: HarnessSpec,
    queued_at: Instant,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    busy: HashSet<String>,
    cancels: HashMap<String, mpsc::UnboundedSender<()>>,
    /// Job ids in start order, for "stop the latest".
    started: Vec<String>,
    closed: bool,
    blocked: Option<String>,
}

#[derive(Clone)]
pub struct JobManager {
    state: Arc<Mutex<State>>,
    events: mpsc::UnboundedSender<JobEvent>,
    store: StoreHandle,
    search_path: Vec<String>,
    timeout: Duration,
    max_queue: usize,
    max_age: Duration,
    kill_grace: Duration,
}

pub enum Submit {
    Accepted,
    QueueFull,
    Closed,
}

impl JobManager {
    pub fn new(
        store: StoreHandle,
        search_path: Vec<String>,
        timeout: Duration,
        max_queue: usize,
        max_age: Duration,
        events: mpsc::UnboundedSender<JobEvent>,
    ) -> JobManager {
        JobManager {
            state: Arc::default(),
            events,
            store,
            search_path,
            timeout,
            max_queue,
            max_age,
            kill_grace: Duration::from_secs(5),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn block(&self, reason: String) {
        self.lock().blocked = Some(reason);
    }

    pub fn blocked(&self) -> Option<String> {
        self.lock().blocked.clone()
    }

    pub fn max_queue(&self) -> usize {
        self.max_queue
    }

    /// Whether a submission would be accepted right now, without enqueuing anything. The service loop is the
    /// only submitter, so capacity can only grow between this check and `submit`.
    pub fn admission(&self) -> Submit {
        let s = self.lock();
        if s.closed || s.blocked.is_some() {
            Submit::Closed
        } else if s.queue.len() >= self.max_queue {
            Submit::QueueFull
        } else {
            Submit::Accepted
        }
    }

    pub fn submit(&self, id: String, prompt: String, spec: HarnessSpec) -> Submit {
        {
            let mut s = self.lock();
            if s.closed || s.blocked.is_some() {
                return Submit::Closed;
            }
            if s.queue.len() >= self.max_queue {
                return Submit::QueueFull;
            }
            s.queue.push_back(Job {
                id,
                prompt,
                spec,
                queued_at: Instant::now(),
            });
        }
        self.pump();
        Submit::Accepted
    }

    /// Cancels the most recently started running job; returns its id.
    pub fn cancel_latest(&self) -> Option<String> {
        let s = self.lock();
        let id = s
            .started
            .iter()
            .rev()
            .find(|i| s.cancels.contains_key(*i))?
            .clone();
        let _ = s.cancels[&id].send(());
        Some(id)
    }

    /// Rejects new work and cancels everything in flight.
    pub fn close(&self) {
        let mut s = self.lock();
        s.closed = true;
        s.queue.clear();
        for c in s.cancels.values() {
            let _ = c.send(());
        }
    }

    pub fn running(&self) -> usize {
        let s = self.lock();
        s.cancels.len() + s.queue.len()
    }

    /// Waits until nothing runs, up to `limit`.
    pub async fn drain(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.running() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn pump(&self) {
        let mut to_start = Vec::new();
        {
            let mut s = self.lock();
            if s.blocked.is_some() || s.closed {
                return;
            }
            let mut i = 0;
            while i < s.queue.len() {
                if s.queue[i].queued_at.elapsed() > self.max_age {
                    let job = s.queue.remove(i).unwrap();
                    let _ = self.events.send(JobEvent::Expired { id: job.id });
                    continue;
                }
                if s.busy.contains(&s.queue[i].spec.workspace) {
                    i += 1;
                    continue;
                }
                let job = s.queue.remove(i).unwrap();
                s.busy.insert(job.spec.workspace.clone());
                let (tx, rx) = mpsc::unbounded_channel();
                s.cancels.insert(job.id.clone(), tx);
                s.started.push(job.id.clone());
                to_start.push((job, rx));
            }
        }
        for (job, cancel) in to_start {
            let me = self.clone();
            tokio::spawn(async move {
                let (result, release) = me.run(&job, cancel).await;
                let _ = me.events.send(JobEvent::Finished {
                    id: job.id.clone(),
                    harness: job.spec.id.clone(),
                    result,
                    clean: release,
                });
                {
                    let mut s = me.lock();
                    s.cancels.remove(&job.id);
                    s.started.retain(|i| i != &job.id);
                    if release {
                        s.busy.remove(&job.spec.workspace);
                    }
                }
                me.pump();
            });
        }
    }

    /// Returns the result and whether the workspace may be released (false when cleanup is unverified).
    async fn run(
        &self,
        job: &Job,
        mut cancel: mpsc::UnboundedReceiver<()>,
    ) -> (HarnessResult, bool) {
        let fail = |summary: String| HarnessResult {
            success: false,
            exit_code: None,
            summary,
        };
        let _ = self.events.send(JobEvent::Started {
            id: job.id.clone(),
            harness: job.spec.id.clone(),
        });
        if job.spec.dry_run {
            return (
                HarnessResult {
                    success: true,
                    exit_code: Some(0),
                    summary: format!("dry run: {}", job.prompt),
                },
                true,
            );
        }
        let mut argv = match resolve_command(&job.spec.executable, &self.search_path) {
            Ok(argv) => argv,
            Err(e) => return (fail(e), true),
        };
        let mut env: Vec<(String, String)> =
            std::env::vars().filter(|(k, _)| k != "PATH").collect();
        env.push(("PATH".into(), self.search_path.join(PATH_SEPARATOR)));
        env.extend(job.spec.env.iter().cloned());
        argv.extend(job.spec.argv(&job.prompt));
        let spec = JobSpec {
            argv,
            cwd: job.spec.workspace.clone(),
            env,
            capture_limit: 64 * 1024,
            kill_grace_ms: self.kill_grace.as_millis() as u64,
            name: job.id.clone(),
            cancel_marker: cancel_marker(&job.id)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };

        let exe_self = std::env::current_exe().unwrap_or_else(|_| "zorad".into());
        let mut guard = Command::new(exe_self);
        guard
            .arg("__guard")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        os::detach(&mut guard);
        let mut child = match guard.spawn() {
            Ok(c) => c,
            Err(e) => return (fail(format!("cannot start job guard: {e}")), true),
        };
        let mut stdin = child.stdin.take();
        let mut lines = FramedRead::new(
            child.stdout.take().unwrap(),
            LinesCodec::new_with_max_length(512 * 1024),
        );
        send_guard(&mut stdin, CoreMsg::Spec(spec)).await;

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut registration: Option<(i32, i64)> = None;
        let mut exited: Option<GuardMsg> = None;
        let mut cleaned: Option<bool> = None;
        let mut failed: Option<String> = None;
        let mut timed_out = false;
        let mut cancel_sent = false;
        // Once the guard has been told to stop (Cancel or EOF), it gets a bounded time to report `cleaned`.
        let stop_limit = self.kill_grace + Duration::from_secs(10);
        let mut give_up: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                line = lines.next() => {
                    // EOF or a framing error: stop reading; cleanup below is bounded either way.
                    let Some(Ok(line)) = line else { break };
                    match serde_json::from_str::<GuardMsg>(&line) {
                        Ok(GuardMsg::Registered { guard_pid, guard_start, pgid, gate_start }) => {
                            let (id, boot) = (job.id.clone(), os::boot_id());
                            let stored = self
                                .store
                                .call(move |s| s.set_job_owner(&id, &boot, guard_pid as i64, guard_start, pgid as i64, gate_start))
                                .await;
                            // Shutdown, cancellation or the deadline may have arrived during the durable write.
                            let cancelled = self.lock().closed || cancel.try_recv().is_ok() || tokio::time::Instant::now() >= deadline;
                            if cancelled {
                                // The gate never gets "go": EOF makes the guard kill it, and nothing executes.
                                registration = Some((pgid, gate_start));
                                cancel_sent = true;
                                stdin = None;
                                give_up = Some(tokio::time::Instant::now() + stop_limit);
                            } else if matches!(stored, Some(Ok(()))) {
                                registration = Some((pgid, gate_start));
                                let id = job.id.clone();
                                self.store.send_critical(move |s| s.set_status(&id, "running"));
                                send_guard(&mut stdin, CoreMsg::Go).await;
                            } else {
                                // Without durable ownership nothing may execute: EOF makes the guard kill the gate.
                                stdin = None;
                                give_up = Some(tokio::time::Instant::now() + stop_limit);
                            }
                        }
                        Ok(m @ GuardMsg::Exited { .. }) => exited = Some(m),
                        Ok(GuardMsg::Cleaned { ok }) => { cleaned = Some(ok); break; }
                        Ok(GuardMsg::Failed { error }) => failed = Some(error),
                        Ok(GuardMsg::Cancelling) => {
                            cancel_sent = true;
                            give_up.get_or_insert(tokio::time::Instant::now() + stop_limit);
                        }
                        Err(_) => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline), if !cancel_sent => {
                    timed_out = true;
                    cancel_sent = true;
                    give_up = Some(tokio::time::Instant::now() + stop_limit);
                    send_guard(&mut stdin, CoreMsg::Cancel).await;
                }
                Some(()) = cancel.recv(), if !cancel_sent => {
                    cancel_sent = true;
                    give_up = Some(tokio::time::Instant::now() + stop_limit);
                    send_guard(&mut stdin, CoreMsg::Cancel).await;
                }
                // A guard that never reports is handled below: killed, and its job cleaned only with verified identity.
                _ = tokio::time::sleep_until(give_up.unwrap_or(deadline)), if give_up.is_some() => break,
            }
        }
        // Close both pipes first: a guard blocked writing an oversized frame must not hang us. Then wait for it
        // with a bound, escalating to its own group.
        drop(stdin);
        drop(lines);
        let guard_pid = child.id().map(|p| p as i32);
        if tokio::time::timeout(self.kill_grace + Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            if let Some(gp) = guard_pid {
                os::kill_detached(gp);
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), child.kill()).await;
        }

        let cleanup_ok = match (cleaned, registration) {
            (Some(ok), _) => ok,
            (None, None) => true, // never registered, so nothing ran
            (None, Some((pgid, gate_start))) => {
                // The guard died without reporting; clean up only with verified identity.
                let id = job.id.clone();
                tokio::task::spawn_blocking(move || {
                    os::reap_job(&id, pgid, gate_start, Duration::from_secs(5))
                })
                .await
                .unwrap_or(false)
            }
        };
        if !cleanup_ok {
            let reason = format!(
                "job process group {} may still be running; check it, then run `zora jobs clear {}`",
                registration.map(|r| r.0).unwrap_or(0),
                job.id
            );
            let id = job.id.clone();
            self.store
                .send_critical(move |s| s.set_status(&id, "ambiguous"));
            self.block(reason.clone());
            let _ = self.events.send(JobEvent::AdmissionClosed { reason });
        }

        if cleanup_ok {
            remove_cancel_marker(&job.id);
        }
        let result = match exited {
            _ if failed.is_some() => fail(failed.unwrap_or_default()),
            _ if timed_out => fail(format!("timed out after {}s", self.timeout.as_secs())),
            Some(GuardMsg::Exited {
                cancelled: true, ..
            }) => fail("cancelled".into()),
            Some(GuardMsg::Exited {
                code: Some(code),
                stdout,
                stderr,
                ..
            }) => job.spec.summarize(code, &stdout, &stderr),
            Some(GuardMsg::Exited {
                signal: Some(sig), ..
            }) => fail(format!("killed by signal {sig}")),
            _ => fail("job guard ended unexpectedly".into()),
        };
        (result, cleanup_ok)
    }
}

fn cancel_dir() -> std::path::PathBuf {
    crate::service::database_path().with_file_name("cancel")
}

/// The file `zorad jobs cancel` creates to ask a Unix guard to cancel job `id` (Windows uses a named event).
pub fn cancel_marker(id: &str) -> Option<std::path::PathBuf> {
    let plain = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    plain.then(|| cancel_dir().join(id))
}

pub fn remove_cancel_marker(id: &str) {
    if let Some(p) = cancel_marker(id) {
        let _ = std::fs::remove_file(p);
    }
}

/// Removes markers of jobs that are not live in the store.
fn prune_cancel_markers(live: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(cancel_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !live.contains(&name) && e.file_type().is_ok_and(|t| !t.is_dir()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

async fn send_guard(stdin: &mut Option<ChildStdin>, msg: CoreMsg) {
    let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
    if let Some(s) = stdin.as_mut() {
        let _ = s.write_all(line.as_bytes()).await;
        let _ = s.flush().await;
    }
}

/// Startup recovery for jobs a previous core left behind. Err means admission must stay closed.
pub async fn recover(store: &StoreHandle) -> Result<usize, String> {
    let jobs = store
        .call(|s| s.non_final_jobs())
        .await
        .ok_or("store unavailable")??;
    prune_cancel_markers(&jobs.iter().map(|j| j.id.clone()).collect());
    let mut recovered = 0;
    for job in jobs {
        let id = job.id.clone();
        let mark = |status: &'static str| {
            if status == "interrupted" {
                remove_cancel_marker(&id);
            }
            let id = id.clone();
            store.send_critical(move |s| s.set_status(&id, status));
        };
        match os::owner(job.boot_id.as_deref()) {
            // A different boot means none of the recorded processes can still exist: no signals, no manual check.
            os::Owner::OtherBoot => {
                mark("interrupted");
                recovered += 1;
                continue;
            }
            // Not visible from here, so not provably gone: admission stays closed.
            os::Owner::Elsewhere(why) => {
                return Err(format!(
                    "job {id} {why}; check it, then run `zora jobs clear {id}`"
                ));
            }
            os::Owner::Here => {}
        }
        if job.status == "ambiguous" {
            return Err(format!(
                "job {} needs a manual check (process group {:?}); then run `zora jobs clear {}`",
                id, job.job_pgid, id
            ));
        }
        // A live guard of ours is already cleaning up (it saw liveness EOF): give it time.
        if let (Some(gpid), Some(gstart)) = (job.guard_pid, job.guard_start) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while os::start_time(gpid as i32) == Some(gstart) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let ok = match (job.job_pgid, job.gate_start) {
            (None, _) => true,
            (Some(pgid), Some(gate_start)) => {
                let id = id.clone();
                tokio::task::spawn_blocking(move || {
                    os::reap_job(&id, pgid as i32, gate_start, Duration::from_secs(5))
                })
                .await
                .unwrap_or(false)
            }
            (Some(pgid), None) => os::job_gone(&id, pgid as i32),
        };
        if ok {
            mark("interrupted");
            recovered += 1;
        } else {
            mark("ambiguous");
            return Err(format!(
                "job {} left process group {:?} that cannot be verified as Zora's; check it, then run `zora jobs clear {}`",
                id, job.job_pgid, id
            ));
        }
    }
    Ok(recovered)
}

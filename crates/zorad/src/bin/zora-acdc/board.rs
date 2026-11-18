//! Turns Zora's job notifications into tasks on the ACDC board: one task per voice command, claimed by Zora while
//! the job runs, moved to review (or done) with the result, or to blocked with the reason it failed.

use crate::http::Transport;
use serde_json::{Value, json};
use std::collections::HashMap;

/// The core cuts notification bodies to this many characters (crates/zorad/src/service.rs).
const BODY_CAP: usize = 1000;
const MAX_TRACKED: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub workspace: Option<String>,
    pub agent: String,
    /// Board status for a job that succeeded: `review` (a person checks the result) or `done`.
    pub on_success: String,
    pub tags: Vec<String>,
    pub claim_minutes: u64,
}

impl Settings {
    pub fn from_config(c: &Value) -> Result<Settings, String> {
        let s = Settings {
            workspace: c["workspace"].as_str().map(String::from),
            agent: c["agent"].as_str().unwrap_or("zora").into(),
            on_success: c["onSuccess"].as_str().unwrap_or("review").into(),
            tags: match &c["tags"] {
                Value::Null => vec!["zora".into()],
                v => serde_json::from_value(v.clone())
                    .map_err(|_| "tags must be a list of strings")?,
            },
            claim_minutes: c["claimMinutes"].as_u64().unwrap_or(60),
        };
        let name_ok = s.agent.len() <= 80
            && s.agent
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && s.agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:@-".contains(c));
        if !name_ok {
            return Err(format!(
                "agent name {:?} is not a valid board agent name",
                s.agent
            ));
        }
        if s.on_success != "review" && s.on_success != "done" {
            return Err("onSuccess must be \"review\" or \"done\"".into());
        }
        if s.tags.len() > 8 {
            return Err("at most 8 tags".into());
        }
        if !(1..=240).contains(&s.claim_minutes) {
            return Err("claimMinutes must be 1 to 240".into());
        }
        Ok(s)
    }
}

/// One `notify` message from the core.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: String,
    pub body: String,
    pub job: Option<String>,
    pub harness: Option<String>,
}

#[derive(Debug, Default)]
struct Job {
    seq: u64,
    task: Option<String>,
    text: Option<String>,
    harness: Option<String>,
}

pub struct Board<T: Transport> {
    pub transport: T,
    settings: Settings,
    jobs: HashMap<String, Job>,
    next: u64,
}

impl<T: Transport> Board<T> {
    pub fn new(transport: T, settings: Settings) -> Self {
        Board {
            transport,
            settings,
            jobs: HashMap::new(),
            next: 0,
        }
    }

    pub fn handle(&mut self, n: &Notice) -> Result<(), String> {
        let job = match (n.kind.as_str(), &n.job) {
            ("commandQueued" | "jobStarted" | "jobSucceeded" | "jobFailed", Some(job)) => {
                job.clone()
            }
            _ => return Ok(()), // nothing about a job: the desktop notifier covers it
        };
        let j = self.track(&job);
        if n.harness.is_some() {
            j.harness = n.harness.clone();
        }
        if n.kind == "commandQueued" {
            j.text = Some(n.body.clone());
        }
        let task = self.ensure_task(&job)?;
        let result = match n.kind.as_str() {
            "jobStarted" => {
                let agent = self.settings.agent.clone();
                let ttl = self.settings.claim_minutes;
                self.post(
                    &format!("/tasks/{task}/claim"),
                    json!({"agent": agent, "ttl_minutes": ttl}),
                )
                .map(|_| ())
            }
            "jobSucceeded" => {
                let status = self.settings.on_success.clone();
                let r = self.finish(&task, &status, &format!("**Zora finished.**\n\n{}", n.body));
                self.jobs.remove(&job);
                r
            }
            "jobFailed" => {
                let r = self.finish(
                    &task,
                    "blocked",
                    &format!("**Zora's job failed:** {}", n.body),
                );
                self.jobs.remove(&job);
                r
            }
            _ => Ok(()),
        };
        self.poke();
        result
    }

    /// Zora is stopping: its jobs are cancelled with it, so say so on their tasks, then sign off.
    pub fn stop(&mut self) {
        let tasks: Vec<String> = self.jobs.values().filter_map(|j| j.task.clone()).collect();
        for task in tasks {
            let _ = self.finish(
                &task,
                "blocked",
                "**Zora stopped before this job finished**, so it did not complete. Say the command again to rerun it.",
            );
        }
        self.jobs.clear();
        let agent = self.settings.agent.clone();
        let _ = self.post(&format!("/agents/{agent}/bye"), json!({"reason": "bye"}));
    }

    /// Puts Zora on the board's agent list. Best effort: the next write by Zora's name registers it too.
    pub fn hello(&mut self) -> Result<(), String> {
        let agent = self.settings.agent.clone();
        self.post(
            &format!("/agents/{agent}"),
            json!({"kind": "other", "role": "voice", "doing": "Listening for voice commands"}),
        )
        .map(|_| ())
    }

    fn track(&mut self, job: &str) -> &mut Job {
        if !self.jobs.contains_key(job) && self.jobs.len() >= MAX_TRACKED {
            // The core runs at most a handful at once; anything this old lost its finish event.
            if let Some(oldest) = self
                .jobs
                .iter()
                .min_by_key(|(_, j)| j.seq)
                .map(|(k, _)| k.clone())
            {
                self.jobs.remove(&oldest);
            }
        }
        self.next += 1;
        let seq = self.next;
        self.jobs.entry(job.to_string()).or_insert_with(|| Job {
            seq,
            ..Job::default()
        })
    }

    fn ensure_task(&mut self, job: &str) -> Result<String, String> {
        let j = &self.jobs[job];
        if let Some(t) = &j.task {
            return Ok(t.clone());
        }
        let short: String = job.chars().take(8).collect();
        let harness = j.harness.clone().unwrap_or_else(|| "its agent".into());
        let (title, detail) = match &j.text {
            Some(text) => {
                let quoted: String = text.lines().map(|l| format!("> {l}\n")).collect();
                let cut = if text.chars().count() >= BODY_CAP {
                    "\n_Shortened. `zorad commands` shows the whole command._\n"
                } else {
                    ""
                };
                (
                    title_of(text).unwrap_or_else(|| format!("Voice command {short}")),
                    format!(
                        "Spoken to Zora and run on `{harness}`.\n\n{quoted}{cut}\nZora job `{job}`."
                    ),
                )
            }
            None => (
                format!("Voice command {short}"),
                format!(
                    "Run by Zora on `{harness}`. The board missed the command text; `zorad commands` has it.\n\nZora job `{job}`."
                ),
            ),
        };
        let mut body = json!({"title": title, "detail": detail, "tags": self.settings.tags});
        if let Some(ws) = &self.settings.workspace {
            body["workspace"] = json!(ws);
        }
        let r = self.post("/tasks", body)?;
        let id = r["id"]
            .as_str()
            .ok_or("board created a task but sent no id")?
            .to_string();
        if let Some(j) = self.jobs.get_mut(job) {
            j.task = Some(id.clone());
        }
        Ok(id)
    }

    fn finish(&mut self, task: &str, status: &str, note: &str) -> Result<(), String> {
        let agent = self.settings.agent.clone();
        let path = format!("/tasks/{task}/status");
        let r = self.post(
            &path,
            json!({"status": status, "note": note, "agent": agent}),
        )?;
        if r["won"] == json!(false) {
            // Someone else holds the task now (a person, or an agent working on it): record the result anyway.
            self.post(&path, json!({"status": status, "note": note}))?;
        }
        Ok(())
    }

    /// Asks the board to refresh its Zora tab now; harmless when that plugin is off.
    fn poke(&mut self) {
        let _ = self
            .transport
            .post("/sources/poke", &json!({"source": "zora"}));
    }

    fn post(&mut self, path: &str, body: Value) -> Result<Value, String> {
        self.transport.post(path, &body)
    }
}

fn title_of(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut title: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        title.push('…');
    }
    Some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: Vec<(String, Value)>,
        down: bool,
        held: bool,
    }

    impl Transport for Fake {
        fn post(&mut self, path: &str, body: &Value) -> Result<Value, String> {
            self.calls.push((path.into(), body.clone()));
            if self.down {
                return Err("board unreachable: connection refused".into());
            }
            Ok(match path {
                "/tasks" => json!({"ok": true, "id": format!("t{}", self.calls.len())}),
                p if p.ends_with("/status") && self.held && body.get("agent").is_some() => {
                    json!({"ok": true, "won": false, "holder": "someone"})
                }
                _ => json!({"ok": true}),
            })
        }
    }

    fn settings() -> Settings {
        Settings::from_config(&json!({})).unwrap()
    }

    fn notice(kind: &str, body: &str) -> Notice {
        Notice {
            kind: kind.into(),
            body: body.into(),
            job: Some("job-1234567890".into()),
            harness: Some("claude".into()),
        }
    }

    fn paths(b: &Board<Fake>) -> Vec<&str> {
        b.transport
            .calls
            .iter()
            .map(|(p, _)| p.as_str())
            .filter(|p| *p != "/sources/poke")
            .collect()
    }

    #[test]
    fn a_job_becomes_a_task_that_ends_in_review() {
        let mut b = Board::new(Fake::default(), settings());
        b.handle(&notice(
            "commandQueued",
            "add a test for the parser\nand run it",
        ))
        .unwrap();
        b.handle(&notice("jobStarted", "Running on claude"))
            .unwrap();
        b.handle(&notice("jobSucceeded", "Added the test."))
            .unwrap();
        assert_eq!(paths(&b), ["/tasks", "/tasks/t1/claim", "/tasks/t1/status"]);
        let create = &b.transport.calls[0].1;
        assert_eq!(create["title"], "add a test for the parser");
        assert_eq!(create["tags"], json!(["zora"]));
        assert!(
            create["detail"]
                .as_str()
                .unwrap()
                .contains("> and run it\n")
        );
        assert!(create.get("workspace").is_none());
        let claim = &b.transport.calls[2].1;
        assert_eq!(claim, &json!({"agent": "zora", "ttl_minutes": 60}));
        let status = &b
            .transport
            .calls
            .iter()
            .rev()
            .find(|(p, _)| p.ends_with("/status"))
            .unwrap()
            .1;
        assert_eq!(status["status"], "review");
        assert!(
            status["note"]
                .as_str()
                .unwrap()
                .ends_with("Added the test.")
        );
        assert!(b.jobs.is_empty());
    }

    #[test]
    fn a_failed_job_blocks_its_task() {
        let mut b = Board::new(Fake::default(), settings());
        b.handle(&notice("commandQueued", "deploy")).unwrap();
        b.handle(&notice("jobFailed", "exit 1")).unwrap();
        let (path, body) = b
            .transport
            .calls
            .iter()
            .rev()
            .find(|(p, _)| p.ends_with("/status"))
            .unwrap();
        assert_eq!(path, "/tasks/t1/status");
        assert_eq!(body["status"], "blocked");
        assert!(body["note"].as_str().unwrap().contains("exit 1"));
    }

    #[test]
    fn a_task_held_by_someone_else_still_gets_the_result() {
        let mut b = Board::new(
            Fake {
                held: true,
                ..Fake::default()
            },
            settings(),
        );
        b.handle(&notice("commandQueued", "x")).unwrap();
        b.handle(&notice("jobSucceeded", "ok")).unwrap();
        let statuses: Vec<&Value> = b
            .transport
            .calls
            .iter()
            .filter(|(p, _)| p.ends_with("/status"))
            .map(|(_, v)| v)
            .collect();
        assert_eq!(statuses.len(), 2);
        assert!(statuses[1].get("agent").is_none());
    }

    #[test]
    fn a_missed_create_is_retried_on_the_next_event() {
        let mut b = Board::new(
            Fake {
                down: true,
                ..Fake::default()
            },
            settings(),
        );
        assert!(
            b.handle(&notice("commandQueued", "rename the module"))
                .is_err()
        );
        b.transport.down = false;
        b.handle(&notice("jobSucceeded", "Renamed.")).unwrap();
        let create = b
            .transport
            .calls
            .iter()
            .find(|(p, _)| p == "/tasks")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(create["title"], "rename the module");
        let created = b
            .transport
            .calls
            .iter()
            .filter(|(p, _)| p == "/tasks")
            .count();
        assert_eq!(created, 2); // the failed attempt, then the one that worked
    }

    #[test]
    fn events_without_a_job_send_nothing() {
        let mut b = Board::new(Fake::default(), settings());
        for kind in ["started", "captureStarted", "captureDiscarded", "degraded"] {
            b.handle(&Notice {
                kind: kind.into(),
                body: "x".into(),
                job: None,
                harness: None,
            })
            .unwrap();
        }
        assert!(b.transport.calls.is_empty());
    }

    #[test]
    fn stop_blocks_unfinished_jobs_and_signs_off() {
        let mut b = Board::new(Fake::default(), settings());
        b.handle(&notice("commandQueued", "long job")).unwrap();
        b.handle(&notice("jobStarted", "")).unwrap();
        b.stop();
        let tail: Vec<&str> = paths(&b).into_iter().rev().take(2).collect();
        assert_eq!(tail, ["/agents/zora/bye", "/tasks/t1/status"]);
    }

    #[test]
    fn a_long_command_is_titled_by_its_first_line_and_marked_shortened() {
        let mut b = Board::new(Fake::default(), settings());
        let long = "x".repeat(BODY_CAP);
        b.handle(&notice("commandQueued", &long)).unwrap();
        let create = &b.transport.calls[0].1;
        assert_eq!(create["title"].as_str().unwrap().chars().count(), 101);
        assert!(create["detail"].as_str().unwrap().contains("Shortened"));
    }

    #[test]
    fn settings_are_checked() {
        let s = Settings::from_config(
            &json!({"workspace": "w-main", "onSuccess": "done", "tags": ["voice"]}),
        )
        .unwrap();
        assert_eq!(
            (s.workspace.as_deref(), s.on_success.as_str()),
            (Some("w-main"), "done")
        );
        assert!(Settings::from_config(&json!({"agent": "a/b"})).is_err());
        assert!(Settings::from_config(&json!({"onSuccess": "closed"})).is_err());
        assert!(Settings::from_config(&json!({"claimMinutes": 0})).is_err());
        assert!(Settings::from_config(&json!({"tags": "zora"})).is_err());
    }
}

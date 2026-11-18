mod guard;
mod host;
mod jobs;
mod os;
mod queue;
mod service;
mod store_thread;

use std::path::PathBuf;
use zora_core::clock::{now_ns, secs};
use zora_core::config::Config;
use zora_core::coordinator::{Coordinator, Effect, Input};
use zora_core::model::{Segment, ZoraEvent};
use zora_core::store::Store;
use zora_core::suppression::DictationSignal;

const USAGE: &str = "usage: zorad <command> [options]

  run [--dry-run]           Run the service (Linux: starts the configured plugins).
  run --attached-plugin     macOS: started by Zora.app, which talks the plugin protocol on fds 3/4.
  status                    Config problems, store, and jobs needing attention.
  transcript [--since 30m] [--last N] [--json]
                            Print the stored transcript (newest N lines with --last).
  commands [--since 4h] [--json]
                            Voice commands and their jobs (never the transcript).
  jobs [cancel|clear <id>]  List unfinished jobs; cancel a running one; clear one after a manual check.
  names [add|remove <name>] List or change the names Zora answers to (applied live, no restart).
  simulate <script>         Feed a scripted transcript through the command logic (dry run).

  --config <path>           Config file (default ~/.config/zora/config.json).";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("__guard") => std::process::exit(guard::guard_main()),
        Some("__gate") => std::process::exit(guard::gate_main(args.get(2..).unwrap_or(&[]))),
        _ => {}
    }
    let mut config_path = None;
    if let Some(i) = args.iter().position(|a| a == "--config")
        && i + 1 < args.len()
    {
        config_path = Some(PathBuf::from(zora_core::config::expand_tilde(&args[i + 1])));
        args.drain(i..=i + 1);
    }
    let command = args.first().cloned().unwrap_or_else(|| "run".into());
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let code = match command.as_str() {
        "run" => cmd_run(config_path, &rest),
        "status" => cmd_status(config_path),
        "transcript" => cmd_transcript(&rest),
        "commands" => cmd_commands(config_path, &rest),
        "jobs" => cmd_jobs(&rest),
        "names" => cmd_names(config_path, &rest),
        "simulate" => cmd_simulate(config_path, &rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn load_config(path: Option<PathBuf>, create: bool) -> Result<Config, String> {
    let path = path.unwrap_or_else(Config::default_path);
    if !path.exists() {
        if !create {
            return Ok(Config::default());
        }
        Config::default().write(&path)?;
        eprintln!(
            "wrote default config to {}; set harnesses.claude.workspace before running without --dry-run",
            path.display()
        );
    }
    Config::load(&path)
}

fn cmd_run(path: Option<PathBuf>, rest: &[String]) -> i32 {
    let path_for_run = path.clone();
    let mut config = match load_config(path, true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zorad: {e}");
            return 0; // clean exit: no restart loop for a broken config
        }
    };
    if rest.iter().any(|a| a == "--dry-run") {
        config.dry_run = true;
    }
    let problems = config.validate();
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("config: {p}");
        }
        return 0;
    }
    let attached = rest.iter().any(|a| a == "--attached-plugin");
    let config_path = path_for_run.unwrap_or_else(Config::default_path);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(service::run(
        config,
        service::Options {
            attached,
            config_path,
        },
    ))
}

fn cmd_status(path: Option<PathBuf>) -> i32 {
    match load_config(path, false) {
        Ok(c) => {
            let p = c.validate();
            println!(
                "config: {}",
                if p.is_empty() {
                    "ok".into()
                } else {
                    p.join("; ")
                }
            );
        }
        Err(e) => println!("config: {e}"),
    }
    let db = service::database_path();
    println!("store: {}", db.display());
    if db.exists()
        && let Ok(s) = Store::open(&db, true)
    {
        for j in s.non_final_jobs().unwrap_or_default() {
            println!("job {} status={} pgid={:?}", j.id, j.status, j.job_pgid);
        }
    }
    0
}

fn cmd_transcript(rest: &[String]) -> i32 {
    const USAGE_T: &str = "usage: zorad transcript [--since 30m] [--last N] [--json]";
    let mut since = 30.0 * 60.0;
    if let Some(i) = rest.iter().position(|a| a == "--since") {
        match rest.get(i + 1).and_then(|s| parse_duration(s)) {
            Some(v) if v >= 0.0 => since = v,
            _ => {
                eprintln!("{USAGE_T}");
                return 2;
            }
        }
    }
    let mut last = usize::MAX;
    if let Some(i) = rest.iter().position(|a| a == "--last") {
        match rest.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
            Some(n) if n > 0 => last = n.min(10_000),
            _ => {
                eprintln!("{USAGE_T}");
                return 2;
            }
        }
    }
    let json = rest.iter().any(|a| a == "--json");
    let wall_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let db = service::database_path();
    let segments = if db.exists() {
        match Store::open(&db, true).and_then(|s| s.last_segments(wall_now - since, last)) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("zorad: cannot read the store at {}: {e}", db.display());
                return 1;
            }
        }
    } else {
        vec![]
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"version": 1, "now": wall_now, "segments": segments})
        );
        return 0;
    }
    for seg in segments {
        let t = seg.start as i64;
        let (h, m, sec) = ((t / 3600) % 24, (t / 60) % 60, t % 60);
        let tag = if seg.kind == "dictation" {
            " [dictation]"
        } else {
            ""
        };
        println!("{h:02}:{m:02}:{sec:02}Z{tag} {}", seg.text);
    }
    0
}

fn cmd_names(path: Option<PathBuf>, rest: &[String]) -> i32 {
    let path = path.unwrap_or_else(Config::default_path);
    let mut config = match load_config(Some(path.clone()), true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zorad: {e}");
            return 1;
        }
    };
    let name = rest.get(1).map(|n| n.trim().to_lowercase());
    match (rest.first().map(String::as_str), name) {
        (None | Some("list"), _) => {}
        (Some("add"), Some(n)) => {
            if !config.wake_names.contains(&n) {
                config.wake_names.push(n);
            }
        }
        (Some("remove"), Some(n)) => config.wake_names.retain(|x| x != &n),
        _ => {
            eprintln!("usage: zorad names [list | add <name> | remove <name>]");
            return 2;
        }
    }
    if rest.first().is_some_and(|c| c != "list") {
        let problems = config
            .validate()
            .into_iter()
            .filter(|p| p.contains("wakeNames"))
            .collect::<Vec<_>>();
        if !problems.is_empty() {
            problems.iter().for_each(|p| eprintln!("{p}"));
            return 2;
        }
        if let Err(e) = config.write(&path) {
            eprintln!("zorad: {e}");
            return 1;
        }
    }
    println!("Zora answers to: {}", config.wake_names.join(", "));
    println!(
        "Say \"Hey <name> … Do it <name>\". Changes apply within a second; no restart needed."
    );
    0
}

fn cmd_commands(path: Option<PathBuf>, rest: &[String]) -> i32 {
    let mut since = 4.0 * 3600.0;
    if let Some(i) = rest.iter().position(|a| a == "--since") {
        match rest.get(i + 1).and_then(|s| parse_duration(s)) {
            Some(v) if v >= 0.0 => since = v,
            _ => {
                eprintln!("usage: zorad commands [--since 4h] [--json]");
                return 2;
            }
        }
    }
    let json = rest.iter().any(|a| a == "--json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let db = service::database_path();
    let commands = if db.exists() {
        match Store::open(&db, true).and_then(|s| s.command_rows(now - since)) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("zorad: cannot read the store at {}: {e}", db.display());
                return 1;
            }
        }
    } else {
        vec![]
    };
    let problems = match load_config(path, false) {
        Ok(c) => c.validate(),
        Err(e) => vec![e],
    };
    if json {
        let out = serde_json::json!({
            "version": 1,
            "now": now,
            "config": {"ok": problems.is_empty(), "problems": problems},
            "commands": commands,
        });
        println!("{out}");
        return 0;
    }
    for p in &problems {
        println!("config: {p}");
    }
    for c in &commands {
        let first = c.text.lines().next().unwrap_or("");
        let first: String = first.chars().take(80).collect();
        let short: String = c.id.chars().take(8).collect();
        println!("{short} {:<11} {:<8} {first}", c.status, c.harness);
    }
    0
}

fn parse_duration(s: &str) -> Option<f64> {
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: f64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60.0),
        "h" => Some(n * 3600.0),
        _ => s.parse().ok(),
    }
}

fn cmd_jobs(rest: &[String]) -> i32 {
    let db = service::database_path();
    if rest.first().map(String::as_str) == Some("cancel") {
        let Some(id) = rest.get(1) else {
            eprintln!("usage: zorad jobs cancel <id>");
            return 2;
        };
        let jobs = Store::open(&db, true)
            .and_then(|s| s.non_final_jobs())
            .unwrap_or_default();
        let Some(job) = jobs.iter().find(|j| j.id.starts_with(id.as_str())) else {
            eprintln!("no running job {id}");
            return 1;
        };
        // Signal only a job group that is provably this job's (leader pid == pgid with the recorded start time);
        // its guard then reaps the group and the core records the outcome.
        match (job.job_pgid, job.gate_start) {
            (Some(pgid), Some(start)) if os::start_time(pgid as i32) == Some(start) => {
                os::signal_group(pgid as i32, libc::SIGTERM);
                println!("job {} cancelled", job.id);
                return 0;
            }
            _ => {
                eprintln!("job {} has no verifiable running process group", job.id);
                return 1;
            }
        }
    }
    if rest.first().map(String::as_str) == Some("clear") {
        let Some(id) = rest.get(1) else {
            eprintln!("usage: zorad jobs clear <id>");
            return 2;
        };
        let Ok(mut s) = Store::open(&db, false) else {
            return 1;
        };
        s.set_status(id, "interrupted");
        println!("job {id} marked interrupted");
        return 0;
    }
    if let Ok(s) = Store::open(&db, true) {
        for j in s.non_final_jobs().unwrap_or_default() {
            println!(
                "{} status={} pgid={:?} guard={:?}",
                j.id, j.status, j.job_pgid, j.guard_pid
            );
        }
    }
    0
}

/// Script lines: `<t> <text>` speech; `dictation <a> <b>`; `gap <t>`; `health ok|unknown`; `# comment`.
fn cmd_simulate(path: Option<PathBuf>, rest: &[String]) -> i32 {
    let Some(file) = rest.first() else {
        eprintln!("usage: zorad simulate <script>");
        return 2;
    };
    let Ok(script) = std::fs::read_to_string(file) else {
        eprintln!("cannot read {file}");
        return 1;
    };
    let mut config = load_config(path, false).unwrap_or_default();
    config.dry_run = true;
    let base = now_ns();
    let at = |t: f64| base + secs(t);
    let mut c = Coordinator::new(&config, base);
    let mut clock = base;
    let mut pending_end: Vec<i64> = vec![];
    let run = |c: &mut Coordinator, input: Input, now: i64| {
        let mut q = std::collections::VecDeque::from([input]);
        while let Some(i) = q.pop_front() {
            for fx in c.handle(i, now) {
                match fx {
                    Effect::Sync { id, .. } => {
                        let s = c.suppression();
                        q.push_back(Input::Synced {
                            id,
                            at: now,
                            healthy: s.healthy(),
                            capturing: s.is_dictating(),
                        });
                    }
                    Effect::Store { segment, kind } => println!(
                        "{:7.2}  {}{}",
                        (segment.start - base) as f64 / 1e9,
                        if kind.as_str() == "dictation" {
                            "[dictation] "
                        } else {
                            ""
                        },
                        segment.text
                    ),
                    Effect::Notify(ev) => {
                        let (title, body) = ev.display();
                        println!("  → [{}] {title}: {body}", ev.kind().as_str());
                    }
                    Effect::Dispatch(cmd) => {
                        let ev = ZoraEvent::CommandQueued {
                            id: cmd.id,
                            text: cmd.text,
                            harness: config.default_harness.clone(),
                        };
                        let (title, body) = ev.display();
                        println!("  → [{}] {title}: {body}", ev.kind().as_str());
                    }
                    _ => {}
                }
            }
        }
    };
    let advance = |c: &mut Coordinator, to: i64, clock: &mut i64, pending_end: &mut Vec<i64>| {
        while *clock < to {
            *clock = (*clock + secs(0.25)).min(to);
            pending_end.sort();
            while pending_end.first().is_some_and(|e| *e <= *clock) {
                let e = pending_end.remove(0);
                run(c, Input::Dictation(DictationSignal::Ended(e)), *clock);
            }
            run(c, Input::Tick, *clock);
        }
    };
    run(
        &mut c,
        Input::Dictation(DictationSignal::Health {
            healthy: true,
            at: base - secs(10.0),
        }),
        base,
    );
    for (n, raw) in script.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let head = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        match head {
            "dictation" => {
                let t: Vec<f64> = tail
                    .split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect();
                if t.len() != 2 {
                    eprintln!("line {}: dictation <start> <end>", n + 1);
                    return 2;
                }
                advance(&mut c, at(t[0]), &mut clock, &mut pending_end);
                run(
                    &mut c,
                    Input::Dictation(DictationSignal::Began(at(t[0]))),
                    clock,
                );
                pending_end.push(at(t[1]));
            }
            "gap" => {
                let t: f64 = tail.trim().parse().unwrap_or(0.0);
                advance(&mut c, at(t), &mut clock, &mut pending_end);
                run(&mut c, Input::Gap, clock);
            }
            "health" => run(
                &mut c,
                Input::Dictation(DictationSignal::Health {
                    healthy: tail.trim() == "ok",
                    at: clock,
                }),
                clock,
            ),
            _ => {
                let Ok(t) = head.parse::<f64>() else {
                    eprintln!("line {}: expected '<time> <text>'", n + 1);
                    return 2;
                };
                advance(&mut c, at(t + 1.0), &mut clock, &mut pending_end);
                let seg = Segment {
                    generation: 0,
                    session: 1,
                    start: at(t),
                    end: at(t + 1.0),
                    text: tail.to_string(),
                };
                run(&mut c, Input::Segment(seg), clock);
            }
        }
    }
    let end = clock
        + secs(config.capture.hold_back_seconds + 3.0)
        + pending_end
            .iter()
            .max()
            .map(|e| (*e - clock).max(0))
            .unwrap_or(0);
    advance(&mut c, end, &mut clock, &mut pending_end);
    0
}

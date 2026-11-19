use zora_core::config::{Config, Harness};
use zora_core::detector::{tokens, units};
use zora_core::harness::{HarnessSpec, last_line_summary, npm_shim_target};
use zora_core::store::Store;

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zora-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    dir.join(name)
}

#[test]
fn normalizer() {
    assert_eq!(tokens("Hey, Zora!"), ["hey", "zora"]);
    assert_eq!(tokens("ZÓRA's  do-it"), ["zoras", "do", "it"]);
    assert!(tokens("…").is_empty());
    assert_eq!(tokens("Zora,fix"), ["zora", "fix"]);
    // Decomposed accent stays with its letter.
    assert_eq!(tokens("Zo\u{0301}ra"), ["zora"]);
    let u = units("(hey) Zora,fix");
    assert_eq!(u[0].leading, "(");
    assert_eq!(u[0].original, "hey)");
}

#[test]
fn config_defaults_fill_missing_keys_and_v1_files_load() {
    let c: Config = serde_json::from_value(serde_json::json!({"capture":{"maxChars":10},"harnesses":{"c":{"workspace":std::env::temp_dir()}},"defaultHarness":"c"})).unwrap();
    assert_eq!(c.capture.max_chars, 10);
    assert_eq!(c.capture.hold_back_seconds, 1.5);
    assert_eq!(c.harnesses["c"].executable, "claude");
    assert!(c.validate().is_empty(), "{:?}", c.validate());
    // A v1 Swift config (written with sortedKeys) still parses.
    let v1 = r#"{"capture":{"holdBackSeconds":1.5,"maxChars":4000,"maxGapSeconds":30,"timeoutSeconds":180},
      "defaultHarness":"claude","dictation":{"bundleIDs":["com.typewhisper.mac"],"padSeconds":1},"dryRun":true,
      "harnesses":{"claude":{"allowedTools":[],"args":[],"env":{},"executable":"claude","extraArgs":[],"type":"claude","workspace":""}},
      "jobs":{"maxAgeSeconds":600,"maxQueue":8,"timeoutSeconds":900},"locale":"en-US",
      "notifications":{"log":["*"]},"path":"/usr/bin","phrases":{"cancel":["zora cancel"],"end":["do it zora"],"start":["hey zora"]},
      "retentionHours":4,"speechDetector":true}"#;
    let c: Config = serde_json::from_str(v1).unwrap();
    assert_eq!(c.dictation.bundle_ids, ["com.typewhisper.mac"]);
    assert!(c.validate().is_empty(), "{:?}", c.validate());
}

#[test]
fn config_rejects_permission_bypass_everywhere() {
    let mut c = Config {
        dry_run: true,
        ..Config::default()
    };
    c.harnesses.get_mut("claude").unwrap().extra_args =
        vec!["--dangerously-skip-permissions".into()];
    assert_eq!(c.validate().len(), 1);
    c.harnesses.get_mut("claude").unwrap().extra_args.clear();
    c.harnesses.get_mut("claude").unwrap().allowed_tools =
        vec!["Read".into(), "--permission-mode=bypassPermissions".into()];
    assert_eq!(c.validate().len(), 2);
    c.harnesses.get_mut("claude").unwrap().allowed_tools = vec![];
    c.harnesses.get_mut("claude").unwrap().permission_mode = Some("bypassPermissions".into());
    assert_eq!(c.validate().len(), 2);
    c.harnesses.get_mut("claude").unwrap().permission_mode = Some("acceptEdits".into());
    c.harnesses.insert(
        "x".into(),
        Harness {
            kind: "argv".into(),
            args: vec!["run".into()],
            ..Harness::default()
        },
    );
    assert_eq!(c.validate(), ["harnesses.x.args must contain \"{prompt}\""]);
}

#[test]
fn config_requires_workspace_unless_dry_run() {
    let mut c = Config::default();
    assert!(!c.validate().is_empty());
    c.dry_run = true;
    assert!(c.validate().is_empty());
}

#[test]
fn harness_argv_and_summaries() {
    let mut h = Harness {
        permission_mode: Some("acceptEdits".into()),
        allowed_tools: vec!["Read".into()],
        ..Harness::default()
    };
    h.workspace = "/tmp".into();
    let spec = HarnessSpec::from_config("claude", &h, &Config::default());
    assert_eq!(
        spec.argv("do x; rm -rf / $(id)"),
        [
            "-p",
            "do x; rm -rf / $(id)",
            "--output-format",
            "text",
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            "Read"
        ]
    );
    let ok = spec.summarize(0, r#"{"result":"Added tests","is_error":false}"#, "");
    assert!(ok.success && ok.summary == "Added tests");
    assert!(
        !spec
            .summarize(0, r#"{"result":"nope","is_error":true}"#, "")
            .success
    );
    let r = last_line_summary(3, "", "boom\n");
    assert_eq!((r.success, r.summary.as_str()), (false, "exit 3: boom"));
}

#[test]
fn npm_shim_target_is_the_script_it_starts() {
    let shim = "@ECHO off\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\
                endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";
    assert_eq!(
        npm_shim_target(shim).as_deref(),
        Some("node_modules\\@anthropic-ai\\claude-code\\cli.js")
    );
    assert_eq!(npm_shim_target("@echo off\r\nrun.exe %*\r\n"), None);
}

#[test]
fn store_batches_prunes_both_tables_and_keeps_live_jobs() {
    let path = tmp("z.sqlite");
    let mut s = Store::open(&path, false).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    s.append(1, now - 7200.0, now - 7190.0, "old", "speech");
    s.append(1, now - 5.0, now - 4.0, "new", "speech");
    s.record_command("a", "cmd", "h", "succeeded");
    s.record_command("b", "live", "h", "running");
    assert_eq!(
        s.segments_since(0.0)
            .iter()
            .map(|x| x.text.as_str())
            .collect::<Vec<_>>(),
        ["old", "new"]
    );
    s.maintenance(3600.0);
    assert_eq!(s.segments_since(0.0).len(), 1);
    s.maintenance(0.0001);
    // Finished commands are pruned; a running job is kept until it is final.
    assert_eq!(s.count("commands"), 1);
    assert!(s.take_failure().is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn store_retag_owner_and_read_only() {
    let path = tmp("z.sqlite");
    let mut s = Store::open(&path, false).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    s.append(1, now - 2.0, now - 1.0, "dictated", "speech");
    s.retag_dictation(now - 3.0, now);
    assert_eq!(s.segments_since(0.0)[0].kind, "dictation");
    s.record_command("j", "c", "h", "queued");
    s.set_job_owner("j", "boot", 10, 11, 12, 13).unwrap();
    assert!(s.set_job_owner("missing", "boot", 1, 1, 1, 1).is_err());
    let jobs = s.non_final_jobs().unwrap();
    assert_eq!(
        (jobs[0].status.as_str(), jobs[0].job_pgid),
        ("starting", Some(12))
    );
    drop(s);
    let mut r = Store::open(&path, true).unwrap();
    assert_eq!(r.segments_since(0.0).len(), 1);
    assert_eq!(r.non_final_jobs().unwrap().len(), 1);
}

mod coordinator_regressions {
    use zora_core::clock::{SECOND, secs};
    use zora_core::config::Config;
    use zora_core::coordinator::{Coordinator, Effect, Input};
    use zora_core::model::Segment;
    use zora_core::suppression::DictationSignal;

    fn say(c: &mut Coordinator, text: &str, start: i64, end: i64, now: i64) -> Vec<Effect> {
        c.handle(
            Input::Segment(Segment {
                generation: 0,
                session: 1,
                start,
                end,
                text: text.into(),
            }),
            now,
        )
    }

    fn setup() -> (Coordinator, i64) {
        let mut cfg = Config::default();
        cfg.capture.hold_back_seconds = 0.0;
        let t0 = secs(10_000.0);
        let mut c = Coordinator::new(&cfg, t0);
        c.handle(
            Input::Dictation(DictationSignal::Health {
                healthy: true,
                at: t0,
            }),
            t0,
        );
        (c, t0)
    }

    #[test]
    fn late_sync_reply_never_dispatches() {
        let (mut c, t0) = setup();
        let now = t0 + 10 * SECOND;
        say(
            &mut c,
            "Hey Zora x. Do it Zora.",
            now - 2 * SECOND,
            now - SECOND,
            now,
        );
        let fx = c.handle(Input::Tick, now);
        let id = fx
            .iter()
            .find_map(|e| {
                if let Effect::Sync { id, .. } = e {
                    Some(*id)
                } else {
                    None
                }
            })
            .unwrap();
        // Reply arrives 5 s later with no tick in between.
        let late = now + 5 * SECOND;
        let fx = c.handle(
            Input::Synced {
                id,
                at: late,
                healthy: true,
                capturing: false,
            },
            late,
        );
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Dispatch(_))),
            "{fx:?}"
        );
    }

    #[test]
    fn speech_older_than_the_history_horizon_is_store_only() {
        let (mut c, t0) = setup();
        let now = t0 + 2000 * SECOND;
        c.handle(Input::Tick, t0 + 1000 * SECOND);
        c.handle(Input::Tick, now);
        let old = t0 + 1200 * SECOND;
        let fx = say(&mut c, "Hey Zora x. Do it Zora.", old, old + SECOND, now);
        assert!(
            fx.iter().all(|e| matches!(e, Effect::Store { .. })),
            "{fx:?}"
        );
    }

    #[test]
    fn extreme_unknown_interval_does_not_panic() {
        let (mut c, t0) = setup();
        c.handle(
            Input::Dictation(DictationSignal::Unknown {
                from: i64::MIN,
                to: t0 + SECOND,
            }),
            t0 + SECOND,
        );
        let fx = say(
            &mut c,
            "Hey Zora x. Do it Zora.",
            t0 + 5 * SECOND,
            t0 + 6 * SECOND,
            t0 + 6 * SECOND,
        );
        assert!(!fx.iter().any(|e| matches!(e, Effect::Dispatch(_))));
    }
}

#[test]
fn flush_reports_a_failed_commit_and_keeps_the_batch() {
    let path = tmp("z.sqlite");
    let mut s = Store::open(&path, false).unwrap();
    s.append(1, 1.0, 2.0, "kept", "speech");
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(s.flush().is_err());
    blocker.execute_batch("COMMIT").unwrap();
    assert!(s.flush().is_ok());
    assert_eq!(s.segments_since(0.0).len(), 1);
}

#[test]
fn claude_event_array_output_is_summarized_from_the_result_event() {
    let spec = HarnessSpec::from_config(
        "claude",
        &Harness {
            workspace: "/tmp".into(),
            ..Harness::default()
        },
        &Config::default(),
    );
    let out = r#"[{"type":"system","subtype":"init"},{"type":"assistant","message":{}},{"type":"result","subtype":"success","is_error":false,"result":"Wrote ~/zora-workspace/test.txt"}]"#;
    let r = spec.summarize(0, out, "");
    assert!(r.success);
    assert_eq!(r.summary, "Wrote ~/zora-workspace/test.txt");
    let err = r#"[{"type":"system"},{"type":"result","is_error":true,"result":"boom"}]"#;
    assert!(!spec.summarize(0, err, "").success);
}

#[test]
fn okay_zora_do_it_ends_rather_than_restarts_with_user_aliases() {
    use zora_core::detector::{CommandDetector, DetectorConfig, DetectorOutput};
    use zora_core::model::Segment;
    let start = ["hey zora", "okay zora", "ok zora", "hey zara"].map(String::from);
    let end = ["do it zora", "zora do it", "do resora"].map(String::from);
    let cancel = ["zora cancel".to_string()];
    let mut d = CommandDetector::new(DetectorConfig::new(
        &start, &end, &cancel, 180.0, 30.0, 4000,
    ));
    let seg = |t: &str| Segment {
        generation: 0,
        session: 1,
        start: 0,
        end: 1_000_000_000,
        text: t.into(),
    };
    let out = d.feed(&seg("Hey Zara, run the tests. Okay Zora, do it."));
    let cmds: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let DetectorOutput::Command(c) = o {
                Some(c.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(cmds, ["run the tests. Okay"]);
    let mut d = CommandDetector::new(DetectorConfig::new(
        &start, &end, &cancel, 180.0, 30.0, 4000,
    ));
    let out = d.feed(&seg("Hey Zora, check delivery. Do resora."));
    assert!(
        out.iter()
            .any(|o| matches!(o, DetectorOutput::Command(c) if c.text == "check delivery"))
    );
}

#[test]
fn claude_output_truncated_at_the_front_still_yields_the_answer() {
    let spec = HarnessSpec::from_config(
        "claude",
        &Harness {
            workspace: "/tmp".into(),
            ..Harness::default()
        },
        &Config::default(),
    );
    let tail = r#"ols":["Task","Bash"]},{"type":"assistant","message":{}},{"type":"result","subtype":"success","is_error":false,"result":"The delivery assistant is running."}]"#;
    let r = spec.summarize(0, tail, "");
    assert!(r.success);
    assert_eq!(r.summary, "The delivery assistant is running.");
}

#[test]
fn name_sound_alikes_match_but_other_words_do_not() {
    use zora_core::detector::name_like;
    for t in ["zora", "zara", "zura", "sora", "zorra", "zorah", "zor"] {
        assert!(name_like(t, "zora"), "{t}");
    }
    for t in [
        "dora", "nora", "zero", "zoo", "sorted", "resora", "the", "a",
    ] {
        assert!(!name_like(t, "zora"), "{t}");
    }
}

#[test]
fn misheard_name_still_delimits_a_command() {
    let mut cfg = Config::default();
    cfg.capture.hold_back_seconds = 0.0;
    let mut d = zora_core::detector::CommandDetector::new(cfg.detector_config());
    let seg = |t: &str, s: i64| zora_core::model::Segment {
        generation: 0,
        session: 1,
        start: s,
        end: s + 1,
        text: t.into(),
    };
    let mut out = d.feed(&seg("Hey, Zora, reply with just the word palm.", 0));
    out.extend(d.feed(&seg("Do it, Zura.", 2)));
    assert!(out.iter().any(|o| matches!(o, zora_core::detector::DetectorOutput::Command(c) if c.text == "reply with just the word palm")), "{out:?}");
}

#[test]
fn any_configured_name_fills_the_name_slot() {
    use zora_core::detector::{CommandDetector, DetectorOutput};
    use zora_core::model::Segment;
    let cfg = Config {
        wake_names: vec!["zora".into(), "nova".into(), "jarvis".into()],
        ..Config::default()
    };
    let seg = |t: &str| Segment {
        generation: 0,
        session: 1,
        start: 0,
        end: 1_000_000_000,
        text: t.into(),
    };
    for (said, want) in [
        ("Hey Nova, list my tasks. Do it, Nova.", "list my tasks"),
        ("Hey Jarvis, open the log. Do it Jarvis.", "open the log"),
        ("Hey Zora, mixed names. Do it, Nova.", "mixed names"),
    ] {
        let mut d = CommandDetector::new(cfg.detector_config());
        let out = d.feed(&seg(said));
        assert!(
            out.iter()
                .any(|o| matches!(o, DetectorOutput::Command(c) if c.text == want)),
            "{said}: {out:?}"
        );
    }
    // A name that is not configured does not trigger.
    let mut d = CommandDetector::new(cfg.detector_config());
    assert!(
        d.feed(&seg("Hey Alexa, list my tasks. Do it, Alexa."))
            .is_empty()
    );
    let mut bad = Config {
        dry_run: true,
        ..Config::default()
    };
    bad.wake_names = vec!["hey you".into()];
    assert!(bad.validate().iter().any(|p| p.contains("wakeNames")));
}

mod early_listening {
    use zora_core::clock::{SECOND, secs};
    use zora_core::config::Config;
    use zora_core::coordinator::{Coordinator, Effect, Input};
    use zora_core::model::{Segment, ZoraEvent};
    use zora_core::suppression::DictationSignal;

    fn listening(fx: &[Effect]) -> usize {
        fx.iter()
            .filter(|e| matches!(e, Effect::Notify(ZoraEvent::CaptureStarted)))
            .count()
    }

    fn setup() -> (Coordinator, i64) {
        let t0 = secs(10_000.0);
        let mut c = Coordinator::new(&Config::default(), t0);
        c.handle(
            Input::Dictation(DictationSignal::Health {
                healthy: true,
                at: t0,
            }),
            t0,
        );
        (c, t0)
    }

    #[test]
    fn live_text_announces_listening_once_and_the_final_does_not_repeat_it() {
        let (mut c, t0) = setup();
        let now = t0 + 10 * SECOND;
        assert_eq!(
            listening(&c.handle(
                Input::Partial {
                    text: "Hey Nova".into()
                },
                now
            )),
            1
        );
        assert_eq!(
            listening(&c.handle(
                Input::Partial {
                    text: "Hey Nova, add a".into()
                },
                now + SECOND / 2
            )),
            0
        );
        let seg = Segment {
            generation: 0,
            session: 1,
            start: now - SECOND,
            end: now + 2 * SECOND,
            text: "Hey Nova, add a task.".into(),
        };
        let fx = c.handle(Input::Segment(seg), now + 3 * SECOND);
        assert_eq!(listening(&fx), 0, "{fx:?}");
        assert!(c.is_capturing());
    }

    #[test]
    fn live_text_is_ignored_while_dictating_or_without_a_wake_phrase() {
        let (mut c, t0) = setup();
        let now = t0 + 10 * SECOND;
        assert_eq!(
            listening(&c.handle(
                Input::Partial {
                    text: "we should ship on friday".into()
                },
                now
            )),
            0
        );
        c.handle(Input::Dictation(DictationSignal::Began(now)), now);
        assert_eq!(
            listening(&c.handle(
                Input::Partial {
                    text: "Hey Zora".into()
                },
                now + SECOND
            )),
            0
        );
    }
}

#[test]
fn every_cancel_phrase_works_with_every_name() {
    use zora_core::detector::{CommandDetector, DetectorOutput};
    use zora_core::model::{DiscardReason, Segment};
    let cfg = Config::default();
    let seg = |t: &str| Segment {
        generation: 0,
        session: 1,
        start: 0,
        end: 1_000_000_000,
        text: t.into(),
    };
    for said in [
        "Hey Zora, wipe it. Dismiss Zora.",
        "Hey Nova, wipe it. Stop it, Nova.",
        "Hey Zora, wipe it. Stop Zora!",
        "Hey Nova, wipe it. Never mind, Nova.",
    ] {
        let mut d = CommandDetector::new(cfg.detector_config());
        let out = d.feed(&seg(said));
        assert!(
            out.contains(&DetectorOutput::Discarded(DiscardReason::Cancelled)),
            "{said}: {out:?}"
        );
        assert!(
            !out.iter().any(|o| matches!(o, DetectorOutput::Command(_))),
            "{said}"
        );
    }
    // Outside a capture, a cancel phrase is ordinary speech.
    let mut d = CommandDetector::new(cfg.detector_config());
    assert!(d.feed(&seg("We should stop Zora from beeping.")).is_empty());
}

#[test]
fn greeting_fused_onto_the_name_still_wakes_zora() {
    use zora_core::detector::{CommandDetector, DetectorOutput};
    use zora_core::model::Segment;
    let cfg = Config::default();
    let seg = |t: &str| Segment {
        generation: 0,
        session: 1,
        start: 0,
        end: 1_000_000_000,
        text: t.into(),
    };
    for (said, want) in [
        (
            "Hazora, figure out the project. Do it, Zora.",
            "figure out the project",
        ),
        ("Heyzora check mail. Do it Zora.", "check mail"),
        ("Anova, list tasks. Do it, Nova.", "list tasks"),
    ] {
        let mut d = CommandDetector::new(cfg.detector_config());
        let out = d.feed(&seg(said));
        assert!(
            out.iter()
                .any(|o| matches!(o, DetectorOutput::Command(c) if c.text == want)),
            "{said}: {out:?}"
        );
        assert!(cfg.detector_config().mentions_start(said), "{said}");
    }
    // Ordinary words that merely end in letters like a name do not wake it.
    let mut d = CommandDetector::new(cfg.detector_config());
    assert!(
        d.feed(&seg(
            "The hazard report and the casanova movie. Do it, Zora."
        ))
        .is_empty()
    );
}

#[test]
fn claude_plain_text_answer_is_the_whole_summary() {
    let spec = HarnessSpec::from_config(
        "claude",
        &Harness {
            workspace: "/tmp".into(),
            ..Harness::default()
        },
        &Config::default(),
    );
    assert!(
        spec.argv("x")
            .windows(2)
            .any(|w| w == ["--output-format", "text"])
    );
    let r = spec.summarize(0, "I sent \"hi\" to Tyler.\n\nLink: https://example\n", "");
    assert!(r.success);
    assert_eq!(
        r.summary,
        "I sent \"hi\" to Tyler.\n\nLink: https://example"
    );
    let f = spec.summarize(1, "Error: not logged in\n", "");
    assert!(!f.success && f.summary.starts_with("exit 1: Error"));
}

#[test]
fn stop_it_outside_a_capture_targets_the_running_job() {
    use zora_core::detector::{CommandDetector, DetectorOutput};
    use zora_core::model::{DiscardReason, Segment};
    let cfg = Config::default();
    let seg = |t: &str| Segment {
        generation: 0,
        session: 1,
        start: 0,
        end: 1_000_000_000,
        text: t.into(),
    };
    let mut d = CommandDetector::new(cfg.detector_config());
    assert_eq!(
        d.feed(&seg("Okay, stop it, Nova.")),
        [DetectorOutput::StopRunning]
    );
    // Inside a capture the same phrase only forgets the capture.
    let mut d = CommandDetector::new(cfg.detector_config());
    let out = d.feed(&seg("Hey Zora, delete it. Stop it, Zora."));
    assert!(out.contains(&DetectorOutput::Discarded(DiscardReason::Cancelled)));
    assert!(!out.contains(&DetectorOutput::StopRunning));
}

#[test]
fn last_segments_returns_the_newest_lines_oldest_first() {
    let path = tmp("last.sqlite");
    let mut s = Store::open(&path, false).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    s.append(1, now - 7200.0, now - 7190.0, "too old", "speech");
    for i in 0..5 {
        let t = now - 50.0 + i as f64;
        s.append(1, t, t + 0.5, &format!("line {i}"), "speech");
    }
    s.flush().unwrap();
    let r = Store::open(&path, true).unwrap();
    let texts =
        |v: Vec<zora_core::store::StoredSegment>| v.into_iter().map(|s| s.text).collect::<Vec<_>>();
    assert_eq!(
        texts(r.last_segments(now - 3600.0, 3).unwrap()),
        ["line 2", "line 3", "line 4"]
    );
    assert_eq!(texts(r.last_segments(now - 3600.0, 100).unwrap()).len(), 5);
    assert_eq!(texts(r.last_segments(0.0, 100).unwrap())[0], "too old");
}

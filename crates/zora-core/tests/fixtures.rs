//! Golden behaviour fixtures (`fixtures/behavior`), first written to prove parity with the removed Swift v1 core.

use serde_json::Value;
use zora_core::clock::{Ns, secs};
use zora_core::config::Config;
use zora_core::coordinator::{Coordinator, Effect, Input};
use zora_core::model::{Segment, ZoraEvent};
use zora_core::suppression::DictationSignal;

const BASE: f64 = 1000.0;

fn t(v: &Value) -> Ns {
    secs(BASE + v.as_f64().expect("number"))
}

fn label(fx: &Effect, out: &mut Vec<String>, kinds: &mut Vec<String>) {
    match fx {
        Effect::Notify(ZoraEvent::CaptureStarted) => out.push("captureStarted".into()),
        Effect::Notify(ZoraEvent::CaptureDiscarded { reason }) => {
            out.push(format!("discarded:{}", reason.as_str()))
        }
        Effect::Notify(ZoraEvent::Degraded { .. }) => out.push("degraded".into()),
        Effect::Dispatch(c) => out.push(format!("command:{}", c.text)),
        Effect::Store { kind, .. } => kinds.push(kind.as_str().into()),
        _ => {}
    }
}

fn run_case(case: &Value) -> (Vec<String>, Vec<String>) {
    let mut config = Config::default();
    config.capture.hold_back_seconds = case.get("holdBack").and_then(Value::as_f64).unwrap_or(0.0);
    if let Some(m) = case.get("maxChars").and_then(Value::as_u64) {
        config.capture.max_chars = m as usize;
    }
    let mut now = secs(BASE);
    let mut c = Coordinator::new(&config, now);
    let (mut out, mut kinds) = (Vec::new(), Vec::new());

    // Answer sync barriers immediately with the monitor's current state.
    let run = |c: &mut Coordinator,
               input: Input,
               now: Ns,
               out: &mut Vec<String>,
               kinds: &mut Vec<String>| {
        let mut queue = std::collections::VecDeque::from([input]);
        while let Some(i) = queue.pop_front() {
            for fx in c.handle(i, now) {
                if let Effect::Sync { id, .. } = fx {
                    let s = c.suppression();
                    queue.push_back(Input::Synced {
                        id,
                        at: now,
                        healthy: s.healthy(),
                        capturing: s.is_dictating(),
                    });
                }
                label(&fx, out, kinds);
            }
        }
    };

    let steps = case["steps"].as_array().expect("steps");
    for step in steps {
        now = now.max(t(&step["at"]));
        let input = if let Some(h) = step.get("health") {
            Input::Dictation(DictationSignal::Health {
                healthy: h.as_bool().unwrap(),
                at: now,
            })
        } else if let Some(v) = step.get("began") {
            Input::Dictation(DictationSignal::Began(t(v)))
        } else if let Some(v) = step.get("ended") {
            Input::Dictation(DictationSignal::Ended(t(v)))
        } else if let Some(v) = step.get("unknown") {
            Input::Dictation(DictationSignal::Unknown {
                from: t(&v[0]),
                to: t(&v[1]),
            })
        } else if step.get("gap").is_some() {
            Input::Gap
        } else if let Some(g) = step.get("generation") {
            Input::TranscriptGeneration(g.as_u64().unwrap())
        } else if let Some(s) = step.get("say") {
            Input::Segment(Segment {
                generation: step.get("gen").and_then(Value::as_u64).unwrap_or(0),
                session: 1,
                start: t(&s[0]),
                end: t(&s[1]),
                text: s[2].as_str().unwrap().to_string(),
            })
        } else {
            Input::Tick
        };
        run(&mut c, input, now, &mut out, &mut kinds);
        run(&mut c, Input::Tick, now, &mut out, &mut kinds);
    }
    let last = now;
    while now < last + secs(config.capture.hold_back_seconds + 3.0) {
        now += secs(0.25);
        run(&mut c, Input::Tick, now, &mut out, &mut kinds);
    }
    (out, kinds)
}

#[test]
fn golden_fixtures() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/behavior");
    let mut failures = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let cases: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for case in cases {
            let cores = case
                .get("cores")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|v| v == "rust"))
                .unwrap_or(true);
            if !cores {
                continue;
            }
            count += 1;
            let expect: Vec<String> =
                serde_json::from_value(case.get("expectRust").unwrap_or(&case["expect"]).clone())
                    .unwrap();
            let (got, kinds) = run_case(&case);
            if got != expect {
                failures.push(format!(
                    "{}: expected {expect:?}, got {got:?}",
                    case["name"]
                ));
            }
            if let Some(k) = case.get("kinds") {
                let want: Vec<String> = serde_json::from_value(k.clone()).unwrap();
                if kinds != want {
                    failures.push(format!(
                        "{} kinds: expected {want:?}, got {kinds:?}",
                        case["name"]
                    ));
                }
            }
        }
    }
    assert!(count > 0);
    assert!(
        failures.is_empty(),
        "{} fixture failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

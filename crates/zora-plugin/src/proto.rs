//! Message types and framing (newline-delimited JSON, docs/protocol.md).

use crate::clock::{CLOCK_DOMAIN, Ns, now_ns};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::Mutex;

pub const PROTOCOL: u64 = 1;
/// Longest line the core accepts.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Notify {
    pub id: u64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub job: Option<String>,
    pub harness: Option<String>,
}

/// A message from the core. Unknown or malformed ones become `Other` and are ignored, as the protocol asks.
#[derive(Debug, Clone, PartialEq)]
pub enum CoreMsg {
    Hello {
        protocol: u64,
        generation: u64,
        clock: String,
        now_ns: Ns,
        config: Value,
    },
    /// `start` and `listening`: `until` is a timed pause's end in Unix seconds.
    Start {
        listening: bool,
        until: Option<f64>,
    },
    Listening {
        listening: bool,
        until: Option<f64>,
    },
    Sync {
        id: u64,
        cutoff_ns: Ns,
    },
    Notify(Notify),
    Clock {
        id: u64,
    },
    Stop,
    Other,
}

pub fn parse(line: &str) -> CoreMsg {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return CoreMsg::Other;
    };
    let s = |k: &str| v[k].as_str().map(String::from);
    // A message without `listening` must never be read as "listen".
    let listening = v["listening"].as_bool();
    match v["t"].as_str() {
        Some("hello") => CoreMsg::Hello {
            protocol: v["protocol"].as_u64().unwrap_or(0),
            generation: v["generation"].as_u64().unwrap_or(0),
            clock: s("clock").unwrap_or_default(),
            now_ns: v["now_ns"].as_i64().unwrap_or(0),
            config: v.get("config").cloned().unwrap_or_else(|| json!({})),
        },
        Some("start") => CoreMsg::Start {
            listening: listening.unwrap_or(false),
            until: v["until"].as_f64(),
        },
        // A malformed `listening` is read as a pause, so it can only ever close the microphone.
        Some("listening") => CoreMsg::Listening {
            listening: listening.unwrap_or(false),
            until: v["until"].as_f64(),
        },
        Some("sync") => match v["id"].as_u64() {
            Some(id) => CoreMsg::Sync {
                id,
                cutoff_ns: v["cutoff_ns"].as_i64().unwrap_or(0),
            },
            None => CoreMsg::Other,
        },
        Some("notify") => match v["id"].as_u64() {
            Some(id) => CoreMsg::Notify(Notify {
                id,
                kind: s("kind").unwrap_or_default(),
                title: s("title").unwrap_or_else(|| "Zora".into()),
                body: s("body").unwrap_or_default(),
                job: s("job"),
                harness: s("harness"),
            }),
            None => CoreMsg::Other,
        },
        Some("clock") => match v["id"].as_u64() {
            Some(id) => CoreMsg::Clock { id },
            None => CoreMsg::Other,
        },
        Some("stop") => CoreMsg::Stop,
        _ => CoreMsg::Other,
    }
}

/// The single writer to the core. It assigns `seq` while holding its lock, so sequence order is wire order.
pub struct Out {
    inner: Mutex<OutState>,
}

struct OutState {
    w: Box<dyn Write + Send>,
    seq: u64,
    generation: u64,
    broken: bool,
}

impl Out {
    pub fn new(w: Box<dyn Write + Send>) -> Out {
        Out {
            inner: Mutex::new(OutState {
                w,
                seq: 0,
                generation: 0,
                broken: false,
            }),
        }
    }

    pub fn stdout() -> Out {
        Out::new(Box::new(std::io::stdout()))
    }

    /// Answers `hello`. Every later message carries this generation.
    pub fn ready(&self, generation: u64, capabilities: &[&str]) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.generation = generation;
        st.seq = 0;
        let v = json!({"t": "ready", "protocol": PROTOCOL, "generation": generation,
                       "capabilities": capabilities, "clock": CLOCK_DOMAIN, "now_ns": now_ns()});
        write_line(&mut st, &v)
    }

    /// Adds `seq` and `generation`. Returns false once the core has stopped reading, or when the message is too
    /// long to send; that one is dropped without using up a sequence number.
    pub fn send(&self, v: Value) -> bool {
        self.send_if(v, || true)
    }

    /// Like `send`, but asks `still` once it holds the writer, just before writing: a message that waited
    /// behind another write is dropped if it has gone stale meanwhile (a pause, say).
    pub fn send_if(&self, mut v: Value, still: impl FnOnce() -> bool) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.broken || !still() {
            return false;
        }
        let seq = st.seq + 1;
        if let Some(o) = v.as_object_mut() {
            o.insert("seq".into(), json!(seq));
            o.insert("generation".into(), json!(st.generation));
        }
        if v.to_string().len() >= MAX_LINE {
            return false;
        }
        st.seq = seq;
        write_line(&mut st, &v)
    }

    pub fn is_broken(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).broken
    }
}

fn write_line(st: &mut OutState, v: &Value) -> bool {
    let mut line = v.to_string();
    if line.len() >= MAX_LINE {
        return false;
    }
    line.push('\n');
    if st.w.write_all(line.as_bytes()).is_err() || st.w.flush().is_err() {
        st.broken = true;
        return false;
    }
    true
}

/// What `hello` told this connection.
#[derive(Debug, Clone)]
pub struct Session {
    pub generation: u64,
    pub config: Value,
}

/// Reads `hello`, checks protocol and clock domain, and answers `ready`.
pub fn handshake(
    input: &mut impl BufRead,
    out: &Out,
    capabilities: &[&str],
) -> Result<Session, String> {
    let line = read_line(input)?.ok_or("core closed the connection before hello")?;
    let CoreMsg::Hello {
        protocol,
        generation,
        clock,
        config,
        ..
    } = parse(&line)
    else {
        return Err("expected hello".into());
    };
    if protocol != PROTOCOL {
        return Err(format!("unsupported protocol {protocol}"));
    }
    if clock != CLOCK_DOMAIN {
        return Err(format!(
            "clock domain {clock} does not match this plugin's {CLOCK_DOMAIN}"
        ));
    }
    if !out.ready(generation, capabilities) {
        return Err("cannot write to the core".into());
    }
    Ok(Session { generation, config })
}

/// One line without its newline; `None` at EOF. A line over the protocol limit is an error.
pub fn read_line(input: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut buf = Vec::new();
    let n = input
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_LINE {
        return Err("line from the core is too long".into());
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| "line from the core is not UTF-8".into())
}

/// The clock probe answer.
pub fn clock_reply(id: u64) -> Value {
    json!({"t": "clock", "id": id, "now_ns": now_ns()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    pub struct Buf(pub Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(b: &Buf) -> Vec<Value> {
        String::from_utf8(b.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn parses_core_messages() {
        assert_eq!(
            parse(r#"{"t":"start","listening":false,"until":1700000000.5}"#),
            CoreMsg::Start {
                listening: false,
                until: Some(1700000000.5)
            }
        );
        assert_eq!(
            parse(r#"{"t":"listening","listening":true}"#),
            CoreMsg::Listening {
                listening: true,
                until: None
            }
        );
        assert_eq!(
            parse(r#"{"t":"sync","id":7,"cutoff_ns":123}"#),
            CoreMsg::Sync {
                id: 7,
                cutoff_ns: 123
            }
        );
        let CoreMsg::Notify(n) = parse(
            r#"{"t":"notify","id":3,"kind":"jobStarted","title":"T","body":"B","job":"j1","harness":"claude","new":1}"#,
        ) else {
            panic!()
        };
        assert_eq!(
            (n.id, n.kind.as_str(), n.job.as_deref()),
            (3, "jobStarted", Some("j1"))
        );
        assert_eq!(parse(r#"{"t":"clock","id":2}"#), CoreMsg::Clock { id: 2 });
        assert_eq!(parse(r#"{"t":"stop"}"#), CoreMsg::Stop);
    }

    #[test]
    fn unknown_or_incomplete_messages_are_ignored() {
        assert_eq!(parse("{not json"), CoreMsg::Other);
        assert_eq!(parse(r#"{"t":"future-thing"}"#), CoreMsg::Other);
        assert_eq!(
            parse(r#"{"t":"listening","listening":"yes"}"#),
            CoreMsg::Listening {
                listening: false,
                until: None
            },
            "a malformed listening message pauses"
        );
        assert_eq!(parse(r#"{"t":"sync"}"#), CoreMsg::Other);
        // `start` without a state must not open the microphone.
        assert_eq!(
            parse(r#"{"t":"start"}"#),
            CoreMsg::Start {
                listening: false,
                until: None
            }
        );
    }

    #[test]
    fn handshake_answers_ready_then_numbers_messages() {
        let buf = Buf::default();
        let out = Out::new(Box::new(buf.clone()));
        let hello = format!(
            "{}\n",
            json!({"t":"hello","protocol":1,"generation":9,"clock":CLOCK_DOMAIN,"now_ns":1,"config":{"apps":["x"]}})
        );
        let s = handshake(&mut hello.as_bytes(), &out, &["transcript"]).unwrap();
        assert_eq!(s.generation, 9);
        assert_eq!(s.config["apps"][0], "x");
        out.send(json!({"t":"gap","at_ns":1,"reason":"x"}));
        out.send(json!({"t":"gap","at_ns":2,"reason":"x"}));
        let l = lines(&buf);
        assert_eq!(l[0]["t"], "ready");
        assert_eq!(l[0]["capabilities"][0], "transcript");
        assert!(l[0].get("seq").is_none());
        assert_eq!(
            (l[1]["seq"].as_u64(), l[2]["seq"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(l[2]["generation"], 9);
    }

    #[test]
    fn handshake_rejects_other_clock_domain() {
        let out = Out::new(Box::new(Buf::default()));
        let hello =
            r#"{"t":"hello","protocol":1,"generation":1,"clock":"elsewhere-ns","now_ns":1}"#;
        assert!(handshake(&mut hello.as_bytes(), &out, &["notify"]).is_err());
    }

    #[test]
    fn overlong_line_is_an_error() {
        let long = "x".repeat(MAX_LINE + 10);
        assert!(read_line(&mut long.as_bytes()).is_err());
        assert_eq!(
            read_line(&mut "ab\ncd".as_bytes()).unwrap().as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn oversized_message_keeps_the_sequence_unbroken() {
        let buf = Buf::default();
        let out = Out::new(Box::new(buf.clone()));
        assert!(out.send(json!({"t": "gap", "reason": "a"})));
        assert!(!out.send(json!({"t": "segment", "text": "x".repeat(MAX_LINE)})));
        assert!(out.send(json!({"t": "gap", "reason": "b"})));
        let l = lines(&buf);
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0]["seq"].as_u64(), l[1]["seq"].as_u64()),
            (Some(1), Some(2))
        );
        assert!(!out.is_broken());
    }

    #[test]
    fn a_message_gone_stale_while_waiting_for_the_writer_is_dropped() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        struct Gate(Mutex<mpsc::Receiver<()>>, Buf);
        impl Write for Gate {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.lock().unwrap().recv();
                self.1.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (release, wait) = mpsc::channel();
        let buf = Buf::default();
        let out = Arc::new(Out::new(Box::new(Gate(Mutex::new(wait), buf.clone()))));
        let allowed = Arc::new(AtomicBool::new(true));
        let a = {
            let out = out.clone();
            std::thread::spawn(move || out.send(json!({"t": "gap", "reason": "first"})))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let b = {
            let (out, allowed) = (out.clone(), allowed.clone());
            std::thread::spawn(move || {
                out.send_if(json!({"t": "segment", "text": "late"}), || {
                    allowed.load(Ordering::SeqCst)
                })
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        allowed.store(false, Ordering::SeqCst); // the pause arrives while both wait
        release.send(()).unwrap();
        release.send(()).unwrap();
        assert!(a.join().unwrap());
        assert!(!b.join().unwrap());
        let l = lines(&buf);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["reason"], "first");
    }

    #[test]
    fn broken_writer_reports_false() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let out = Out::new(Box::new(Fail));
        assert!(!out.send(json!({"t":"gap"})));
        assert!(out.is_broken());
    }
}

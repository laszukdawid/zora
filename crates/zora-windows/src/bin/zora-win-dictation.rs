//! The `dictation` plugin on Windows: reports when a configured dictation app (`dictation.windowsApps`) has an
//! active capture audio session, so Zora never runs a command spoken into it. It fails closed: no configured app,
//! any WASAPI failure, a capture device change or a stalled poll all make the state unknown. It never records audio.
//!
//! `zora-win-dictation watch` prints every capture session's process and state: start the dictation tool and
//! dictate to see which name to put in `dictation.windowsApps`.

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use zora_plugin::dictation::apps_from_config;
    use zora_plugin::proto::{CoreMsg, Out, clock_reply, handshake, parse, read_line};
    use zora_windows::com::Apartment;
    use zora_windows::sessions::{Req, SessionSource, Watcher, run};
    use zora_windows::wasapi::Reconnecting;

    // Entered once for this thread, and left after everything below has released its interfaces.
    let _com = match Apartment::enter(true) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zora-win-dictation: {e}");
            std::process::exit(1);
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("watch") => {
            println!(
                "Capture sessions (Ctrl+C to stop). Dictate now; put the program name into dictation.windowsApps."
            );
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut src = Reconnecting::new(tx);
            let mut last = None;
            loop {
                let now = src.snapshot();
                if last.as_ref() != Some(&now) {
                    match &now {
                        Ok(s) if s.sessions.is_empty() => println!("(no capture sessions)"),
                        Ok(s) => {
                            for x in &s.sessions {
                                let state = if x.active { "active" } else { "inactive" };
                                println!(
                                    "{state:9} {}",
                                    x.image.as_deref().unwrap_or("(unidentified)")
                                );
                            }
                        }
                        Err(e) => println!("cannot read sessions: {e}"),
                    }
                    println!("--");
                    last = Some(now);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        Some(_) => {
            eprintln!("usage: zora-win-dictation [watch]");
            std::process::exit(2);
        }
        None => {}
    }

    let out = Arc::new(Out::stdout());
    let session = match handshake(&mut std::io::stdin().lock(), &out, &["dictation"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zora-win-dictation: {e}");
            std::process::exit(1);
        }
    };
    let apps = apps_from_config(&session.config);
    let experimental = session.config["windowsExperimental"]
        .as_bool()
        .unwrap_or(false);
    eprintln!("zora-win-dictation: watching {} app name(s)", apps.len());
    let (tx, rx) = std::sync::mpsc::channel::<Req>();
    let mut src = Reconnecting::new(tx.clone());
    {
        let out = out.clone();
        std::thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            while let Ok(Some(line)) = read_line(&mut input) {
                match parse(&line) {
                    CoreMsg::Sync { id, .. } => {
                        let _ = tx.send(Req::Sync(id));
                    }
                    CoreMsg::Clock { id } => {
                        out.send(clock_reply(id));
                    }
                    CoreMsg::Stop => {
                        out.send(serde_json::json!({"t": "stopped"}));
                    }
                    _ => {}
                }
            }
            let _ = tx.send(Req::Quit);
        });
    }
    run(Watcher::new(apps, experimental), &mut src, rx, |msgs| {
        for m in msgs {
            out.send(m.to_json());
        }
    });
}

#[cfg(not(windows))]
fn main() {
    eprintln!("zora-win-dictation is Windows-only");
    std::process::exit(1);
}

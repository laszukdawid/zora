//! The `dictation` plugin: reports when a configured dictation app (`dictation.linuxApps`) captures audio, so
//! Zora never runs a command spoken into it. It fails closed: no PipeWire, no configured app, or a stalled
//! loop all report health unknown. It never records audio.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;
    use zora_linux::pw::{Req, run};
    use zora_plugin::dictation::apps_from_config;
    use zora_plugin::proto::{CoreMsg, Out, clock_reply, handshake, parse, read_line};

    let out = Arc::new(Out::stdout());
    let session = match handshake(&mut std::io::stdin().lock(), &out, &["dictation"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zora-dictation: {e}");
            std::process::exit(1);
        }
    };
    let apps = apps_from_config(&session.config);
    eprintln!("zora-dictation: watching {} app name(s)", apps.len());
    let (tx, rx) = pipewire::channel::channel::<Req>();
    let reader = {
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
        })
    };
    if let Err(e) = run(apps, out, rx) {
        eprintln!("zora-dictation: {e}");
        std::process::exit(1);
    }
    drop(reader);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("zora-dictation is Linux-only");
    std::process::exit(1);
}

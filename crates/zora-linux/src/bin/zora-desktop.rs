//! The `notify` plugin for the Linux desktop: notifications and the tray icon.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;
    use zora_plugin::proto::{Out, handshake, parse, read_line};

    let out = Arc::new(Out::stdout());
    let session = match handshake(&mut std::io::stdin().lock(), &out, &["notify"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zora-desktop: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        while let Ok(Some(line)) = read_line(&mut input) {
            if tx.send(parse(&line)).is_err() {
                break;
            }
        }
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(zora_linux::desktop::run(session.config, out, rx));
    std::process::exit(code);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("zora-desktop is Linux-only");
    std::process::exit(1);
}

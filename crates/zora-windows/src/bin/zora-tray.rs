//! The `notify` plugin for the Windows desktop: the notification-area icon, its menu and notifications.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use zora_plugin::proto::{Out, handshake, parse, read_line};

    let out = Arc::new(Out::stdout());
    let session = match handshake(&mut std::io::stdin().lock(), &out, &["notify"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zora-tray: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        while let Ok(Some(line)) = read_line(&mut input) {
            if tx.send(Some(parse(&line))).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
    std::process::exit(zora_windows::tray::run(&session.config, out, rx));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("zora-tray is Windows-only");
    std::process::exit(1);
}

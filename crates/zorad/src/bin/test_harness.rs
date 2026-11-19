//! Harness stand-in for the end-to-end tests on Windows, where they can't use `/bin/sh`. The last argument is
//! always the prompt.
//!
//!   print <text> | ran | pwd | sleep <secs> <text> | noisy   output for the job result
//!   stubborn <pidfile>   starts `hold` as a descendant, then waits, ignoring Ctrl+Break
//!   hold <pidfile>       writes its pid, then waits, ignoring Ctrl+Break

use std::io::Write;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let prompt = args.last().cloned().unwrap_or_default();
    match arg(0).as_str() {
        "print" => println!("{}", arg(1)),
        "ran" => println!("ran: {prompt}"),
        "pwd" => println!("{}", std::env::current_dir().unwrap().display()),
        "sleep" => {
            std::thread::sleep(Duration::from_secs(arg(1).parse().unwrap_or(60)));
            println!("{}", arg(2));
        }
        "noisy" => {
            let _ = std::io::stdout().write_all(&[0u8; 65536]);
            let _ = std::io::stderr().write_all(&[0u8; 65536]);
        }
        "stubborn" => {
            ignore_ctrl();
            let _ = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["hold", &arg(1)])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            std::thread::sleep(Duration::from_secs(120));
        }
        "hold" => {
            ignore_ctrl();
            let _ = std::fs::write(arg(1), std::process::id().to_string());
            std::thread::sleep(Duration::from_secs(120));
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }
}

#[cfg(windows)]
fn ignore_ctrl() {
    unsafe extern "system" fn ignore(_event: u32) -> i32 {
        1
    }
    // SAFETY: registers a handler that only returns TRUE.
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore), 1) };
}

#[cfg(unix)]
fn ignore_ctrl() {}

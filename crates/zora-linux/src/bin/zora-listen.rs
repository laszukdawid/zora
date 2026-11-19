//! The `transcript` plugin: local speech to text. Nothing is sent anywhere and no audio is stored.

use std::path::PathBuf;
use zora_plugin::listen::{FileInput, ScriptedRecognizer, run_listen_plugin};

const USAGE: &str = "usage: zora-listen [--file <wav>] [--fake-stt <script>]
       zora-listen setup [--force]       download the speech model (about 130 MB, once)
       zora-listen transcribe <wav>      print what the model hears in a WAV file

With no arguments it is a zorad plugin (docs/protocol.md) that listens to the default microphone.
--file plays a WAV file instead of the microphone, in real time. --fake-stt (tests only) replaces the
model: each stretch of sound becomes the next line of the script.";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            0
        }
        Some("setup") => linux::setup(args.iter().any(|a| a == "--force")),
        Some("transcribe") => match args.get(1) {
            Some(f) => linux::transcribe(&PathBuf::from(f)),
            None => {
                eprintln!("{USAGE}");
                2
            }
        },
        _ => plugin(&args),
    };
    std::process::exit(code);
}

fn plugin(args: &[String]) -> i32 {
    let file = flag(args, "--file").map(PathBuf::from);
    let fake = flag(args, "--fake-stt").map(PathBuf::from);
    let input = match &file {
        Some(f) => match FileInput::open_wav(f) {
            Ok(i) => Some(i),
            Err(e) => {
                eprintln!("zora-listen: {e}");
                return 1;
            }
        },
        None => None,
    };
    match (input, fake) {
        (Some(input), Some(script)) => {
            let lines = match std::fs::read_to_string(&script) {
                Ok(s) => s.lines().map(String::from).collect::<Vec<_>>(),
                Err(e) => {
                    eprintln!("zora-listen: {}: {e}", script.display());
                    return 1;
                }
            };
            run_listen_plugin(move || input, move |_| Ok(ScriptedRecognizer::new(lines)))
        }
        (input, None) => linux::run(input),
        (None, Some(_)) => {
            eprintln!("zora-listen: --fake-stt needs --file");
            2
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use serde_json::json;
    use std::path::Path;
    use zora_linux::capture::CpalInput;
    use zora_linux::stt::{self, Sherpa};
    use zora_plugin::listen::{FileInput, Recognizer, run_listen_plugin};

    pub fn run(file: Option<FileInput>) -> i32 {
        match file {
            Some(input) => run_listen_plugin(move || input, Sherpa::load),
            None => run_listen_plugin(CpalInput::default, Sherpa::load),
        }
    }

    pub fn setup(force: bool) -> i32 {
        match stt::setup(&json!({}), force) {
            Ok(dir) => {
                println!("speech model ready in {}", dir.display());
                0
            }
            Err(e) => {
                eprintln!("zora-listen setup: {e}");
                1
            }
        }
    }

    pub fn transcribe(path: &Path) -> i32 {
        let input = match FileInput::open_wav(path) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("zora-listen: {e}");
                return 1;
            }
        };
        let mut rec = match Sherpa::load(&json!({})) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("zora-listen: {e}");
                return 1;
            }
        };
        let samples = input.mono16k();
        let secs = |n: u64| n as f64 / 16_000.0;
        for chunk in samples.chunks(1600) {
            rec.accept(chunk);
            while let Some(u) = rec.endpoint() {
                println!("{:7.2} {:7.2}  {}", secs(u.start), secs(u.end), u.text);
            }
        }
        if let Some(u) = rec.finish() {
            println!("{:7.2} {:7.2}  {}", secs(u.start), secs(u.end), u.text);
        }
        0
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use std::path::Path;
    use zora_plugin::listen::FileInput;

    const ONLY: &str = "zora-listen: the microphone and speech model are Linux-only; elsewhere only --file with --fake-stt works";

    pub fn run(_: Option<FileInput>) -> i32 {
        eprintln!("{ONLY}");
        1
    }
    pub fn setup(_: bool) -> i32 {
        eprintln!("{ONLY}");
        1
    }
    pub fn transcribe(_: &Path) -> i32 {
        eprintln!("{ONLY}");
        1
    }
}

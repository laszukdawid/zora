# Developing Zora

Zora is two programs. `zorad` is the core, written in Rust: it holds all the logic (command detection, dictation
suppression, the transcript store, running jobs). Plugins are native programs that do what only the operating
system can: listen to the microphone, transcribe, watch for dictation apps, and show notifications. They talk to
the core over a small [JSON-lines protocol](protocol.md). On macOS the plugin is `Zora.app` itself, which also owns
the menu-bar icon. [design.md](design.md) explains the architecture and the decisions behind it.

## Layout

| Path | What |
|---|---|
| `crates/zora-core` | Platform-neutral logic: detector, dictation suppression, coordinator, store, config, harness argv |
| `crates/zorad` | Daemon and CLI: plugin host, job guard/gate, recovery, pause state. `zora-script-plugin` is a reference plugin; `zora-acdc` puts voice commands on the ACDC task board. |
| `crates/zora-plugin` | The plugin side of the protocol, shared by native plugins: framing, the microphone gate, the listening engine, dictation health rules, the tray model. No OS APIs. |
| `crates/zora-listen` | The speech plugin `zora-listen` (Linux; its file mode builds everywhere). A crate of its own so only it links the ONNX runtime. |
| `crates/zora-linux` | The other Linux plugins: `zora-dictation` (PipeWire) and `zora-desktop` (notifications and tray) |
| `Sources/ZoraMac`, `Sources/zora` | macOS plugin (Swift): SpeechAnalyzer, CoreAudio dictation monitor, notifications, menu bar and Settings window. This is `Zora.app`'s main executable. |
| `Sources/ZoraCore`, `Tests/ZoraCoreTests` | The v1 Swift core, kept until the Rust port has proven parity |
| `fixtures/behavior` | Golden behaviour fixtures that both cores must pass |
| `packaging/macos`, `packaging/linux` | App bundle and signing; systemd user unit, installer and example config |

## Build and test

```bash
cargo test --workspace     # golden fixtures, core units, end-to-end tests with the reference plugin
swift test                 # macOS adapters and the v1 core, including the same golden fixtures
docker run --rm --platform linux/arm64 -v "$PWD":/src:ro -w /src rust:1.98 bash -c \
  'apt-get update && apt-get install -y pkg-config clang libclang-dev libasound2-dev libpipewire-0.3-dev \
   && cp -r /src /w && cd /w && cargo test --workspace'        # the Rust suite on Linux, with the Linux plugins
packaging/macos/bundle.sh --install                            # build, sign and install Zora.app
```

`Zora.app` starts the core itself (`Contents/MacOS/zorad`), so microphone and notification permissions stay with
the app's identity. `ZORA_SIGN_IDENTITY` picks the signing identity; the default is the first "Apple Development"
one. An ad-hoc signature would lose the microphone permission on every rebuild.

## Linux

Targets are Ubuntu 24.04 or newer and Arch, with PipeWire as the sound server (the default on both). Three plugins
do the OS work; `zorad` starts them from the config's `"plugins"` map:

| Plugin | Capability | How |
|---|---|---|
| `zora-listen` | transcript | Microphone through cpal (PipeWire, or ALSA as a fallback); speech to text with sherpa-onnx's streaming Zipformer (`sherpa-onnx-streaming-zipformer-en-20M-2023-02-17`, int8, one thread, its own endpointing). English only. Audio stays in the process and is never written anywhere. |
| `zora-dictation` | dictation | Binds PipeWire nodes and watches capture streams (`Stream/Input/Audio`) whose `application.process.binary`, `application.name` or `application.id` equals a name in `dictation.linuxApps`. A stream in the running state means the app is dictating. |
| `zora-desktop` | notify | Desktop notifications (`org.freedesktop.Notifications` on the session bus) and the tray icon (StatusNotifierItem). |

### Build

```bash
# Ubuntu 24.04+
sudo apt install build-essential curl pkg-config clang libclang-dev libasound2-dev libpipewire-0.3-dev bzip2
# Arch
sudo pacman -S --needed base-devel curl pkgconf clang alsa-lib pipewire bzip2
packaging/linux/install.sh          # builds, installs into ~/.local/bin, downloads the speech model, installs the unit
systemctl --user enable --now zora
```

The build downloads sherpa-onnx's prebuilt static library from its GitHub release (the `sherpa-onnx-sys` crate does
this), so it needs network access. Ubuntu 22.04 is too old: its PipeWire headers predate what cpal needs.

At run time the plugins need PipeWire with its session manager (WirePlumber), `curl` and `bzip2` for
`zora-listen setup`, and for the tray a desktop that shows StatusNotifierItem icons: KDE Plasma and most panels do;
on GNOME it needs the AppIndicator extension (preinstalled on Ubuntu; `gnome-shell-extension-appindicator` on Arch).
The tray's Settings dialogs use `zenity` or `kdialog`; without either, the tray says which command to run instead.

### Config

`packaging/linux/config.json` is a complete starting config; `install.sh` copies it when there is none yet.

```json
"dictation": { "linuxApps": ["nerd-dictation"], "monitor": "plugin" },
"plugins": {
  "listen":    { "command": ["~/.local/bin/zora-listen"] },
  "dictation": { "command": ["~/.local/bin/zora-dictation"] },
  "desktop":   { "command": ["~/.local/bin/zora-desktop"] }
}
```

- The plugin named `desktop` gets the events listed under `notifications.desktop` (by default the same ones the
  macOS app shows). Its config takes `tray` and `notifications` (both default `true`), `zorad` (path to `zorad`,
  default: next to the plugin, then `PATH`) and `zoradConfig` (a `--config` path for the tray's `zorad` calls).
- `zora-listen` takes `model` (a model directory; default `$XDG_DATA_HOME/zora/models/<model>`), `threads` (default
  1), `endpointSeconds` (silence that ends an utterance, default 1.0) and `partials` (live text for the early
  "listening" notice, default `true`).
- `linuxApps` lists the dictation tool by process binary, application name or id; `pw-dump | grep application.`
  while it records shows what it reports. With no names listed, the monitor reports itself unhealthy and no command
  runs. Without a dictation tool, set `"dictation": {"monitor": "none"}` and drop the `dictation` plugin.

### How the pieces behave

- **Microphone.** `zora-listen` opens the microphone only after its model has loaded and the core has said
  `listening: true`, closes it on a pause, and drops audio it has not transcribed yet. It also closes it when the
  core goes away (it is the core's child and exits when its input closes). Open, close and stop run in one thread in
  message order, so a late resume can never reopen it after a pause or a stop.
- **Dictation monitor.** It fails closed: health is unknown until the first complete snapshot (a PipeWire core
  `sync` round trip after binding every node), when PipeWire is not running or the connection drops (it reconnects
  every second), and when observations stall for more than 1.5 s. A pure PulseAudio or ALSA system is not supported
  and stays unknown, so no command runs there.
- **Tray.** `zora-desktop` is a notify plugin that `zorad` starts, not a separate autostart program: the protocol
  already delivers the listening state and every event it shows, and the icon goes away with the service instead of
  claiming a state nobody reports. Its menu matches the macOS one (state, Pause Listening / Resume Listening, Pause
  for 1 Hour, Settings, the last five results, Restart Zora) and changes things only through `zorad pause`, `resume`
  and `settings set`. Unlike on macOS the tray can't close the microphone itself; the core passes a pause on within
  a second. Settings edits the names and the project folder; phrases are edited with `zorad settings set`.
- **Testing without a microphone.** `zora-listen --file x.wav` plays a WAV file as the microphone (in real time, so
  it goes through the same pause handling). `zora-listen transcribe x.wav` prints what the model hears.
  `--fake-stt <script>` (tests only) replaces the model: each stretch of sound becomes the next line of the script;
  `crates/zora-listen/tests/e2e_file.rs` runs `zorad` with it.

## Command line

| Command | What it does |
|---|---|
| `zorad status` | Config problems, listening state, store location, jobs that need attention |
| `zorad pause [30m]` / `zorad resume` | Turn listening off (until resumed, or for 1 minute to a week) and on again |
| `zorad names [add\|remove <name>]` | List or change the names Zora answers to |
| `zorad settings [set '<json>']` | Read or change the settings the menu bar edits (`wakeNames`, `phrases`, `workspace`) |
| `zorad transcript --since 30m [--last N] [--json]` | Print the stored transcript (the newest N lines with `--last`) |
| `zorad commands [--since 4h] [--json]` | Voice commands and their jobs, never the transcript |
| `zorad jobs [cancel\|clear <id>]` | List unfinished jobs; cancel a running one; clear one after checking it by hand |
| `zorad simulate script.txt` | Feed a scripted transcript through the command logic (dry run) |
| `zora setup [--project <dir>]` (macOS) | Permissions, speech model, config and project folder |
| `zora status` (macOS) | Permissions, speech model, login item, dictation monitor |
| `zora install` / `uninstall` (macOS) | Start Zora at login, or stop doing so |

On macOS both live in `~/Applications/Zora.app/Contents/MacOS/`.

The menu bar on every platform should go through `zorad` (`pause`, `resume`, `settings`) rather than edit files
itself, so validation and the pause rules live in one place. The running core picks up changes within a second.

## Pause

`zorad pause` writes `pause.json` next to the transcript store; `zorad resume` deletes it. The core checks it once a
second and tells the plugins (`start` and `listening` messages in [protocol.md](protocol.md)). While paused:

- the speech plugin (`Zora.app` on macOS, `zora-listen` on Linux) releases the microphone and throws away audio it
  had not transcribed yet,
- the core drops any transcript text that still arrives, and anything that started before the last resume,
- an open capture ends; a command already completed before the pause still runs.

A file that can't be read counts as paused, so a damaged file never turns the microphone back on. The menu's
**Pause** also closes the microphone at once, before the core has seen the file, and the macOS app keeps the
microphone closed whenever the core is not running.

## Names and phrases

Zora answers to several names, so anyone can pick one they can say comfortably. The defaults are `zora` and `nova`.
Phrases use the word "zora" as the name slot, so `"hey zora"` and `"do it zora"` cover every listed name. Each name
also matches its sound-alikes (for "zora": "zara", "zura", "sora"), because speech engines rarely spell made-up names
the same way twice. A name must be one word of at least 3 letters.

## Configuration

Config lives at `~/.config/zora/config.json`. Missing keys fall back to defaults, and v1 config files still load.
Names, phrases, capture limits and the default harness apply live; everything else needs a restart.

```json
{
  "locale": "en-US",
  "wakeNames": ["zora", "nova"],
  "retentionHours": 4,
  "phrases": { "start": ["hey zora"], "end": ["do it zora", "zora do it"], "cancel": ["zora cancel", "never mind zora"] },
  "capture": { "holdBackSeconds": 1.5, "timeoutSeconds": 180, "maxGapSeconds": 30, "maxChars": 4000 },
  "dictation": { "bundleIDs": ["com.typewhisper.mac"], "linuxApps": ["my-dictation-tool"], "padSeconds": 1, "monitor": "plugin" },
  "defaultHarness": "claude",
  "harnesses": {
    "claude": { "type": "claude", "workspace": "~/projects/some-repo", "permissionMode": "acceptEdits" },
    "codex":  { "type": "argv", "workspace": "~/projects/some-repo", "executable": "codex", "args": ["exec", "{prompt}"] }
  },
  "plugins": { "macos": { "config": { "sounds": true } } },
  "notifications": {
    "macos": ["captureStarted", "commandQueued", "jobStarted", "jobSucceeded", "jobFailed", "captureDiscarded", "degraded"],
    "log": ["*"]
  }
}
```

- `permissionMode` is one of `acceptEdits`, `auto`, `dontAsk`, `manual` or `plan`. Without it, Claude Code uses its
  own settings, and anything it would ask about is refused because nobody is there to answer.
- `{prompt}` is always passed as a single argument and never goes through a shell.
- Zora won't start if any harness setting contains a permission-bypass flag (`bypassPermissions`,
  `--dangerously-skip-permissions`).
- Jobs on the same workspace run one at a time.
- `plugins.macos.config.sounds: false` turns the menu-bar sounds off.

## ACDC task board

`zora-acdc` is a notify plugin for the [ACDC](https://github.com/osano/acdc) task board on the same machine. Each
voice command becomes a board task (tag `zora`). Zora claims it while the job runs, then moves it to review with the
agent's summary, or to blocked with the reason it failed. If Zora stops mid-job, the task is blocked with a note.
The board's own Zora plugin reads `zorad commands --json` and `zorad transcript --json` and shows the jobs and the
last 100 transcript lines on a Zora tab. That copies the transcript into the board's database, where agents can read
it; the plugin's `transcript` setting turns that off.

```json
"plugins": { "acdc": { "command": ["~/Applications/Zora.app/Contents/MacOS/zora-acdc"],
                       "config": { "url": "http://127.0.0.1:8787/api", "workspace": null, "onSuccess": "review" } } },
"notifications": { "acdc": ["commandQueued", "jobStarted", "jobSucceeded", "jobFailed"] }
```

- On Linux the binary is `~/.local/bin/zora-acdc`.
- The URL must be `127.0.0.1` or `localhost`, so command text stays on this machine. The board holds it like any
  task text, and agents on the board can read it.
- Other settings: `agent` (the name Zora works under, default `zora`), `tags`, `claimMinutes` (default 60),
  `onSuccess` (`review` or `done`).
- Notification bodies are cut at 1000 characters, so a longer command shows shortened on its task.

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
| `fixtures/behavior` | Golden behaviour fixtures: transcript and dictation events in, expected commands out |
| `packaging/macos`, `packaging/linux` | App bundle and signing; systemd user unit, installer and example config |

## Build and test

```bash
cargo test --workspace     # golden fixtures, core units, end-to-end tests with the reference plugin
swift build                # the macOS app (Swift); its logic lives in the Rust core and is tested there
docker run --rm --platform linux/arm64 -v "$PWD":/src:ro -w /src rust:1.98 bash -c \
  'apt-get update && apt-get install -y pkg-config clang libclang-dev libasound2-dev libpipewire-0.3-dev \
   && cp -r /src /w && cd /w && cargo test --workspace'        # the Rust suite on Linux, with the Linux plugins
packaging/macos/bundle.sh --install                            # build, sign and install Zora.app
```

A source build needs Xcode 26, Rust 1.89 or later, and an "Apple Development" signing certificate (Xcode creates
one for free with any Apple ID under Xcode → Settings → Accounts). `bundle.sh --install` installs to
`~/Applications`; run `~/Applications/Zora.app/Contents/MacOS/zora setup` afterwards.

`Zora.app` starts the core itself (`Contents/MacOS/zorad`), so microphone and notification permissions stay with
the app's identity. `ZORA_SIGN_IDENTITY` picks the signing identity; the default is the first "Apple Development"
one. An ad-hoc signature would lose the microphone permission on every rebuild.

## Releases

Pushing a tag `v<version>` that matches the Cargo workspace version runs `.github/workflows/release.yml`:

1. **build** compiles an unsigned Zora.app. It has no secrets.
2. **sign** runs in the `release` environment, which holds the secrets. It signs the app with the Developer ID
   certificate, notarizes and staples the app and `Zora-<version>.dmg`, and checks the app inside the dmg with
   Gatekeeper.
3. **publish** checks that the tag still points at the built commit, then creates a **draft** GitHub release with
   the dmg and its SHA-256. Check it, then publish it.

Secrets of the `release` environment:

| Secret | What |
|---|---|
| `MACOS_CERT_P12` | The "Developer ID Application" certificate and its private key, exported as .p12 and base64-encoded |
| `MACOS_CERT_PASSWORD` | The .p12 export password |
| `NOTARY_KEY` | Contents of an App Store Connect API key (`AuthKey_<id>.p8`) with the Developer role |
| `NOTARY_KEY_ID`, `NOTARY_ISSUER` | That key's ID and the issuer ID shown on App Store Connect → Users and Access → Integrations |

Anyone who can push a `v*` tag can run the signing job, and it runs scripts from the tagged commit. So keep the
secrets behind two repository settings: the `release` environment requires your approval (Settings →
Environments → release → Required reviewers), and a tag ruleset lets only maintainers create `v*` tags and nobody
move or delete them. The build job also refuses a tag whose commit isn't on `main` or whose CI run on `main`
hasn't passed, so wait for CI to finish before you push the tag. Approve the signing job only for a tag you pushed
yourself.

To build a release locally instead, store the notarization credentials once with
`xcrun notarytool store-credentials zora-notary --key <path.p8> --key-id <id> --issuer <issuer>`, then run
`ZORA_NOTARY_PROFILE=zora-notary packaging/macos/release.sh`. The dmg lands in `dist/`.

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

`packaging/linux/config.json` is a complete starting config; `install.sh` copies it to `~/.config/zora/config.json`
(where `zorad` reads it) when there is none yet. It lists no dictation app, so no command runs until you name yours
or turn the monitor off (below).

```json
"dictation": { "linuxApps": [], "monitor": "plugin" },
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
- `linuxApps` names the program that actually records for your dictation tool, by process binary, application name
  or id. That is often not the tool itself: Nerd Dictation, for one, records through a helper such as `parec` by
  default. Run `zora-dictation watch`, dictate, and use the name it shows for the stream that turns running (a
  generic helper like `parec` also pauses Zora when anything else records with it, which is the safe direction).
  With no names listed, the monitor reports itself unhealthy and no command runs. Without a dictation tool, set
  `"dictation": {"monitor": "none"}` and drop the `dictation` plugin.

### How the pieces behave

- **Microphone.** `zora-listen` opens the microphone only after its model has loaded and the core has said
  `start` and then `listening: true`, closes it on a pause, and drops audio it has not transcribed yet (from the moment
  the pause arrives, before the device has finished closing; a result still being decoded is not sent). It also closes it when the
  core goes away (it is the core's child and exits when its input closes). Open, close and stop run in one thread in
  message order, so a late resume can never reopen it after a pause or a stop. One window remains: with cpal's
  PipeWire backend (and WASAPI on Windows, where `play()` queues `IAudioClient::Start` for cpal's worker thread),
  `play()` only queues the start, so a pause that arrives in that moment can find the device starting; a pause
  seen right after `play()` stops the stream at once. No audio from it is used (it belongs to a revoked epoch), and the stream is closed as soon as the mic
  thread handles the pause; the device is only briefly active.
- **Dictation monitor.** It reports a dictation stream starting or stopping as soon as PipeWire says so, so even a
  short one between two checks is suppressed. It fails closed: health is unknown until the first complete snapshot
  (a PipeWire core `sync` round trip that returns after every bound node has sent its state), when PipeWire is not
  running, when the connection drops or a round trip goes unanswered for 3 s (it reconnects every second), and when
  observations stall for more than 1.5 s. A node counts as unknown from its discovery until its properties show
  what it is; a capture stream that never names its app stays unknown for as long as it exists, so no command runs
  meanwhile. A running dictation stream that disappears, or was running when the connection dropped, is held:
  it still counts as capturing (with health unknown) until PipeWire shows its state again, the server turns out to
  have restarted, or 10 s pass. PipeWire also "removes" a node it merely hides from this client, and a stream
  that comes back, even under another name, is still the dictation stream. A pure PulseAudio or ALSA system is not supported and stays unknown, so
  no command runs there.
- **Gate: dictation must be visible.** A capture stream that PipeWire permissions (a portal or permission manager)
  hide from Zora can't be detected at all. The dictation tool must stay visible to the user's session.
- **Speech model.** `zora-listen setup` installs into `<models>/versions/` (marked as setup's own) and points
  `<models>/<model>` at the new version with one atomic link rename, holding a lock so only one setup runs at a time.
  Setup never deletes a version. `zora-listen prune` removes all but the current one from that directory (and
  nothing else); run it while Zora is stopped, since a starting plugin may be about to read an older version. A
  model directory placed at `<models>/<model>` by hand is used as it is; `setup --force` refuses to replace it.
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

## Windows

The core (`zorad`) builds and passes the full test suite on Windows 10 and later; CI runs it on `windows-2025`.
The plugins are new and have so far only been compiled, not run on Windows. **Windows is experimental: no
voice command runs there by default.** The dictation monitor reports health unknown until you opt in with
`"dictation": {"windowsExperimental": true}` (or set `"monitor": "none"` if you use no dictation app). WASAPI
offers no way to prove that every session callback has been delivered when the core asks for a `sync`: the monitor
drains what has arrived and treats anything it can't account for as unknown, but a callback still in flight at that
moment can't be seen. Until that is verified on hardware, the safe default is to run no commands.

| Plugin | Capability | How |
|---|---|---|
| `zora-listen` | transcript | The same plugin as on Linux: cpal (WASAPI, the default microphone) and sherpa-onnx's streaming Zipformer. The model goes to `%LOCALAPPDATA%\Zora\models`; `zora-listen setup` needs the `curl.exe` and `tar.exe` that ship with Windows 10 and later. Links need privileges on Windows, so setup names the model version in use in `<model>.version` instead of linking it. |
| `zora-win-dictation` | dictation | Event-driven on WASAPI: it registers for endpoint changes, for new sessions on every capture endpoint (before enumerating them) and for each session's state changes, and reports dictation as the events arrive, so a capture that starts and stops between two checks is still seen. A session that is active and whose process image is named in `dictation.windowsApps` (by file name, with or without `.exe`, any case) means dictation; so does an active session whose process can't be identified, including one shared by several processes. Every 250 ms a snapshot reconciles: a session no event announced, a state the events didn't report, a device change, a failed registration or callback, or an event that arrives out of order makes the time since the last agreement unknown. Health is also unknown with no names configured, on any WASAPI error and after a stalled check. `zora-win-dictation watch` lists capture sessions so you can find the name to configure. |
| `zora-tray` | notify | The notification-area icon with the same menu as macOS and Linux (state, Pause/Resume Listening, Pause for 1 Hour, Settings → Names / Project Folder, the last five results, Restart Zora) and notifications as balloons, which Windows 10 and 11 show as toasts. Changes go only through `zorad pause`, `resume` and `settings`. Restart Zora runs the scheduled task `\Zora\Zora` (option `task`). |

```powershell
cargo test --workspace -- --test-threads=4   # needs the MSVC build tools (bundled SQLite is compiled from C)
cargo run -p zorad -- run --dry-run
powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1
```

`install.ps1` builds the four programs (or takes them from `-From <folder>`), copies them to
`%LOCALAPPDATA%\Zora\bin`, writes `%APPDATA%\Zora\config.json` only if there is none, runs `zora-listen setup`
(`-NoModel` skips it) and registers a scheduled task `\Zora\Zora` that starts `zorad run` at logon without a console
window (`-NoTask` skips it). Running the task again stops the old instance first, which is how the tray restarts
Zora. The config lists no dictation app and no project folder: set both before Zora runs commands (the script
prints how). zorad's own log goes nowhere when started by the task; run `zorad run` in a terminal to see it. It never replaces a task of that name that starts something else, and never overwrites an existing config,
even one created while it runs.

**Gates (Windows, untested on hardware).** None of the Windows plugins has run on a real machine yet:
- the dictation monitor's events and reconciliation against a real dictation tool, a multi-process session, a
  headset plugged in or removed while dictating, and an audio service restart;
- `zora-listen` live capture, including the short window in which a pause arriving just as `play()` queues
  `IAudioClient::Start` can find the device starting (a pause seen right after `play()` stops it at once; cpal
  offers no hook to close it further);
- the tray's menu, balloons, settings dialogs and Restart, and `install.ps1` with its scheduled task.

What differs from Unix (details in [design.md](design.md)):

- Files: config in `%APPDATA%\Zora\config.json`, store and pause state in `%LOCALAPPDATA%\Zora`. They rely on
  the user profile's permissions instead of `0600`.
- Jobs run inside a named Job Object instead of a process group; Ctrl+Break stands in for SIGTERM.
- The `path` setting is `;`-separated. Its default looks in `~\.local\bin` (the native Claude Code installer),
  `%APPDATA%\npm`, Node.js, Git and the system folders. A batch file never receives the prompt: npm's
  `claude.cmd` is run as the `node` script it wraps, and other `.cmd`/`.bat` harnesses are refused.
- `zorad run --attached-plugin` talks the protocol on stdin/stdout instead of fds 3/4 (see
  [protocol.md](protocol.md)).
- The end-to-end tests use `zora-test-harness` in place of `/bin/sh` scripts. The escaped-pipe test is Unix-only:
  nothing can leave a Windows job.

To type-check the Windows code from macOS or Linux, use `rustup target add x86_64-pc-windows-msvc` and
`cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc`. Bundled SQLite needs a C compiler for
that target, so point `CC_x86_64_pc_windows_msvc` and `AR_x86_64_pc_windows_msvc` at a stub that only creates
its output file (nothing is linked), or rely on CI.

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

On macOS both live in `Zora.app/Contents/MacOS/`: `/Applications` for a release, `~/Applications` for a
`bundle.sh --install` build.

`zorad run` exits with status 78 (`EX_CONFIG`) when the config is invalid. Supervisors must not restart it in a
loop: the macOS app shows "needs setup" in the menu bar and starts the core again once `zorad settings` reports no
problems, and the systemd unit sets `RestartPreventExitStatus=78`.

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
  "dictation": { "bundleIDs": ["com.typewhisper.mac"], "linuxApps": ["my-dictation-tool"], "windowsApps": ["MyDictation.exe"], "windowsExperimental": false, "padSeconds": 1, "monitor": "plugin" },
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

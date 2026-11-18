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
| `crates/zorad` | Daemon and CLI: plugin host, job guard/gate, recovery, pause state. `zora-script-plugin` is a reference plugin. |
| `Sources/ZoraMac`, `Sources/zora` | macOS plugin (Swift): SpeechAnalyzer, CoreAudio dictation monitor, notifications, menu bar and Settings window. This is `Zora.app`'s main executable. |
| `Sources/ZoraCore`, `Tests/ZoraCoreTests` | The v1 Swift core, kept until the Rust port has proven parity |
| `fixtures/behavior` | Golden behaviour fixtures that both cores must pass |
| `packaging/macos`, `packaging/linux` | App bundle and signing; systemd user unit |

## Build and test

```bash
cargo test --workspace     # golden fixtures, core units, end-to-end tests with the reference plugin
swift test                 # macOS adapters and the v1 core, including the same golden fixtures
docker run --rm --platform linux/arm64 -v "$PWD":/src:ro -w /src rust:1.98 \
  bash -c 'cp -r /src /w && cd /w && cargo test --workspace'   # the Rust suite on Linux
packaging/macos/bundle.sh --install                            # build, sign and install Zora.app
```

`Zora.app` starts the core itself (`Contents/MacOS/zorad`), so microphone and notification permissions stay with
the app's identity. `ZORA_SIGN_IDENTITY` picks the signing identity; the default is the first "Apple Development"
one. An ad-hoc signature would lose the microphone permission on every rebuild.

## Linux

The core and its packaging are ready; the speech, dictation and notification plugins are not written yet.

```bash
packaging/linux/install.sh          # builds zorad into ~/.local/bin, installs a systemd user unit
systemctl --user enable --now zora
```

Plugins are listed in the config under `"plugins"`, each as a command that speaks the protocol. Without a
dictation tool, set `"dictation": {"monitor": "none"}`.

## Command line

| Command | What it does |
|---|---|
| `zorad status` | Config problems, listening state, store location, jobs that need attention |
| `zorad pause [30m]` / `zorad resume` | Turn listening off (until resumed, or for 1 minute to a week) and on again |
| `zorad names [add\|remove <name>]` | List or change the names Zora answers to |
| `zorad settings [set '<json>']` | Read or change the settings the menu bar edits (`wakeNames`, `phrases`, `workspace`) |
| `zorad transcript --since 30m` | Print the stored transcript |
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

- the macOS plugin releases the microphone and throws away audio it had not transcribed yet,
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

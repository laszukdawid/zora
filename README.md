# Zora

Zora is an always-on background service. It keeps a rolling transcript of what your microphone hears and runs voice commands on a coding agent.

Say **"Hey Zora"**, then the task, then **"Do it Zora"**. Zora sends the text in between to Claude Code (or any agent you configure) and reports the result as a desktop notification. It never speaks back, so you can use it mid-conversation.

- **Cross-platform core, native plugins.** The core (`zorad`, Rust) holds all the logic. Speech, dictation detection and notifications are plugins that talk to it over a small [JSON-lines protocol](docs/protocol.md).
- **Platforms.** macOS works today. On Linux, the core and its packaging are ready, but the speech, dictation and notification plugins are still to be written.
- **Local.** Audio stays inside the speech plugin and is never stored. The last 4 hours of transcript are kept as text in a private SQLite file. Command text leaves the machine only through the agent you configure.
- **Dictation wins.** While your push-to-talk dictation app (TypeWhisper on macOS) is capturing the mic, nothing said can trigger a command. If Zora can't tell whether you're dictating, it runs nothing.
- **Jobs never outlive Zora.** Every agent run happens under a guard process that owns its process group. If the core crashes, the guard kills the job, and Zora checks for leftovers on restart.

See [docs/design.md](docs/design.md) for the architecture and the decisions behind it.

## Layout

| Path | What |
|---|---|
| `crates/zora-core` | Platform-neutral logic: detector, dictation suppression, coordinator, store, config, harness argv |
| `crates/zorad` | Daemon and CLI: plugin host, job guard/gate, recovery. `zora-script-plugin` is a reference plugin. |
| `Sources/ZoraMac`, `Sources/zora` | macOS plugin (Swift): SpeechAnalyzer, CoreAudio dictation monitor, notifications. This is `Zora.app`'s main executable. |
| `Sources/ZoraCore`, `Tests/ZoraCoreTests` | The v1 Swift core, kept until the Rust port has proven parity |
| `fixtures/behavior` | Golden behaviour fixtures that both cores must pass |
| `packaging/macos`, `packaging/linux` | App bundle and signing; systemd user unit |

## macOS

Requires macOS 26 on Apple Silicon, Xcode 26 or later, Rust 1.85 or later, and an **Apple Development** signing identity (it keeps microphone permission across rebuilds).

```bash
packaging/macos/bundle.sh --install                          # builds both binaries, signs, copies to ~/Applications
~/Applications/Zora.app/Contents/MacOS/zora setup            # mic + notification permission, speech model, config
$EDITOR ~/.config/zora/config.json                           # set harnesses.claude.workspace
~/Applications/Zora.app/Contents/MacOS/zora run --dry-run    # try it: commands are shown, not executed
~/Applications/Zora.app/Contents/MacOS/zora install          # start at login
```

`Zora.app` launches the core itself (`Contents/MacOS/zorad`), so permissions stay with the app.

## Linux

```bash
packaging/linux/install.sh          # builds zorad into ~/.local/bin, installs a systemd user unit
systemctl --user enable --now zora
```

Plugins are listed in the config under `"plugins"`, each as a command that speaks the protocol. Without a dictation tool, set `"dictation": {"monitor": "none"}`.

## Using it

| Command | What it does |
|---|---|
| `zorad status` | config problems, store location, jobs that need attention |
| `zorad transcript --since 30m` | print the stored transcript |
| `zorad jobs [clear <id>]` | list unfinished jobs; clear one after checking it by hand |
| `zorad simulate script.txt` | feed a scripted transcript through the command logic (dry run) |
| `zora status` (macOS) | permissions, speech model, login item, dictation monitor |

## Names

Zora answers to several names, so anyone can pick one they can say comfortably. The defaults are `zora` and `nova`.

```bash
zorad names               # list them
zorad names add jarvis    # "Hey Jarvis … Do it Jarvis" now works too
zorad names remove nova
```

Changes apply within a second, with no restart. You can also edit `wakeNames` in the config. Phrases use the word
"zora" as the name slot, so `"hey zora"` and `"do it zora"` cover every listed name. Each name also matches its
sound-alikes (for "zora": "zara", "zura", "sora"), because speech engines rarely spell made-up names the same way twice.

## Cancelling

While Zora is listening, say "stop it, Zora", "stop Zora", "dismiss Zora" or "never mind, Zora" (or the same with any
of its names). Zora forgets what it heard and sends nothing. The "listening" notification disappears and there's no
banner or sound. Outside a capture, these phrases are ordinary speech. To stop a command that is already running, use
`zorad jobs` and `zorad jobs cancel <id>`.

## Configuration

Config lives at `~/.config/zora/config.json`. Missing keys fall back to defaults, and v1 config files still load.

```json
{
  "locale": "en-US",
  "retentionHours": 4,
  "phrases": { "start": ["hey zora"], "end": ["do it zora", "zora do it"], "cancel": ["zora cancel"] },
  "capture": { "holdBackSeconds": 1.5, "timeoutSeconds": 180, "maxGapSeconds": 30, "maxChars": 4000 },
  "dictation": { "bundleIDs": ["com.typewhisper.mac"], "linuxApps": ["my-dictation-tool"], "padSeconds": 1, "monitor": "plugin" },
  "defaultHarness": "claude",
  "harnesses": {
    "claude": { "type": "claude", "workspace": "~/projects/some-repo", "permissionMode": "acceptEdits" },
    "codex":  { "type": "argv", "workspace": "~/projects/some-repo", "executable": "codex", "args": ["exec", "{prompt}"] }
  },
  "plugins": { "desktop": { "command": ["/path/to/notify-plugin"] } },
  "notifications": { "macos": ["commandQueued", "jobSucceeded", "jobFailed", "captureDiscarded", "degraded"], "log": ["*"] }
}
```

- `{prompt}` is always passed as a single argument and never goes through a shell.
- Zora won't start if any harness setting contains a permission-bypass flag (`bypassPermissions`, `--dangerously-skip-permissions`).
- Jobs on the same workspace run one at a time.

## Development

```bash
cargo test --workspace     # golden fixtures, core units, end-to-end tests with the reference plugin
swift test                 # macOS adapters and the v1 core, including the same golden fixtures
docker run --rm --platform linux/arm64 -v "$PWD":/src:ro -w /src rust:1.98 \
  bash -c 'cp -r /src /w && cd /w && cargo test --workspace'   # the Rust suite on Linux
```

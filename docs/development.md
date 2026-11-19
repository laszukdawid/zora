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
| `Sources/ZoraMac`, `Sources/zora` | macOS plugin (Swift): SpeechAnalyzer, CoreAudio dictation monitor, notifications, menu bar and Settings window. This is `Zora.app`'s main executable. |
| `fixtures/behavior` | Golden behaviour fixtures: transcript and dictation events in, expected commands out |
| `packaging/macos`, `packaging/linux` | App bundle and signing; systemd user unit |

## Build and test

```bash
cargo test --workspace     # golden fixtures, core units, end-to-end tests with the reference plugin
swift build                # the macOS app (Swift); its logic lives in the Rust core and is tested there
docker run --rm --platform linux/arm64 -v "$PWD":/src:ro -w /src rust:1.98 \
  bash -c 'cp -r /src /w && cd /w && cargo test --workspace'   # the Rust suite on Linux
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
move or delete them. The build job also refuses a tag whose commit isn't on `main` or whose CI run on `main` hasn't passed, so wait
for CI to finish before you push the tag. Approve
the signing job only for a tag you pushed yourself.

To build a release locally instead, store the notarization credentials once with
`xcrun notarytool store-credentials zora-notary --key <path.p8> --key-id <id> --issuer <issuer>`, then run
`ZORA_NOTARY_PROFILE=zora-notary packaging/macos/release.sh`. The dmg lands in `dist/`.

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

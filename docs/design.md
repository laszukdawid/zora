# Zora design (v2): cross-platform core with native plugins

v1 was a macOS-only Swift build. v2 keeps all of its behaviour and safety rules and moves the logic into a cross-platform Rust core. **Gates** are checks that need real hardware or a desktop session before release.

## Goal

Zora is an always-on background service that does two things:

1. **Rolling transcript.** It keeps a rolling local transcript of everything the microphone hears (default: the last 4 hours, as text only).
2. **Voice commands.** It runs commands spoken between "Hey Zora" and "Do it Zora" on a configurable AI harness. Claude Code is the first; others are configured as argv. Results are reported through notifications, because the user is usually mid-conversation.

Constraints:

- **Platforms.** macOS first (it's where development happens now); Linux (Ubuntu, Arch) is the daily target. Most code is cross-platform. Speech, dictation detection and notifications are native **plugins**.
- **Small footprint.** We budget the whole system: the core, the plugins, and the speech services they use.
- **Dictation wins.** Push-to-talk dictation (TypeWhisper on macOS; whatever tool is configured on Linux) must not trigger commands. Detection is a heuristic, and it **fails closed**: when in doubt, nothing runs.
- **Local processing.** Audio never leaves the speech plugin and is never stored. Command text leaves the machine only through the configured harness.

## Components

```
                ┌──────────────────────── zorad (Rust) ────────────────────────┐
plugin(s) ──JSON lines──► PluginHost ──► InputQueue (ordered, bounded) ──► Coordinator
  transcript / dictation /   (generation, seq,     dictation never dropped,      │ detector, suppression,
  notify                      bounds, supervision)  loss markers → gap            │ coverage, hold-back + sync barrier
                                                                                  ├─► Store (SQLite)
                                                                                  ├─► Dispatcher ─► Guard ─► Gate ─► harness
                                                                                  └─► NotifierHub ─► notify plugins / log
                └──────────────────────────────────────────────────────────────┘
```

### Core (`crates/zora-core`, Rust): platform-neutral, no native dependencies beyond SQLite

- **Detector.** Token units keep their original text, so prompts are sliced exactly at the phrase boundaries. Phrases are configurable. Limits (length, duration, gap between segments) are checked when a command is emitted. A 1 s tick drives expiry.
- **Suppression.**
  - Tracks dictation intervals and **unknown intervals**. Unknown covers stalls, a missing heartbeat, an unhealthy monitor and plugin restarts.
  - Tracks **coverageStart**: the first healthy complete snapshot of the current dictation generation.
  - A segment reaches the detector only if `start − pad − 1 s ≥ max(coverageStart, retainedHistoryStart)`. Otherwise it is stored but never matched, and any open capture is aborted.
  - History is pruned beyond `max(captureTimeout + holdBack + 2·pad + 60 s, 600 s)`, and never at or after the start of an open or pending capture.
- **Coordinator.**
  - The single owner of policy state. It receives everything in order through the `InputQueue`.
  - After the end phrase, it holds the command back (default 1.5 s), using a wake delivered through the same queue. It then sends a `sync` barrier to the dictation plugin (see [protocol](protocol.md)) and releases the command only on a clean `synced` reply.
  - Shutdown closes admission first.
- **InputQueue.** Dictation and control events are never dropped. Transcript events and ticks are capped; a dropped transcript event leaves a loss marker at its position, which becomes a gap.
- **Store.**
  - SQLite in WAL mode, with the same schema as v1 plus job-ownership columns. Writes are batched, retention covers both tables, and secure_delete is on.
  - A read-only mode for CLI queries.
  - Paths: `~/Library/Application Support/Zora/zora.sqlite` (macOS) or `$XDG_STATE_HOME/zora/zora.sqlite` (Linux), with `0700`/`0600` permissions.
- **Config.** JSON, the same as v1 (`~/.config/zora/config.json` on both platforms). It adds a `plugins` section. Validation rejects permission-bypass flags anywhere in a harness's argv, including `allowedTools`.
- **Harness.** argv with `{prompt}` passed as a single argument and no shell; Claude's JSON output is summarized; the permission-mode allowlist applies.

### Daemon (`crates/zorad`, Rust): tokio current-thread runtime, plugin host, jobs, CLI

- **Plugin host.** Implements [protocol.md](protocol.md): handshake and clock check, generations, seq, 64 KiB lines, a 1 MiB queue per plugin, restart with backoff, heartbeat lease, and a 4 KiB/s stderr cap. Raw frames are never logged.
- **Jobs: guard and gate** (so jobs never survive the core).
  1. The core starts `zorad __guard` for each job in its own process group, with a liveness pipe.
  2. The guard starts `zorad __gate` in a new process group, where `pgid` is the gate's pid. The gate blocks on a go-pipe.
  3. The guard reports `{guard_pid, guard_start, job_pgid, gate_start}`. The core commits the job row (with `boot_id`) and acknowledges. Only then does the guard write *go*, and the gate execs the harness (same pid and pgid).
  4. With no acknowledgement within 5 s, the guard kills the gate and nothing ever ran.
  - The guard waits for the leader to exit, a cancel, or liveness EOF. In every case it reaps the whole group (TERM, grace period, KILL, then poll `kill(−pgid, 0) == ESRCH`), reports `cleaned`, and exits.
  - If a guard dies without reporting `cleaned`, the core keeps the workspace reserved and cleans the recorded group itself. If that cleanup fails, admission closes globally and Zora reports it as degraded.
  - **Startup recovery,** for each non-final job:
    - a different `boot_id` → mark it interrupted
    - the guard is alive with a matching start time → wait up to 15 s
    - the group is empty → mark it interrupted
    - the leader's pid equals the pgid and its start time matches the gate's → clean the group
    - otherwise → **don't signal anything**; admission stays closed and a degraded notification names the pgid (`zora jobs clear <id>` after a manual check)
  - Linux also runs under a systemd user unit with `KillMode=control-group`.
- **CLI:** `zora run | status | transcript --since | simulate <script> | jobs [clear <id>]`. On macOS, `setup` and `install` belong to the app (permissions, speech model, `SMAppService`).

### Plugins

| Plugin | OS | Language | Capabilities | Notes |
|---|---|---|---|---|
| `zora-macos` (= `Zora.app` main) | macOS 26+ | Swift | transcript, dictation, notify | SpeechAnalyzer + SpeechDetector; CoreAudio process objects; UserNotifications. It spawns `zorad --attached-plugin` (fds 3/4) and restarts it with backoff. Keeps the v1 identity: signed bundle, `NSMicrophoneUsageDescription`, `SMAppService.agent` called by the same app. |
| `zora-dictation` (was `pipewire-dictation`) | Linux | Rust (native `pipewire` crate), `crates/zora-linux` | dictation | Binds every node; matching input-stream nodes (`application.process.binary` / `application.name` / `application.id` from config) count as dictating while their node state is running. A core `sync` round trip is the complete snapshot (initial, every second, and for each `sync`). Handles removal and reconnection. Applications using pipewire-pulse are covered; a pure PulseAudio server or direct ALSA is unsupported (health unknown). |
| `zora-acdc` | macOS, Linux | Rust (`zorad` package, std-only HTTP to loopback) | notify | Puts each voice command on the local ACDC task board as a task and moves it with the job. Uses the notify message's `job` field. |
| `zora-desktop` (was `freedesktop-notify`) | Linux | Rust (`zbus` + `ksni` on tokio), `crates/zora-linux` | notify | `org.freedesktop.Notifications`, plus the StatusNotifierItem tray with the macOS menu. Spawned by `zorad` like any plugin, so it gets `start`/`listening` and every event over the protocol; changes go through `zorad pause/resume/settings`. |
| `zora-listen` (was `local-stt`) | Linux (file mode everywhere) | Rust, `crates/zora-listen` on `crates/zora-plugin` | transcript | cpal with its PipeWire backend (cpal ≥ 0.18), plus **sherpa-onnx streaming Zipformer `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17` int8**, 1 thread, native endpointing. Times are anchored to captured samples. Silero gating and whisper.cpp are only added if measurements justify them. |

### Migration from v1

The Swift v1 core was removed once the Rust core passed the shared golden-fixture suite (`fixtures/behavior/*.json`: input events, including Unicode, dictation, unknown intervals, gaps and ticks, with the expected outputs). The config and database formats are unchanged, so v1 config files still load.

## Gates

| Area | Gate |
|---|---|
| macOS | Launch-at-login TCC and notification identity through `SMAppService`; real TypeWhisper dictation; sleep/wake; whole-system power. |
| Linux, on the user's machine | Live mic coexistence with the dictation tool; observing that tool's PipeWire stream behaviour (idle, push-to-talk, release, persistent streams); notifications and the tray icon/menu/Settings dialogs on the real desktop (GNOME with AppIndicator, KDE); the systemd user unit across login, including the tray appearing when the service starts before the desktop; suspend/resume; a USB microphone unplugged and replugged. |
| Linux STT targets | Idle CPU < 3% of one core, RSS < 150 MB, end phrase to dispatch < 4 s, plus missed/false command rates on a fixture set. |
| Jobs | SIGKILL the core mid-job (and after the leader has exited, with a TERM-ignoring descendant still alive): the descendant must be dead before the next dispatch. |

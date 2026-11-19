# Zora plugin protocol, version 1

Plugins are separate executables. They exchange **newline-delimited JSON** (one object per line, UTF-8) with the core (`zorad`). Every object has a type field `t`.

- **Linux:** `zorad` starts each configured plugin, using the plugin's stdin and stdout for the protocol.
- **macOS (inverted launch):** `Zora.app`'s main executable *is* the macOS plugin. It starts `zorad --attached-plugin`, handing over a pipe pair on file descriptors **3** (core reads) and **4** (core writes). This keeps microphone, speech and notification permissions with the app's own identity.
- **Windows (inverted launch, for the coming tray app):** the app starts `zorad run --attached-plugin` with its stdin and stdout redirected to pipes, and the protocol runs over them: the core reads stdin and writes stdout. The core makes both handles non-inheritable at start, so no child (plugin, job guard, harness) keeps the connection open, and it writes its logs to stderr only. When the app closes the pipes the core stops, like EOF on fd 3 on macOS. Windows has no numbered descriptors to hand over, and every process launcher can redirect the standard handles.

Everything else is the same in every mode. In attached mode `zorad` also starts every other configured plugin as in Linux mode, but those may declare only `notify`: the app stays the one source of transcript and dictation.

## Clock

Times are **integer nanoseconds** on a named monotonic clock domain:

| Domain | Source |
|---|---|
| `darwin-uptime-raw-ns` | `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)` (same as `mach_absolute_time` converted) |
| `linux-monotonic-ns` | `clock_gettime(CLOCK_MONOTONIC)` |
| `windows-qpc-ns` | `QueryPerformanceCounter`, scaled by `QueryPerformanceFrequency` to nanoseconds |

The domains must match.

**Skew check.** The core records `t_send` just before writing `hello` and `t_recv` when it reads `ready`. The plugin's `now_ns` must fall inside `[t_send − 5 ms, t_recv + 5 ms]`. If it doesn't, the core sends `clock{id}` probes (up to 3 attempts) and expects `clock{id, now_ns}` back. If every probe fails, the plugin is rejected.

## Lifecycle

```
core → hello{protocol:1, generation, clock, now_ns, config}
plugin → ready{protocol:1, generation, capabilities:[...], clock, now_ns}
core → start{listening, until?, epoch}
... events / requests ...
core → stop
plugin → stopped        (after finalizing transcript audio; deadline 5 s)
plugin exits on stdin/fd EOF
```

- `generation` is assigned by the core and increments with every (re)start of the plugin connection. Every plugin message carries it. A message from an older generation is store-only (segments) or ignored (everything else).
- Every plugin message carries a strictly increasing `seq`. A gap or reorder is a protocol error.
- **Protocol errors:** malformed JSON, an unknown `t`, a line over 64 KiB (enforced while reading), a timestamp out of range, or a seq violation. On error, the core treats the connection as broken: it records a transcript gap and sets dictation health to unknown, kills the plugin, and restarts it with backoff (1 s, 2 s, 4 s … 60 s). In attached mode, `zorad` exits and the app restarts it.
- A plugin ignores core messages it does not know, so new ones can be added within version 1.
- At most 1 MiB of plugin output may be queued in the core. If control messages overflow, the core sets health to unknown and restarts the plugin.

## Capabilities and messages

### `transcript` (plugin → core)

| Message | Fields | Notes |
|---|---|---|
| `segment` | `seq, generation, session, start_ns, end_ns, text` | Final results only, never revised. Requires `end_ns ≥ start_ns`, both within `[now − 15 min, now + 1 s]`. |
| `gap` | `seq, generation, at_ns, reason` | Audio continuity was lost (dropped audio, device change, sleep/wake, engine error). |
| `status` | `seq, generation, ready, reason` | e.g. model missing. |
| `partial` | `seq, generation, session, text, epoch` | Optional live, unfinished text (at most ~4/s). The core uses it **only** to show the "listening" notification early; it never affects commands and may be dropped under load. `epoch` is the listening epoch in force when the audio was captured; the core drops a partial whose epoch isn't the current one, or that has none. |

Sessions are scoped to a generation. After a new `hello`, the plugin must start a new speech session.

### `dictation` (plugin → core)

| Message | Fields | Notes |
|---|---|---|
| `dictation` | `seq, generation, state: "began"\|"ended", at_ns` | A configured dictation app started or stopped capturing input. |
| `health` | `seq, generation, healthy, capturing, reason, at_ns` | Sent on change **and as a heartbeat every 2 s** from the monitoring loop itself. |
| `unknown` | `seq, generation, from_ns, to_ns` | The monitoring loop stalled (no completed observation for more than 1.5 s). |
| `synced` | `seq, generation, id, at_ns, healthy, capturing` | Reply to `sync`. |

Rules:
- Health starts **unknown**. It becomes healthy only after a complete snapshot.
- The core marks health unknown when no heartbeat arrives for 6 s.
- The plugin runs a 1 s timer on the same serial loop as its OS callbacks. If more than 1.5 s passes between two completed observations, it sends `unknown{from: last_confirmed, to: now}` before any further state or `synced` message.
- **`sync{id, cutoff_ns}` (core → plugin)** is the barrier before a held-back command is released. The plugin first runs the stall check, then takes a fresh complete snapshot, then replies `synced`. The core releases the command only if all of these hold:
  - the reply arrived within 1 s
  - `healthy` is true and `capturing` is false
  - `at_ns ≥ cutoff_ns`
  - no dictation or unknown interval overlaps `[captureStart − pad − 1 s, at_ns]`

  Otherwise the command is discarded with reason `suppressionUnknown`.

### `notify` (core → plugin)

| Message | Fields |
|---|---|
| `notify` | `id, kind, title, body`, and `job, harness` when the event is about a job |
| `ack` (plugin → core) | `seq, generation, id, ok, error?` |

`job` (the job id) is set for `commandQueued`, `jobStarted`, `jobSucceeded` and `jobFailed`, so a notifier can follow one command through its life; `harness` for the first two. Plugins ignore fields they don't know.

At most 16 notifications may be awaiting an ack. The core never waits for an ack while processing policy.

### Listening on/off (core → plugin)

| Message | Fields |
|---|---|
| `start` | `listening, until?, epoch` |
| `listening` | `listening, until?, epoch, confirmed?` |

`listening: false` means the user paused Zora (`zorad pause`, or the tray). `until` is the end of a timed pause in
Unix seconds; it is absent for a pause that lasts until resumed. The state is also sent with `start`, so a plugin
never opens the microphone just to close it again.

**Epochs.** `epoch` belongs to one connection: it is 1 in `start` and goes up by one each time listening turns on or
off. A new `hello` starts again at 1. A `listening` message that only changes `until` keeps the epoch. If listening
turned on and off again before the core could tell a plugin, the core sends only the latest state, with the epoch
advanced past every transition the plugin missed: it never replays a superseded state. An epoch that jumps
invalidates everything before it, so a plugin still listening under an older epoch restarts its capture.

- A `transcript` plugin must declare the `listening` capability in `ready`: it releases the microphone while paused,
  never starts it before `start` says `listening: true`, and drops audio heard but not yet transcribed rather than
  finishing it. Work queued before a pause or a disconnect (a pending start, an analyzer callback) belongs to its
  epoch and connection and must be discarded, never re-stamped with a newer epoch.
- After applying an epoch, the plugin answers `mic{seq, generation, epoch, open}`, and again with the same epoch
  whenever its microphone opens or closes for another reason (a device failure, a restart after a device change).
  `open: false` means capture resources are released, not merely that no audio arrives; a failed start or restart
  cleans up first and reports the reason with `status`. Retries happen only while listening is still wanted. A
  plugin sends `mic{open: true}` before any transcript output from that epoch, and sends `mic` only after it got an
  epoch-bearing `start` (an older core would reject the unknown message).
- **Confirmation.** On a pause the core drops speech at once and waits up to 5 s for `mic{open: false}` from every
  `transcript` plugin. Then it repeats the `listening` message with the same epoch and `confirmed: true`, or
  `confirmed: false` if a plugin didn't confirm in time. Trays show "Pausing…" until then, and "Pause not confirmed"
  on `false`. A spawned plugin that doesn't confirm is stopped and reaped, and isn't restarted until listening
  resumes. A `transcript` plugin without the `listening` capability is stopped at once on a pause (no 5 s wait) and
  started again on resume; while listening, its segments are accepted and its partials (which carry no epoch)
  dropped. The attached macOS app can't be stopped; it must close its engine on its own on a pause or when the
  connection is lost, and the core only reports a missing confirmation. Each failure sends a `degraded`
  notification naming the plugin. Confirmations and timeouts for an epoch that is no longer current are ignored. A
  `mic{open: true}` for a paused epoch is a failure too: a spawned plugin is stopped until resume. The core stops a
  spawned plugin that hasn't confirmed within a few seconds even if the plugin isn't reading its input. Only a
  confirmation or a verified reap of the plugin's processes counts as closed; an attached app that disconnects, or
  a plugin whose processes couldn't be reaped, leaves the pause unconfirmed (`confirmed: false` and `degraded`). A
  group that couldn't be reaped keeps counting, in later pauses too, until it is verified gone; a new connection of
  the same plugin can't confirm for it. The timeout is reported once; if every source is later confirmed or
  verified gone during the same pause, the core repeats `listening` with `confirmed: true`.
- Whatever a plugin still sends while paused, the core drops: segments and partial text are neither stored nor
  matched (this takes precedence over storing old-generation segments), and an open capture ends. Final segments
  keep their capture-time check after a resume. Commands completed before the pause still run.
- Dictation monitoring continues while paused; it never records audio.

| Message (plugin → core) | Fields | Notes |
|---|---|---|
| `mic` | `seq, generation, epoch, open` | Microphone state after applying `epoch`, or after it changed on its own. |

### Clock probe

`clock{id}` (core → plugin), answered by `clock{seq, generation, id, now_ns}`.

## Privacy

Plugins write logs to stderr. Those logs must contain metadata only, never transcript or prompt text. The core forwards plugin stderr at no more than 4 KiB/s, and never logs raw protocol frames.

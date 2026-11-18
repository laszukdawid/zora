# Zora plugin protocol, version 1

Plugins are separate executables. They exchange **newline-delimited JSON** (one object per line, UTF-8) with the core (`zorad`). Every object has a type field `t`.

- **Linux:** `zorad` starts each configured plugin, using the plugin's stdin and stdout for the protocol.
- **macOS (inverted launch):** `Zora.app`'s main executable *is* the macOS plugin. It starts `zorad --attached-plugin`, handing over a pipe pair on file descriptors **3** (core reads) and **4** (core writes). This keeps microphone, speech and notification permissions with the app's own identity.

Everything else is the same in both modes. In attached mode `zorad` also starts every other configured plugin as in Linux mode, but those may declare only `notify`: the app stays the one source of transcript and dictation.

## Clock

Times are **integer nanoseconds** on a named monotonic clock domain:

| Domain | Source |
|---|---|
| `darwin-uptime-raw-ns` | `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)` (same as `mach_absolute_time` converted) |
| `linux-monotonic-ns` | `clock_gettime(CLOCK_MONOTONIC)` |

The domains must match.

**Skew check.** The core records `t_send` just before writing `hello` and `t_recv` when it reads `ready`. The plugin's `now_ns` must fall inside `[t_send − 5 ms, t_recv + 5 ms]`. If it doesn't, the core sends `clock{id}` probes (up to 3 attempts) and expects `clock{id, now_ns}` back. If every probe fails, the plugin is rejected.

## Lifecycle

```
core → hello{protocol:1, generation, clock, now_ns, config}
plugin → ready{protocol:1, generation, capabilities:[...], clock, now_ns}
core → start{listening, until?}
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
| `partial` | `seq, generation, session, text` | Optional live, unfinished text (at most ~4/s). The core uses it **only** to show the "listening" notification early; it never affects commands and may be dropped under load. |

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
| `start` | `listening, until?` |
| `listening` | `listening, until?` |

`listening: false` means the user paused Zora (`zorad pause`, or the tray). `until` is the end of a timed pause in
Unix seconds; it is absent for a pause that lasts until resumed. The state is also sent with `start`, so a plugin
never opens the microphone just to close it again.

- A `transcript` plugin must release the microphone while paused and must not start it before `start` says
  `listening: true`. It should drop audio heard but not yet transcribed rather than finish it.
- Whatever a plugin still sends while paused, the core drops: segments and partial text are neither stored nor
  matched, and an open capture ends. Commands completed before the pause still run.
- Dictation monitoring continues while paused; it never records audio.
- Trays use the same messages to show the paused state.

### Clock probe

`clock{id}` (core → plugin), answered by `clock{seq, generation, id, now_ns}`.

## Privacy

Plugins write logs to stderr. Those logs must contain metadata only, never transcript or prompt text. The core forwards plugin stderr at no more than 4 KiB/s, and never logs raw protocol frames.

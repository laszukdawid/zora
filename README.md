# Zora

A Linux-first, voice-triggered task dispatcher for coding agents.

Zora listens locally for an explicit start phrase, transcribes a command, waits for an explicit end phrase, and submits the captured text to Claude Code. It runs without voice responses; completion and failures are reported via desktop notifications.

## MVP behavior

- Start: **"Hey Zora"**, **"Zora, start"**
- Finish: **"Zora, do it"**, **"Okay Zora, do it"**
- Anything between the phrases becomes a prompt to Claude Code.
- Multiple executions may run concurrently; recording continues.
- No ambient conversation is sent to the agent before an explicit start phrase.
- Audio is processed locally using faster-whisper; no audio files are stored by Zora.

## Installation

Requires Linux, Python 3.11+, a microphone, `libportaudio`, a working [Claude Code](https://docs.anthropic.com/en/docs/claude-code/overview) CLI installation, and `notify-send` (typically from libnotify).

```bash
git clone https://github.com/laszukdawid/zora.git
cd zora
python -m venv .venv
source .venv/bin/activate
pip install -e '.[test]'
zora --workspace /absolute/path/to/safe/repository
```

Default model: `base.en` (English). For mixed Polish/English use `--model base`. Models are downloaded on first run. On Linux, `sounddevice` needs a usable PortAudio input device.

To inspect transcription and command boundaries **without executing anything**:

```bash
zora --workspace /absolute/path/to/repository --dry-run
```

## Architecture

```text
Microphone -> energy-based utterance segmenter -> faster-whisper
                                               -> delimiter state machine
                                               -> task queue -> Claude Code CLI
                                                             -> notify-send
```

For now the segmenter uses energy thresholds, not a dedicated VAD model. This keeps the MVP small, but means background noise, quiet voices, transcription latency, and split utterances need testing. The threshold can be adjusted via `--rms-threshold`. Audio segmentation occurs locally; Whisper is invoked only for detected utterances, rather than continuously every N seconds.

The parser keeps a rolling text window to recognize end phrases across transcription segments. It does not incorporate ambient context from before the start delimiter.

## Safety and limitations

- **Commands can edit code and invoke tools.** Run Zora only against a repository you are willing to modify.
- Claude Code retains its own permissions. Zora does **not** use `--dangerously-skip-permissions`; unattended commands requiring approval may stop or fail, depending on CLI behavior.
- Agents are launched with a fixed working directory; no shell interpolation of transcribed text.
- No automatic execution without both delimiters.
- A `--dry-run` mode is available for voice-command calibration.
- This is an experimental prototype, **not yet validated on physical Linux audio hardware**.

## Next steps

1. Test microphone segmentation and phrase recognition on Fedora/Ubuntu.
2. Replace energy segmentation with Silero VAD, and benchmark Whisper on CPU vs GPU.
3. Improve multi-utterance parsing, cancellation, timeout handling, and explicit user approvals.
4. Add a systemd user service and richer desktop status.
5. Support pluggable agents (Codex, OpenCode) behind a backend interface.

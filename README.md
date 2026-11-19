# Zora

Zora lets you hand a task to your coding agent by voice, in the middle of a conversation, without touching the
keyboard. Say **"Hey Zora"**, say the task, then say **"Do it, Zora"**. Zora passes what you said in between to your
coding agent (Claude Code, Codex, Pi, or one you set up), and a notification tells you when it's done.

> "Hey Zora, add a test for the empty-cart case in the checkout service. Do it, Zora."

## Why

You're on a call, at a whiteboard, or thinking out loud, and you hit something you want done: a test, a fix, a quick
investigation. Switching to the keyboard breaks the flow, and by the time you've typed it out the thought has moved
on. With Zora you say the task out loud, keep talking, and the agent works in the background. Zora never talks back,
so it doesn't interrupt anyone.

Zora also keeps a short running transcript of what it hears, so you can look back at what was just said.

## How it works

```mermaid
sequenceDiagram
    actor You
    participant Zora as Zora (menu bar)
    participant Agent as Coding agent
    You->>Zora: "Hey Zora"
    Note over Zora: icon changes to a microphone
    You->>Zora: "add a test for the empty-cart case"
    You->>Zora: "Do it, Zora"
    Zora->>Agent: runs the task in your project folder
    Agent-->>Zora: finishes
    Zora-->>You: notification with the agent's answer
```

Zora listens all the time, but only acts on what you say between "Hey Zora" and "Do it, Zora". Everything else goes
into the transcript and nowhere else.

## Install

You need a Mac with Apple Silicon running macOS 26, and a coding agent (Claude Code, Codex, Pi or another
command-line agent) that is installed and signed in.

Zora ships as `Zora-<version>.dmg` on the [latest release](https://github.com/laszukdawid/zora/releases/latest). The
easiest way to install it is to ask your coding agent. Paste this, with your own folder and agent:

```text
Install Zora on this Mac by following
https://github.com/laszukdawid/zora/blob/main/docs/agentic-installation.md
My project folder is ~/code/my-app and my coding agent is Claude Code.
Ask me before any step that needs me.
```

It downloads the app, checks it, runs setup and connects your agent. It will ask you to allow microphone and
notification access, and to say a test phrase. To do it by hand instead, follow the same
[guide](docs/agentic-installation.md) yourself.

## Using Zora

### Give a task

"Hey Zora, *the task*. Do it, Zora." You can also end with "Zora, do it". Pauses while you speak are fine: you have
up to three minutes, with gaps of up to 30 seconds.

### Change your mind

Before you say "Do it", say "Never mind, Zora", "Stop it, Zora" or "Cancel, Zora". Zora drops the task and sends
nothing. (What you said still appears in the transcript.)

### Stop a running task

Say "Stop it, Zora" while the agent is working. Zora stops the most recently started task, and the notification says
so.

### Settings

Click the menu-bar icon and choose **Settings…** to change:

- **Names.** Zora also answers to "Nova" out of the box. Add any one-word name of at least three letters that you
  find easy to say ("Jarvis", "Echo"), and "Hey Jarvis … Do it, Jarvis" works.
- **Coding agent.** Claude Code, Codex, Pi, or a custom command such as `opencode run {prompt}`. Your words go where
  `{prompt}` is, or at the end.
- **Project folder.** Where the agent works. It can change files there.

Changes apply within a second.

### Pause

**Pause Listening** in the menu turns Zora's microphone off within a second, and nothing is heard or saved until you
choose **Resume Listening**. The orange microphone dot in the macOS menu bar goes away too, unless another app is
using the microphone. A pause stays in force across restarts. **Pause for 1 Hour** resumes on its own.

### Read the transcript

Zora keeps about the last 4 hours of what it heard, as text:

```bash
/Applications/Zora.app/Contents/MacOS/zorad transcript --since 30m
```

### Show tasks on the ACDC board

If you use the [ACDC](https://github.com/osano/acdc) task board, Zora can put each voice task on it and move it to
review or blocked when the agent finishes. See [docs/development.md](docs/development.md#acdc-task-board) to turn it
on.

## The menu-bar icon

| Icon | Meaning |
|---|---|
| Waveform | Listening for "Hey Zora" |
| Microphone | Heard "Hey Zora", waiting for "Do it, Zora" |
| Hourglass | Your agent is working on the task |
| Check mark | Done. The notification shows the agent's answer |
| Warning triangle | The task failed or was dropped |
| Crossed-out microphone | Paused. The microphone is off |
| Gear with a cross | Zora needs setup and isn't listening. The menu says what's wrong |

The menu also lists the last few results, so you can read an answer after the notification has gone. Short sounds
play too: a tick when Zora hears its name, a pop when the task starts, a chime when it's done.

## Privacy and safety

- Speech stays on your Mac. Transcription uses Apple's on-device speech model, and audio is never stored or sent
  anywhere.
- The transcript is a private file on your Mac. Zora removes entries older than 4 hours.
- Zora itself sends nothing over the network. It hands your task to your coding agent, which works as it does when
  you use it yourself: it sends the task, and whatever project files it reads, to its model provider.
- Pausing turns Zora's microphone off. Anything that still reaches Zora while paused is thrown away.
- Your dictation app wins. If you use a push-to-talk dictation app (TypeWhisper by default), nothing you dictate can
  trigger a task, and when Zora can't tell whether you're dictating, it runs nothing.
- Nobody is there to answer the agent's questions while it works, so what it may do depends on the agent:
  - **Claude Code** follows its own permission settings and refuses anything it would normally ask about. To let it
    edit files, set its permission mode to `acceptEdits` (the install guide asks you).
  - **Codex** can edit the project folder, and its commands run in Codex's sandbox. The folder must be a Git
    repository.
  - **Pi** has no permission prompts or sandbox: whatever it decides to run, it runs.
  - **A custom command** gets exactly the arguments you give it.
- Pick a project folder under version control, and review what the agent did as you would review a colleague's
  change. Zora refuses to start if the agent's settings contain a known permission-bypass flag.

## Troubleshooting

| Problem | What to do |
|---|---|
| The icon is a gear with a cross | Open the menu to see what's wrong. A missing project folder is fixed in **Settings…**; `/Applications/Zora.app/Contents/MacOS/zorad status` lists other config problems. Zora starts listening by itself once they're fixed. |
| No icon in the menu bar | Zora isn't running. Check `~/Library/Logs/Zora/zora.log`, then run `/Applications/Zora.app/Contents/MacOS/zora install`. |
| The icon never changes when you say "Hey Zora" | Make sure it isn't paused (crossed-out microphone), then run `/Applications/Zora.app/Contents/MacOS/zora status` to check microphone access and the speech model. |
| "Hey Zora" is heard, but the task never runs | A dictation app may have been listening at the same time, or the task took longer than three minutes. The notification says why it was dropped. |
| A task fails with "not found on PATH" | Zora can't find your agent. The [install guide](docs/agentic-installation.md#7-edit-the-config-file-where-needed) shows how to add its folder. |
| Your name is often misheard | Add a second, more distinct name in **Settings…**. Zora already accepts common mishearings ("Zara", "Sora" for "Zora"). |

## Other platforms

Zora is built and supported for macOS. Linux and Windows ports exist (speech, dictation and tray plugins for each)
but have seen little real-world use. Try them if you're curious; reports and fixes are welcome. See
[docs/development.md](docs/development.md#linux).

## For developers

Building from source, the tests, the full configuration reference and the plugin protocol are in
[docs/development.md](docs/development.md), [docs/design.md](docs/design.md) and
[docs/protocol.md](docs/protocol.md).

# Installing Zora (guide for an AI agent)

This guide is for a coding agent installing Zora on someone's Mac. Follow the steps in order and check each result
before moving on. A few steps need the person at the keyboard (permission dialogs, speaking a test phrase): stop and
ask them when you get there. Don't skip a check because the previous step printed no error.

Throughout, `Z` is the folder with Zora's command-line tools:

```bash
Z=/Applications/Zora.app/Contents/MacOS
```

## 1. Ask the person

Ask these before you start, and keep the answers for later steps:

1. **Project folder.** Where should the coding agent work? It runs there and can change files, so a folder under
   version control is best.
2. **Coding agent.** Claude Code (`claude`), Codex (`codex`), Pi (`pi`), or another command-line agent (for
   example `opencode run {prompt}`)? It must already be installed and signed in.
3. **Dictation app.** Do they use a push-to-talk dictation app (TypeWhisper, Superwhisper, macOS Dictation…)? Zora
   holds back while it is dictating, so it needs to know which one.
4. **Claude Code only:** may it edit files without asking? If yes, you'll set `permissionMode` to `acceptEdits`.

## 2. Check the Mac

```bash
uname -m                 # must print arm64 (Apple Silicon)
sw_vers -productVersion  # must be 26.0 or later
command -v <agent>       # e.g. command -v codex; note the folder it prints
```

If any check fails, stop and tell the person. Zora needs Apple Silicon and macOS 26 for Apple's on-device speech
model.

## 3. Download and verify

Get the latest `Zora-<version>.dmg` and its `.sha256` from <https://github.com/laszukdawid/zora/releases/latest>.
With the GitHub CLI:

```bash
dl=$(mktemp -d)
gh release download --repo laszukdawid/zora --pattern 'Zora-*.dmg*' --dir "$dl"
(cd "$dl" && shasum -a 256 -c Zora-*.dmg.sha256)   # must print OK
```

Without `gh`, download both files from the release page into a new, empty folder and run the same `shasum` line.

## 4. Copy the app into Applications

If Zora is already installed and running, step 11 restarts it. Then:

```bash
vol=$(hdiutil attach -nobrowse -readonly "$dl"/Zora-*.dmg | awk -F'\t' '/\/Volumes\//{print $NF}')
rm -rf /Applications/Zora.app
ditto "$vol/Zora.app" /Applications/Zora.app
hdiutil detach "$vol"
spctl -a -vv /Applications/Zora.app   # must say "accepted" and "Notarized Developer ID"
```

The app is notarized and stapled, so macOS opens it without warnings.

## 5. Run setup (needs the person)

Tell the person that macOS is about to ask for **microphone** and **notification** access, and that they should
click **Allow** on both. Then run:

```bash
"$Z/zora" setup --project "<project folder>"
```

Setup also downloads Apple's speech model for the configured language, which can take a few minutes. It exits 0
when the config has no problems. It writes the config to `~/.config/zora/config.json`. Any `config:` lines it
prints are problems to fix in the steps below.

## 6. Choose the coding agent

Claude Code is the default. For another agent:

```bash
"$Z/zorad" settings set '{"harness": "codex"}'
"$Z/zorad" settings set '{"harness": "pi"}'
"$Z/zorad" settings set '{"harness": "custom", "customCommand": "opencode run {prompt}"}'
```

Each prints the settings as JSON. `problems` must be empty (`"problems": []`). For a custom command, the spoken task
goes where `{prompt}` is, or at the end. It runs without a shell, so quote words that contain spaces.

What each agent may do:

- **Claude Code** follows its own permission settings. Nobody is there to approve a prompt, so anything it would ask
  about is refused. If the person said yes in step 1, set `permissionMode` (step 7).
- **Codex** runs with `--sandbox workspace-write`: it can edit the project folder and its commands run in Codex's
  sandbox. The project folder must be inside a Git repository.
- **Pi** has no permission prompts or sandbox: it runs whatever it decides to. Make sure the person knows this.

Zora refuses to start with flags that skip an agent's permission checks (for example
`--dangerously-skip-permissions`, `bypassPermissions`, `--yolo`). Don't add them.

## 7. Edit the config file where needed

These settings have no `zorad settings` command, so edit `~/.config/zora/config.json` with `jq`. Replace the file
with `cat >` rather than `mv`, so it keeps its private permissions:

```bash
cfg=~/.config/zora/config.json
edit() { jq "$@" "$cfg" > "$cfg.new" && cat "$cfg.new" > "$cfg" && rm "$cfg.new"; }
```

**Make the agent findable.** Zora doesn't read the shell's `PATH`. It searches the `path` setting, which by default
is `/opt/homebrew/bin:/usr/local/bin:~/.local/bin:/usr/bin:/bin`. If the folder from `command -v <agent>` (step 2)
isn't in it, add it at the front. Agents installed with npm under a Node version manager (nvm, fnm, volta) also need
`node`, which normally sits in the same folder:

```bash
edit --arg d "$(dirname "$(command -v pi)")" '.path = $d + ":" + .path'
```

**Claude Code edits (if the person agreed):**

```bash
edit '.harnesses.claude.permissionMode = "acceptEdits"'
```

**Dictation app.** The default is TypeWhisper (`com.typewhisper.mac`). For another push-to-talk app, add its bundle
ID:

```bash
id=$(osascript -e 'id of app "Superwhisper"')
edit --arg id "$id" '.dictation.bundleIDs += [$id]'
```

Afterwards, `"$Z/zorad" settings` must still show `"problems": []`.

## 8. Check the agent runs the way Zora runs it

From the project folder, run the same command Zora will run, with a tiny prompt. This catches a missing login or a
folder that isn't a Git repository:

```bash
cd "<project folder>"
claude -p "Reply with exactly: OK" --output-format text
codex exec --sandbox workspace-write -- "Reply with exactly: OK"
pi -p "Reply with exactly: OK"
```

Run only the line for the chosen agent (for a custom command, its own line). It should print `OK`. If it asks to
log in or fails, have the person fix that in the agent first.

## 9. Try it in dry-run mode (needs the person)

Ask the person to run this in a terminal of their own and say **"Hey Zora, say hello. Do it, Zora."**:

```bash
/Applications/Zora.app/Contents/MacOS/zora run --dry-run
```

A notification should show the task text, but nothing runs. They stop it with Ctrl-C. If they'd rather skip this,
go on.

## 10. Start Zora at login

```bash
"$Z/zora" install
"$Z/zora" status
"$Z/zorad" status
```

`zora status` should show microphone `granted`, notifications `authorized`, login item `enabled`, the speech model
installed and config `ok`. If the login item says it requires approval, ask the person to turn Zora on in System
Settings → General → Login Items. A waveform icon then appears in the menu bar.

## 11. Updating

For a new version, repeat steps 3 and 4, then restart the running copy (it starts again by itself):

```bash
launchctl kickstart -k gui/$(id -u)/dev.zora.agent
```

## 12. Tell the person

Summarize what you set up: the project folder, the agent, any changes to `path`, `permissionMode` or dictation apps.
Then remind them how to use it:

- Say **"Hey Zora"**, the task, then **"Do it, Zora"**. A notification shows the agent's answer.
- **"Never mind, Zora"** before "Do it" drops the task. **"Stop it, Zora"** stops a running one.
- The menu-bar icon has **Pause Listening** and **Settings…** (names, agent, project folder).

## If something goes wrong

| Symptom | Check |
|---|---|
| Menu bar shows a gear with a cross | `"$Z/zorad" status` lists the config problems. Zora starts listening by itself once they're fixed. |
| `<agent> not found on PATH` in a failed task | The agent's folder isn't in the `path` setting (step 7). |
| Codex tasks fail with a Git error | The project folder isn't in a Git repository. Use one, or add `--skip-git-repo-check` to `harnesses.codex.extraArgs`. |
| Microphone `denied` in `zora status` | System Settings → Privacy & Security → Microphone: turn Zora on. |
| No menu-bar icon | Check `~/Library/Logs/Zora/zora.log`, then run `"$Z/zora" install` again. |

## Uninstalling

```bash
"$Z/zora" uninstall
rm -rf /Applications/Zora.app
rm -rf ~/.config/zora "$HOME/Library/Application Support/Zora" ~/Library/Logs/Zora   # settings, transcript, logs
```

Ask before removing the settings and transcript.

# Contributing to Zora

Bug reports, fixes and new platform plugins are welcome. Open an issue first for anything bigger than a small fix,
so we can agree on the approach before you spend time on it.

## Build and test

[docs/development.md](docs/development.md) explains the layout, the config and the CLI. The short version:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # golden fixtures, unit tests, end-to-end tests with a scripted plugin
swift build                     # macOS app only
```

CI runs the same checks on macOS and Linux for every pull request.

## What a pull request should include

- **A test for the behaviour you changed.** Command detection and dictation suppression changes need a case in
  `fixtures/behavior/`. Changes to the daemon (jobs, pause, plugins, config reload) need an end-to-end test in
  `crates/zorad/tests/e2e.rs`. That file's scripted plugin can play any sequence of speech and dictation events.
- **A note on how you tested it on real hardware** if you touched a plugin (microphone, speech, dictation
  monitoring, notifications, the menu bar). Tests can't cover these well, so say what you tried and on which OS.
- **Docs.** Update `docs/protocol.md` if a message changes, `docs/development.md` for config or CLI changes, and the
  README if users will notice.

## Rules that protect users

Reviews hold these firmly, because breaking them hurts people who never read the code:

- **Dictation wins, and doubt means no.** If Zora can't tell whether the user is dictating, nothing runs. Don't add
  a path that runs a command while suppression state is unknown.
- **Audio stays on the machine.** It is never stored or sent anywhere. Only the transcript text is kept, and only
  for the retention window.
- **Paused means off.** While paused, plugins release the microphone and drop audio they haven't transcribed.
- **No permission bypass.** Zora refuses to start with harness flags that skip the agent's permission checks. Keep
  it that way.

## Style

- Rust: `cargo fmt`, no clippy warnings. Swift: follow the surrounding code.
- Keep comments short. Say why, not what.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org) (`feat: ...`, `fix(linux): ...`).

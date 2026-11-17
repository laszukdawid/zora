#!/usr/bin/env bash
# Builds zorad and installs it with a systemd user unit. Plugins (speech, dictation, notifications) are
# configured in ~/.config/zora/config.json under "plugins".
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p zorad
install -Dm755 target/release/zorad "$HOME/.local/bin/zorad"
install -Dm644 packaging/linux/zora.service "$HOME/.config/systemd/user/zora.service"
systemctl --user daemon-reload
echo "installed; configure ~/.config/zora/config.json, then: systemctl --user enable --now zora"

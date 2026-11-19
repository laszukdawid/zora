#!/usr/bin/env bash
# Builds zorad and the Linux plugins, installs them into ~/.local/bin with a systemd user unit, and downloads the
# speech model. Build needs: pkg-config, clang/libclang, ALSA and PipeWire headers (see docs/development.md).
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p zorad -p zora-listen -p zora-linux
for bin in zorad zora-acdc zora-listen zora-dictation zora-desktop; do
    install -Dm755 "target/release/$bin" "$HOME/.local/bin/$bin"
done
install -Dm644 packaging/linux/zora.service "$HOME/.config/systemd/user/zora.service"
systemctl --user daemon-reload

"$HOME/.local/bin/zora-listen" setup

config="${XDG_CONFIG_HOME:-$HOME/.config}/zora/config.json"
if [ ! -e "$config" ]; then
    install -Dm600 packaging/linux/config.json "$config"
    echo "wrote $config; set harnesses.claude.workspace and dictation.linuxApps"
else
    echo "$config exists; merge the \"plugins\" and \"dictation\" parts of packaging/linux/config.json into it"
fi
echo "then: zorad status && systemctl --user enable --now zora"

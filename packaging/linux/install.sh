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

# The same path zorad reads (it does not follow XDG_CONFIG_HOME). A hard link never replaces an existing file, so
# a config written meanwhile is kept.
config="$HOME/.config/zora/config.json"
mkdir -p "$(dirname "$config")"
tmp="$(mktemp "$(dirname "$config")/.config.json.XXXXXX")"
trap 'rm -f -- "$tmp"' EXIT
install -m600 packaging/linux/config.json "$tmp"
if [ -e "$config" ] || [ -L "$config" ]; then
    echo "$config exists; merge the \"plugins\" and \"dictation\" parts of packaging/linux/config.json into it"
elif ln -T -- "$tmp" "$config"; then
    echo "wrote $config"
    echo "set harnesses.claude.workspace, and either name your dictation tool in dictation.linuxApps"
    echo "(run zora-dictation watch and dictate to see its name) or set dictation.monitor to \"none\""
elif [ -e "$config" ] || [ -L "$config" ]; then
    echo "$config appeared meanwhile; merge packaging/linux/config.json into it by hand"
else
    echo "cannot write $config" >&2
    exit 1
fi
echo "then: zorad status && systemctl --user enable --now zora"

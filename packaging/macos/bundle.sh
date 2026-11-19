#!/usr/bin/env bash
# Builds Zora.app: the Swift app (macOS plugin: mic, speech, dictation monitor, notifications) as the main
# executable, plus the Rust core `zorad` next to it. Both are signed with one stable identity so macOS keeps
# permissions across rebuilds.
# Usage: packaging/macos/bundle.sh [--install] [--release | --no-sign]
#   --install copies to ~/Applications.
#   --release signs for distribution (see sign.sh); packaging/macos/release.sh notarizes.
#   --no-sign leaves the app unsigned, for signing elsewhere (the release workflow).
set -euo pipefail
cd "$(dirname "$0")/../.."

install=0 sign=(packaging/macos/sign.sh)
for a in "$@"; do
  case "$a" in
    --install) install=1 ;;
    --release) sign+=(--release) ;;
    --no-sign) sign=() ;;
    *) echo "usage: $0 [--install] [--release | --no-sign]" >&2; exit 2 ;;
  esac
done
version=$(packaging/macos/version.sh)

swift build -c release --product zora
cargo build --release -p zorad --bin zorad --bin zora-acdc

app=.build/Zora.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp .build/release/zora "$app/Contents/MacOS/zora"
cp target/release/zorad "$app/Contents/MacOS/zorad"
cp target/release/zora-acdc "$app/Contents/MacOS/zora-acdc"
cp packaging/macos/Info.plist "$app/Contents/Info.plist"
# The app's version follows the Cargo workspace version.
plutil -replace CFBundleShortVersionString -string "$version" "$app/Contents/Info.plist"
cp packaging/macos/dev.zora.agent.plist "$app/Contents/Library/LaunchAgents/"

if (( ${#sign[@]} )); then
  "${sign[0]}" "$app" ${sign[1]+"${sign[1]}"}
fi
echo "built $app"

if (( install )); then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/Zora.app"
  cp -R "$app" "$HOME/Applications/Zora.app"
  echo "installed $HOME/Applications/Zora.app"
fi

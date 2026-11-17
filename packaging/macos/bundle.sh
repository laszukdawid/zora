#!/usr/bin/env bash
# Builds Zora.app: the Swift app (macOS plugin: mic, speech, dictation monitor, notifications) as the main
# executable, plus the Rust core `zorad` next to it. Both are signed with one stable identity so macOS keeps
# permissions across rebuilds.
# Usage: packaging/macos/bundle.sh [--install]   (--install copies to ~/Applications)
# ZORA_SIGN_IDENTITY picks the identity; default is the first "Apple Development" one.
set -euo pipefail
cd "$(dirname "$0")/../.."

swift build -c release --product zora
cargo build --release -p zorad --bin zorad

app=.build/Zora.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp .build/release/zora "$app/Contents/MacOS/zora"
cp target/release/zorad "$app/Contents/MacOS/zorad"
cp packaging/macos/Info.plist "$app/Contents/Info.plist"
cp packaging/macos/dev.zora.agent.plist "$app/Contents/Library/LaunchAgents/"

identity="${ZORA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/ {print $2; exit}')}"
if [[ -z "$identity" ]]; then
  echo "No 'Apple Development' signing identity found; set ZORA_SIGN_IDENTITY." >&2
  echo "An ad-hoc signature would lose microphone permission on every rebuild." >&2
  exit 1
fi
codesign --force --options runtime --timestamp=none --sign "$identity" "$app/Contents/MacOS/zorad"
codesign --force --options runtime --timestamp=none \
  --entitlements packaging/macos/Zora.entitlements --sign "$identity" "$app"
codesign --verify --strict --deep "$app"
echo "built $app (signed by $identity)"

if [[ "${1:-}" == "--install" ]]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/Zora.app"
  cp -R "$app" "$HOME/Applications/Zora.app"
  echo "installed $HOME/Applications/Zora.app"
fi

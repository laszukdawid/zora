#!/usr/bin/env bash
# Builds Zora.app: the Swift app (macOS plugin: mic, speech, dictation monitor, notifications) as the main
# executable, plus the Rust core `zorad` next to it. Both are signed with one stable identity so macOS keeps
# permissions across rebuilds.
# Usage: packaging/macos/bundle.sh [--install] [--release]
#   --install copies to ~/Applications.
#   --release signs for distribution: a "Developer ID Application" identity and a secure timestamp, as
#             notarization requires (packaging/macos/release.sh does the rest).
# ZORA_SIGN_IDENTITY picks the identity; the default is the first "Apple Development" one (or "Developer ID
# Application" with --release).
set -euo pipefail
cd "$(dirname "$0")/../.."

install=0 release=0
for a in "$@"; do
  case "$a" in
    --install) install=1 ;;
    --release) release=1 ;;
    *) echo "usage: $0 [--install] [--release]" >&2; exit 2 ;;
  esac
done
kind="Apple Development" timestamp="--timestamp=none"
if (( release )); then kind="Developer ID Application" timestamp="--timestamp"; fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

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

identity="${ZORA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' -v k="$kind" 'index($0, k) {print $2; exit}')}"
if [[ -z "$identity" ]]; then
  echo "No '$kind' signing identity found; set ZORA_SIGN_IDENTITY." >&2
  echo "An ad-hoc signature would lose microphone permission on every rebuild." >&2
  exit 1
fi
codesign --force --options runtime "$timestamp" --sign "$identity" "$app/Contents/MacOS/zorad"
codesign --force --options runtime "$timestamp" --sign "$identity" "$app/Contents/MacOS/zora-acdc"
codesign --force --options runtime "$timestamp" \
  --entitlements packaging/macos/Zora.entitlements --sign "$identity" "$app"
codesign --verify --strict --deep "$app"
echo "built $app (signed by $identity)"

if (( install )); then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/Zora.app"
  cp -R "$app" "$HOME/Applications/Zora.app"
  echo "installed $HOME/Applications/Zora.app"
fi

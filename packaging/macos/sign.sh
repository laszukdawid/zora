#!/usr/bin/env bash
# Signs a built Zora.app inside out (zorad, zora-acdc, then the app) with the hardened runtime.
# Usage: packaging/macos/sign.sh <Zora.app> [--release]
#   --release: a "Developer ID Application" identity and a secure timestamp, as notarization requires.
# ZORA_SIGN_IDENTITY picks the identity (name or SHA-1); the default is the first "Apple Development" one (or
# "Developer ID Application" with --release). ZORA_KEYCHAIN limits the search to one keychain file.
set -euo pipefail
cd "$(dirname "$0")/../.."
app=${1:?usage: sign.sh <Zora.app> [--release]}
kind="Apple Development" timestamp="--timestamp=none"
if [[ "${2:-}" == "--release" ]]; then kind="Developer ID Application" timestamp="--timestamp"; fi

keychain=()
if [[ -n "${ZORA_KEYCHAIN:-}" ]]; then keychain=(--keychain "$ZORA_KEYCHAIN"); fi
identity="${ZORA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning ${ZORA_KEYCHAIN:+"$ZORA_KEYCHAIN"} |
  awk -F'"' -v k="$kind" 'index($0, k) {print $2; exit}')}"
if [[ -z "$identity" ]]; then
  echo "No '$kind' signing identity found; set ZORA_SIGN_IDENTITY." >&2
  echo "An ad-hoc signature would lose microphone permission on every rebuild." >&2
  exit 1
fi
sign() { codesign --force --options runtime "$timestamp" ${keychain[@]+"${keychain[@]}"} --sign "$identity" "$@"; }
sign "$app/Contents/MacOS/zorad"
sign "$app/Contents/MacOS/zora-acdc"
sign --entitlements packaging/macos/Zora.entitlements "$app"
codesign --verify --strict --deep "$app"
echo "signed $app ($identity)"

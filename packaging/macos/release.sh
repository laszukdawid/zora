#!/usr/bin/env bash
# Signs, notarizes and staples Zora.app and Zora-<version>.dmg, and puts the dmg and its SHA-256 in dist/.
#
# Needs a "Developer ID Application" identity (see sign.sh for ZORA_SIGN_IDENTITY and ZORA_KEYCHAIN) and
# notarization credentials, either:
#   ZORA_NOTARY_PROFILE  a notarytool keychain profile (xcrun notarytool store-credentials <name> ...), or
#   ZORA_NOTARY_KEY, ZORA_NOTARY_KEY_ID, ZORA_NOTARY_ISSUER
#                        an App Store Connect API key: path to the .p8 file, its key ID and the issuer ID.
# Usage: packaging/macos/release.sh [<unsigned Zora.app>]   (without one, it builds the app first)
set -euo pipefail
cd "$(dirname "$0")/../.."

if [[ -n "${ZORA_NOTARY_PROFILE:-}" ]]; then
  notary=(--keychain-profile "$ZORA_NOTARY_PROFILE")
elif [[ -n "${ZORA_NOTARY_KEY:-}" && -n "${ZORA_NOTARY_KEY_ID:-}" && -n "${ZORA_NOTARY_ISSUER:-}" ]]; then
  notary=(--key "$ZORA_NOTARY_KEY" --key-id "$ZORA_NOTARY_KEY_ID" --issuer "$ZORA_NOTARY_ISSUER")
else
  echo "Set ZORA_NOTARY_PROFILE, or ZORA_NOTARY_KEY, ZORA_NOTARY_KEY_ID and ZORA_NOTARY_ISSUER." >&2
  exit 1
fi

version=$(packaging/macos/version.sh)
if [[ "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME:-}" != "v$version" ]]; then
  echo "Tag $GITHUB_REF_NAME does not match the Cargo workspace version $version." >&2
  exit 1
fi

work=$(mktemp -d)
mount="$work/mnt"
cleanup() {
  if [[ -d "$mount" ]]; then hdiutil detach -quiet "$mount" || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

if [[ -n "${1:-}" ]]; then
  cp -R "$1" "$work/Zora.app"
else
  packaging/macos/bundle.sh --no-sign
  cp -R .build/Zora.app "$work/Zora.app"
fi
app="$work/Zora.app"
[[ $(plutil -extract CFBundleShortVersionString raw "$app/Contents/Info.plist") == "$version" ]] ||
  { echo "Zora.app's version is not $version." >&2; exit 1; }
packaging/macos/sign.sh "$app" --release
identity=$(codesign -dvv "$app" 2>&1 | sed -n 's/^Authority=\(Developer ID Application:.*\)/\1/p' | head -1)
[[ -n "$identity" ]] || { echo "Zora.app is not signed with a Developer ID identity." >&2; exit 1; }

notarize() {
  local out id
  out=$(xcrun notarytool submit "$1" "${notary[@]}" --wait --output-format json)
  echo "$out"
  id=$(plutil -extract id raw - <<<"$out")
  if [[ $(plutil -extract status raw - <<<"$out") != "Accepted" ]]; then
    xcrun notarytool log "$id" "${notary[@]}" >&2 || true
    echo "Notarization of $(basename "$1") failed." >&2
    exit 1
  fi
}

# The app itself is notarized and stapled, so it also passes Gatekeeper offline once copied out of the dmg.
ditto -c -k --keepParent "$app" "$work/Zora.zip"
notarize "$work/Zora.zip"
xcrun stapler staple "$app"

name="Zora-$version.dmg"
dmg="$work/$name"
mkdir "$work/stage"
cp -R "$app" "$work/stage/"
ln -s /Applications "$work/stage/Applications"
hdiutil create -volname "Zora $version" -srcfolder "$work/stage" -fs APFS -format ULFO "$dmg" >/dev/null
keychain=()
if [[ -n "${ZORA_KEYCHAIN:-}" ]]; then keychain=(--keychain "$ZORA_KEYCHAIN"); fi
codesign --force --timestamp ${keychain[@]+"${keychain[@]}"} --sign "${ZORA_SIGN_IDENTITY:-$identity}" "$dmg"
notarize "$dmg"
xcrun stapler staple "$dmg"

# Check what users get: the stapled dmg, and the app inside it as they will copy it out.
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose "$dmg"
mkdir "$mount"
hdiutil attach -readonly -nobrowse -quiet -mountpoint "$mount" "$dmg"
xcrun stapler validate "$mount/Zora.app"
spctl --assess --type execute --verbose "$mount/Zora.app"
codesign --verify --strict --deep "$mount/Zora.app"
hdiutil detach -quiet "$mount"
rmdir "$mount"

mkdir -p dist
mv "$dmg" "dist/$name"
(cd dist && shasum -a 256 "$name" > "$name.sha256")
echo "built dist/$name"

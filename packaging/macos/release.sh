#!/usr/bin/env bash
# Builds a signed, notarized and stapled Zora-<version>.dmg in dist/, ready for GitHub Releases.
#
# Needs a "Developer ID Application" signing identity in the keychain and notarization credentials, either:
#   ZORA_NOTARY_PROFILE  a notarytool keychain profile (xcrun notarytool store-credentials <name> ...), or
#   ZORA_NOTARY_KEY, ZORA_NOTARY_KEY_ID, ZORA_NOTARY_ISSUER
#                        an App Store Connect API key: path to the .p8 file, its key ID and the issuer ID.
# Usage: packaging/macos/release.sh
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

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ -n "${GITHUB_REF_NAME:-}" && "$GITHUB_REF_NAME" == v* && "$GITHUB_REF_NAME" != "v$version" ]]; then
  echo "Tag $GITHUB_REF_NAME does not match the Cargo workspace version $version." >&2
  exit 1
fi

packaging/macos/bundle.sh --release
app=.build/Zora.app
identity=$(codesign -dvv "$app" 2>&1 | sed -n 's/^Authority=\(Developer ID Application:.*\)/\1/p' | head -1)
[[ -n "$identity" ]] || { echo "Zora.app is not signed with a Developer ID identity." >&2; exit 1; }

notarize() {
  local out id
  out=$(xcrun notarytool submit "$1" "${notary[@]}" --wait --output-format json)
  echo "$out"
  id=$(plutil -extract id raw - <<<"$out")
  if [[ $(plutil -extract status raw - <<<"$out") != "Accepted" ]]; then
    xcrun notarytool log "$id" "${notary[@]}" >&2 || true
    echo "Notarization of $1 failed." >&2
    exit 1
  fi
}

dist=dist
rm -rf "$dist"
mkdir -p "$dist"

# Notarize and staple the app itself, so it also passes Gatekeeper offline once copied out of the disk image.
ditto -c -k --keepParent "$app" "$dist/Zora.zip"
notarize "$dist/Zora.zip"
rm "$dist/Zora.zip"
xcrun stapler staple "$app"

dmg="$dist/Zora-$version.dmg"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -volname "Zora $version" -srcfolder "$stage" -fs APFS -format ULFO -ov "$dmg" >/dev/null
codesign --force --timestamp --sign "$identity" "$dmg"
notarize "$dmg"
xcrun stapler staple "$dmg"

spctl --assess --type execute --verbose "$app"
spctl --assess --type open --context context:primary-signature --verbose "$dmg"
(cd "$dist" && shasum -a 256 "$(basename "$dmg")" > "$(basename "$dmg").sha256")
echo "built $dmg"

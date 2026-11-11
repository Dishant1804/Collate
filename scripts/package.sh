#!/bin/zsh
# Builds the release downloads into dist/: a universal (Apple silicon and Intel) Collate.app
# as a zip and as a disk image, the bare `collate` binary as a tarball, and their SHA-256 sums.
#
#   scripts/package.sh
#
# With COLLATE_SIGN_IDENTITY set ("Developer ID Application: Name (TEAMID)"), everything is
# signed with it and notarized by Apple, the app and disk image stapled so they open offline.
# Notarizing needs an App Store Connect API key: NOTARY_KEY_PATH (the .p8 file),
# NOTARY_KEY_ID and NOTARY_ISSUER_ID. Without an identity the app is signed ad hoc and nothing
# is notarized, for trying the packaging on this Mac.
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name=Collate-$version
dist=dist
identity=${COLLATE_SIGN_IDENTITY:-}
if [[ -n $identity ]]; then
  : ${NOTARY_KEY_PATH:?is needed to notarize} ${NOTARY_KEY_ID:?is needed to notarize} ${NOTARY_ISSUER_ID:?is needed to notarize}
fi

# Sends a zip or disk image to Apple and waits; fails with Apple's log unless it's accepted.
notarize() {
  local out
  out=$(xcrun notarytool submit "$1" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID" --wait --output-format json)
  if [[ $(plutil -extract status raw - <<< "$out") != Accepted ]]; then
    echo "package: Apple didn't notarize $1: $out" >&2
    xcrun notarytool log "$(plutil -extract id raw - <<< "$out")" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID" >&2 || true
    exit 1
  fi
}

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --target $target
done

rm -rf "$dist"
mkdir -p "$dist"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

binary=$work/collate
lipo -create -output "$binary" \
  target/aarch64-apple-darwin/release/collate target/x86_64-apple-darwin/release/collate
scripts/bundle.sh "$binary" "$work"
app=$work/Collate.app

if [[ -n $identity ]]; then
  codesign --force --options runtime --timestamp --entitlements scripts/Collate.entitlements --sign "$identity" "$binary"
  # A bare binary can't be stapled; notarized, Gatekeeper finds its ticket online.
  ditto -c -k "$binary" "$work/collate.zip"
  notarize "$work/collate.zip"
  ditto -c -k --keepParent "$app" "$work/Collate.zip"
  notarize "$work/Collate.zip"
  xcrun stapler staple "$app"
fi

ditto -c -k --keepParent "$app" "$dist/$name-macos-universal.zip"
tar -czf "$dist/collate-$version-macos-universal.tar.gz" -C "$work" collate

# The disk image: the app beside a link to Applications, to drag it onto.
image=$work/image
mkdir "$image"
ditto "$app" "$image/Collate.app"
ln -s /Applications "$image/Applications"
dmg=$dist/$name-macos-universal.dmg
hdiutil create -volname Collate -srcfolder "$image" -format UDZO -ov "$dmg" >/dev/null
if [[ -n $identity ]]; then
  codesign --force --timestamp --sign "$identity" "$dmg"
  notarize "$dmg"
  xcrun stapler staple "$dmg"
fi

(cd "$dist" && shasum -a 256 * > SHA256SUMS)
ls -l "$dist"

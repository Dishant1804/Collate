#!/bin/zsh
# Builds the release downloads into dist/: a universal (Apple silicon and Intel) Collate.app
# as a DMG and a zip, the bare `collate` binary as a tarball, and their SHA-256 sums.
#
#   scripts/package.sh
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name=Collate-$version
dist=dist

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

# The DMG opens on the app beside a link to /Applications, to drag it across.
stage=$work/dmg
mkdir -p "$stage"
ditto "$work/Collate.app" "$stage/Collate.app"
ln -s /Applications "$stage/Applications"
hdiutil create -volname "Collate $version" -srcfolder "$stage" -fs HFS+ -format UDZO \
  -ov "$dist/$name.dmg" >/dev/null

ditto -c -k --keepParent "$work/Collate.app" "$dist/$name-macos-universal.zip"
tar -czf "$dist/collate-$version-macos-universal.tar.gz" -C "$work" collate

(cd "$dist" && shasum -a 256 * > SHA256SUMS)
ls -l "$dist"

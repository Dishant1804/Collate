#!/bin/bash
# Builds this machine's Linux download into dist/: collate-<version>-linux-<arch>.tar.gz, one
# folder with the binary in bin/, the launcher entry and icon under share/, and install.sh,
# which installs them into ~/.local. The updater looks for bin/collate in it.
#
#   scripts/package-linux.sh
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name=collate-$version-linux-$(uname -m)
dist=dist

cargo build --release
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/$name
mkdir -p "$root/bin" "$root/share/applications" "$root/share/icons/hicolor/scalable/apps" "$dist"
install -m 755 "${CARGO_TARGET_DIR:-target}/release/collate" "$root/bin/collate"
install -m 644 scripts/linux/collate.desktop "$root/share/applications/collate.desktop"
install -m 644 assets/icon.svg "$root/share/icons/hicolor/scalable/apps/collate.svg"
install -m 755 scripts/linux/install.sh "$root/install.sh"
install -m 644 LICENSE "$root/LICENSE"
tar -czf "$dist/$name.tar.gz" -C "$work" "$name"
ls -l "$dist/$name.tar.gz"

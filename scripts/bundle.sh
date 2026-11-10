#!/bin/zsh
# Makes Collate.app from a built binary: the icon, Info.plist and an ad-hoc signature.
# Shared by install.sh (this Mac) and package.sh (the release downloads).
#
#   scripts/bundle.sh <binary> <out-dir>   writes <out-dir>/Collate.app
set -euo pipefail

(( $# == 2 )) || { echo "usage: scripts/bundle.sh <binary> <out-dir>" >&2; exit 2; }
binary=$1
out=$2
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$out/Collate.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/Collate"

# The icon: the SVG rendered once at 1024, then every size macOS asks for.
qlmanage -t -s 1024 -o "$work" assets/icon.svg >/dev/null 2>&1
png=$work/icon.svg.png
[[ -f $png ]] || { echo "bundle: couldn't render assets/icon.svg" >&2; exit 1; }
set=$work/Collate.iconset
mkdir -p "$set"
for size in 16 32 128 256 512; do
  sips -z $size $size "$png" --out "$set/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$png" --out "$set/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$set" -o "$app/Contents/Resources/Collate.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Collate</string>
  <key>CFBundleDisplayName</key><string>Collate</string>
  <key>CFBundleIdentifier</key><string>local.collate.diff</string>
  <key>CFBundleExecutable</key><string>Collate</string>
  <key>CFBundleIconFile</key><string>Collate</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# Apple silicon runs only signed code; an ad-hoc signature is all we have without a
# Developer ID, so a downloaded copy needs its quarantine cleared once (see the README).
codesign --force --sign - "$app" >/dev/null

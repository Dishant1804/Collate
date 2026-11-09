#!/bin/zsh
# Builds Collate and installs it as a macOS app: /Applications/Collate.app (Launchpad, Spotlight,
# the Dock), plus a `collate` command in ~/.local/bin for opening a repository from a terminal.
#
#   scripts/install.sh            build, bundle, install
#   scripts/install.sh --no-build bundle and install the existing release binary
set -euo pipefail

cd "$(dirname "$0")/.."
here=$PWD
apps=/Applications
[[ -w $apps ]] || apps=$HOME/Applications
bin=$HOME/.local/bin
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [[ ${1:-} != --no-build ]]; then
  cargo build --release
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$work/Collate.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/collate "$app/Contents/MacOS/Collate"

# The icon: the SVG rendered once at 1024, then every size macOS asks for.
qlmanage -t -s 1024 -o "$work" assets/icon.svg >/dev/null 2>&1
png=$work/icon.svg.png
[[ -f $png ]] || { echo "install: couldn't render assets/icon.svg" >&2; exit 1; }
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

# Apple silicon runs only signed code; an ad-hoc signature is enough on this Mac.
codesign --force --sign - "$app" >/dev/null

# Replace an earlier install only after the new bundle is complete.
if pgrep -x Collate >/dev/null; then
  echo "install: quit Collate first (it's running)" >&2
  exit 1
fi
rm -rf "$apps/Collate.app"
ditto "$app" "$apps/Collate.app"
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f "$apps/Collate.app"

# `collate [path]` from a terminal opens the app on that repository.
mkdir -p "$bin"
cat > "$bin/collate" <<'CLI'
#!/bin/sh
# Opens Collate on a repository: `collate` (the current folder) or `collate <path>`.
dir=$(cd "${1:-.}" 2>/dev/null && pwd) || { echo "collate: no such folder: ${1:-.}" >&2; exit 1; }
exec open -n -a Collate --args "$dir"
CLI
chmod +x "$bin/collate"

echo "Installed $apps/Collate.app ($version) and $bin/collate"

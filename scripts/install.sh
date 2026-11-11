#!/bin/zsh
# Builds Collate and installs it as a macOS app: /Applications/Collate.app (Launchpad, Spotlight,
# the Dock), plus a `collate` command in ~/.local/bin for opening a repository from a terminal.
#
#   scripts/install.sh            build, bundle, install
#   scripts/install.sh --no-build bundle and install the existing release binary
set -euo pipefail

cd "$(dirname "$0")/.."
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
scripts/bundle.sh target/release/collate "$work"

# Replace an earlier install only after the new bundle is complete.
if pgrep -x Collate >/dev/null; then
  echo "install: quit Collate first (it's running)" >&2
  exit 1
fi
rm -rf "$apps/Collate.app"
ditto "$app" "$apps/Collate.app"
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f "$apps/Collate.app"

# `collate` from a terminal: on its own or with a folder, opens the app on that repository in
# a window of its own; with anything else (two files or folders, a patch, `git diff | collate`,
# flags) runs it here and waits for its window to close.
mkdir -p "$bin"
cat > "$bin/collate" <<CLI
#!/bin/sh
# Opens Collate: \`collate [folder]\` on a repository, \`collate a b\` compares two files or
# folders, \`collate x.patch\` or \`git diff | collate\` reads a patch.
if [ -t 0 ] && { [ \$# -eq 0 ] || { [ \$# -eq 1 ] && [ -d "\$1" ]; }; }; then
  dir=\$(cd "\${1:-.}" 2>/dev/null && pwd) || { echo "collate: no such folder: \${1:-.}" >&2; exit 1; }
  exec open -n -a Collate --args "\$dir"
fi
exec "$apps/Collate.app/Contents/MacOS/Collate" "\$@"
CLI
chmod +x "$bin/collate"

echo "Installed $apps/Collate.app ($version) and $bin/collate"

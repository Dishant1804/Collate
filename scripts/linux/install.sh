#!/bin/sh
# Installs this download for the reader alone: the binary in ~/.local/bin, the launcher entry
# and icon under ~/.local/share. Run it from the folder the tarball unpacked into.
#
#   ./install.sh              install or update
#   ./install.sh --uninstall  remove the binary, entry and icon (keeps settings)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
data=${XDG_DATA_HOME:-$HOME/.local/share}
bin=$prefix/bin
entry=$data/applications/collate.desktop
icon=$data/icons/hicolor/scalable/apps/collate.svg

if [ "${1:-}" = --uninstall ]; then
  rm -f "$bin/collate" "$entry" "$icon"
  echo "Removed Collate. Settings are in ${XDG_CONFIG_HOME:-$HOME/.config}/collate and data in $data/collate."
  exit 0
fi

mkdir -p "$bin" "$(dirname "$entry")" "$(dirname "$icon")"
# Copied beside and renamed over, so a running Collate keeps its own file.
cp "$here/bin/collate" "$bin/.collate-next"
chmod 755 "$bin/.collate-next"
mv -f "$bin/.collate-next" "$bin/collate"
cp "$here/share/icons/hicolor/scalable/apps/collate.svg" "$icon"
# The launcher names the binary by its full path: a desktop session's PATH often lacks ~/.local/bin.
sed "s|^Exec=collate|Exec=$bin/collate|" "$here/share/applications/collate.desktop" > "$entry"
command -v update-desktop-database >/dev/null && update-desktop-database "$(dirname "$entry")" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$data/icons/hicolor" 2>/dev/null || true

echo "Installed $bin/collate"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "Add $bin to your PATH to run \`collate\` from a terminal." ;;
esac

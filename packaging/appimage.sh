#!/usr/bin/env bash
#
# Build a portable AppImage of WinDrop.
#
#   ./packaging/appimage.sh [--output-dir DIR]
#
# What this produces is deliberately *not* a Wine bundle. An AppImage that
# carried its own Wine would be several hundred megabytes, would pin a Wine
# version the user cannot change, and would still need the host's graphics
# drivers. WinDrop's job is to find and drive a Wine — so the AppImage ships
# WinDrop, and Wine comes from the host, exactly as it does for a distribution
# package. `windrop doctor` says what is missing if it is missing.
#
# Requires: linuxdeploy, linuxdeploy-plugin-gtk, and a GTK4 development
# environment (Arch: `pacman -S base-devel gtk4 pkgconf`). Both tools are
# downloaded automatically when they are not already on PATH.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

OUTPUT_DIR="$PROJECT_ROOT/dist"
WORK_DIR="$PROJECT_ROOT/target/appimage"
APP_ID="org.windrop.WinDrop"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir) OUTPUT_DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- prerequisites

for tool in cargo install desktop-file-validate; do
  command -v "$tool" >/dev/null || die "$tool is required but not installed"
done

# linuxdeploy and its GTK plugin are fetched once and kept out of the tree.
TOOLS_DIR="$WORK_DIR/tools"
mkdir -p "$TOOLS_DIR" "$OUTPUT_DIR"

fetch_tool() {
  local name="$1" url="$2" target="$TOOLS_DIR/$1"
  [[ -x "$target" ]] && { echo "$target"; return; }
  say "downloading $name" >&2
  curl -fsSL -o "$target" "$url"
  chmod +x "$target"
  echo "$target"
}

LINUXDEPLOY="$(command -v linuxdeploy || true)"
if [[ -z "$LINUXDEPLOY" ]]; then
  LINUXDEPLOY="$(fetch_tool linuxdeploy \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${ARCH:-x86_64}.AppImage")"
fi

# linuxdeploy needs its plugins on PATH, named exactly `linuxdeploy-plugin-gtk`.
PLUGIN_DIR="$TOOLS_DIR/plugins"
mkdir -p "$PLUGIN_DIR"
if ! command -v linuxdeploy-plugin-gtk >/dev/null; then
  PLUGIN="$PLUGIN_DIR/linuxdeploy-plugin-gtk"
  if [[ ! -x "$PLUGIN" ]]; then
    say "downloading linuxdeploy-plugin-gtk"
    curl -fsSL -o "$PLUGIN" \
      "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"
    chmod +x "$PLUGIN"
  fi
fi
export PATH="$PLUGIN_DIR:$PATH"

# ------------------------------------------------------------------- build

say "building release binaries"
cargo build --release --locked

APPDIR="$WORK_DIR/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/metainfo" "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -Dm755 target/release/windrop     "$APPDIR/usr/bin/windrop"
install -Dm755 target/release/windrop-gui "$APPDIR/usr/bin/windrop-gui"
install -Dm644 data/org.windrop.WinDrop.desktop \
  "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -Dm644 data/org.windrop.WinDrop.metainfo.xml \
  "$APPDIR/usr/share/metainfo/$APP_ID.metainfo.xml"
install -Dm644 data/icons/hicolor/scalable/apps/windrop.svg \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps/windrop.svg"

# linuxdeploy wants the entry point, the desktop file and an icon at the root of
# the AppDir. The icon has to be a PNG: it is what the file manager reads, and
# SVG support in that path is not dependable.
ln -sf "usr/share/applications/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"

ICON_SVG="data/icons/hicolor/scalable/apps/windrop.svg"
if command -v rsvg-convert >/dev/null; then
  rsvg-convert -w 512 -h 512 -o "$APPDIR/windrop.png" "$ICON_SVG"
elif command -v inkscape >/dev/null; then
  inkscape --export-type=png --export-width=512 --export-filename="$APPDIR/windrop.png" "$ICON_SVG"
elif command -v convert >/dev/null; then
  convert -background none -resize 512x512 "$ICON_SVG" "$APPDIR/windrop.png"
else
  die "one of rsvg-convert, inkscape or ImageMagick is needed to make the PNG icon"
fi

say "validating the desktop entry"
desktop-file-validate "$APPDIR/$APP_ID.desktop"

# ----------------------------------------------------------------- package

say "running linuxdeploy"
cd "$APPDIR/.."
ARCH="${ARCH:-x86_64}" \
OUTPUT="$OUTPUT_DIR/WinDrop-${ARCH}.AppImage" \
"$LINUXDEPLOY" \
  --appdir "$APPDIR" \
  --desktop-file "$APPDIR/$APP_ID.desktop" \
  --icon-file "$APPDIR/windrop.png" \
  --plugin gtk \
  --output appimage

cd "$PROJECT_ROOT"
say "done:"
ls -lh "$OUTPUT_DIR"/*.AppImage
cat <<'EOF'

Wine is not bundled. Anyone running this needs a Wine on their PATH, or a managed
build downloaded from WinDrop's settings pane.
EOF

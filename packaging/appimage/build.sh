#!/usr/bin/env bash
# Packages target/release/mirrorwire as an AppImage with linuxdeploy.
# libva and libpipewire stay with the host system: their drivers and modules
# must match the versions installed there.
# usage: packaging/appimage/build.sh <out-dir>
set -euo pipefail
OUT=$1
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$(mktemp -d)
mkdir -p "$OUT"

# Pinned releases (not the moving "continuous" tag) so checksums stay stable.
LINUXDEPLOY_VERSION=1-alpha-20251107-1
LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
LINUXDEPLOY_PLUGIN_VERSION=1-alpha-20250213-1
LINUXDEPLOY_PLUGIN_SHA256=992d502a248e14ab185448ddf6f6e7d25558cb84d4623c354c3af350c25fccb3

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

fetch() {
  local url=$1 dest=$2 expect=$3
  curl --proto '=https' --tlsv1.2 -fsSL -o "$dest" "$url"
  local actual
  actual=$(sha256 "$dest")
  if [[ "$actual" != "$expect" ]]; then
    echo "checksum mismatch for $dest: expected $expect, got $actual" >&2
    exit 1
  fi
}

fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_VERSION}/linuxdeploy-x86_64.AppImage" \
  "$WORK/linuxdeploy" "$LINUXDEPLOY_SHA256"
# The appimage output plugin bundles appimagetool; linuxdeploy looks for it next
# to itself or on PATH, named linuxdeploy-plugin-appimage.
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/${LINUXDEPLOY_PLUGIN_VERSION}/linuxdeploy-plugin-appimage-x86_64.AppImage" \
  "$WORK/linuxdeploy-plugin-appimage" "$LINUXDEPLOY_PLUGIN_SHA256"
chmod +x "$WORK/linuxdeploy" "$WORK/linuxdeploy-plugin-appimage"
export PATH="$WORK:$PATH"
export APPIMAGE_EXTRACT_AND_RUN=1
export OUTPUT="$OUT/MirrorWire-x86_64.AppImage"
"$WORK/linuxdeploy" \
  --appdir "$WORK/AppDir" \
  --executable "$ROOT/target/release/mirrorwire" \
  --desktop-file "$ROOT/packaging/linux/io.github.windwerfer.MirrorWire.desktop" \
  --icon-file "$ROOT/packaging/icons/io.github.windwerfer.MirrorWire.png" \
  --exclude-library 'libva*' \
  --exclude-library 'libpipewire-0.3*' \
  --output appimage
ls -la "$OUTPUT"

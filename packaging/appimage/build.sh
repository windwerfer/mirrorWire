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
curl -sfL -o "$WORK/linuxdeploy" \
  https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
chmod +x "$WORK/linuxdeploy"
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

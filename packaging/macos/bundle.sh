#!/usr/bin/env bash
# Wraps a built mirrorwire binary into MirrorWire.app and zips it.
# usage: packaging/macos/bundle.sh <binary> <arch-label> <min-macos> <out-dir>
set -euo pipefail
BIN=$1; ARCH=$2; MIN=$3; OUT=$4
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)

mkdir -p "$OUT"
APP="$OUT/MirrorWire.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/mirrorwire"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MIN/g" "$ROOT/packaging/macos/Info.plist.in" > "$APP/Contents/Info.plist"

ICONSET=$(mktemp -d)/MirrorWire.iconset
mkdir -p "$ICONSET"
SRC="$ROOT/packaging/icons/io.github.windwerfer.MirrorWire.png"
for s in 16 32 128 256; do
  sips -z $s $s "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
cp "$SRC" "$ICONSET/icon_512x512.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/MirrorWire.icns"

echo "== linked libraries"
otool -L "$APP/Contents/MacOS/mirrorwire"
if otool -L "$APP/Contents/MacOS/mirrorwire" | tail -n +2 | grep -vE '^\s*(/usr/lib/|/System/Library/)'; then
  echo "error: binary links libraries outside the OS" >&2
  exit 1
fi
echo "== minimum macOS"
otool -l "$APP/Contents/MacOS/mirrorwire" | grep -A4 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX' | grep -E 'minos|version' | head -2

codesign --force --deep --sign - "$APP"
ditto -c -k --keepParent "$APP" "$OUT/MirrorWire-macos-$ARCH.zip"
echo "wrote $OUT/MirrorWire-macos-$ARCH.zip"

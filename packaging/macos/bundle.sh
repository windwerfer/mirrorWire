#!/usr/bin/env bash
# Wraps a built mirrorwire binary into MirrorWire.app and a UDZO DMG
# (app + /Applications symlink, ready to drag and drop).
# usage: packaging/macos/bundle.sh <binary> <arch-label> <min-macos> <out-dir>
set -euo pipefail
BIN=$1; ARCH=$2; MIN=$3; OUT=$4
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)

# Tagged builds use the cargo/tag version; CI builds on main use the short SHA
# so each Actions download has a distinct name.
if [[ "${GITHUB_REF:-}" == refs/tags/v* ]]; then
  LABEL="${GITHUB_REF_NAME#v}"
elif [[ -n "${VERSION:-}" && -n "$(git -C "$ROOT" rev-parse --short=7 HEAD 2>/dev/null)" ]]; then
  # Prefer the SHA on untagged builds so artifact names don't collide across commits.
  LABEL=$(git -C "$ROOT" rev-parse --short=7 HEAD)
else
  LABEL=$VERSION
fi

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

# Sign the .app before it goes into the DMG (ad-hoc; not notarized).
codesign --force --deep --sign - "$APP"

STAGING=$(mktemp -d)
# ditto preserves the code signature and resource forks; cp -R may not.
ditto "$APP" "$STAGING/MirrorWire.app"
ln -s /Applications "$STAGING/Applications"

DMG="$OUT/MirrorWire-${LABEL}-macos-${ARCH}.dmg"
rm -f "$DMG"
# HFS+ so the image opens on macOS 10.13 (the Intel minimum).
hdiutil create \
  -volname MirrorWire \
  -srcfolder "$STAGING" \
  -ov \
  -fs HFS+ \
  -format UDZO \
  -imagekey zlib-level=9 \
  "$DMG"
rm -rf "$STAGING"

echo "wrote $DMG"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "dmg=$DMG" >> "$GITHUB_OUTPUT"
fi

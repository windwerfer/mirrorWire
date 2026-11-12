#!/usr/bin/env bash
# Builds a minimal static FFmpeg (plus x264 on Linux) with only what MirrorWire uses:
# the H.264 decoder/parser, swscale, and the platform's hardware encoder
# (VideoToolbox on macOS, VAAPI + libx264 on Linux).
#
# usage: packaging/build-ffmpeg.sh <prefix>
# env:   MACOSX_DEPLOYMENT_TARGET (macOS), JOBS
set -euo pipefail

PREFIX=$(mkdir -p "$1" && cd "$1" && pwd)
FFMPEG_VERSION=7.1.5
FFMPEG_SHA256=de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f
X264_COMMIT=b35605ace3ddf7c1a5d67a2eb553f034aef41d55
X264_SHA256=cd71a7515b0e9a012e1ac9b1f8415bebcaf6fc97d4db32286642ac4c0fbe24f9
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)}
WORK=${WORK:-$(mktemp -d)}
OS=$(uname -s)

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# Download a URL over TLS 1.2+ HTTPS only and check its SHA-256.
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

cd "$WORK"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

common=(
  --prefix="$PREFIX" --enable-static --disable-shared --enable-pic
  --disable-programs --disable-doc --disable-network --disable-autodetect
  --disable-everything --disable-debug
  --enable-decoder=h264 --enable-parser=h264
  --enable-swscale
  --enable-avfilter
)

if [[ "$OS" == Darwin ]]; then
  : "${MACOSX_DEPLOYMENT_TARGET:=10.13}"
  export MACOSX_DEPLOYMENT_TARGET
  platform=(
    --enable-videotoolbox
    --enable-encoder=h264_videotoolbox --enable-hwaccel=h264_videotoolbox
    --enable-filter=buffer,buffersink,format,scale,hwupload,hwdownload,hwmap,null
    --extra-cflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
    --extra-ldflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
  )
else
  if [[ ! -f "$PREFIX/lib/libx264.a" ]]; then
    fetch "https://code.videolan.org/videolan/x264/-/archive/$X264_COMMIT/x264-$X264_COMMIT.tar.gz" \
      "x264-$X264_COMMIT.tar.gz" "$X264_SHA256"
    tar xz -f "x264-$X264_COMMIT.tar.gz"
    (cd "x264-$X264_COMMIT" &&
      ./configure --prefix="$PREFIX" --enable-static --enable-pic --disable-cli \
        --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash &&
      make -j"$JOBS" && make install)
  fi
  platform=(
    --enable-gpl --enable-libx264 --enable-encoder=libx264
    --enable-vaapi --enable-encoder=h264_vaapi --enable-hwaccel=h264_vaapi
    --enable-filter=buffer,buffersink,format,scale,hwupload,hwdownload,hwmap,null,scale_vaapi,vpp_vaapi
    --pkg-config-flags=--static
  )
fi

fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" \
  "ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
tar xf "ffmpeg-$FFMPEG_VERSION.tar.xz"
cd "ffmpeg-$FFMPEG_VERSION"
./configure "${common[@]}" "${platform[@]}" || { tail -50 ffbuild/config.log; exit 1; }
make -j"$JOBS"
make install
echo "FFmpeg $FFMPEG_VERSION installed to $PREFIX"

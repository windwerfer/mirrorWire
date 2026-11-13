#!/usr/bin/env bash
# End-to-end loopback test: test-pattern presenter -> QUIC -> headless viewer.
# usage: scripts/loopback-test.sh [seconds] [presenter args...] [-- viewer args...]
# Extra cargo features via MIRRORWIRE_FEATURES, e.g.
#   MIRRORWIRE_FEATURES=filmcraft scripts/loopback-test.sh 10 --encoder filmcraft
# MIRRORWIRE_BIN=/path/to/mirrorwire skips the build (compare two builds).
# Bottleneck + loss (adaptive bitrate), e.g.
#   scripts/loopback-test.sh 30 --encoder x264 -- --simulate-bandwidth 8 --simulate-loss 2
set -euo pipefail
cd "$(dirname "$0")/.."
FEAT=(); [[ -n "${MIRRORWIRE_FEATURES:-}" ]] && FEAT=(--features "$MIRRORWIRE_FEATURES")
BIN=${MIRRORWIRE_BIN:-}
if [[ -z "$BIN" ]]; then
  cargo build --release -q -p mirrorwire-app --no-default-features "${FEAT[@]}"
  BIN=target/release/mirrorwire
fi
DUR=${1:-10}; shift || true
PA=(); while [[ $# -gt 0 && "$1" != "--" ]]; do PA+=("$1"); shift; done
[[ $# -gt 0 ]] && shift
WORK=$(mktemp -d)
trap 'kill -INT ${PP:-0} 2>/dev/null || true; rm -rf "$WORK"' EXIT
"$BIN" --config-dir "$WORK/p" present --source test --no-mdns --port 0 \
  --uri-file "$WORK/uri" --advertise-ip 127.0.0.1 "${PA[@]}" > "$WORK/present.log" 2>&1 &
PP=$!
for _ in $(seq 100); do [[ -s "$WORK/uri" ]] && break; sleep 0.1; done
"$BIN" --config-dir "$WORK/v" view --headless --duration "$DUR" "$@" "$(cat "$WORK/uri")"
grep -E "presenter:" "$WORK/present.log" | tail -1

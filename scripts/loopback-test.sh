#!/usr/bin/env bash
# End-to-end loopback test: test-pattern presenter -> QUIC -> headless viewer.
# usage: scripts/loopback-test.sh [seconds] [presenter args...] [-- viewer args...]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q -p mirrorwire-app --no-default-features
BIN=target/release/mirrorwire
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

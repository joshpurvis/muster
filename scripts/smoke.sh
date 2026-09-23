#!/usr/bin/env bash
# Build muster and run the end-to-end smoke test against a throwaway daemon
# (temp data/runtime dirs, port 17433). Deno permissions are scoped to exactly what it needs.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet
BIN="$PWD/target/debug/muster"
TMP="$(mktemp -d -t muster-smoke.XXXXXX)"
cleanup() {
  # Stop a daemon the test may have left behind, then remove the temp dir.
  if [[ -f "$TMP/run/daemon.json" ]]; then
    pid="$(sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$TMP/run/daemon.json")"
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    sleep 1
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
deno run \
  --allow-net=127.0.0.1:17433 \
  --allow-read="$TMP,$BIN" \
  --allow-write="$TMP" \
  --allow-run="$BIN,git" \
  scripts/smoke.ts "$TMP" "$BIN"

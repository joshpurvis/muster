#!/usr/bin/env bash
# Build muster and run the end-to-end smoke test against a throwaway daemon
# (temp data/runtime dirs, port 17433). Deno permissions are scoped to exactly what it needs.
set -euo pipefail
# Never the owner's real OpenRouter key (or settings inherited from an OpenRouter session).
unset OPENROUTER_API_KEY MUSTER_PROVIDER
cd "$(dirname "$0")/.."
CARGO_TARGET_DIR="$PWD/target" cargo build --workspace --quiet
BIN="$PWD/target/debug/muster"
TMP="$(mktemp -d -t muster-smoke.XXXXXX)"
cleanup() {
  test_status=$?
  if [[ "$test_status" -eq 0 ]]; then
    rm -rf "$TMP"
  else
    echo "Failed test artifacts retained: $TMP" >&2
  fi
}
trap cleanup EXIT
deno run \
  --allow-net=127.0.0.1 \
  --allow-read="$TMP,$BIN" \
  --allow-write="$TMP" \
  --allow-run="$BIN,git,python3" \
  scripts/smoke.ts "$TMP" "$BIN"

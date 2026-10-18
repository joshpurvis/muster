#!/usr/bin/env bash
# Run development checks with no route to the user's running Muster instance.
# Usage: scripts/isolated.sh cargo test --workspace
# Every child, including a helper launched by a test, inherits these boundaries.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -eq 0 ]]; then
  echo 'usage: scripts/isolated.sh <command> [args...]' >&2
  exit 2
fi
test_root="$(mktemp -d -t muster-isolated.XXXXXXXX)"
chmod 700 "$test_root"
export MUSTER_RUNTIME_DIR="$test_root/run"
export MUSTER_DATA_DIR="$test_root/data"
# Port 0 requests a fresh OS-assigned port for direct daemon invocations.
# Suites using a fixed test port must override it explicitly in their child env.
export MUSTER_PORT=0
export CARGO_TARGET_DIR="$PWD/target"
mkdir -m 700 "$MUSTER_RUNTIME_DIR" "$MUSTER_DATA_DIR"
unset MUSTER_URL MUSTER_TOKEN MUSTER_BIN MUSTER_SESSION_ID MUSTER_CHANNEL_ID MUSTER_RUN
# Keep failed test artifacts for diagnosis. This wrapper deliberately never kills
# a PID read from a discovery file: integration fixtures own their child handles.
echo "Isolated test files: $test_root" >&2
"$@"

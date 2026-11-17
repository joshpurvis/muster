#!/usr/bin/env bash
# Desktop UI screenshots in WebKitGTK against a fresh, isolated mock (AGENTS.md, "Screenshots of
# the UI"). Starts the mock on its own port, runs web/dev/webkit_shot.py, stops the mock.
#
#   bash scripts/isolated.sh deno task web          # build web/dist first (after every UI change)
#   bash web/dev/webkit_shot.sh <steps.json> <out-dir> [--size WxH] [--theme dark|light]
#
# MOCK_PORT picks the port (default 17840; give each parallel run its own). MOCK_ADDONS=1 starts
# the mock with the counter add-on registered. Steps format: web/dev/webkit_shot.py.
set -euo pipefail
if [[ $# -lt 2 ]]; then
  echo 'usage: web/dev/webkit_shot.sh <steps.json> <out-dir> [--size WxH] [--theme dark|light]' >&2
  exit 2
fi
steps="$(realpath "$1")"
out="$(realpath -m "$2")"
shift 2
cd "$(dirname "$0")/../.."
if [[ ! -f web/dist/app.js ]]; then
  echo 'web/dist is missing: run `bash scripts/isolated.sh deno task web` first' >&2
  exit 2
fi
port="${MOCK_PORT:-17840}"
if curl -s --max-time 2 -o /dev/null "http://127.0.0.1:$port/api/health"; then
  echo "port $port is already in use; set MOCK_PORT to a free one" >&2
  exit 2
fi
log="$(mktemp -t muster-webkit-mock.XXXXXX)"
MOCK_PORT="$port" setsid bash scripts/isolated.sh deno task --cwd web mock >"$log" 2>&1 &
mock=$!
stop_mock() {
  kill -- -"$mock" 2>/dev/null || true
  wait "$mock" 2>/dev/null || true
  # The mock's server exits a moment after its shell: wait (up to 5 s) until the port is free,
  # so the next run can take it.
  for _ in $(seq 1 50); do
    curl -s --max-time 2 -o /dev/null "http://127.0.0.1:$port/api/health" || return 0
    sleep 0.1
  done
}
trap stop_mock EXIT
ready=
for _ in $(seq 1 150); do
  # Alive first: a mock that lost the port to another run must not take that run's answer.
  if ! kill -0 "$mock" 2>/dev/null; then
    echo "the mock did not start; its log: $log" >&2
    exit 1
  fi
  health="$(curl -s --max-time 2 "http://127.0.0.1:$port/api/health" 2>/dev/null || true)"
  if [[ "$health" == *'"version":"mock"'* ]]; then
    ready=1
    break
  fi
  sleep 0.1
done
if [[ -z "$ready" ]]; then
  echo "the mock did not answer within 15 s; its log: $log" >&2
  exit 1
fi
python3 -I web/dev/webkit_shot.py "http://127.0.0.1:$port/#token=dev" "$steps" "$out" "$@"

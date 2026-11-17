#!/usr/bin/env bash
# Read-only access to each feature file and the real registry, even in an isolated test run.
set -euo pipefail
cd "$(dirname "$0")/../.."
read_files="$(python3 -c 'import json; print(",".join(json.load(open("web/dev/addon_files.json"))))')"
registry="${XDG_DATA_HOME:-$HOME/.local/share}/muster/addons.json"
read_files="$read_files,$registry"
if [[ ! -e "$registry" && -z "${MUSTER_ADDON_LINT_NAMES:-}" ]]; then printf '%s\n' 'skipped: no registry'; fi
if [[ -n "${MUSTER_ADDON_LINT_NAMES:-}" ]]; then read_files="$read_files,$MUSTER_ADDON_LINT_NAMES"; fi
exec deno test --quiet --config web/deno.json \
  --allow-env=HOME,XDG_DATA_HOME,MUSTER_ADDON_LINT_NAMES --allow-read="$read_files" web/dev/addon_lint_test.ts

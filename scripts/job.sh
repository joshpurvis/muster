#!/usr/bin/env bash
# Disposable work gets its own OOM boundary. This does not contact the Muster daemon.
# Usage: scripts/job.sh build|test|browser COMMAND [ARG...]
set -euo pipefail
profile="${1:-}"
case "$profile" in
  build|test) default_high=3G; default_max=5G; default_swap=512M ;;
  browser) default_high=1G; default_max=2G; default_swap=256M ;;
  *) echo 'usage: scripts/job.sh build|test|browser COMMAND [ARG...]' >&2; exit 2 ;;
esac
shift
if [[ $# -eq 0 ]]; then echo 'job command is required' >&2; exit 2; fi
# Avoid nested scopes for tasks that invoke another guarded task. Do not inherit this marker
# into long-lived sessions (the backend scrubs it alongside other launcher-only variables).
if [[ "${MUSTER_JOB_SCOPE:-}" == 1 ]]; then exec "$@"; fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
mode="${MUSTER_JOB_ISOLATION:-auto}"
case "$mode" in auto|required|off) ;; *) echo 'MUSTER_JOB_ISOLATION must be auto, required or off' >&2; exit 2 ;; esac
if [[ "$mode" != off ]] && command -v systemd-run >/dev/null &&
    [[ -f /sys/fs/cgroup/cgroup.controllers ]] && systemctl --user show --property=Version --value >/dev/null 2>&1; then
  exec systemd-run --user --scope --quiet --collect \
    --unit="muster-job-$profile-$$-$RANDOM" --slice=muster-jobs.slice \
    --property="MemoryHigh=${MUSTER_JOB_MEMORY_HIGH:-$default_high}" \
    --property="MemoryMax=${MUSTER_JOB_MEMORY_MAX:-$default_max}" \
    --property="MemorySwapMax=${MUSTER_JOB_MEMORY_SWAP_MAX:-$default_swap}" \
    -- env MUSTER_JOB_SCOPE=1 "$@"
fi
if [[ "$mode" == required ]]; then
  echo 'systemd user manager and cgroup v2 are required for this job' >&2
  exit 1
fi
if [[ "$mode" != off ]]; then
  echo 'muster job: systemd user manager unavailable; running without memory limits' >&2
fi
exec "$@"

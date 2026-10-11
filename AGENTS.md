# Agent rules for this repo

- Never run `muster stop`, `muster daemon` or any other muster CLI command against the real
  daemon unless the user asks. In tests, set `MUSTER_RUNTIME_DIR`, `MUSTER_DATA_DIR` and
  `MUSTER_PORT` to temp values on every call (as `scripts/smoke.sh` does). On 2026-10-11 a bare
  `muster stop` during testing killed the user's live daemon and every session in it.

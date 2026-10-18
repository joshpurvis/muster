# Agent rules for this repo

- Never run `muster stop`, `muster daemon` or any other muster CLI command against the real
  daemon unless the user asks. In tests, set `MUSTER_RUNTIME_DIR`, `MUSTER_DATA_DIR` and
  `MUSTER_PORT` to temp values on every call (as `scripts/smoke.sh` does). On 2026-10-11 a bare
  `muster stop` during testing killed the user's live daemon and every session in it.
- Exception: after a successful `deno task build:all` in the main checkout
  (`/home/josh/projects/muster`, on `master`), you may apply the build to the live daemon
  without asking, using that checkout's binary by its full path:
  - backend changes (`src/`): `/home/josh/projects/muster/target/release/muster reload`
    (terminals keep running); run it before any UI install.
  - UI changes (`web/`): `/home/josh/projects/muster/target/release/muster ui-install web/dist`,
    then tell the user to reload the window.

  Never apply from a worktree or another branch: `reload` restarts the backend from whichever
  binary runs it, so a worktree build would replace the live backend with unmerged code. Never
  use `$MUSTER_BIN` for this: it is the running backend's own copy, so `reload` through it
  restarts the old build. If `reload` or `ui-install` refuses, stop and report the message; do
  not work around it. `muster stop`, `muster daemon` and everything else stay forbidden.
- If a change touches `crates/` (terminal host, protocol or core), it only takes effect after a
  full restart, which ends every session. Say so in your final message, in these words: "Full
  restart needed (ends all sessions): `muster stop`, then `muster`." Never run it yourself. If
  the change alters the host wire format (requests, responses, frames or their fields), raise
  `VERSION` in `crates/terminal-protocol/src/lib.rs` so `muster reload` refuses an incompatible
  host instead of connecting to it.

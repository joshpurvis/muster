# Agent rules for this repo

- Never run `muster stop`, `muster daemon` or any other muster CLI command against the real
  daemon unless the user asks. In tests, set `MUSTER_RUNTIME_DIR`, `MUSTER_DATA_DIR` and
  `MUSTER_PORT` to temp values on every call (as `scripts/smoke.sh` does). A bare `muster stop`
  during testing kills the user's live daemon and every session in it.
- Exception: after a successful `deno task build:all` in the main checkout
  (the repository's primary working tree, not a `git worktree`, on `master`), you may apply the
  build to the live daemon without asking, using that checkout's own binary, run from its root:
  - backend changes (`src/`): `./target/release/muster reload`
    (terminals keep running); run it before any UI install.
  - UI changes (`web/`): `./target/release/muster ui-install web/dist`,
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

## Screenshots of the UI

- Desktop UI (everything in `web/` except the phone page `web/src/m/`): every picture for a pull
  request, `docs/screenshots/` or a design review comes from WebKitGTK, the desktop window's
  engine, through `deno task shot:desktop` (usage, flags and requirements in the headers of
  `web/dev/webkit_shot.sh` and `web/dev/webkit_shot.py`; `web/dev/webkit_shots/palette.json` is
  a worked example). Headless Chromium hides bugs the window shows. If WebKitGTK can't run, say
  so in the report instead of sending Chromium pictures as the desktop's. Design reviews get these
  pictures at the desktop window size, both themes when colours change, and the WebKitGTK version
  that took them (the tool prints it).
- The headless Chromium scripts in `web/dev/` (`e2e*.ts`, `cdp.ts`) stay the behaviour checks;
  their PNGs show that a step happened, not how the window draws it.
- Phone UI (`web/src/m/`, served at `/m`): headless Chromium with mobile emulation
  (docs/ANDROID.md, "Test plan").
- This repo is public: every picture uses the mock and invented sample data only. Save working
  pictures in `.muster-shots/` in your worktree; convert the ones the repo keeps to webp
  (`magick in.png -quality 80 out.webp`) in `docs/screenshots/`.

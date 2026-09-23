# Muster client protocol (v1)

The daemon serves everything on one localhost port (default 7433, else a free port).
Discovery file: `$XDG_RUNTIME_DIR/muster/daemon.json` (fallback `~/.local/state/muster/daemon.json`),
mode 0600: `{"pid": 123, "port": 7433, "token": "<client token>", "version": "0.1.0"}`.

| Route | Auth | Purpose |
|---|---|---|
| `GET /` and static assets | none | Frontend (embedded `web/dist`). |
| `GET /ws?token=<client token>` | client token | Client protocol below. |
| `POST /hook/<event>` | `Authorization: Bearer <session token>` | Agent hook callbacks (body: JSON payload). |
| `POST /mcp` | `Authorization: Bearer <session token>` | MCP (streamable HTTP, JSON responses). |
| `GET /api/health` | none | `{"ok":true,"version":"..."}` |
| `POST /api/rpc` | `Authorization: Bearer <client token>` | Same JSON methods as the WebSocket (body `{id?, method, params}`, reply `{id, result}` / `{id, error}`), except `session.attach`/`session.detach`. Used by the CLI. |
| `POST /api/launch-code` | `Authorization: Bearer <client token>` | `{"code": "<64 hex>"}`: a single-use launch code, valid 60 s. Used by the CLI to open the UI. |
| `POST /api/exchange` | the launch code | Body `{"code": "..."}` → `200 {"token": "<client token>"}` and the code is consumed. Unknown, used or expired code, or a bad body: `403 {"error": {"message": "invalid or expired launch code"}}`. |

### Opening the UI

`muster` and `muster open` get a launch code from `POST /api/launch-code` and open
`http://127.0.0.1:<port>/#code=<code>`. The client token itself never goes on the browser's
command line (`/proc/<pid>/cmdline` is readable by every user). The frontend:

1. If `location.hash` has `code=<code>`: `POST /api/exchange` with
   `Content-Type: application/json` and body `{"code": "<code>"}` (same origin). On `200`, store
   `token` from the reply like a `#token=` token. On `403`, the code was already used or has
   expired (60 s): fall back to a token already in `sessionStorage`, else tell the user to run
   `muster` again (or `muster url`).
2. Else if `location.hash` has `token=<client token>` (printed by `muster url`, and still accepted
   for compatibility): use it directly.
3. Either way, strip the hash from the URL (`history.replaceState`) and keep the token in
   `sessionStorage`, so a reload doesn't need the (already consumed) code.

`muster url` prints the `#token=` URL, because the user asked for it explicitly.

The daemon rejects `/ws` upgrades, `POST /api/exchange` and `POST /api/launch-code` whose
`Origin` is present and not `http://127.0.0.1:<port>` or `http://localhost:<port>` (or a
`tauri://`/`wry://` origin); those get `403`.

## WebSocket framing

Text frames carry JSON. Binary frames carry terminal bytes.

### JSON: requests / responses / events

```jsonc
// client -> daemon
{"id": 7, "method": "session.spawn", "params": {...}}
// daemon -> client
{"id": 7, "result": {...}}
{"id": 7, "error": {"message": "..."}}
// daemon -> client, unsolicited (see ServerEvent in src/hub.rs)
{"event": "session_upserted", "data": {...SessionInfo}}
```

### Binary frames

`[u8 type][u32 session_id, big endian][payload...]`

| type | direction | meaning |
|---|---|---|
| `0x01` | daemon → client | Live output bytes. |
| `0x02` | client → daemon | Keyboard input bytes (from xterm `onData`, UTF-8). |
| `0x03` | daemon → client | Replay: scrollback bytes. Client must `reset()` the terminal, then write. Always precedes live `0x01` frames for that attach. |

Terminal queries: the daemon itself answers DA1 (`CSI c`), DSR status (`CSI 5 n`) and cursor
position requests (`CSI 6 n`) that programs print, whether or not clients are attached. An
input frame that consists **only** of answers to those (`ESC [ ? <n;…> c`, `ESC [ 0 n`, and,
within 5 s of a cursor position request, `ESC [ <row> ; <col> R`) is dropped, so a program gets
one answer no matter how many clients watch. Frames with anything else in them are written as
they are (so keystrokes such as Shift+F3, `ESC [ 1 ; 2 R`, are never eaten outside that window).

Replay: the ring buffer holds the last 2 MiB of output, so the sequences that turned on terminal
modes may have scrolled out of it. Every replay therefore starts with the current input modes
(application cursor/keypad, bracketed paste, mouse reporting and encoding, from the daemon's
terminal emulator) and, if the replayed bytes begin inside the alternate screen, a switch to it
(`CSI ? 1049 h`). When the ring still holds everything, later bytes set the same modes again,
which is harmless. Replays of an exited session from an earlier daemon run (served from
output.log) get the same prefix, computed from the log.

## Methods

All ids are numbers. Types (`Workspace`, `Channel`, `SessionInfo`, `Event`) are in `src/model.rs`.

| Method | Params | Result |
|---|---|---|
| `hello` | `{}` | `{version, workspaces[], channels[], sessions[], home, default_shell}` |
| `workspace.add` | `{path, name?}` | `Workspace` (path may start with `~`) |
| `workspace.remove` | `{id}` | `{}` (archives its channels, kills their sessions, keeps worktrees) |
| `channel.create` | `CreateChannel` (src/hub.rs) | `Channel` (plus sessions arrive via `session_upserted`) |
| `channel.rename` | `{id, name}` | `Channel` |
| `channel.archive` | `{id, remove_worktree: bool}` | `{}` (kills sessions; emits `channel_removed`) |
| `channel.set_layout` | `{id, layout}` | `{}` (opaque JSON; no event echo to avoid fights) |
| `channel.mark_read` | `{id}` | `{}` |
| `channel.reorder` | `{ids: []}` | `{}` |
| `channel.post` | `{id, text}` | `Event` (a `note`) |
| `channel.timeline` | `{id, before?, limit?}` | `Event[]` (oldest first) |
| `channel.diff` | `{id}` | `{diff: string}` |
| `session.spawn` | `SpawnSession` (src/hub.rs) | `SessionInfo` |
| `session.attach` | `{id}` | `{}` then a `0x03` replay frame, then live `0x01` frames |
| `session.detach` | `{id}` | `{}` |
| `session.resize` | `{id, cols, rows}` | `{}` (last resizer wins) |
| `session.kill` | `{id}` | `{}` |
| `session.restart` | `{id}` | `SessionInfo` (new process, same id, same kind/cwd/name; replay cleared) |
| `session.remove` | `{id}` | `{}` (kills if alive, deletes; emits `session_removed`) |
| `session.rename` | `{id, name}` | `SessionInfo` |
| `session.send` | `{id, text, mode?: "queue"\|"interrupt"}` | `{delivered, queued}`. For shells the text is preceded by Ctrl-E Ctrl-U, which clears the input line (bash, zsh and fish in emacs mode; bash and zsh keep the cleared text in the kill ring, Ctrl-Y). A half-typed command is discarded rather than prepended to the delivered one. |
| `session.approve` | `{id, decision: "approve"\|"approve_always"\|"deny"}` | `{}` |
| `session.interrupt` | `{id}` | `{}` |
| `session.clear_queue` | `{id}` | `{}` |
| `session.screen` | `{id}` | `{lines: string[]}` current visible screen as text (inbox previews) |
| `fs.complete` | `{prefix}` | `{entries: string[]}` directory completions for the "add workspace" path box |

Exited sessions keep their scrollback (persisted under `$XDG_DATA_HOME/muster/sessions/<id>/output.log`)
so they can be attached (replay only) and restarted after a daemon restart. The file is
append-only: every 3 s new output is appended (no fsync). It is rewritten from the in-memory
ring (last 2 MiB) when it grows past 4 MiB, when more output arrived than the ring holds, and
for a new process; it is fsynced when the process exits and on daemon shutdown. Replays read
from it return at most the last 2 MiB.

Session processes: `session.kill`, `session.remove`, `session.restart` and daemon shutdown
signal the whole session (the process muster started is a session leader), also after that
process exited, so background jobs it left behind (`sleep 1000 &` then `exit`) are killed:
SIGHUP, then SIGKILL after 3 s. The session id and the run's `MUSTER_TOKEN` are stored, and
leftovers of an earlier daemon run are killed at the next start (only processes in that session
whose environment carries that session's `MUSTER_SESSION_ID` and that run's `MUSTER_TOKEN`).

## Event log

`channel.timeline` reads the event log. Retention: events older than 30 days, and all but the
newest 5000 per channel, are deleted at daemon start and every hour. `session.remove` deletes
the session's `hook` and `state_change` events. A `hook` event stores only a summary:
`{session_id, event, summary: {hook_event_name?, tool_name?, notification_type?, matcher?,
source?, reason?, type?}}` (string fields, 120 chars max), never the full payload.

## Session environment

Every session gets: `MUSTER=1`, `MUSTER_SESSION_ID`, `MUSTER_CHANNEL_ID`, `MUSTER_URL`,
`MUSTER_TOKEN` (session token), `MUSTER_BIN`, `PORT` and `MUSTER_PORT_BASE` (channel port block
of 10), `TERM=xterm-256color`, `COLORTERM=truecolor`.

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
| `POST /api/exchange` | the launch code | Body `{"code": "..."}` → `200 {"token": "<client token>"}` and the code is consumed. Unknown, used or expired code, or a bad body: `403 {"error": {"message": "invalid or expired launch code"}}`. Also `403` (code not consumed) when the request has no `Origin` or a foreign one, or when the connecting TCP socket isn't owned by the daemon's own uid (looked up in `/proc/net/tcp` / `tcp6`): a code on a browser command line is readable by other local users, who must not be able to redeem it. |

### Opening the UI

With the native window (`gui` feature, the default UI when built with it), `muster` and
`muster open` hand the client token to the window in-process: an initialization script puts it
in `sessionStorage["muster.token"]` (only on the daemon's origin) before the page loads, and the
window loads the plain URL. No token or code ever goes into a URL. Otherwise (or with
`MUSTER_BROWSER` set), `muster` and `muster open` get a launch code from `POST /api/launch-code`
and open `http://127.0.0.1:<port>/#code=<code>` in a browser. The client token itself never goes on the browser's
command line (`/proc/<pid>/cmdline` is readable by every user). The frontend:

1. If `location.hash` has `code=<code>`: `POST /api/exchange` with
   `Content-Type: application/json` and body `{"code": "<code>"}` (same origin). On `200`, store
   `token` from the reply like a `#token=` token. On `403`, the code was already used or has
   expired (60 s): fall back to a token already in `sessionStorage`, else tell the user to run
   `muster` again (or `muster url`). The `code` is removed from the URL only after the daemon
   answered (`200` or `403`); after a network error it stays, so a reload retries.
2. Else if `location.hash` has `token=<client token>` (printed by `muster url`, and still accepted
   for compatibility): use it directly.
3. Else use the token in `sessionStorage` (put there by 1, 2 or the native window).
4. Strip `token=` from the URL (`history.replaceState`) and keep the token in `sessionStorage`,
   so a reload doesn't need the (already consumed) code.

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

Terminal queries: the daemon is the **only** responder for DA1 (`CSI c` → `CSI ? 1 ; 2 c`),
DA2 (`CSI > c` → `CSI > 0 ; 276 ; 0 c`), DSR status (`CSI 5 n` → `CSI 0 n`) and cursor position
requests (`CSI 6 n` → `CSI <row> ; <col> R`, `CSI ? 6 n` → `CSI ? <row> ; <col> R`) that programs
print, whether or not clients are attached. Clients must not answer these (the web UI registers
xterm.js parser handlers that swallow them), so a program gets exactly one answer however many
clients watch, and input frames are always written to the PTY as they are (keys such as
Shift+F3, `ESC [ 1 ; 2 R`, are never mistaken for replies). Queries only a client can answer
(colors via OSC 4/10/11/12, DECRQM, DECRQSS) are answered live by the client, but not while it
parses replayed scrollback (the web UI brackets each replay with private in-band OSC markers).

Replay: the ring buffer holds the last 2 MiB of output, so the sequences that turned on terminal
modes may have scrolled out of it. Every replay therefore starts with the current input modes
(application cursor/keypad, bracketed paste, mouse reporting and encoding, from the daemon's
terminal emulator) and, if the replayed bytes begin inside the alternate screen, a switch to it
(`CSI ? 1049 h`). When the ring still holds everything, later bytes set the same modes again,
which is harmless. Replays of an exited session from an earlier daemon run (served from
output.log) get the same prefix, computed from the log (off the async workers, and cached per
session while the log is unchanged). output.log holds the ring's bytes exactly; readers skip a
partial first line once it has wrapped.

## Methods

All ids are numbers. Types (`Workspace`, `Channel`, `SessionInfo`, `Event`) are in `src/model.rs`.

| Method | Params | Result |
|---|---|---|
| `hello` | `{}` | `{version, workspaces[], channels[], sessions[], windows[], home, default_shell}` |
| `workspace.add` | `{path, name?}` | `Workspace` (path may start with `~`). Idempotent: an existing, non-removed workspace with the same canonical path is returned as is. |
| `workspace.remove` | `{id}` | `{}` (archives its channels, kills their sessions, keeps worktrees) |
| `channel.create` | `CreateChannel` (src/hub.rs): `{workspace_id, name? , worktree? = false, base?, agent?, prompt?, permissions?: Permissions, window_id?, cols? = 120, rows? = 32}` | `Channel` (plus sessions arrive via `session_upserted`). No `name` (or blank) = auto-named (see Windows, tabs and naming). `window_id` must name an existing window (else an error); without it, see "Where new channels go". `cols`/`rows` size the first agent's terminal. `permissions` defaults to the CLIs' defaults. |
| `channel.rename` | `{id, name}` | `Channel` (sets `auto_named: false`; renames a muster-owned worktree branch without upstream to `muster/<slug>`) |
| `channel.archive` | `{id, remove_worktree: bool}` | `{}` (kills sessions; emits `channel_removed`) |
| `channel.set_permissions` | `{id, permissions: {codex_sandbox, claude_mode}}` (both required) | `Channel` (emits `channel_upserted`). Human only: the MCP server has no way to call it. Applies to sessions started or restarted afterwards; running sessions keep the permissions they started with. |
| `channel.set_layout` | `{id, layout}` | `{}` (opaque JSON; no event echo to avoid fights) |
| `channel.mark_read` | `{id, at?}` | `{}` (`last_read_at` = max(current, `at` or now); `at` is capped at now) |
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
| `channel.move` | `{id, window_id, index?}` | `Channel` (moves the tab to another window, at `index` or the end; emits `channel_upserted` and `window_upserted` for both windows, or `window_removed` for the old one when nobody holds it and it has no tabs left) |
| `window.claim` | `{id?}` | `Window`. WebSocket only. Declares which window this connection shows. With `id`: that window (created if missing, so a stale id still works). Without: the lowest-id window that has tabs and no connection holding it, else a new window. A connection holds at most one window; claiming again releases the previous one. Claiming a window another connection holds takes it over (a reload's new connection may arrive before the old one closes). Also adopts orphaned channels (below). |
| `window.create` | `{name?}` | `Window` (a new window, not held; used by `muster open` to put a tab in it before the UI claims it). New windows get the lowest free id (1, 2, 3...). |
| `window.focused` | `{id}` | `{}` (records `last_focused_at`; clients call it on window focus) |
| `window.rename` | `{id, name}` | `Window` |
| `window.reorder_tabs` | `{id, channel_ids: []}` | `Window` (ids not in the window are ignored; tabs left out keep their relative order after the listed ones) |
| `window.raise` | `{id}` | `{raised: bool}`. Asks the window manager to focus the OS window titled `muster · <name>`: `hyprctl dispatch focuswindow title:^(...)$` when `HYPRLAND_INSTANCE_SIGNATURE` is set, `swaymsg '[title="^...$"] focus'` when `SWAYSOCK` is set, else `wmctrl -F -a <title>` or `xdotool search --name ^...$ windowactivate` on X11 (`DISPLAY`). `false` when no helper worked or no connection holds the window; clients then pull the tab instead. |
| `session.wait` | `{id, until?: string[], timeout_ms? = 600000, after_ms?}` | `{session: SessionInfo, matched: bool, timed_out: bool}`. Returns as soon as the state is in `until` (default `["idle","needs_approval","asking_question","error","stuck","exited"]`). `until` may also contain `needs_human` (any `needs_human()` state) and `done` (= `idle`); unknown names are an error. Returns immediately when already matching. If the session exits while waiting for other states: `matched: false, timed_out: false`. Max timeout 1 h. `after_ms` (unix ms, e.g. taken just before a `session.send`) applies MCP `wait_for_state`'s staleness rules (docs/MCP.md): a state entered before it doesn't count, and an `idle` from before it only counts once no state change showed up for 15 s. |
| `session.input` | `{id, data}` | `{}` raw bytes (UTF-8 string, escapes already applied) written to the PTY, like a keyboard `0x02` frame. |
| `session.output` | `{id, lines? = 200}` | `{lines: string[]}` the last `lines` lines of scrollback plus screen as plain text (ANSI stripped), oldest first, without trailing blank lines. Max 10000. Exited sessions are read from output.log. |

Exited sessions keep their scrollback (persisted under `$XDG_DATA_HOME/muster/sessions/<id>/output.log`)
so they can be attached (replay only) and restarted after a daemon restart. The file is
append-only: every 3 s new output is appended (no fsync). It is rewritten from the in-memory
ring (last 2 MiB) when it grows past 4 MiB, when more output arrived than the ring holds, and
for a new process; it is fsynced when the process exits and on daemon shutdown. Replays read
from it return at most the last 2 MiB.

Session processes: `session.kill`, `session.remove`, `session.restart` and daemon shutdown
signal the whole session (the process muster started is a session leader), also after that
process exited, so background jobs it left behind (`sleep 1000 &` then `exit`) are killed:
SIGHUP, then SIGKILL after 3 s. Once the process muster started has been reaped, its pid (the
session id) could be reused, so from then on only processes of that session that carry the run's
markers (`MUSTER_RUN` and `MUSTER_SESSION_ID` in their environment), or that hold this PTY's
slave open while the daemon still has its master, are signalled. `session.restart` waits until
the old process and its leftovers are gone (SIGKILL after the grace period) before starting the
new process, so e.g. a dev server can't keep the port. The process session id and the run's
markers are stored; `session.remove` keeps them (in a pending-reap record) until the kill is
confirmed. At the next daemon start, leftovers of earlier runs are killed (only processes in
that session whose environment carries every marker), and the start waits until they are gone
(up to about 4 s); records of processes that survive are kept for the next start.

## Windows, tabs and naming

A **window** is a client window; a channel is shown as a **tab** in exactly one window.

```ts
type Window = { id: number; name: string /* default: String(id) */; channel_ids: number[] /* tab order */;
                open: boolean /* a connection holds it */; last_focused_at: number; created_at: number };
```

`Channel` gains `window_id: number | null` and `auto_named: boolean`. Events:
`{"event":"window_upserted","data":Window}` and `{"event":"window_removed","data":{"id":n}}`.
When a connection holding a window closes, the window is released (`open: false`), and deleted
(`window_removed`) if it has no non-archived channels. Store migration: one window (`1`) is
created and every existing non-archived channel is put in it, in `sort_order`; existing
channels get `auto_named: false`. Channels whose `window_id` is null or points at a deleted
window are adopted by the next `window.claim`.

Where new channels go: `window_id` if given; for MCP `spawn_agent` creating a channel, the
caller's channel's window; else the open window with the latest `last_focused_at`, else the
lowest-id window, else a new window `1`.

**Auto naming.** A channel created without a name gets `auto_named: true` and `name` = the folder's
base name. While `auto_named`, the daemon re-derives `name` (emitting `channel_upserted` only on
change), in priority order: (1) the latest OSC title of an agent (claude/codex) session in the
channel, cleaned: leading non-alphanumeric glyphs (spinners like `✳ ⠂ ⠐ ·`) and surrounding
whitespace stripped, ignored when empty, when it equals (case-insensitively) `claude`,
`claude code`, `codex`, `codex cli`, or the kind name (for shells also a shell's name: `sh`,
`bash`, `zsh`, `fish`, `dash`, `ksh`, `nu`, `tcsh`), when it looks like a path (starts with
`/` or `~`) or a `user@host` prompt; (2) the first prompt sent to the channel (`channel.create`
`prompt` or `session.send`/MCP `send_message` to one of its sessions): its first line, first 6
words, at most 40 chars; (3) the latest title of a shell session, with the same filters; (4) the
folder base name. Names are capped at 60 chars. `channel.rename` ends auto naming. A title
that is filtered out is ignored (the previous usable title stays). Titles are kept in memory;
the first prompt is stored with the channel.

**Opening a specific window.** The launcher passes the window id to the page: the native window's init script sets `sessionStorage["muster.window"]`, and browser URLs carry `&window=<id>` in the hash (`#code=...&window=3`). The frontend claims that id, keeps it in `sessionStorage`, and strips it from the URL.

**Worktrees.** `worktree` now defaults to `false`. A worktree is created at
`<data>/worktrees/<workspace slug>-<workspace id>/<channel id>` on branch `muster/<channel id>`
(deduplicated). When `channel.create` already has a name (given, or derived from its `prompt`),
the branch is renamed right away, so the result carries it. Otherwise the first time an
auto-named channel gets a derived name (priority 1–3), and on every `channel.rename` (whose
result carries the new branch), the daemon renames the branch to `muster/<slug(name)>` (deduplicated with
`-2`, `-3`...) if `owns_worktree` and the branch has no upstream (`git rev-parse
--abbrev-ref <branch>@{upstream}` fails). Failures are logged, never fatal. The worktree
directory never moves.

## Channel permissions

Every `Channel` has `permissions: {codex_sandbox, claude_mode}`, a profile the human picks per
channel. The daemon maps it to CLI flags whenever a session's process starts (spawn and
restart), for every agent in the channel, including agents spawned through MCP:

| `codex_sandbox` | Codex flags |
|---|---|
| `read_only` | `--sandbox read-only` |
| `workspace_write` (default) | `--sandbox workspace-write -c sandbox_workspace_write.network_access=false` |
| `workspace_write_network` | `--sandbox workspace-write -c sandbox_workspace_write.network_access=true` |
| `full_access` | `--sandbox danger-full-access` |

| `claude_mode` | Claude Code flags |
|---|---|
| `default` (default) | no flag: your own Claude Code setting (`permissions.defaultMode`) applies |
| `accept_edits` | `--permission-mode acceptEdits` |
| `plan` | `--permission-mode plan` |
| `bypass` | `--permission-mode bypassPermissions` |

The flags are always passed, so the channel profile (not a `sandbox_mode`, `network_access` or
`defaultMode` in the user's own config) decides how a session starts. Approval policies are not
touched. The user's `extra_args` for a session come after these flags. Shells are not affected.
Channels created before this existed have the defaults (store migration v3). `permissions` in
`channel.create` and `channel.set_permissions` must name both fields (a partial profile is
rejected, so it can't silently reset the other half).

Limits: the profile bounds what muster starts agents with; it is not a boundary against the
agents themselves. Codex's workspace-write sandbox can still read the whole disk, including
the discovery file with the client token, so with network access (`workspace_write_network`),
and in any unsandboxed mode (`full_access`, `bypass`, or commands the human approves), an agent
can call this API as the user, e.g. `channel.set_permissions` on its own channel. Pick those
modes only for work you would let run with your own access. Sources: `codex
--help` (0.155.1), the Codex config reference and "agent approvals & security" docs
(`sandbox_workspace_write.network_access`, off by default), `claude --help` (2.1.280) and
code.claude.com/docs/en/permissions (`default` is labelled "Manual", `manual` is an alias).

## Event log

`channel.timeline` reads the event log. Retention applies to noise only (`hook`,
`state_change`, `mcp_call`): those older than 30 days, and all but the newest 5000 of them per
channel, are deleted at daemon start and every hour. Notes, prompts, agent posts, approvals,
session start/exit and system notices are never pruned. `session.remove` deletes
the session's `hook` and `state_change` events. A `hook` event stores only a summary:
`{session_id, event, summary: {hook_event_name?, tool_name?, notification_type?, matcher?,
source?, reason?, type?}}` (string fields, 120 chars max), never the full payload.

## Session environment

Every session gets: `MUSTER=1`, `MUSTER_SESSION_ID`, `MUSTER_CHANNEL_ID`, `MUSTER_URL`,
`MUSTER_TOKEN` (session token), `MUSTER_RUN` (random per process start, not a secret: it marks
the run's processes for reaping, and unlike `MUSTER_TOKEN` it survives agent CLIs that strip
`*TOKEN*`/`*KEY*`/`*SECRET*` variables from the commands they run), `MUSTER_BIN`, `PORT` and `MUSTER_PORT_BASE` (channel port block
of 10), `TERM=xterm-256color`, `COLORTERM=truecolor`.

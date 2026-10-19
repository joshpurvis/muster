# Muster client protocol (v1)

The daemon serves its UI and API on one localhost port (default 7433, else a free port).
Panels use a second, ephemeral `127.0.0.1` listener serving only `GET /proxy`.
Discovery file: `$XDG_RUNTIME_DIR/muster/daemon.json` (fallback `~/.local/state/muster/daemon.json`),
mode 0600: `{"pid": 123, "port": 7433, "panel_port": 51234, "token": "<client token>", "version": "0.1.0"}`.

| Route | Auth | Purpose |
|---|---|---|
| `GET /` and static assets | none | Frontend (embedded `web/dist`). |
| `GET /ws?token=<client token>` | client token | Client protocol below. |
| `GET /ws` | device cookie | The same protocol for a paired phone (see "Remote devices"). |
| `POST /api/device/pair` | pairing code | `{code, name?}` → `{device}` plus the device cookie. |
| `GET /api/device/me` | device cookie | `{device}` or `401`. |
| `POST /api/device/logout` | device cookie | Revokes the calling device, clears the cookie. |
| `GET /m/…` | none | The phone UI (`web/dist/m`), with a strict CSP. `/m` redirects to `/m/`. |
| `POST /hook/<event>` | `Authorization: Bearer <session token>` | Agent hook callbacks (body: JSON payload). |
| `POST /mcp` | `Authorization: Bearer <session token>` | MCP (streamable HTTP, JSON responses). |
| `GET /api/health` | none | `{"ok":true,"version":"..."}` |
| `POST /api/rpc` | `Authorization: Bearer <client token>` | Same JSON methods as the WebSocket (body `{id?, method, params}`, reply `{id, result}` / `{id, error}`), except `session.attach`/`session.detach`. Used by the CLI. |
| `GET /api/events` | `Authorization: Bearer <client token>` | The event stream as NDJSON (below). Used by `muster events`. Like `/ws`, a request with a foreign `Origin` gets `403`. |
| `POST /api/launch-code` | `Authorization: Bearer <client token>` | `{"code": "<64 hex>"}`: a single-use launch code, valid 60 s. Used by the CLI to open the UI. |
| `POST /api/exchange` | the launch code | Body `{"code": "..."}` → `200 {"token": "<client token>"}` and the code is consumed. Unknown, used or expired code, or a bad body: `403 {"error": {"message": "invalid or expired launch code"}}`. Also `403` (code not consumed) when the request has no `Origin` or a foreign one, or when the connecting TCP socket isn't owned by the daemon's own uid (looked up in `/proc/net/tcp` / `tcp6`): a code on a browser command line is readable by other local users, who must not be able to redeem it. |

### Event stream (`GET /api/events`)

`200`, `Content-Type: application/x-ndjson`, chunked, never ends while the daemon runs. Each
line is one event exactly as the WebSocket sends it (`{"event": "...", "data": {...}}`), for
`session_upserted`, `session_removed`, `channel_upserted`, `channel_removed`,
`window_upserted`, `window_removed`, `agent_upserted`, `agent_removed` and `agent_message`
only: no event-log entries (`event`), notices,
workspace events, method results or terminal output (nothing needs attaching). The daemon
subscribes before it sends the response headers, so a client that reads the headers and then
calls `hello` misses nothing in between. A reader that falls more than the bus's buffer behind
gets `{"event":"lagged","data":{"missed":N}}` and the stream goes on; call `hello` to re-sync.
The stream ends when the daemon stops. `401` without a valid client token.

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
| `0x02` | client → daemon | Keyboard input bytes (from xterm `onData`, UTF-8). Queued to the terminal in order without waiting for it. If any input for a run is lost, refused or dropped, that run's input is paused: later frames for it are discarded (never delivered after the gap) and the session gets `input_interrupted: true` until `session.continue_input`. |
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
| `hello` | `{}` | `{version, workspaces[], channels[], sessions[], panels[], panel_origin, windows[], account_profiles[], home, default_shell, updates, prefs}` (`updates`, `prefs`: see "Updates and window preferences") |
| `workspace.add` | `{path, name?}` | `Workspace` (path may start with `~`). Idempotent: an existing, non-removed workspace with the same canonical path is returned as is. |
| `workspace.remove` | `{id}` | `{}` (archives its channels, kills their sessions, keeps worktrees) |
| `channel.create` | `CreateChannel` (src/hub.rs): `{workspace_id, name? , worktree? = false, base?, agent?, prompt?, extra_args?: string[], permissions?: Permissions, window_id?, cols? = 120, rows? = 32, from_session?, account_profile?}` | `Channel` (plus sessions arrive via `session_upserted`). No `name` (or blank) = auto-named (see Windows, tabs and naming). `extra_args` (alias `args`) go to the first agent's CLI after the permission flags, as `session.spawn`'s `extra_args` do, and are kept for that session's restarts; they need `agent` (else an error). `window_id` must name an existing window (else an error); without it, see "Where new channels go". `cols`/`rows` size the first agent's terminal. `from_session` (`muster new` passes `$MUSTER_SESSION_ID`): the first session starts with that session's provenance one hop on (docs/MCP.md "Agents"), and a Claude agent is refused while it is on a non-human chain. `permissions` defaults to the CLIs' defaults. The first session is shown in the channel's window. |
| `channel.rename` | `{id, name}` | `Channel` (sets `auto_named: false`; renames a muster-owned worktree branch without upstream to `muster/<slug>`) |
| `channel.archive` | `{id, remove_worktree: bool}` | `{}` (kills sessions; emits `channel_removed`). With `remove_worktree`, refused (error, nothing closed) while a running or starting session in another channel has its cwd inside this worktree (it was moved out, see `session.move`); if one moves out while the close runs, the worktree is kept and a notice says why. From that re-check until the delete finishes, the directory is reserved: starting or restarting a session whose cwd is inside it fails. |
| `channel.close_if_empty` | `{id}` | `{closed: bool}`. Archives the channel (keeping any worktree) only if it has no sessions, checked and marked in one step, so a session spawned or moved into it meanwhile is never closed with it. |
| `channel.set_permissions` | `{id, permissions: {codex_sandbox, claude_mode}}` (both required) | `Channel` (emits `channel_upserted`). Human only: the MCP server has no way to call it. Applies to sessions started or restarted afterwards; running sessions keep the permissions they started with. |
| `channel.set_layout` | `{id, layout}` | `{}` (opaque JSON; no event echo to avoid fights) |
| `channel.mark_read` | `{id, at?}` | `{}` (`last_read_at` = max(current, `at` or now); `at` is capped at now) |
| `channel.reorder` | `{ids: []}` | `{}` |
| `channel.post` | `{id, text}` | `Event` (a `note`) |
| `channel.timeline` | `{id, before?, limit?}` | `Event[]` (oldest first) |
| `channel.diff` | `{id}` | `{diff: string}` |
| `session.spawn` | `SpawnSession` (src/hub.rs): `{channel_id, kind, name?, cwd?, prompt?, extra_args?, cols?, rows?, parent_id?, window_id?, from_session?, account_profile?}` | `SessionInfo`. `from_session` (as for `channel.create`): the new session starts with that session's provenance; a Claude session from a non-human chain is refused. `window_id` must name an existing window (else an error); without it, see "Groups layout (v2)". |
| `session.attach` | `{id, replay_bytes?}` | `{}` then a `0x03` replay frame, then live `0x01` frames. `replay_bytes` (at least 4096) replays only the last that many bytes of output, from a line start (the phone asks for 256 KiB instead of the whole 2 MiB buffer); the replays that follow a restart or a lagging client use the same limit. |
| `session.detach` | `{id}` | `{}` |
| `session.resize` | `{id, cols, rows}` | `{}` (last resizer wins). Sets `SessionInfo.size_owner` to `"local"` (client token) or `"device:<id>"`; a change of owner alone also emits `session_upserted`. `size_owner` is runtime only (null after a daemon restart). |
| `session.kill` | `{id}` | `{}` |
| `session.restart` | `{id}` | `SessionInfo` (new process, same id, same kind/cwd/name; replay cleared) |
| `session.remove` | `{id}` | `{}` (kills if alive, deletes; emits `session_removed`). Removing a channel's last session closes the channel (see "Groups layout (v2)"). |
| `session.rename` | `{id, name}` | `SessionInfo` |
| `session.move` | `{id, channel_id, from_channel_id?, close_empty_source? = false}` | `SessionInfo`. Moves the session (live or exited) to another channel: emits `session_upserted` with the new `channel_id` and re-derives both channels' auto names (`channel_upserted` on change). Moving into the session's own channel is a no-op; an unknown or archived channel, or a session whose process is being started or restarted right now (the start reads its channel), is an error; a session merely reporting the `starting` state (its process already launched) can move. With `from_channel_id`, the move is refused unless the session is still in that channel, so a client acting on a stale view can't move a session another client already moved. The process is not touched: it keeps its cwd, the permission flags it started with and its environment, so `MUSTER_CHANNEL_ID`, `PORT` and `MUSTER_PORT_BASE` still name the old channel until it restarts. **A restart applies the new channel's permission profile**, which may be more or less permissive than the one it ran with. MCP calls and hooks resolve the caller's channel from the session's current info (its token), never from the environment. The UI uses it to merge a tab into another (docs/UI.md); the emptied channel stays open until the client archives it. Refused while the source channel is being closed (archived), so a session can't slip out of an archive in progress. With `close_empty_source`, the source channel is then closed if this move emptied it (as `channel.close_if_empty`): the UI's tab merge uses this instead of archiving the source itself. The process keeps the port block it started with; that block stays reserved (not handed to a new channel) while the process runs. |
| `session.set_window` | `{id, window_id, from_window_id?}` | `SessionInfo`. Shows the session (tab) in another window; see "Groups layout (v2)". |
| `session.send` | `{id, text, mode?: "queue"\|"interrupt", from_session?}` | `{delivered, queued}`. `from_session` (`muster send` passes `$MUSTER_SESSION_ID`): an agent session's send runs MCP `send_message` as that session (guards, provenance; the reply then also has `sent_at_ms`); a shell session's carries that shell's provenance (docs/MCP.md "Agents"). For shells the text is preceded by Ctrl-E Ctrl-U, which clears the input line (bash, zsh and fish in emacs mode; bash and zsh keep the cleared text in the kill ring, Ctrl-Y). A half-typed command is discarded rather than prepended to the delivered one. |
| `session.approve` | `{id, decision: "approve"\|"approve_always"\|"deny", state_seq?, state_since?}` | `{}`. With `state_seq` and/or `state_since` (from the `SessionInfo` the caller rendered), refused with an error starting `stale:` when they no longer match. `state_seq` changes on every state or detail change and whenever a prompt is answered with it, so an approval never answers a prompt the caller didn't see, and a delayed duplicate never answers the next one. The `approval` event records `from` (`"human"` or `"device:<id>"`). |
| `session.interrupt` | `{id}` | `{}` |
| `session.queue` | `{id}` | `{queued: string[], uncertain: string\|null, stage: "selected"\|"typing"\|"submitting"\|null, paused: boolean}`. Waiting prompts survive backend replacement. A crash during typing/submission pauses delivery for human review; uncertain input is never automatically replayed. |
| `session.clear_queue` | `{id}` | `{}`. Discards waiting and paused delivery records. Does not undo input already sent, or cancel delivery currently in progress. |
| `session.continue_input` | `{id}` | `{}`. After an input interruption (`SessionInfo.input_interrupted`), accepts typing into the session again and lets waiting prompts be delivered. Nothing that may have been lost is resent. |
| `session.resume_queue` | `{id}` | `{}`. Forgets the paused (uncertain) delivery record and resumes the prompts waiting behind it. The uncertain prompt is not resent. Error when no delivery is paused. |
| `session.screen` | `{id}` | `{lines: string[]}` current visible screen as text (inbox previews) |
| `fs.complete` | `{prefix}` | `{entries: string[]}` directory completions for the "add workspace" path box |
| `channel.move` | `{id, window_id, index?}` | `Channel` (moves the task and all its sessions to another window, at `index` among the window's `channel_ids` or the end; emits `channel_upserted`, `session_upserted` for each session that changed window, and `window_upserted` for both windows, or `window_removed` for a window left unheld without tabs or tasks) |
| `window.claim` | `{id?}` | `Window`. WebSocket only. Declares which window this connection shows. With `id`: that window (created if missing, so a stale id still works). Without: the lowest-id window that has tabs (sessions) and no connection holding it, else a new window. A connection holds at most one window; claiming again releases the previous one. Claiming a window another connection holds takes it over (a reload's new connection may arrive before the old one closes). Also adopts orphaned channels and sessions (below). |
| `window.create` | `{name?}` | `Window` (a new window, not held; used by `muster open` to put a tab in it before the UI claims it). New windows get the lowest free id (1, 2, 3...). |
| `window.set_layout` | `{id, layout}` | `{}`. Stores the window's split layout (opaque JSON; `null` clears it). No event echo (like `channel.set_layout`). Unknown window: error. |
| `window.focused` | `{id}` | `{}` (records `last_focused_at`; clients call it on window focus) |
| `window.rename` | `{id, name}` | `Window` |
| `window.reorder_tabs` | `{id, channel_ids: []}` | `Window` (ids not in the window are ignored; tabs left out keep their relative order after the listed ones) |
| `window.show` | `{id}` | `{shown: "raised" \| "open" \| "launching" \| "launched"}`. Puts window `id` on screen. When a connection holds it: raises it (`"raised"`, or `"open"` when no WM helper could), never a second client. Otherwise the daemon launches `<muster binary> open --window <id>` detached (a native window can't open another one itself); Whether a client holds it and whether a launch is already on its way are decided under one lock, so concurrent calls can't both launch; repeated calls within 15 s, before a client claims the window, return `"launching"`. A launcher that exits non-zero within 3 s is an error (the UI then falls back); a later failure drops the launch record (so the next call retries) and sends a `notice`. |
| `window.raise` | `{id}` | `{raised: bool}`. Asks the window manager to focus the OS window titled `muster · <name>`: `hyprctl dispatch focuswindow title:^(...)$` when `HYPRLAND_INSTANCE_SIGNATURE` is set, `swaymsg '[title="^...$"] focus'` when `SWAYSOCK` is set, else `wmctrl -F -a <title>` or `xdotool search --name ^...$ windowactivate` on X11 (`DISPLAY`). `false` when no helper worked or no connection holds the window; clients then pull the tab instead. |
| `session.wait` | `{id, until?: string[], timeout_ms? = 600000, after_ms?}` | `{session: SessionInfo, matched: bool, timed_out: bool}`. Returns as soon as the state is in `until` (default `["idle","needs_approval","asking_question","error","stuck","exited"]`). `until` may also contain `needs_human` (any `needs_human()` state) and `done` (= `idle`); unknown names are an error. Returns immediately when already matching. If the session exits while waiting for other states: `matched: false, timed_out: false`. Max timeout 1 h. `after_ms` (unix ms, e.g. taken just before a `session.send`) applies MCP `wait_for_state`'s staleness rules (docs/MCP.md): a state entered before it doesn't count, and an `idle` from before it only counts once no state change showed up for 15 s. |
| `session.input` | `{id, data, keys? = false, from_session?}` | `{}` raw bytes (UTF-8 string, escapes already applied) written to the PTY, like a keyboard `0x02` frame. Without `from_session` (and from a shell with no provenance) it counts as the human's input for provenance; from an agent session (`muster keys` passes `$MUSTER_SESSION_ID`) it doesn't, and is refused for a Claude target while that agent is on a non-human chain. With `keys: true`, unmodified cursor keys (`CSI A`–`D`, `CSI H`, `CSI F`) are sent as `SS3` while the terminal is in application cursor mode, as a keyboard would. Refused while the session's input is interrupted (see `0x02`). |
| `agent.create` | `{name, kind? = "claude", model?, effort?, persona?, accepts?: ("human"\|"schedule"\|"webhook")[], permissions?: Permissions, autostart? = false}` | `Agent` (below). Names: 1-40 letters, digits, `-`, `_`, unique case-insensitively. Creates `<data>/agents/<lowercase name>/` (0700) with `memory/` and `CLAUDE.md` / `AGENTS.md` from the persona; a symbolic link at any of those paths is refused and the persona file is opened with `O_NOFOLLOW`. `model`/`effort` must be one plain word (no leading `-`) and go through the agent-argument allowlist. Default `accepts`: `["human"]` for claude, all three for codex. `permissions` (both fields): the profile its tab gets (default: the defaults). Emits `agent_upserted`. |
| `agent.update` | `{name, model?, effort?, persona?, accepts?, autostart?, permissions?}` | `Agent`. Absent fields stay; an empty `model`/`effort` clears it; unknown fields (`kind`, `name`) are refused. A new persona rewrites the persona file (memory/ untouched); a new profile is also set on the agent's tab. Model, effort and permissions apply from its next start. Emits `agent_upserted`. |
| `agent.templates` | `{}` | `Template[]` (below): Blank, Orchestrator, Researcher, Reviewer, PR reviewer, Background worker, for the New agent dialog. |
| `agent.messages` | `{agent?, before?, limit? = 50 (max 200)}` | `{messages: AgentMessage[], more: bool}`, newest first: messages to `agent` (else every agent) with id below `before`. |
| `agent.memory.list` | `{name}` | `{dir, files: [{path, size, modified}]}`: files under the agent's `memory/` (at most 500; links skipped), `/`-separated relative paths. |
| `agent.memory.read` | `{name, path}` | `{path, text, size}`. `path` is relative to `memory/`: no absolute path, no `..`, `.` or empty parts. Every component is opened from the one before it (`openat` with `O_NOFOLLOW`, from the home folder's fd), so a link anywhere, also one swapped in during the call, is refused. At most 256 KB, UTF-8 only. |
| `agent.memory.write` | `{name, path, text}` | `{path, size}`. Same path rules and walk; creates missing folders (`mkdirat`); refuses a link at the path; writes a temporary file of its own (random name, `O_EXCL`) and renames it into place (`renameat`). At most 256 KB. |
| `agent.list` | `{}` | `Agent[]` |
| `agent.remove` | `{name}` | `{removed: Agent}`. Removes its session too; the home dir stays. Emits `agent_removed`. |
| `agent.start` | `{name, fresh? = false, window_id?}` | `{agent: Agent, session: SessionInfo, started}`. The live session, else its exited one restarted with fresh agent args (`--resume <id>` for Claude), else a new session (cwd = home dir) in the agent's own channel (created once, named after the agent, in the "agents" workspace `<data>/agents`, and stored as the agent's `channel_id`; never an existing channel found by name). `fresh` forgets the conversation id and restarts the process (also a live one) without `--resume`. `window_id` (must exist): where a new tab goes (the agents panel passes its window). Starts are serialized. Emits `agent_upserted`. |
| `agent.ask` | `{agent, text, timeout_ms? = 600000 (1000..1800000), from_session?}` | The MCP `ask_agent` result (docs/MCP.md). `from_session` (`muster ask` passes `$MUSTER_SESSION_ID`): an agent session's ask runs MCP `ask_agent` as that session (its provenance, permission check, rate limit, audit); a shell session's carries that shell's provenance; without it the caller is the human (`human`, hop 0). No parameter sets the origin: schedules and webhooks will call the daemon-internal `agents::ask` with one. Refused (error) by the origin rule, hop limit or daily cap; a refused message is recorded with the reason (`agent_message`). |
| `session.output` | `{id, lines? = 200}` | `{lines: string[]}` the last `lines` lines of scrollback plus screen as plain text (ANSI stripped), oldest first, without trailing blank lines. Max 10000. Exited sessions are read from output.log. |

```ts
type Origin = "human" | "schedule" | "webhook";
type Agent = { id: number; name: string; kind: "claude" | "codex"; model: string | null; effort: string | null;
             persona: string; permissions: Permissions /* its tab's, when muster creates it */;
             accepts_origins: Origin[]; autostart: boolean; home_dir: string; session_id: number | null;
             channel_id: number | null /* its own tab */; state: AgentState | null /* its session's */; detail: string | null;
             resumable: boolean; created_at: number };
type AgentMessage = { id: number; from: string /* agent name, "session:<id>", "human" */; from_session: number | null;
             to: string /* recipient name, "#<id>" once removed */; to_agent: number; to_session: number | null;
             origin: Origin; hop: number; reply_to: number | null; created_at: number;
             text: string | null /* null: sent before text was kept */; reply: string | null; replied_at: number | null;
             refused: string | null /* why the policy refused it: never delivered */ };
type Template = { id: string; label: string; about: string; name: string; kind: "claude" | "codex";
             model: string | null; effort: string | null; persona: string; permissions: Permissions;
             accepts: Origin[]; autostart: boolean };
```

Agent events (also on `GET /api/events`; never sent to paired devices):
`{"event":"agent_upserted","data":Agent}` when an agent is created or its record changes (edit,
start, its tab, its conversation id), `{"event":"agent_removed","data":{"id","name"}}`, and
`{"event":"agent_message","data":AgentMessage}` when a message is recorded, refused or gets its
reply (the same id again). An agent's state changes arrive as its session's `session_upserted`.
A named agent's session always restarts (`session.restart`, the UI's Restart, `agent.start`)
with arguments rebuilt from the agent's record: model, effort, and `--resume` only with its
current conversation id.
Agent writes (`agent.create`, `agent.update`, `agent.remove`, `agent.memory.write`) need the
client token; no `agent.*` method has a device scope.

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

## Agent panels

These methods require the desktop client token; device clients cannot call them and receive
neither panel records nor panel events. The proxy listener has no daemon routes or credentials.

| Method | Params | Result |
|---|---|---|
| `panel.open/update/close/list/wait/fetch_event` | `{from_session, arguments}` | Corresponding MCP tool result (`fetch_event` is `panel_event`); same task scope and guards |
| `panel.resource` | `{id}` | `{panel, html, data}` |
| `panel.event` | `{panel_id, kind, payload}` | `{}`; 16 KiB payload limit, 30 events/minute/panel |
| `panel.context` | `{panel_id, context}` | `{}`; latest context, 16 KiB limit |
| `panel.dismiss` | `{id}` | `{}`; close with a human `closed` event |
| `panel.move` | `{id, window_id}` | `{}` |
| `panel.tool` | `{panel_id, name: "get_diff", arguments: {max_bytes?}}` | MCP tool result for the panel's task |

WebSocket pushes: `panel_upserted` carries a full panel, `panel_removed` carries `{id}`.
`hello.panels` contains open panels and `panel_origin` is `http://127.0.0.1:<panel_port>`.
Records contain `id`, `channel_id`, `session_id` (opener), `window_id`, `title`, `app`, `csp`,
`revision`, `html_revision`, timestamps, `closed`, latest `context`, and `dropped_events`.
Schema v14 stores records in `panels` and queued events in `panel_events`; v15 adds `parked`
(events a prompt showed as a placeholder, kept for `panel_event`). Immutable revision files
under `<data>/panels/<id>/` are written, mode 0600, under temporary names outside the panel
lock and renamed into place as the row commits. A closed panel's files are deleted; the
startup sweep removes files no record names and closed panels with nothing left to deliver.

Events admitted to an idle prompt move, in the same transaction, from `panel_events` into that
prompt's entry in the `prompt-queues` runtime record (as an optional `panel` field, with the
existing human-origin `Chain` source, so a backend without panels still loads the queue). They
return to `panel_events` with their ids if the entry is dropped before typing starts.

The UI embeds `/proxy?csp=<JSON>` on the separate origin, with sandbox permissions
`allow-scripts allow-same-origin`. The proxy creates an opaque `srcdoc` app with only
`allow-scripts`, validating exact parent origin and frame source in both directions. Its HTTP
CSP and the app's prepended meta CSP block connections, forms, objects and nested app frames.
Asset sources extend only script/style/image/font/media directives. `connectDomains` are
stored for future per-panel approval and grant no access. No token enters either frame.

The bridge speaks MCP Apps 2026-01-26 JSON-RPC: sandbox-ready/resource-ready handshake,
initialize/initialized, tool-input/result, host-context-changed, size-changed, open-link,
message, update-model-context, request-display-mode, resource-teardown and ping. Each data
revision sends fresh tool-input/result in the existing frame; changed HTML creates a frame.
Fullscreen zooms the group; pip keeps the current mode. Unknown methods/tools are refused.
Events use `ui/message` with user text; `_meta.muster.kind` optionally names a custom action.
The host checks transient user activation synchronously, plus a visible, focused panel frame,
before accepting a message or opening an HTTP(S) link. Context updates alone do not inject.
Native IPC also requires a random per-window secret injected only into the main UI frame.

## Windows, tabs and naming

A **window** is a client window; a channel is shown as a **tab** in exactly one window.

```ts
type Window = { id: number; name: string /* default: String(id) */; channel_ids: number[] /* tasks, in order */;
                open: boolean /* a connection holds it */; last_focused_at: number; created_at: number;
                layout: object | null /* v2: the frontend's split layout */ };
```

(Since v2 a tab is a session: see "Groups layout (v2)". What this section says about channels
as tabs still holds for `channel_ids`, the tasks whose window it is.)

`Channel` gains `window_id: number | null` and `auto_named: boolean`. Events:
`{"event":"window_upserted","data":Window}` and `{"event":"window_removed","data":{"id":n}}`.
When a connection holding a window closes, the window is released (`open: false`), and deleted
(`window_removed`) if it has no sessions and no non-archived channels. Store migration: one window (`1`) is
created and every existing non-archived channel is put in it, in `sort_order`; existing
channels get `auto_named: false`. Channels and sessions whose `window_id` is null or points at
a deleted window are adopted by the next `window.claim` (with `channel_upserted` /
`session_upserted`).

Where new channels go: `window_id` if given; for MCP `spawn_session` creating a channel, the
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

## Subagents

`SessionInfo.subagents` counts the built-in subagents a
Claude session is running right now (its Agent/Task tool): +1 on the tool's `PreToolUse`, -1
on its `PostToolUse`/`PostToolUseFailure`, reset to 0 on `SessionStart`, `SessionEnd`, `Stop`, `StopFailure` (a turn's end) and when the process exits, so a missed hook can't leave it stuck. Background
subagents return at launch, so only foreground ones are counted. Changes emit
`session_upserted`. The count survives application-backend replacement for the same
terminal generation and resets on an explicit terminal restart. Hook-derived state,
count, grace timestamp and reply attribution commit with the inbox cursor, so a
callback recovered after a lost acknowledgement cannot increment the count twice.

## Groups layout (v2)

The UI's structure is window → split groups → tabs, where a tab is one session. So a session
belongs to a window directly, and the split layout belongs to the window. A channel is a
**task**: folder, worktree, permissions, port block, name; its `window_id` is the task's default
window (for new sessions) and what `Window.channel_ids` lists.

- `SessionInfo.window_id: number | null`: the window the session is a tab in. Store migration
  v5: every existing session gets its channel's `window_id`; windows gain a `layout` column.
- Where a new session goes (`session.spawn`): `window_id` if given (must exist); else the
  parent session's window (`parent_id`: MCP `spawn_session` sets it to the caller, so an agent's
  new sessions land in the caller's window); else the channel's window; else where a new
  channel would go ("Where new channels go", which may create window 1). `channel.create`'s
  first session gets the channel's window. MCP `spawn_session` with `new_channel` puts the new
  channel in the caller's window (its own, else its channel's).
- `Window.layout: object | null`: opaque JSON owned by the frontend (the split tree of groups),
  in `hello` `windows[]` and every `window_upserted`. `window.set_layout {id, layout}` → `{}`
  with no event. Stored as JSON: the content round-trips, object key order may not (keys come
  back sorted), as with `channel.set_layout`.
- `session.set_window {id, window_id, from_window_id?}` → `SessionInfo`. Unknown window or
  session: error. With `from_window_id`, refused (`session N is no longer in window A (now in
  B)`) unless the session is still in that window. Moving to its own window is a no-op (no
  event). Otherwise emits `session_upserted`. The session stays in its channel (task), and
  keeps its process. The task's window **follows its tabs**: it becomes the new window when all
  its sessions are there, or when none of them is left in the task's window (so a task never
  holds on to a window without any of its tabs); then `channel_upserted` and `window_upserted`
  for the task's old and new windows. The window the session left, and the task's old window,
  are deleted (`window_removed`) when nobody holds them and they have no sessions and no
  non-archived channels.
- A window "has tabs" when it has sessions: `window.claim` without an id takes the lowest-id
  unheld window with sessions; a released window is deleted only when it has no sessions and no
  non-archived channels (also at daemon start, for every window).
- A channel whose last session is removed (`session.remove`) is closed by the daemon, as
  `channel.close_if_empty` (worktree kept): `session_removed`, then `channel_removed` and the
  window's update. The check and the close happen under one lock, and a spawn checks that its
  channel is open and inserts its session under that same lock, so a session spawned (or moved)
  into the channel meanwhile keeps it open. Only removals close: a channel created without an
  agent, or emptied by `session.move`, stays until one of its sessions is removed or the client
  closes it. When the channel stays open, its window follows its remaining tabs as above.
  `channel.archive` still closes a channel with all its sessions.
- `session.move` (between channels) doesn't change the session's window.
- `channel.move` stays for API clients: it moves the task and all its sessions to the window.
- `muster ls` shows each session's own `window_id`; `muster move <target> --window N` calls
  `session.set_window`.

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

Codex's flags are always passed, so its channel profile (not a `sandbox_mode` or
`network_access` in the user's own config) decides how a Codex session starts. Claude's
`default` profile passes no mode flag, so the user's own `permissions.defaultMode` applies
there; the other Claude profiles pass `--permission-mode`. Approval policies are not touched.
The user's `extra_args` for a session (`session.spawn`, `channel.create` for its first agent,
MCP `spawn_session` `model`/`effort`/`args`) come after these flags, and since both CLIs let a
later flag win they are checked against an **allowlist** (also again on every restart):
Claude `--model`, `--effort`, `--fallback-model`, `--append-system-prompt`, `-c/--continue`,
`-r/--resume [id]`, `--verbose`, `-d/--debug [filter]`; Codex `-m/--model`, `-c/--config`
with the keys `model`, `model_reasoning_effort`, `model_reasoning_summary`, `model_verbosity`,
`hide_agent_reasoning`, `show_raw_agent_reasoning`, `-i/--image`, `--no-alt-screen`. Values
may be attached (`--model=x`, `-mx`, `-cmodel=x`). Anything else (sandbox, approval policy,
allowed tools, extra dirs, settings, profiles, positional arguments) is refused. Shells are
not affected.

Claude sessions also get `permissions.allow: ["mcp__muster"]` in their per-session settings
file, so calls to muster's own MCP tools skip Claude Code's `auto`-mode safety classifier
(about 1.5-4 s per call otherwise). Muster's own guardrails still apply to those tools.
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
For agents (PLAN.md §10) the daemon also reads, before summarizing: SessionStart's `session_id`
and Codex notify's `thread-id` (the agent's conversation id), UserPromptSubmit's `prompt` (only its
`[muster msg …]` envelope) and the turn's final message (Stop `last_assistant_message`, Codex
notify `last-assistant-message`), which is stored as the reply in `agent_messages`. `muster hook`
cuts payload strings at 4000 bytes, those final-message fields at 64 KiB.

## Claude account profiles

`AccountProfile` is `{id, name, config_dir, enabled, created_at}`. It references an existing
canonical directory, never credentials. `hello.account_profiles` and
`account_profiles_changed` carry the complete list. These fields are additive; older clients
may ignore them. `Workspace`, `SessionInfo`, and named-agent records include nullable
`account_profile_id`. Migration v13 leaves all existing records null.

| Method | Params | Result |
|---|---|---|
| `account_profile.list` | `{}` | `AccountProfile[]` |
| `account_profile.create` | `{name, config_dir}` | `AccountProfile`; requires an existing directory, rejects duplicate canonical paths and case-insensitive duplicate names |
| `account_profile.update` | `{id, name?, enabled?}` | `AccountProfile`; the directory is immutable; disabling leaves running processes alone |
| `workspace.set_account_profile` | `{id, account_profile}` | `Workspace`; profile ID or `"default"`; affects only future selection |

These management methods are local-client-only (not available to paired devices or MCP).
There is no login, token, configuration-file copy, or directory deletion API.

`channel.create`, `session.spawn`, and `agent.create` accept `account_profile?: number |
"default"`. It is valid only for Claude. For new sessions, absent/null inherits from a Claude
parent/caller, then the workspace default; explicit `"default"` bypasses inheritance.
`agent.create` instead defaults to ordinary inherited Claude behavior and permanently pins
its choice on the agent record. `agent.update` cannot change it.

Selection is resolved before creation and saved on the session. Restarts never re-resolve
workspace defaults. Named-agent replacement sessions use the agent's pinned selection and
resume ID. Missing/disabled profiles, unavailable or redirected directories, and known
inherited authentication/endpoint conflicts fail rather than falling back. Restart performs
this preflight before interrupting the previous run. Error messages name conflicting
environment variables without their values. Default/null-pinned sessions retain the legacy
environment behavior, including any inherited `CLAUDE_CONFIG_DIR`.

The launcher sets only `CLAUDE_CONFIG_DIR` in the selected child's existing environment (or,
for a profile at the canonical `$HOME/.claude` with an absolute HOME, removes it, so Claude
Code uses `~/.claude` and `~/.claude.json`) and
uses the ordinary adapter arguments. It never opens profile contents or invokes Claude auth
commands. Configuration and account changes made outside Muster remain Claude Code's
responsibility; a profile ID is not a verified account identity. The terminal host's complete
environment field already carries the selection, so no host wire change is needed.

## Session environment

Every session gets: `MUSTER=1`, `MUSTER_SESSION_ID`, `MUSTER_CHANNEL_ID`, `MUSTER_URL`,
`MUSTER_TOKEN` (session token), `MUSTER_RUN` (random per process start, not a secret: it marks
the run's processes for reaping, and unlike `MUSTER_TOKEN` it survives agent CLIs that strip
`*TOKEN*`/`*KEY*`/`*SECRET*` variables from the commands they run), `MUSTER_BIN`, `PORT` and `MUSTER_PORT_BASE` (channel port block
of 10), `TERM=xterm-256color`, `COLORTERM=truecolor`.

They are set when the process starts. After `session.move` the channel values (`MUSTER_CHANNEL_ID`,
`PORT`, `MUSTER_PORT_BASE`) are stale until the session restarts; `MUSTER_SESSION_ID` stays
right, so tools should look the channel up from the session (as `muster new` does).

## Remote devices

A phone reaches the daemon through `tailscale serve` (docs/ANDROID.md). The daemon still binds
127.0.0.1 only.

**Proxied requests.** A request is *proxied* when its TCP peer socket belongs to the proxy uid
(`MUSTER_PROXY_UID`, default 0, i.e. a system tailscaled; ignored when the daemon runs as that
uid) or when it carries any `X-Forwarded-*`, `Forwarded` or `Tailscale-*` header. Proxied
requests reach only `/m/*`, `/api/device/*`, `/ws`, `/api/rpc` and `/api/health` (everything
else is `404`), and a client token is refused there (`401`): through the proxy only a device
cookie authenticates. `Tailscale-Funnel-Request` is always `403`. `X-Forwarded-For` (for logs and
rate limits) and `Tailscale-User-Login` are read only from a trusted proxy (the uid check).

**Pairing.** `device.pair_code` (client token only) starts a pairing; one is pending at a time
(a new one replaces it), valid `ttl_s` (default 120, 10..600), single use, with a 64-hex `code`
(the QR link `https://<host>/m/#pair=<code>`) and a 9-character `short_code` (base32 without
I/O/0/1; case, spaces and dashes ignored). Three wrong codes burn it; 5 attempts per minute per
source. Its URL's origin joins the persisted `remote_origins` list.

**Device cookie.** `__Host-muster_device=<64 hex>; HttpOnly; Secure; SameSite=Strict; Path=/;
Max-Age=2592000`, stored hashed (SHA-256). Honoured only without `?token=`/`Authorization`, and
(on `/ws`, `POST /api/rpc`, `pair`, `logout`) only with an `Origin` that is in `remote_origins` or
this daemon's local origin; cookie `POST`s also need `X-Muster: 1`. Devices expire after 30 days
without use (the expiry slides, renewed at most daily by `/api/device/me`, which re-sends the
cookie). A device's WebSocket closes at once on revoke, and within 30 s of expiry. A device
paired through a trusted proxy with a `Tailscale-User-Login` is refused when a later request
carries a different login.

**Scopes.** Checked on every call (so a revoked or expired device fails on its next call, and
its sockets close at once on revoke):

| Scope | Methods |
|---|---|
| `view` | `hello`, `session.attach/detach/screen/output/wait`, `channel.timeline/diff` |
| `interact` | `view` + `session.send/input/approve/interrupt/clear_queue/resume_queue/continue_input/resize`, `channel.mark_read/post`, binary `0x02` input frames |
| `manage` | `interact` + `channel.create/archive/rename`, `session.spawn/kill/restart/remove/rename`, `workspace.add`, `fs.complete`; never with a `permissions` or non-empty `extra_args` param |
| never | `device.*`, `agent.*`, `window.*`, `daemon.*`, `prefs.*`, `channel.set_permissions/set_layout/reorder/move`, `session.move/set_window`, `workspace.remove`, `channel.close_if_empty` |

Prompts a device sends are logged with `from: "device:<id>"`.

| Method (client token only) | Params | Result |
|---|---|---|
| `device.pair_code` | `{ttl_s?, name?, scopes? = ["view","interact"], url?}` | `{pairing_id, code, short_code, url, origin, expires_at, port}`. `url` defaults to `https://<Self.DNSName from tailscale status --json>`. |
| `device.pair_status` | `{pairing_id}` | `{state: "pending"}`, `{state: "paired", device}`, or `{state: "burnt"\|"expired"\|"replaced"}` |
| `device.list` | `{}` | `Device[]`: `{id, name, scopes, ts_login, created_at, last_seen_at, last_source, expires_at, revoked_at}` |
| `device.revoke` | `{id}` or `{all: true}` | `{revoked: id[]}` |

### Backend lifetime and compatibility

`hello.terminal_host` identifies the standalone owner (`protocol`, `instance`, `pid`,
`data_dir`), or is null when no host is configured. Clients use this capability to
verify backend-only reload is safe. A legacy backend without this capability must
not be stopped as part of a supposedly non-disruptive update.

`state_seq` values are exact JavaScript integers reserved durably before use. They
change across backend restarts, so an approval queued against an old view is stale.
`hello.paused_deliveries` lists session IDs whose prompt queues need review after an
interrupted submission; use `session.queue` to inspect them.

`hello.ui_protocol` is the UI/application protocol major version, currently 1. A missing
value denotes the original v1 API. A UI receiving a different version rejects queued
calls and input until a compatible UI is loaded; this version is independent of the
terminal host IPC version and of the application release number.

UI builds include `muster-ui.json` with `{"protocol":1}`. `muster ui-install` copies a
complete build to `<data>/ui/releases/<id>` and atomically replaces `<data>/ui/current`.
The backend reads this pointer for each page request, falling back to embedded assets
only when no external release is configured. Invalid configured releases produce 503,
without changing running sessions or API availability. Installed HTML pins JS/CSS to
`/_ui/<id>/...` (desktop) or `/m/_ui/<id>/...` (phone), so old pages cannot mix bundles
with a later release. Phone release assets retain the phone CSP and proxy restrictions.

### Updates and window preferences

`hello.updates` (and the `updates_changed` event, sent whenever it changes) says what is
waiting: `{ui_release, backend, host}`.

- `ui_release`: the installed UI release new pages load (the `<id>` that `ui/current` names), or
  null while the backend serves embedded assets or `MUSTER_WEB_DIR`. A page compares it with
  the release in its own script path (`/_ui/<id>/app.js`); a difference means `ui-install`
  activated a newer UI, and reloading the page loads it.
- `backend`: a content id (the first 12 hex digits of a SHA-256) when the executable this
  backend was started from (its original path, such as `target/release/muster`, not the
  snapshot under the data dir) now holds a different build; null otherwise.
- `host`: a content id when the `muster-terminal-host` beside that executable differs from the
  running host (compared with the host's `/proc/<pid>/exe`); null otherwise, or when the
  running host's executable can't be read.

The backend checks every 2 s with `stat` only, and hashes a file once its size, mtime and
inode have held still for one check, so a build still being written is never hashed. No file
watcher and no polling by clients.

| Method | Params | Result |
|---|---|---|
| `daemon.reload` | `{expect}` | `{exe}`. `expect`: the `updates.backend` id the human was offered (at least 12 hex digits of its SHA-256). The backend hashes the file at its launch path `exe` again and refuses unless it is that build (refused: still being written, or replaced since) and not the build already running (refused: a second window's late click after the restart). Then it runs `<exe> reload` detached: the CLI's own safety checks, then it stops this backend and starts the build at `exe`. Terminals keep running; clients reconnect as after any backend restart. A reload the CLI refuses leaves this backend running and sends an error `notice`. Clients never queue it for a later connection. |
| `daemon.shutdown` | `{}` | `{}`. What `muster stop` does: runs this backend's own `muster stop` detached (it stops the backend and then the terminal host, ending every session), and once that has started emits `daemon_stopping` `{reason: "shutdown"}`. If it can't start, the call fails and nothing is emitted. Clients stop reconnecting only on `daemon_stopping` or this call's reply, never on a dropped connection. |
| `prefs.get` | `{}` | `{decorations, revision}`: preferences every window shares (stored in the database, so a new native window reads them before it is built). `decorations`: native windows show the system title bar (default true). `revision` rises with every `prefs.set`. |
| `prefs.set` | `{decorations?}` | The new preferences; fields left out stay, unknown fields are refused. Writes are serialized from read to broadcast; each emits `prefs_changed` with the result. A client ignores a reply or event with a lower `revision` than one it has already applied (a reply can arrive after a later change's event). |

What "human only" means for `daemon.reload`, `daemon.shutdown` (and `prefs.set`, which needs
the client token too): they are refused to paired devices (no device scope), to MCP (no tool)
and hooks (session tokens can't reach this API), and `daemon.*` to calls carrying a
`from_session` param (the CLI inside a session sends one). That is all. Any process running as
the user that holds the client token (the discovery file and `<data>/client_token` are readable
by the user's agents), or that can run `muster stop` / `muster reload` itself, can still restart
or stop Muster, as with `channel.set_permissions` (see "Channel permissions", Limits). Binding
a restart to the offered build (`expect`) does not change that either: a process of the user
can replace the launch file between the check and the start.

### Callback recovery

New host-managed runs set `MUSTER_HOOK_DB` for `muster hook`. Helpers append JSON callbacks
to the application's private `hook-inbox.sqlite`, independently of HTTP availability.
The backend reads insertion IDs in order; the application transaction saves agent reply,
resume ID, attribution and inbox cursor together before acknowledging deletion. A
repeated inbox row cannot be assigned to a newer message. Credentials must match the
session's recorded terminal generation. The existing `POST /hook/<event>` remains
available for legacy callbacks and does not acquire automatic transport retries.

The inbox limits pending payloads to 64 MiB and 100,000 events. Overflow is recorded
and reported, and the helper remains bounded by its two-second write deadline. Full
payloads stay in the private inbox only until acknowledged; the timeline still records
hook summaries. These are application features, outside terminal-host IPC.

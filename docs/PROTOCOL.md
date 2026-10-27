# Muster client protocol (v1)

The daemon serves its UI and API on one localhost port (default 7433, else a free port).
Panels use a second, ephemeral `127.0.0.1` listener serving only `GET /proxy` and `GET /preview`.
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
| `POST /api/trigger/<name>` | `Authorization: Bearer <the trigger's secret>` | Fire a URL trigger (body: up to 16 KiB of data, handed to its agent as quoted untrusted text). This machine only; see "Schedules", Phase 2. |
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
| `hello` | `{}` | `{version, workspaces[], channels[], sessions[], panels[], panel_origin, notes[] (summaries, see "Notes"), windows[], account_profiles[], home, default_shell, login_shell, updates, prefs, pid, build, openrouter: {key_present}}` (`updates`, `prefs`: see "Updates and window preferences"; `build`: the running backend build's content id, the first 12 hex digits of its SHA-256 as `updates.backend` names a waiting one, so a restart can tell which build answers; `pid`: the backend's own, so a CLI that can't see its process (a sandboxed command in its own PID namespace) can still tell that the discovery file names this backend; `openrouter`: see "OpenRouter", never the key itself) |
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
| `session.remove` | `{id}` | `{}` (kills if alive, deletes; emits `session_removed`). Removing a channel's last session closes the channel (see "Groups layout (v2)"). A named agent's session is never removed this way: it is hidden instead, as `session.hide`, and the reply is `{hidden: true, session: SessionInfo}` (so no client can stop an agent by closing its tab, whatever it knows about agents); `agent.remove` removes it. |
| `session.hide` | `{id}` | `SessionInfo`. Closes a named agent's tab without stopping it: the session keeps its process, scrollback and channel and gets `hidden: true`, `window_id: null` (see "Groups layout (v2)", hidden agent tabs). Hiding a hidden one is a no-op; a session that isn't a named agent's is refused (`session N is not a named agent's`), so ordinary tabs close only with `session.remove`. `session.set_window` shows it again. No device scope. |
| `session.rename` | `{id, name}` | `SessionInfo` |
| `session.move` | `{id, channel_id, from_channel_id?, close_empty_source? = false}` | `SessionInfo`. Moves the session (live or exited) to another channel: emits `session_upserted` with the new `channel_id` and re-derives both channels' auto names (`channel_upserted` on change). Moving into the session's own channel is a no-op; an unknown or archived channel, or a session whose process is being started or restarted right now (the start reads its channel), is an error; a session merely reporting the `starting` state (its process already launched) can move. With `from_channel_id`, the move is refused unless the session is still in that channel, so a client acting on a stale view can't move a session another client already moved. The process is not touched: it keeps its cwd, the permission flags it started with and its environment, so `MUSTER_CHANNEL_ID`, `PORT` and `MUSTER_PORT_BASE` still name the old channel until it restarts. **A restart applies the new channel's permission profile**, which may be more or less permissive than the one it ran with. MCP calls and hooks resolve the caller's channel from the session's current info (its token), never from the environment. The UI uses it to merge a tab into another (docs/UI.md); the emptied channel stays open until the client archives it. Refused while the source channel is being closed (archived), so a session can't slip out of an archive in progress. With `close_empty_source`, the source channel is then closed if this move emptied it (as `channel.close_if_empty`): the UI's tab merge uses this instead of archiving the source itself. The process keeps the port block it started with; that block stays reserved (not handed to a new channel) while the process runs. |
| `session.set_window` | `{id, window_id, from_window_id?}` | `SessionInfo`. Shows the session (tab) in another window, or a hidden agent tab again; see "Groups layout (v2)". |
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
| `agent.create` | `{name, kind? = "claude", provider? = "subscription", model?, effort?, persona?, accepts?: ("human"\|"schedule"\|"webhook")[], permissions?: Permissions, autostart? = false}` | `Agent` (below). Names: 1-40 letters, digits, `-`, `_`, unique case-insensitively. Creates `<data>/agents/<lowercase name>/` (0700) with `memory/` and `CLAUDE.md` / `AGENTS.md` from the persona; a symbolic link at any of those paths is refused and the persona file is opened with `O_NOFOLLOW`. `model`/`effort` must be one plain word (no leading `-`) and go through the agent-argument allowlist. Default `accepts`: `["human"]` for claude, all three for codex. `permissions` (both fields): the profile its tab gets (default: the defaults). `provider: "openrouter"` runs it on OpenRouter (see "OpenRouter"): refused while the backend has no key, and for Codex without a `model`. Emits `agent_upserted`. |
| `agent.update` | `{name, provider?, model?, effort?, persona?, accepts?, autostart?, permissions?}` | `Agent`. Absent fields stay; an empty `model`/`effort` clears it; unknown fields (`kind`, `name`) are refused. A new persona rewrites the persona file (memory/ untouched); a new profile is also set on the agent's tab. Provider, model, effort and permissions apply from its next start. Switching to `openrouter` needs the key (editing an agent already there doesn't); a Codex agent there keeps needing a model. Emits `agent_upserted`. |
| `agent.templates` | `{}` | `Template[]` (below): Blank, Orchestrator, Researcher, Reviewer, PR reviewer, Background worker, for the New agent dialog. |
| `agent.models` | `{}` | `{claude: KindModels, codex: KindModels}` (below): the models and effort levels the agent dialog offers per CLI. A curated list in `src/agents.rs` (`model_catalog`), not read from the CLIs; any other one-word value is still accepted by `agent.create` / `agent.update`. |
| `openrouter.models` | `{}` | `{key_present, models: OpenRouterModel[], fetched_at, stale, error}`: OpenRouter's models a coding agent can use, for the agent dialog (see "OpenRouter"). No device scope. |
| `agent.messages` | `{agent?, before?, limit? = 50 (max 200)}` | `{messages: AgentMessage[], more: bool}`, newest first: messages to `agent` (else every agent) with id below `before`. |
| `agent.memory.list` | `{name}` | `{dir, files: [{path, size, modified}]}`: files under the agent's `memory/` (at most 500; links skipped), `/`-separated relative paths. |
| `agent.memory.read` | `{name, path}` | `{path, text, size}`. `path` is relative to `memory/`, used exactly as given (never trimmed: `notes.md ` is its own file): no absolute path, no `..`, `.` or empty parts. Every component is opened from the one before it (`openat` with `O_NOFOLLOW`, from the home folder's fd), so a link anywhere, also one swapped in during the call, is refused. At most 256 KB, UTF-8 only. |
| `agent.memory.write` | `{name, path, text}` | `{path, size}`. Same path rules and walk. Saving a file that exists keeps its name; a new file (and its new folders) must have parts without control characters, Unicode default-ignorable code points or spaces before or after them. Creates missing folders (`mkdirat`); refuses a link at the path; writes a temporary file of its own (random name, `O_EXCL`) and renames it into place (`renameat`). At most 256 KB. |
| `agent.memory.add` | `{name, file_name, text}` | `{path, size, renamed}`. Adds a new file at the top of `memory/` (the UI's Add files… and drop) and never replaces anything: a taken name (file, folder or link) gets `-2` … `-99` before its extension (`notes.md` → `notes-2.md`; `renamed` says so). `file_name` is one plain name, exactly as given (refused, never trimmed, when it starts or ends with a space): at most 200 bytes, no `/` or `\`, not starting with `.` (no `.`, `..` or hidden files), no control characters and no Unicode default-ignorable code points (soft hyphen, combining grapheme joiner, zero-width, bidi marks, embeddings, overrides, isolates, variation selectors, fillers, BOM, tags). `text`: at most 256 KB, no NUL characters (binary); valid UTF-8 by being a JSON string (a lone surrogate escape fails to parse). Written to a temporary file (`O_EXCL`, 0600) in `memory/` (opened as for a read) and linked into place with `linkat`, which fails on any existing entry, so nothing is overwritten or written through. |
| `agent.memory.delete` | `{name, path}` | `{path}`. Removes one regular file under `memory/` (same path rules and walk as a read); a folder or a link is refused. |
| `agent.list` | `{}` | `Agent[]` |
| `agent.remove` | `{name}` | `{removed: Agent}`. Removes its session too; the home dir stays. Emits `agent_removed`. |
| `agent.start` | `{name, fresh? = false, window_id?}` | `{agent: Agent, session: SessionInfo, started, provider_pending}` (`provider_pending`: the running process keeps the provider it started with, not the agent's saved one; see "OpenRouter"). The live session, else its exited one restarted with fresh agent args (`--resume <id>` for Claude), else a new session (cwd = home dir) in the agent's own channel (created once, named after the agent, in the "agents" workspace `<data>/agents`, and stored as the agent's `channel_id`; never an existing channel found by name). `fresh` forgets the conversation id and restarts the process (also a live one) without `--resume`. `window_id` (must exist): where a new tab goes (the agents panel passes its window). A hidden agent tab stays hidden (also when it is restarted): the client shows it with `session.set_window`. Starts are serialized. Emits `agent_upserted`. |
| `agent.ask` | `{agent, text, timeout_ms? = 600000 (1000..1800000), from_session?}` | The MCP `ask_agent` result (docs/MCP.md). `from_session` (`muster ask` passes `$MUSTER_SESSION_ID`): an agent session's ask runs MCP `ask_agent` as that session (its provenance, permission check, rate limit, audit); a shell session's carries that shell's provenance; without it the caller is the human (`human`, hop 0). No parameter sets the origin: schedule runs go through the same daemon-internal path with origin `schedule` (see "Schedules"). Refused (error) by the origin rule, hop limit or daily cap; a refused message is recorded with the reason (`agent_message`). |
| `session.output` | `{id, lines? = 200}` | `{lines: string[]}` the last `lines` lines of scrollback plus screen as plain text (ANSI stripped), oldest first, without trailing blank lines. Max 10000. Exited sessions are read from output.log. |

```ts
type Origin = "human" | "schedule" | "webhook";
type Agent = { id: number; name: string; kind: "claude" | "codex"; provider: "subscription" | "openrouter";
             model: string | null; effort: string | null;
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
type KindModels = { models: { id: string /* passed to --model / -m */; note: string | null;
             efforts: string[] | null /* this model's levels where they differ; null: the CLI's */ }[];
             efforts: string[] /* passed to --effort / -c model_reasoning_effort= */ };
type OpenRouterModel = { id: string /* --model / -m, e.g. "anthropic/claude-sonnet-5.5" */; name: string;
             vendor: string /* the id's prefix, for grouping */; context_length: number | null;
             prompt_per_m: number | null; completion_per_m: number | null /* USD per million tokens */;
             created: number | null /* unix s */; alias_of: string | null /* for "~vendor/…-latest" aliases */ };
```

Agent events (also on `GET /api/events`; never sent to paired devices):
`{"event":"agent_upserted","data":Agent}` when an agent is created or its record changes (edit,
start, its tab, its conversation id), `{"event":"agent_removed","data":{"id","name"}}`, and
`{"event":"agent_message","data":AgentMessage}` when a message is recorded, refused or gets its
reply (the same id again). An agent's state changes arrive as its session's `session_upserted`.
A named agent's session always restarts (`session.restart`, the UI's Restart, `agent.start`)
with arguments rebuilt from the agent's record: model, effort, and `--resume` only with its
current conversation id.
Agent writes (`agent.create`, `agent.update`, `agent.remove`, `agent.memory.write`,
`agent.memory.add`, `agent.memory.delete`) need the client token; no `agent.*` method has a
device scope.

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
| `panel.navigate` | `{id, url}` | `{url, changed}`; the user's address for a `web_preview` panel: completed (`3000`, `localhost:3000`) and checked like `panel_update`, stored as a data update, and queued to the agent as a `navigated` event `{url, previous}`; the same address returns `changed: false` and stores nothing. Refusals are worded for the user |
| `panel.preview_check` | `{id}` | A `web_preview` panel's dev server, checked by the backend at its stored URL only: `{reachable, answered?, status?, refused?, leaves_to?}` (below) |
| `panel.event` | `{panel_id, kind, payload}` | `{}`; 16 KiB payload limit, 30 events/minute/panel |
| `panel.context` | `{panel_id, context}` | `{}`; latest context, 16 KiB limit |
| `panel.dismiss` | `{id}` | `{}`; close with a human `closed` event |
| `panel.move` | `{id, window_id}` | `{}` |
| `panel.tool` | `{panel_id, name: "get_diff", arguments: {max_bytes?}}` | MCP tool result for the panel's task |

WebSocket pushes: `panel_upserted` carries a full panel, `panel_removed` carries `{id}`.
`hello.panels` contains open panels and `panel_origin` is `http://127.0.0.1:<panel_port>`.
Records contain `id`, `channel_id`, `session_id` (opener), `window_id`, `title`, `app`, `csp`,
`revision`, `data_revision` (the revision of the last update that carried data), `html_revision`, timestamps, `closed`, latest `context`, and `dropped_events`.
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

A `web_preview` panel skips the proxy: the UI draws its bar and embeds
`/preview?url=<url>` on the panel origin, sandboxed with `allow-scripts allow-same-origin
allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-downloads` (no top
navigation). The listener checks the URL again (loopback http(s), not the UI's or its own port)
and answers a page with no script whose iframe loads the URL with the same sandbox, under the
CSP `default-src 'none'; style-src 'unsafe-inline'; frame-src <the URL's origin>; ...;
frame-ancestors <UI origins>`, so the dev page may navigate and redirect only within its own
origin. IPv6 literals are refused, since a CSP source cannot name an IPv6 host.
`panel.preview_check` connects to the stored URL (localhost tries 127.0.0.1, then ::1);
for http it sends one `GET` and reads the response head: `refused` names an `X-Frame-Options`
or `frame-ancestors` that would block the panel's frame, `leaves_to` a redirect to another
origin; `answered: false` means no head within 3 s; https is only connected to
(`answered: null`). Muster's own HTML pages send `X-Frame-Options: DENY` and
`frame-ancestors 'none'`, so they cannot be framed by a preview or any site.

The bridge speaks MCP Apps 2026-01-26 JSON-RPC: sandbox-ready/resource-ready handshake,
initialize/initialized, tool-input/result, host-context-changed, size-changed, open-link,
message, update-model-context, request-display-mode, resource-teardown and ping. Each data
revision sends fresh tool-input/result in the existing frame; changed HTML creates a frame.
Fullscreen zooms the group; pip keeps the current mode. Unknown methods/tools are refused.
Events use `ui/message` with user text; `_meta.muster.kind` optionally names a custom action.
The host checks transient user activation synchronously, plus a visible, focused panel frame,
before accepting a message or opening an HTTP(S) link. Context updates alone do not inject.
Native IPC also requires a random per-window secret injected only into the main UI frame.

## Notes

Plain-text notes the human and every agent share (`src/notes.rs`). The daemon holds each note's
text; `rev` counts changes to the text (a rename or a move doesn't change it). Desktop client
token only: devices have no `note.*` methods and get neither `hello.notes` nor note events.

```ts
type Note = { id: number; title: string; body: string; rev: number; version: number /* raised by every change */;
              created_at: number; updated_at: number;
              updated_by: { kind: "human" | "agent"; name?: string; session_id?: number };
              window_id: number | null /* its tab's window; null: closed */; beside: number | null;
              triggers: boolean /* "Allow triggers": note triggers may fire on it (default false) */ };
type NoteSummary = Omit<Note, "body"> & { open: boolean; bytes: number; lines: number; preview: string /* first non-blank line, ≤120 chars */ };
```

`hello.notes` and `note.list` carry `NoteSummary[]`, never text, so they stay small however many
notes there are (the summary is worked out by SQLite; only the first 240 characters of a text
leave the database). A client fetches a note's text with `note.get` when it shows it, and gets
every later change with its text from `note_upserted`. `version` orders records whatever changed
(text, title or window): a client keeps the record with the higher `version`.

| Method | Params | Result |
|---|---|---|
| `note.list` | `{}` | `NoteSummary[]` |
| `note.search` | `{query}` | `{id, line}[]`: at most 20 notes whose text holds `query` (2–200 bytes, ASCII case-insensitive), newest first, with the line holding it (shortened around the match). Jump and the notes list use it. |
| `note.get` | `{id}` | `Note` |
| `note.create` | `{title?, body? = "", window_id?, from_session?}` | `Note` (rev 1). No title: `Note <id>`. With `window_id` its tab opens there. |
| `note.write` | `{id, body, base_rev, title?, from_session?}` | `Note`. Replaces the text. Refused unless `base_rev` is the current `rev`: `note N changed since revision B: it is at revision R (last edited by …)`. The same text again changes nothing. |
| `note.append` | `{id, text, from_session?}` | `Note`. Adds `text` at the end, after a newline unless the note is empty or already ends with one. Needs no revision. |
| `note.rename` | `{id, name}` | `Note` |
| `note.open` | `{id, window_id?, beside?, from_session?}` | `Note`. Shows its tab in `window_id` (moving it from another window). Without `window_id`: in `from_session`'s window, beside that session. |
| `note.close` | `{id}` | `Note` with `window_id: null`. The note stays. |
| `note.delete` | `{id, from_session?}` | `{}`. Refused when `from_session` is an agent's session (a guard against an agent's mistake, not a security boundary). |
| `note.set_triggers` | `{id, on, from_session?}` | `Note`. "Allow triggers" (see "Schedules", Phase 2): the only way `triggers` changes. Refused from an agent's session or a shell an agent drives; no MCP tool. |

`from_session` (the CLI passes `$MUSTER_SESSION_ID`) says who wrote: a shell tab, or no
`from_session`, is the human. An agent's session (Claude or Codex) makes `updated_by` that agent
(a named agent's name, else `Claude (task name)` / `Codex (task name)`), and its `note.create`,
`note.write` and `note.append` run as its MCP `note_write` / `note_append`: the same rate limit
(shared with posts and panel updates) and the same audit event. Text is plain: CRLF and CR become
LF, NUL, ESC and other control characters except tab and newline are dropped. Limits: 1 MiB of
UTF-8 per note, titles 1–200 bytes on one line, 1000 notes. `POST /api/rpc` and `POST /mcp` take
bodies up to 8 MiB, so a full note fits even when every character arrives JSON-escaped.

Every change emits `note_upserted` with the full note (text included); `note.delete` emits
`note_removed {id}`. Notes never keep a window alive and `window.claim` doesn't adopt them: when a
window is deleted (no tabs left when its client closed, or at startup) its notes close in the same
transaction (`window_id` and `beside` cleared, with `note_upserted`), so a later window that gets
the same id doesn't inherit them; startup also clears any id that names no window. Store
migration v17 adds the `notes` table. In saved layouts a note's tab is `-(2^40 + id)`, below
every panel tab.

**Concurrent edits.** Writes are optimistic: every full write names the revision it was made
from, so nothing overwrites text its writer hasn't seen. The UI keeps the text on screen and the
revision it is based on, and saves typing 0.5 s after it stops. When a newer revision arrives (an
agent's append or write) while typing is unsaved, or a save is refused because one arrived first,
the UI merges its typing onto the new text (`web/src/note_merge.ts`): each side's change is one
span (common prefix and suffix cut off, never between the halves of a surrogate pair); spans that
don't overlap are both applied (the human's first at the same point), and overlapping ones keep
the human's version followed by the other one between two marker lines (`──── <who> changed this
part at the same time; their version: ────` … `──── end of their version ────`). Then it saves
again from the new revision. An update that is exactly the UI's own last write (its echo, after
the write's answer was lost) is taken as its own, not merged. No typed text is lost on either side.

**Unsaved typing survives the page.** Until the daemon has acknowledged it, a note's unsaved
typing is also kept in the browser's localStorage under `muster.noteDraft.<window id>.<note id>`
(`{base, baseRev, text, at}`), written 0.3 s after typing and at once when the page goes away.
The next page of the same window (a reload, the window opened again) restores it, merges it onto
the note as it is then, saves it, and forgets the copy; copies older than 30 days are dropped.
Saving before a reload or Close window waits for any write on its way and then sends what was
typed meanwhile; if typing could be neither saved nor kept (storage blocked), the window asks
before it goes.

## Schedules

Recurring and one-off messages to a named agent (`src/schedules/`, docs/SCHEDULING_PLAN.md
Phase 1). Each run is an ordinary agent message with origin `schedule` at hop 0 from sender
`schedule:<id>` (the envelope reads `[muster msg <n> from schedule:<id> origin=schedule hop=0]`),
sent through the same path as `agent.ask`, so the agent's accepted origins, the hop limit, the
200-a-day cap and the unattended cap all apply, and a refusal is recorded like any other
(`agent_message`). Desktop client token only: devices get no `schedule.*` method and no schedule
events. Only the human makes, approves or changes schedules: a mutating call whose
`from_session` is an agent's session, or a shell tab with an agent's provenance (one an agent
spawned or typed into), is refused. Agents propose schedules and set one-off reminders through
their MCP tools (Phase 3, below; docs/MCP.md "Schedules").

```ts
type When = { kind: "cron"; cron: string /* 5 fields, or @hourly/@daily/@weekly/@monthly/@yearly */ }
          | { kind: "once"; at: number /* unix ms */ };
// What clients send: a When, or a one-off as a wall-clock time read in the schedule's `tz`
// (a time skipped by a daylight-saving change moves after it). Stored and returned as `at`.
type WhenInput = When | { kind: "once"; local: string /* "YYYY-MM-DDTHH:MM" */ };
type Schedule = {
  id: number; agent_id: number; agent: string | null; name: string; prompt: string;
  when: When; tz: string /* IANA name, e.g. "Europe/London" */;
  reading: string /* "At 09:00, Monday to Friday", "Once, Tue 21 Oct 2026 at 15:30" */;
  missed: "auto" | "run_once" | "skip"; missed_effective: "run_once" | "skip";
  conversation: "continue" | "fresh"; notify: "on_failure" | "always" | "never";
  note_id: number | null; note_title: string | null; quiet_hours: boolean; daily_cap: number | null;
  paused: boolean; skip_next: boolean /* the next slot will be skipped */;
  next_run: number | null /* null: a one-off that ran */; last_slot: number | null;
  status: "active" | "paused" | "expired" | "done";
  expires_at: number | null /* Claude subscription agents' recurring schedules only */; expires_soon: boolean;
  created_at: number; updated_at: number; last_run: Run | null };
type Run = {
  id: number; schedule_id: number; slot: number /* the planned time; now for Run now */;
  kind: "scheduled" | "catch_up" | "manual"; started_at: number; finished_at: number | null;
  outcome: "pending" | "replied" | "blocked" | "no_reply" | "ended" | "timed_out" | "refused" | "skipped" | "failed";
  late_reply: boolean /* replied after the wait ended (outcome reads "replied") */;
  reason: string | null /* why it was skipped, refused, blocked or failed; a catch-up's note */;
  msg_id: number | null; message: AgentMessage | null /* text and reply, as agent.messages has it */ };
type ScheduleSettings = { paused: boolean /* "Pause all schedules" */; quiet_start: string; quiet_end: string /* "HH:MM" */;
  unattended_cap: number /* per agent per 24 h, default 24 */; tz: string /* this machine's zone, read-only */ };
```

| Method | Params | Result |
|---|---|---|
| `schedule.list` | `{agent?}` | `{schedules: Schedule[], settings: ScheduleSettings}` |
| `schedule.create` | `{agent, prompt, when: WhenInput, name?, tz? = local zone, missed? = "auto", conversation? = "continue", notify? = "on_failure", note?: {id} \| {title}, quiet_hours?, daily_cap?, paused? = false, from_session?}` | `Schedule`. Refused when the agent doesn't accept the `schedule` origin (a Claude agent only does if the human ticked Schedule, with its warning). `name` defaults to the prompt's first line (60 bytes). `note: {title}` uses the note with that title (case-insensitive) or makes one. `quiet_hours` defaults on for a Claude agent on its subscription, off for Codex and OpenRouter agents. `daily_cap`: runs that reach the agent per 24 hours (1–200, `null` or 0: none). A cron line may not run more often than every 15 minutes; a one-off must be in the future and at most a year ahead. Prompt up to 16 KB; 200 schedules in all. |
| `schedule.update` | `{id, ...any create field but agent}` | `Schedule`. A `when` or `tz` that fires at other times recomputes the next run and clears Skip next; the same timing written differently (`@daily` for `0 0 * * *`) changes nothing. `note: null` and `daily_cap: null` clear them. |
| `schedule.remove` | `{id}` | `{removed: id}`; its runs go too. Removing an agent removes its schedules. |
| `schedule.pause` | `{id, on? = true}` | `Schedule`. Paused, its slots are recorded once as `skipped: paused` and pass. |
| `schedule.skip` | `{id, on? = true}` | `Schedule`. Skip the next run (`skip_next`), or undo that. |
| `schedule.run` | `{id}` | `Run` (pending, or skipped when its previous run is still going). Runs now as origin `schedule`, outside its timing, quiet hours and its daily cap; the agent's caps still apply. |
| `schedule.renew` | `{id}` | `Schedule`. Restarts the 7-day window of a Claude subscription agent's recurring schedule; an expired one runs again from its next time. |
| `schedule.runs` | `{id, before?, limit? = 20 (1..200)}` | `{runs: Run[], more}`, newest first, below run id `before`. The newest 500 runs per schedule are kept. |
| `schedule.preview` | `{when: WhenInput, tz?, quiet_hours?, agent?}` | `{reading, tz, when /* resolved */, next: number[] /* up to 3 */, quiet: boolean[] /* each falls in quiet hours (with quiet_hours, else the agent's default) */, quiet_hours_default, missed_default: "run_once" \| "skip", expires: boolean}`. Errors as `schedule.create` would give them. The dialog calls it as you type. |
| `schedule.settings.get` | `{}` | `ScheduleSettings` |
| `schedule.settings.set` | `{paused?, quiet_start?, quiet_end?, unattended_cap? (1..200)}` | `ScheduleSettings`. Emits `schedule_settings` and a `schedule_upserted` per schedule. |

Events: `schedule_upserted` (`Schedule`) when one is made or changed or its next run moves,
`schedule_removed {id, agent_id}`, `schedule_run` (`Run`) when a run is recorded or changes
(sent, finished), and `schedule_settings` (`ScheduleSettings`).

**Timing.** The day rule is Vixie cron's, as cronie has it: a day field written starting with
`*` (`*`, `*/2`) is a wildcard, and then a day must match both fields (`*/2` with `*` is odd days,
`*/1` with `mon` is Mondays); when neither is a wildcard a day matches either (`1-31` with `mon`
is every day). A cron line is read in the schedule's time zone, so 09:00 stays 09:00 across daylight
saving: a time skipped when the clocks go forward runs just after the change (02:30 becomes
03:30), and a time that happens twice when they go back runs once, the first time. The
scheduler sleeps on the monotonic clock until the next slot, at most a minute at a time, then
reads the wall clock; a gap of 2 minutes or more between the two (a suspend, the clock being set)
is logged and the slots are re-checked. Each slot is claimed in the store (its run row, unique
per schedule and slot, saved together with the schedule's next slot) before the message is sent,
so a slot never runs twice: not after a crash, a reload or the clock going back.

**What a due slot does**, in this order: a Claude subscription agent's recurring schedule more
than 7 days past its creation or renewal (at the time it would run, so a late catch-up counts) is
`skipped: expired` (one notice, a `schedule_upserted` when it expires, and a notice and desktop
notification the day before). The agent counts as on its subscription while either its record or
its running process is (a switch to OpenRouter applies from the next start); "Pause all" or the schedule's own pause skips it (recorded
once per paused stretch); a slot more than 10 minutes late (Muster was off or asleep) follows
"If missed": `auto` runs once (the newest missed slot, as a `catch_up` run) for one-offs and
schedules that run less often than hourly, and skips (`missed: … (N runs)`) for hourly-or-faster
ones; Skip next skips that one occurrence (a later catch-up still runs); a slot less than 15
minutes after the last one that ran, or after this schedule's last prompt was actually
submitted, is skipped (`too soon…`: daylight-saving changes can bunch a
cron line's times); quiet hours skip it (`quiet hours (23:00–07:00)`, checked at the slot's
time, or for a catch-up at the time it would run, in this machine's zone); the schedule's daily
cap (timed runs only: Run now doesn't count toward it, only toward the agent's caps) refuses it.
Otherwise it runs.

**A run.** Skipped when the same schedule's previous run is still going: pending, or sent and its
turn still running in the agent although the run stopped waiting for it (timed out, or blocked on
an unanswered prompt). `schedule.run` answers that at once (a `skipped` run). Otherwise it waits
in line: one schedule run at a time per agent, and until the agent is between turns with nothing
queued (an agent that isn't running is started), for up to an hour from when it was due, then
`skipped: busy` (also when the hour passed while the machine slept). Catch-ups after a start
wait 30 s, and 30 s after one another. Just before sending, an automatic run checks again what
may have changed while it waited (pauses, expiry, quiet hours, its daily cap), then every
delivery check (origin, hop, both daily caps) runs before anything is restarted, so a refused run
leaves the conversation alone. With "Conversation: fresh each run" the agent is restarted in a new
conversation only if, checked under the agent start lock together with the restart, it is
between turns and the human hasn't typed into it in the last two minutes; otherwise the run goes
back in line. The prompt's `{date}` (the slot's date in the schedule's zone), `{last_run}` (the
previous run that reached the agent, `YYYY-MM-DD HH:MM`, or "never") and `{last_reply}` (the
newest reply, up to 4000 bytes, quoted line by line under a label saying it is earlier output and
data, not instructions) are filled in. A prompt can still wait in the agent's prompt queue (an
unsent draft in its input box holds it): right before it is typed and again right before Enter,
a schedule's prompt is checked once more (its run still pending, its schedule there, and for a
timed run the same rules as above); one that may no longer go is taken back, never typed, and
the run ends `skipped: not sent: …` (its message is forgotten and counts toward no cap). Pausing a
schedule (or all), removing it or its expiry also take back its prompts still waiting, and so does
the end of the hour for the reply. Every message muster delivers to an agent is plain text
(control characters dropped), so stored text can't end a bracketed paste or press keys. The run
then waits for the reply until an hour after the message was sent (across restarts) and ends `replied`, `blocked` (an approval prompt, a question
or an error: never approved for you), `no_reply`, `ended`, `timed_out`, `refused` (with the
agent's reason) or `failed`. A reply is also appended to the schedule's note, if it has one,
under a `## Tue 21 Oct 2026 09:00 · <name>` heading, written as the agent, exactly once per run, also
when the reply comes after the run stopped waiting for it. "Notify me": an in-app
notice and a desktop notification on failure (blocked, no reply, ended, timed out, refused,
failed), on every outcome (`always`), or never. A backend restart picks up unfinished runs: one
already sent waits for its reply again; one whose message was recorded but never typed and is in
no prompt queue (the backend stopped in between) is `failed`; one still in line goes back in line
until its hour is up. The daily caps count a run by when its message was sent. The newest 500 runs
per schedule are kept, whichever way they were made.

**Caps.** Besides the 200-a-day cap, each agent takes at most `unattended_cap` (default 24)
messages a day whose origin isn't `human` (scheduled or triggered, at any hop): over it, a
message is refused `agent X already got N unattended … messages in the last 24 hours`.

Debug builds only: `MUSTER_TEST_CLOCK_OFFSET_MS` shifts the scheduler's clock and
`MUSTER_TEST_CATCHUP_GAP_MS` replaces the 30 s catch-up spacing (the smoke test uses both).
Store migration v19 adds `schedules` and `schedule_runs`.

### Triggers, "Then…" steps and the spend cap (Phase 2)

Code: `src/schedules/triggers.rs`, `src/schedules/run.rs` (steps), `src/spend.rs`.

```ts
// More kinds of `When` (and of `WhenInput`, which may name a note by title: {kind:"note", note:"Ideas"};
// a title several notes share is refused: give note_id, as the dialog does).
type TriggerWhen =
  | { kind: "note"; note_id: number; quiet_min: number /* 1..120, default 2 */ }
  | { kind: "git"; repo: string /* absolute, resolved */; branch: string }
  | { kind: "github"; repo: string /* OWNER/NAME */; event: "pr_opened" | "ci_failed"; branch: string | null; every_min: number /* 2..60, default 5 */ }
  | { kind: "url"; name: string /* a-z 0-9 - _, 1-40, unique */ };
type Step = { notify: boolean /* failure only */; agent: string | null; agent_id: number | null; prompt: string; then: Then | null };
type Then = { on_success: Step | null; on_failure: Step | null };
// Added to Schedule:
//   trigger: boolean; origin: "schedule" | "webhook"; then: Then | null;
//   pending: { at, text, count, payload: boolean } | null   // an event waiting for its run (at next_run)
//   watch: { state: "ok" | "waiting" | "gh_not_signed_in" | "gh_missing" | "error"; detail; checked_at; next_at } | null  // git, github
//   note: { id, title, triggers: boolean } | null          // a note trigger's note
//   url: string | null                                     // a URL trigger's address (never its secret)
// Added to Run: kind "triggered" | "chain"; parent_run, step ("s"/"f" per level), agent (a step's), event ({at,text,count,payload}).
// Added to ScheduleSettings: spend: { key_present, today: number | null, cap, other, agents: {id,name,spent,cap|null}[], checked_at, error, resets_at }.
```

| Method | Params | Result |
|---|---|---|
| `schedule.create` / `.update` | also `when: TriggerWhen`, `then: {on_success?: {agent, prompt}, on_failure?: {notify: true} \| {agent, prompt}} \| null` (steps may nest `then`, at most 4 levels) | `Schedule`. A trigger needs an agent that accepts `webhook` (a step's agent the schedule's origin); `daily_cap` defaults to 10 for a trigger. A URL trigger gets a secret. |
| `schedule.preview` | also `id` (the schedule being edited) and `then_agents` (the steps' agents) | For a trigger: `{reading, trigger: true, next: [], note_triggers_off, daily_cap_default, expires, …}` |
| `schedule.settings.set` | also `spend_cap` (dollars, 0.01..1000; it can't be cleared) and `agent_spend_caps: {name: dollars \| null}` | `ScheduleSettings`. `schedule.settings.get` also reads the key again when the last reading is over a minute old (a `schedule_settings` event follows). |
| `trigger.fire` | `{name, payload?}` | `{queued, trigger, runs_at}`. `muster trigger`: the human's client, no secret. |
| `trigger.secret` | `{id} \| {name}, rotate?` | `{name, url, secret, curl}`. `rotate`: a new secret; the old one stops working. |
| `note.set_triggers` | `{id, on}` | `Note`. "Allow triggers" on a note. |

`trigger.*`, `note.set_triggers` and every schedule change are the human's: refused from an
agent's session or a shell with an agent's provenance, like the schedule calls (a guard against
an agent's mistake, not a security boundary: the client token can do anything). Devices get none.

**Trigger URL.** `POST /api/trigger/<name>` with `Authorization: Bearer <secret>`, on the
daemon's loopback listener only (a request through `tailscale serve` is refused); the body (at
most 16 KiB, else 413) is the payload. No secret: 401; a wrong secret or an unknown name: 403
(the same answer for both). Answers `{queued: true, runs_at}`.

**How a trigger runs.** An event never sends anything by itself: it is held on its schedule
(`pending`, merged with one already waiting: the lines join, `count` rises, the newest payload
wins) with a time to run (`next_run`), and the scheduler handles it like a due slot, as a run of
kind `triggered`, origin `webhook`, sender `trigger:<id>`, at hop 0. So pause, Pause all, the
7-day expiry (a trigger with a Claude subscription agent anywhere in it), quiet hours (checked
when it runs), the daily caps, the busy rule, alerts, the reply note and cold-start recording all
apply as for a timed run; a paused or expired trigger drops its events (recorded once). Never
sooner than 15 minutes after its last run (its slot, and when its prompt was really submitted):
an event in that time stays held until then, never spent on a run the floor would skip. An event
while its previous run is still going waits a minute at a time. The prompt's `{event}` is what
happened and `{payload}` the data sent, each quoted line by line under a label saying it comes
from outside Muster and is data, not instructions (cut to 8 KiB and 16 KiB); a prompt that uses
neither gets them added at its end. Placeholders are filled in one pass, so nothing put in is read
for placeholders again. Run now on a trigger runs it without an event. Skip next doesn't apply.

- **Note**: on each change of the note's text (`note.write`, `note.append`, MCP or CLI), only
  while the note's `triggers` ("Allow triggers") is on (off for every note until the human
  switches it on; no write of the text can change it, and there is no MCP tool for it). Changes
  that never count: by the trigger's own agent, by any session whose provenance is a scheduled or
  triggered chain (every run a trigger started is one), and Muster's own append of a run's reply.
  Debounced: the run is `quiet_min` minutes after the last counting edit. At most 10 runs a day
  across the triggers on one note (`refused: daily cap …`), counted from runs claimed (going or
  sent), under the lock every claim takes, so triggers due together can't pass it. Switching
  "Allow triggers" off drops any event waiting on it and takes back its prompts still waiting in
  an agent's queue; the switch is checked again when a run is claimed, before it is sent and right
  before its prompt is typed and submitted (off: `skipped`).
- **Git**: the branch's commit is read from the repository's ref files (`refs/heads/<branch>`,
  else `packed-refs`; git itself only for a reftable repository) every 15 s while the trigger is
  active; no hook is installed. The first look records it; a change holds an event with the
  subjects of up to 10 new commits (`git log old..new`), or says it was rewound. The commit last
  seen is kept with the schedule, so a move while Muster was off fires once after it starts.
- **GitHub**: `gh api repos/<repo>/pulls?state=all&sort=created…` (PR opened: every state, so a
  PR opened and closed or merged between polls, or while Muster was off, still counts; the newest
  50) or `…/actions/runs?status=failure[&branch=…]` (CI failed) through the safe runner of
  src/pr.rs, with the owner's own `gh` login (Muster never sees or stores a token), every
  `every_min` minutes while active. The first poll records what is there. Then a PR number above
  the highest seen is new, and a failed run (id and attempt, the latest 200 remembered) not seen
  before is new, whatever its id: runs are listed by when they started, so one can fail after a
  newer one did, and a re-run keeps its id
  ([workflow runs](https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository)).
  An event names number, author, title and URL, or workflow, branch, conclusion and URL. A
  failure backs off (the interval doubling, at most an hour) and shows in `watch`:
  `gh_not_signed_in` (run `gh auth login`), `gh_missing`, or `error` with the detail.

A git or GitHub watcher saves what it has seen and the event it found in one write, so a crash
can't keep the one and lose the other; an answer for a trigger changed while it was being looked at
(another repo or branch: `watch.generation` rises with each change) is thrown away. Every new PR or
failed run is reported: the first 10 in full, the rest by number on one `+N more …` line, and a
whole page of new ones says there may be more than one check lists.

The daily caps count delivered runs, and history pruning (the newest 500 runs per schedule) never
removes a run that reached its agent in the last two days, so skips and refusals can't push one out
and reopen a cap.
- **URL**: `trigger.fire` or the trigger URL hold an event at once.

What it costs while idle: a note or URL trigger nothing at all; a git trigger a stat and a small
read of one or two files every 15 s; a GitHub trigger one `gh api` call (one GitHub API request)
per interval. A paused or expired trigger costs nothing.

**"Then…" steps.** When a run (or a step) ends `replied`, its `on_success` step runs; when it
fails (blocked, no reply, ended, timed out, refused, failed), its `on_failure` step; a skip starts
nothing. A notify step is a notice and a desktop notification. An ask step is a run of kind
`chain` (`parent_run`, `step`, its own `agent`), sent to that agent as a message one hop past the
run it follows (`reply_to` that run's message; sender `<schedule's sender>/then:<run id>`), so
the 4-hop limit bounds a chain and every delivery rule applies to it; the schedule's own daily cap
and 15-minute floor don't count steps. Its prompt may use `{reply}` (the previous reply, quoted as
earlier output), `{outcome}` (`replied`, `blocked`, …, followed by the reason quoted as data: it
can come from the agent's screen) and `{event}`. A step's reply doesn't go to the schedule's note,
and the row's `last_run` is the schedule's own run. Steps don't hold back the schedule's next run.
A run's end and the step it starts are saved in one transaction (a restart in between can't lose
the step; a run's step is recorded once), including a run refused as it is claimed (a daily cap),
which takes its failure branch like any failure. A step's agent and prompt are read together from
the schedule as it is when the step runs; a step changed or removed meanwhile fails with that
reason. `schedule.preview` takes `then_agents` (names) so a step's Claude subscription agent shows
its rules (quiet hours by default, the 7-day expiry) before saving.

**Spend cap.** For agents on OpenRouter (their record, or their running process), an overall
daily cap (`spend_cap`, default $5, always on) and optional per-agent caps. At a cap, every
message to such an agent whose origin isn't the human's (scheduled, triggered, or an agent's ask on
such a chain) is refused `budget: …` (checked in `agents::precheck` before anything is started,
again in `deliver`, and by the injector right before such a prompt is typed and again before Enter,
so one queued before the cap was reached is taken back: a schedule's run ends `refused`, any other
message is recorded refused; before typing, a stale or missing reading is first renewed, so a
prompt waiting from before a reload is judged on a real reading, never refused only because none
was taken yet), and for raw keys (`session.input`, `muster keys`) sent on such a chain to an
OpenRouter session, with one alert per cap per day; a run already going finishes, and
what the human types is never blocked. Spent is read from OpenRouter's `GET /api/v1/key` with the
daemon's own key (passed to curl as a config on its stdin, never in an argument, the environment,
a log or a reply): `usage_daily` (OpenRouter's UTC day) for the overall cap; per agent (an
estimate), each rise in `usage` between two readings goes to the OpenRouter agents that were in a
turn in between (from every session state change and every prompt submitted, so no turn is too
short to see; split evenly when several were), or to `other` when none was. A rise across 00:00 UTC
counts toward the new day only up to its `usage_daily`. The key is read while an OpenRouter agent
works or has prompts waiting (at most every minute, and once when the last turn ends), before an
unattended message when the last reading is over a minute old or from another UTC day, and when
the settings are read; never while they are idle. With no reading of today in the last 15 minutes
(OpenRouter unreachable, or an answer without a usable `usage_daily`, which fails the reading),
or no key in this backend at all (an OpenRouter agent from an earlier backend can still be running
with its own), unattended work on OpenRouter is refused too. Each OpenRouter run's launch record
keeps a one-way fingerprint of the key it was launched with (a salted SHA-256, 64 bits; never the
key); a running agent whose fingerprint isn't that of the key this backend reads (a reload with
another key) is refused unattended work until it restarts ("Start fresh"). Kept in `runtime_state`
`openrouter-spend`. Why this source: it is what OpenRouter bills (cache discounts, reasoning
tokens and the provider it routed to included), for every request through the key; the CLIs don't
report OpenRouter's price, and the per-generation stats endpoint
(`GET /api/v1/generation?id=…`, `total_cost` in USD) needs generation ids that Claude Code and
Codex don't show. Sources (checked 2026-10-21):
[limits and the key endpoint](https://openrouter.ai/docs/api/reference/limits),
[generation stats](https://openrouter.ai/docs/api/api-reference/generations/get-generation).
guess: one credit is one dollar, as OpenRouter's dashboard shows it.

Store migration v21 adds `schedule_runs.parent_run`, `step`, `agent_id` and `event`, keeps
timed slots (`scheduled`, `catch_up`) unique per schedule, and adds `notes.triggers`. Debug builds
only: `MUSTER_TEST_TRIGGER_DELAY_MS` replaces a note's quiet minutes, `MUSTER_TEST_WATCH_EVERY_MS`
the git and GitHub intervals, `MUSTER_TEST_GH` the `gh` program and
`MUSTER_TEST_OPENROUTER_KEY_URL` the key endpoint (http allowed there).

### Agents' proposals and reminders, the file trigger (Phase 3)

Code: `src/schedules/agent.rs` (the agent tools and the estimate), `src/schedules/triggers.rs`
(files). The agent side is MCP (docs/MCP.md "Schedules"); this is the client side.

```ts
// Added to Schedule:
//   made_by: { agent_id, agent, kind: "proposal" | "reminder", reason, at, approved_at: number | null } | null
//            // null: the human made it. A proposal with approved_at null waits for approval.
//   estimate: { runs_per_week: number | null, max_per_day: number | null,
//               billing: "claude_subscription" | "openrouter" | "codex" | "other",
//               recent_unattended_7d: number, lines: string[], text: string } | null   // agent-made only
//   status: also "proposed" (waiting for approval: it has no next_run and never runs)
// Another kind of When / WhenInput (origin webhook, like every trigger):
type FilesWhen = { kind: "files"; folder: string /* absolute, links resolved */; pattern: string | null /* "*.pdf": * and ? on file names */;
                   recursive: boolean /* up to 3 levels down */; quiet_min: number /* 1..120, default 2 */ };
// WhenInput also takes folder as "~/…"; pattern "" or "*" is none.
// Added to ScheduleSettings: watch_folders: string[]  // folders file triggers may watch; [] (the default): none
```

| Method | Params | Result |
|---|---|---|
| `schedule.approve` | `{id}` | `Schedule`. The human approves an agent's proposal: checked again as if made now (its agent, and every step's, still takes the work; a one-off's time hasn't passed: otherwise refused, to edit or dismiss), then it runs like any schedule, and a Claude subscription agent's 7 days start now. Refused from an agent's session or an agent-driven shell (its own proposal included). Dismissing is `schedule.remove`. |
| `schedule.propose` | `{from_session, prompt, reason, cron? \| at? \| in_minutes?, agent?, name?, tz?, conversation?, quiet_hours?, daily_cap?, note_id?}` | MCP `propose_schedule`'s result. Only from an agent's own session (`muster schedule propose`): it runs that tool as the agent (its rules, rate limit and audit). Anyone else is refused. |
| `schedule.remind` | `{from_session, prompt, in_minutes? \| at?, name?, tz?}` | MCP `set_reminder`'s result, likewise. |
| `schedule.pause` / `schedule.remove` | from an agent's session | Run as its MCP `pause_schedule` / `delete_schedule` (pausing only: `on: false` stays the human's). |
| `schedule.settings.set` | also `watch_folders: string[]` (the whole list; existing folders, `~/` allowed, made absolute with links resolved; not `/`; at most 20) | `ScheduleSettings`. Removing a folder stops the file triggers inside it: their waiting events go, their prompts still in an agent's queue are taken back, and a run due is `skipped`. |

A proposal is stored like any schedule with `made_by` and no `next_run`; every write to it keeps
`next_run` empty while it waits (the human may edit it before approving), and the scheduler,
Run now and Skip next refuse it. A proposal nobody approves is removed after 7 days, and a reminder
one day after it ran. Every schedule an agent made carries `estimate`: its runs in the coming 7
days after quiet hours and the caps (its own daily cap and the agent's unattended cap), what pays
(the agent's provider), and the caps that bound it, with the agent's unattended messages of the
last 7 days. Making a proposal sends a notice (and a desktop notification) that it waits.

**Files.** Only inside a folder in `watch_folders` (checked when the trigger is made, at every
look, when a run is claimed, before it is sent and before its prompt is submitted). The folder is
listed every 15 s while the trigger is active (not paused, not expired): at most 5000 entries per
look (more: `watch.state` `error`, nothing read), `recursive` up to 3 levels, hidden names
(`.…`), `node_modules`, `target`, `__pycache__` and partial files (`~`, `.swp`, `.tmp`, `.part`,
`.crdownload`, `.download`, `.partial`) skipped, symbolic links never followed. A file whose
change time (ctime: set by the system on every write, rename or move into the folder) is newer
than the newest seen is new or changed; the newest is kept with the schedule
(`watch.stamp`), so a change while Muster was off fires once after it starts, and the first look
only records it. Removing a file starts nothing. An event names the folder and up to 10 changed
files (`New or changed in ~/inbox: a.pdf, b.pdf (+3 more)`), quoted to the agent as data, and is
debounced like a note's (`quiet_min` after the last change). A change seen while the trigger's
own agent is working, or while any session handling scheduled or triggered work is, and for 30 s
after, is taken as an agent's own: recorded as seen, never an event (so a file trigger can't feed
itself or another). guess: ctime is the system's clock, so a clock set back can hide changes
until it catches up. What it costs while idle: one directory listing (and a stat per entry) per
15 s. Debug builds: `MUSTER_TEST_WATCH_EVERY_MS` and `MUSTER_TEST_TRIGGER_DELAY_MS` apply too.

No store migration: schedule records and the settings are JSON (an older backend ignores a
`files` schedule it can't read, and keeps a proposal waiting).

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

## Cold prompt cache

Claude and Codex cache the conversation they send their model; a request after the cache has
expired re-reads the whole conversation at full cost. Code: `src/cache.rs`.

`SessionInfo.last_turn_at` (unix ms or null) is the last model request the daemon saw succeed
for a Claude or Codex session. Only real model activity moves it, and never backwards:

- Hooks, when the CLI sends them: the model answering (`PreToolUse`, `PermissionRequest`), a
  turn's `Stop`, Codex's `notify` `agent-turn-complete` (not its title-generation turns). A
  subagent's hooks (`agent_id`) don't count (its own cache), nor does `StopFailure` (a rate limit
  or auth error may never have reached the model), `UserPromptSubmit` or `PostToolUse`. A callback
  replayed from the hook inbox after an outage counts from when the helper captured it (the inbox's
  `captured_at`; older helpers' rows count from when they are read).
- The screen, only for a session that has sent no hook this run: entering or leaving `working`.
  Never a timer's `stuck` (or its way out), a process exit, or leaving the `working` that backend
  recovery sets to re-read a saved dialog.

It is null before the conversation's first turn and for shells. Claude's `SessionStart` with
`source: "clear"` (`/clear`) clears it, and so does any new run that doesn't continue its
conversation: only Claude's `--resume`/`--continue`, in any spelling the argument check accepts
(`-r<id>`, `-r <id>`, `--resume=<id>`, `-c`), and a Codex run that reopens its saved thread
(`codex resume … -- <id>`), keep it. Stored with the session (migration v20),
so it survives a backend reload and restart. A session saved before v20 has none; at the next
backend start, one that is resting (not `working`, `starting` or `exited`) and has had a turn (its
CLI has set a task title, which Claude Code and Codex do after the first prompt) gets its
`state_since` as an estimate. When a named agent's session is removed (its task
closed), its conversation's clock is kept with the agent (`runtime_state` `agent-turn:<agent>`,
with its resume id) and given back to the session that resumes that same conversation.

`SessionInfo.cache_ttl_min` is the lifetime that applies to the run, and
`SessionInfo.cache_cold_at` = `last_turn_at` + it; both null for shells, before the first turn
(`cache_cold_at` only), or with `cache_warn` off. Claude's lifetime also follows its launch
environment (read at each start, and from the saved launch after a backend restart; credentials
are only tested for being set, never read or stored):

- `DISABLE_PROMPT_CACHING`: no cache, so no warning; else `FORCE_PROMPT_CACHING_5M`: 5 min; else
  `CLAUDE_CODE_PROMPT_CACHE_TTL` (`5m`, `1h`); else `ENABLE_PROMPT_CACHING_1H`: 60 min.
- Otherwise a Claude on its subscription launched with `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY` bills the API and
  gets `cache_ttl.claude_openrouter` (the API's 5 min), like OpenRouter. guess: a key set only in
  Claude Code's own settings (`apiKeyHelper`, `settings.json` `env`) is not seen and counts as the
  subscription.

Derived, not stored: every session follows a `prefs.set` at once (`session_upserted` for each
that changed). A session is **cold** while it waits (any state but `working`, `starting` and
`exited`) and now >= `cache_cold_at`.

Why this clock: both providers refresh the cache on each request that reads it, and Claude Code
itself treats the cache as warm "for one cache TTL after Claude Code last sent a request in this
conversation or Claude last responded" (https://code.claude.com/docs/en/prompt-caching). The
end of the last turn is the newest request the daemon can observe.

Default lifetimes (`prefs.cache_ttl`, minutes; researched 2026-10):

| Key | Default | Source |
|---|---|---|
| `claude` (Claude Code, subscription) | 60 | Claude Code asks for a 1-hour cache for the main conversation while within the plan's usage (5 min on usage credits); `promptCacheTtl` / `CLAUDE_CODE_PROMPT_CACHE_TTL` change it. code.claude.com/docs/en/prompt-caching |
| `claude_openrouter` (OpenRouter, or an API key) | 5 | Anthropic's API default is 5 min; 1 h only when Claude Code is set to ask for it (`ENABLE_PROMPT_CACHING_1H`, `promptCacheTtl: "1h"`). platform.claude.com/docs/en/build-with-claude/prompt-caching, openrouter.ai/docs/features/prompt-caching |
| `codex` (ChatGPT plan) | 30 | guess: not documented for ChatGPT plans; OpenAI's API default for GPT-5.6 and later is 30 min (a 12 h option exists but Codex doesn't ask for it). developers.openai.com/api/docs/guides/prompt-caching |
| `codex_openrouter` | 10 | guess: OpenRouter's sticky routing ends after 10 idle minutes, after which a request may reach another provider's (cold) cache. openrouter.ai/docs/features/prompt-caching |

Settings: `prefs.get`/`prefs.set` (see "Updates and window preferences") carry `cache_warn`
(bool, default true) and `cache_ttl` (`{claude, claude_openrouter, codex, codex_openrouter}`,
each 1 to 1440 minutes). `prefs.set {cache_ttl: {codex: 720}}` changes only the fields given;
an out-of-range value or unknown key refuses the whole call.

**Cold sends.** When the injector submits a prompt Muster delivers (an agent message, a
schedule run's message, panel events; not the human's own `session.send`) to a cold session, it
is still delivered, and recorded just before Enter is pressed (so a crash right after can't leave
it unmarked): the agent message gets `cold_idle_ms` (how long the agent had been idle; also on
the `agent_message` event and in a schedule run's `message`), and the channel's timeline a
`system` event `{text, level: "info", cold_start: {session_id, idle_ms, msg_id, panel}}`. A retry
of the same delivery (its message, else its panel batch, else its text) is recorded once
(`runtime_state` `cold-send:<session>`).

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
- **Hidden agent tabs.** `SessionInfo.hidden: boolean` (store migration v16, default false):
  a named agent's tab the human closed (`session.hide`; the UI's tab close does this for agent
  tabs). The session runs on with `window_id: null`, receives messages and keystrokes as
  before, and is never adopted by `window.claim`. When none of its channel's sessions is left in
  a window (all hidden), the channel's `window_id` becomes null too (`channel_upserted`, then
  the old window's update or `window_removed`), and `window.claim` doesn't adopt that channel
  either. `session.set_window` (and `channel.move`) clears `hidden`; the channel then follows its
  tab as above. Kept across backend reloads and restarts. Every `session_upserted` is published
  under the lock that made the change, so events reach clients in the order the changes were
  made (a hook's state change can't re-show a tab hidden just before). `muster ls --json` and
  `muster move` keep a hidden session's null window. `session.kill` stops a hidden agent
  like any session (its tab stays hidden, now exited); `session.remove` and `agent.remove` remove
  it.
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
there; the other Claude profiles pass `--permission-mode`.
New and restarted Codex sessions also receive `-c approval_policy="on-request"` and
`-c approvals_reviewer="auto_review"`: the automatic approval-review behavior of
`--approve-for-me`, while preserving the channel's sandbox and network settings. The shorthand
itself conflicts with `--sandbox`, so Muster passes the equivalent settings separately.
These are per-process overrides; the user's Codex config is not changed, and already-running
sessions keep their current approval settings. Because Codex fires its `PermissionRequest` hook
before the automatic reviewer decides, a Codex session reports `needs_approval` only when the
approval dialog is on its screen, never from that hook. See the
[Codex CLI reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli) and
[sandbox configuration](https://learn.chatgpt.com/docs/sandboxing).
The user's `extra_args` for a session (`session.spawn`, `channel.create` for its first agent,
MCP `spawn_session` `model`/`effort`/`args`) come after these flags, and since both CLIs let a
later flag win they are checked against an **allowlist** (also again on every restart):
Claude `--model`, `--effort`, `--fallback-model`, `--append-system-prompt`, `-c/--continue`,
`-r/--resume [id]`, `--verbose`, `-d/--debug [filter]`; Codex `-m/--model`, `-c/--config`
with the keys `model`, `model_reasoning_effort`, `model_reasoning_summary`, `model_verbosity`,
`hide_agent_reasoning`, `show_raw_agent_reasoning`, `-i/--image`, `--no-alt-screen`. (A Codex
run that reopens its conversation launches as `codex resume <the same flags> -- <thread id>`;
the thread is kept outside `extra_args`, so the stored arguments stay valid for older
backends: `runtime_state` `codex-thread:<session>` for a tab, the agent's record for an
agent.) Values
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
--help` (0.164.0), the Codex config reference and "agent approvals & security" docs
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

## OpenRouter

Named agents can run on [OpenRouter](https://openrouter.ai) instead of their CLI's own login
(`Agent.provider`, store migration v18; existing agents stay on `subscription`). OpenRouter
bills per token to the owner's key, so the UI badges these agents and their tabs "OpenRouter ·
pay per use". Code: `src/openrouter.rs`.

**The key.** `OPENROUTER_API_KEY` in the backend's environment (at least 16 characters, no
spaces), read once when it starts. Then, before any thread starts, it leaves the backend's own
environment together with every other variable holding it, so git, gh, the terminal host and
sessions inherit none of it. It is held in memory only: never in the store, a log, `muster ls`,
an MCP or RPC reply (clients get `hello.openrouter.key_present`), the persona files, or the
launch records. Every run's saved launch, whatever its provider, has the variables Muster set to
the key (`ANTHROPIC_AUTH_TOKEN` or `OPENROUTER_API_KEY`) stored empty and listed beside the launch
(`key_refs`), and the key cut out of every other value and argument (a prompt that happened to
contain it), for good. Re-sending a launch fills in only the listed variables, and only in a
launch made for OpenRouter (`MUSTER_PROVIDER=openrouter`), so no text a user or an agent wrote can
ask for the key. It stays readable in the backend's own `/proc/<pid>/environ` (only by you), which keeps
the environment the process started with.

**Setting or changing it.** Restart the backend with it, from a shell that has it:

```sh
env OPENROUTER_API_KEY=sk-or-… muster reload        # terminals keep running
# with Bitwarden (fish): the field name is the example from your own note
env OPENROUTER_API_KEY=(bwx get bw://Projects/muster/local/OPENROUTER_API_KEY) muster reload
env OPENROUTER_API_KEY= muster reload               # clears it
```

`muster reload` normally starts the new backend with the environment of the process that runs
it. With `OPENROUTER_API_KEY` set there (even empty), that is what happens: the shell's
environment, and its key or none. Without the variable, while the backend has a key (an agent
applying a build from its session, where `OPENROUTER_API_KEY` never is), the command asks the
backend to restart itself (`daemon.reload_handoff`): the backend runs `<its launch path> reload`
with its own environment plus its key, as the window's "Restart backend" banner does, so the key
carries over without passing through argv, a file or a reply. The backend does this only when
the command is the binary it was started from (compared as canonical paths); otherwise the
command reloads from its own environment and prints that the key is dropped. A command that can't
see the backend's process (Claude Code's Bash in an OpenRouter session runs in its own PID
namespace) always has the backend restart itself, since it couldn't stop it; there an
`OPENROUTER_API_KEY` set in the command is refused. Such a command finds the backend by its
discovery file and token: the backend's `hello` must report the pid the file names. `muster stop` then
`env OPENROUTER_API_KEY=… muster` works too, but ends every session.

**Inherited settings.** Every session's inherited environment loses `OPENROUTER_API_KEY` and
every variable whose value contains the key. OpenRouter's own Claude Code setup has the owner
export `ANTHROPIC_AUTH_TOKEN=<key>`, `ANTHROPIC_BASE_URL=https://openrouter.ai/api` and an empty
`ANTHROPIC_API_KEY`; when the inherited `ANTHROPIC_AUTH_TOKEN` is the backend's key, the
OpenRouter `ANTHROPIC_BASE_URL` and the empty `ANTHROPIC_API_KEY` go with it, so no subscription
session or shell bills OpenRouter. A gateway or token of the owner's that isn't this key stays
(as before). A backend started from an OpenRouter session (marked `MUSTER_PROVIDER=openrouter`)
drops all the variables below from its environment.

**Where it goes.** Only into the sessions of agents set to OpenRouter, at each start:

| CLI | Environment | Arguments |
|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=https://openrouter.ai/api`, `ANTHROPIC_AUTH_TOKEN=<key>`, `ANTHROPIC_API_KEY=` (empty), `ANTHROPIC_DEFAULT_{OPUS,SONNET,FABLE}_MODEL=~anthropic/claude-{opus,sonnet,fable}-latest[1m]`, `ANTHROPIC_DEFAULT_HAIKU_MODEL=~anthropic/claude-haiku-latest`, `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`, `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`; every inherited `CLAUDE_CODE_USE_*` provider selector (Bedrock, Vertex, Foundry, Mantle, Claude Platform on AWS, and later ones; not `CLAUDE_CODE_USE_POWERSHELL_TOOL`), `ANTHROPIC_MODEL` and `ANTHROPIC_SMALL_FAST_MODEL` are removed | `--model <OpenRouter id>` when the agent has one (else Claude Code's default, through the aliases above) |
| Codex | `OPENROUTER_API_KEY=<key>` | `-c model_provider="muster_openrouter" -c model_providers.muster_openrouter.name="OpenRouter (Muster)" -c model_providers.muster_openrouter.base_url="https://openrouter.ai/api/v1" -c model_providers.muster_openrouter.env_key="OPENROUTER_API_KEY" -c model_providers.muster_openrouter.wire_api="responses" -c shell_environment_policy.ignore_default_excludes=false` before Muster's own `-c` overrides (never stored with the session), then `-m <OpenRouter id>` (required: Codex's own model names lack the `openai/` prefix) |

Both also get `MUSTER_PROVIDER=openrouter`. Every other session (subscription agents, tabs,
shells, helpers an OpenRouter agent spawns) starts without any of it. The provider is read from
the agent's record at each start and saved on the session (`SessionInfo.provider`, what the
current or last run bills to); changing it applies from the next start, and `agent.start` on a
running agent whose run has the other provider answers `provider_pending: true`.

- `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST`: Claude Code then ignores provider, endpoint and
  authentication variables (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`,
  `CLAUDE_CODE_USE_*`) in settings files, so a profile's, project's or user's `env` block can't
  send the key to another endpoint. It also makes Claude Code skip the telemetry opt-out it
  otherwise applies on third-party providers (telemetry then follows `DISABLE_TELEMETRY`).
- `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`: the Bash commands, hooks and stdio MCP servers Claude Code
  starts get no credentials (by name or value: the key, `MUSTER_TOKEN`, `CLAUDE_CONFIG_DIR`), so
  a routine `env` in Bash can't print the key into the scrollback Muster keeps; on Linux Bash also
  runs in its own PID namespace (it can't read the backend's `/proc/<pid>/environ`; `ps` and
  `kill` don't see processes outside it). Muster's hooks, which lose `MUSTER_TOKEN` this way, read
  the run's session token from `<data>/sessions/<id>/hook-token` (private, rewritten at each
  start). Muster's MCP server is HTTP with the token in the per-session config file, untouched.
- What remains: the CLI process itself holds the key (it has to). In a Codex session, Codex keeps
  `*KEY*`/`*SECRET*`/`*TOKEN*` variables out of the commands it runs (forced on above); its hooks,
  `notify` command and stdio MCP servers may still inherit `OPENROUTER_API_KEY` (guess: Codex has
  no scrub for those). The scrub matches credentials by name and value shape; a copy of the key
  under an innocent name and an innocent-looking value would get through (Muster never makes one).

Sources (checked 2026-10-20): OpenRouter's [Claude Code guide](https://openrouter.ai/docs/guides/guides/claude-code-integration)
(the variables, "`ANTHROPIC_API_KEY` must be explicitly empty", the alias values), Claude Code's
[environment variables](https://code.claude.com/docs/en/env-vars) (the provider selectors,
`CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST`, `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` and what the scrub
removes) and [authentication](https://code.claude.com/docs/en/authentication)
(`ANTHROPIC_AUTH_TOKEN` ranks above a subscription login), OpenRouter's
[Codex guide](https://openrouter.ai/docs/cookbook/coding-agents/codex-cli.md) and Codex's
[config reference](https://developers.openai.com/codex/config-reference) (`responses` is the only
wire API; `auth` can't sit next to `env_key`, hence Muster's own provider id). OpenRouter says
Claude Code is only guaranteed with Anthropic's own models, and Codex works best with OpenAI's;
the dialog says so. If Claude Code was signed in to a subscription in that configuration folder,
it may warn about an auth conflict on start (OpenRouter suggests `/logout` once); the auth token
still wins.

**Profiles.** A Claude profile still selects `CLAUDE_CONFIG_DIR` (settings, history); its check
for inherited auth or endpoint overrides runs on the inherited environment, before the OpenRouter
variables are added, so the two combine.

**Starting without the key.** An OpenRouter agent's start, restart or autostart fails with an
error naming `OPENROUTER_API_KEY` (before a running process is touched); `agent.create` and a
switch to `openrouter` are refused the same way.

**Recovery.** A launch prepared before a backend crash is completed by the next backend. If the
terminal host already started it, the next backend adopts that process as it runs and never
re-sends the launch (with the key since changed or gone, the host would refuse a different
launch, and a refused run is retired). One the host never started, prepared for OpenRouter
(`key_refs` not empty), waits while the backend has no key (nothing runs; starting that session
says it waits for the key) and is sent with the key by a backend that has one.

**The model list** (`openrouter.models`). The backend fetches `https://openrouter.ai/api/v1/models`
(public, no key; curl, https only, 20 s, at most 16 MB read as it arrives, the process killed past
that) and keeps the result in memory and in `<data>/openrouter-models.json`, refreshed at most
hourly (in the background once a list exists; after a failure, no new try for 5 minutes) and
served from the file when offline. With no list yet, every caller waits for the one fetch in
flight. Kept: models with `tools` in `supported_parameters` and text in and out; dropped: `:batch`
copies and `openrouter/*` routers. Sorted by vendor, newest first. `error` says why the last fetch
failed; with no list at all the dialog offers Custom… only (and asks again next time).
`stale`: the copy is over an hour old.

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
| `daemon.reload_handoff` | `{exe}` | `{exe}`. The backend restarts itself: it runs `<its launch path> reload` detached with its own environment plus its OpenRouter key (if any), as `daemon.reload` does. `muster reload` asks for it when run without `OPENROUTER_API_KEY` while the backend has a key (so the key carries over), and always from a command that can't see the backend's process (a sandboxed command in its own PID namespace, which couldn't signal it). Refused (an error starting `handoff refused:`; the CLI then reloads the usual way where it can) without a known launch path, or when `exe` (the calling binary) isn't the launch path (canonical paths). Human only. |
| `daemon.reload` | `{expect}` | `{exe}`. `expect`: the `updates.backend` id the human was offered (at least 12 hex digits of its SHA-256). The backend hashes the file at its launch path `exe` again and refuses unless it is that build (refused: still being written, or replaced since) and not the build already running (refused: a second window's late click after the restart). Then it runs `<exe> reload` detached: the CLI's own safety checks, then it stops this backend and starts the build at `exe`. Terminals keep running; clients reconnect as after any backend restart. A reload the CLI refuses leaves this backend running and sends an error `notice`. Clients never queue it for a later connection. |
| `daemon.shutdown` | `{}` | `{}`. What `muster stop` does: runs this backend's own `muster stop` detached (it stops the backend and then the terminal host, ending every session), and once that has started emits `daemon_stopping` `{reason: "shutdown"}`. If it can't start, the call fails and nothing is emitted. Clients stop reconnecting only on `daemon_stopping` or this call's reply, never on a dropped connection. |
| `daemon.restart` | `{backend, host}` | `{exe}`. What `muster stop` then `muster` do, in one step: every session ends, then a new backend and terminal host start from the launch path `exe`, picked as a normal start picks them (so a waiting terminal host update is installed, and a waiting backend build too). `backend`, `host`: the `updates.backend` and `updates.host` ids the window was shown (null: none waiting). The backend hashes the launch file and the host beside it again and refuses unless they are still those builds (refused: still being written, or replaced since). It runs `<exe> restart-all --report-fd 3 --launch <exe> --app <sha256> [--host <sha256>]` detached (the full hashes it just read), with its own environment plus its OpenRouter key (if any), as `daemon.reload` runs `reload`. Before anything stops, that command takes the lifecycle lock (`<runtime>/lifecycle.lock`, which `muster stop` and `muster reload` also take, so one from a shell meanwhile is refused), checks that it can signal the backend and stop the terminal host (one of another protocol version too: identified by its `terminal.json` and the peer credentials of its socket, then sent SIGTERM), and prepares the release the new daemon will run (the snapshot a daemon start makes, under `<data>/application-releases/`), refusing unless its files have exactly those hashes and are executable. It reports on a socket pair (descriptor 3): `refused: <why>` (the call fails with that reason), or `ready`; then this backend emits `daemon_stopping` `{reason: "restart"}` and answers `go`, and only then does the command stop anything: this backend (waiting up to 120 s until it has really exited), then the host (30 s), then it starts the daemon from the release snapshot (`daemon --foreground --launch <exe>`: it runs that release and looks for updates at `exe`), without opening a window, and checks that the backend answering is the one it started (its `hello.build` that release's), on a new host started from the release's host. Before `ready` it also runs the snapshot once: its backend answers `--version` as this build, and its host starts on scratch directories and answers this build's protocol hello holding the fence it was given; a truncated host or one of another protocol version is refused there. If a step doesn't finish, it stops there and nothing is started over it. A command that exits without a word (an older build without `restart-all`) or says nothing for 20 s is ended and the call fails: nothing was stopped. A failure after `go` comes back as an error `notice` if this backend still runs, and is written to `<runtime>/restart-failed` for the next `muster` to print. Clients keep reconnecting after `{reason: "restart"}` and take only a `hello` from a backend with another `pid`, on a terminal host with another `instance`, as the restarted one; one whose `build` isn't the backend build the window was shown (or, with none shown, its previous build) is taken as restarted by something else, and says so. Never queued for a later connection. |
| `prefs.get` | `{}` | `{decorations, shell?, cache_warn, cache_ttl, revision}`: preferences every window shares (stored in the database, so a new native window reads them before it is built). `decorations`: native windows show the system title bar (default true). `shell`: absolute path of the shell new shell sessions run; absent means the account's login shell (from the password database), else `$SHELL`, else `/bin/bash` (`hello.login_shell`; `hello.default_shell` is the one in use). `revision` rises with every `prefs.set`. |
| `prefs.set` | `{decorations?, shell?, cache_warn?, cache_ttl?}` | The new preferences; fields left out stay, unknown fields are refused. `cache_warn`, `cache_ttl`: see "Cold prompt cache". `shell` takes a path or a name on the backend's `PATH` and stores the absolute path of an executable file (else an error); `""` goes back to the login shell. Setting it is local only, like `daemon.shutdown`. Running shells keep theirs; new and restarted ones use it. Writes are serialized from read to broadcast; each emits `prefs_changed` with the result. A client ignores a reply or event with a lower `revision` than one it has already applied (a reply can arrive after a later change's event). |

`daemon.reload`, `daemon.reload_handoff`, `daemon.restart` and `daemon.shutdown` run one at a time: while one
this backend started may still happen (until it fails, or this backend stops), the others are refused with
"Muster is already restarting or shutting down" (`muster reload` reports that instead of waiting).

What "human only" means for `daemon.reload`, `daemon.reload_handoff`, `daemon.restart`, `daemon.shutdown` (and `prefs.set`, which needs
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

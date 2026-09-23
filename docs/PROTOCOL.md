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

The UI is opened as `http://127.0.0.1:<port>/#token=<client token>`. The frontend reads the
token from the hash, stores it in `sessionStorage`, and strips it from the URL.
The daemon rejects `/ws` upgrades whose `Origin` is present and not `http://127.0.0.1:<port>`
or `http://localhost:<port>` (or a `tauri://`/`wry://` origin).

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
| `session.send` | `{id, text, mode?: "queue"\|"interrupt"}` | `{delivered, queued}` |
| `session.approve` | `{id, decision: "approve"\|"approve_always"\|"deny"}` | `{}` |
| `session.interrupt` | `{id}` | `{}` |
| `session.clear_queue` | `{id}` | `{}` |
| `session.screen` | `{id}` | `{lines: string[]}` current visible screen as text (inbox previews) |
| `fs.complete` | `{prefix}` | `{entries: string[]}` directory completions for the "add workspace" path box |

Exited sessions keep their scrollback (persisted under `$XDG_DATA_HOME/muster/sessions/<id>/output.log`,
capped at 2 MiB) so they can be attached (replay only) and restarted after a daemon restart.

## Session environment

Every session gets: `MUSTER=1`, `MUSTER_SESSION_ID`, `MUSTER_CHANNEL_ID`, `MUSTER_URL`,
`MUSTER_TOKEN` (session token), `MUSTER_BIN`, `PORT` and `MUSTER_PORT_BASE` (channel port block
of 10), `TERM=xterm-256color`, `COLORTERM=truecolor`.

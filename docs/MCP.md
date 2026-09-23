# muster MCP server

The daemon runs an MCP server so agents inside muster sessions can see and drive the environment:
list what's running, spawn helpers, message other agents, read their terminals, wait for them,
post to the human's timeline, and review diffs.

Code: `src/mcp/` (`protocol.rs` transport + JSON-RPC, `tools.rs` tools, `guard.rs` guardrails,
`wait.rs` wait rules, `backend.rs` hub adapter).

## Endpoint contract

| | |
|---|---|
| URL | `POST {MUSTER_URL}/mcp`, e.g. `http://127.0.0.1:7433/mcp`. No trailing slash: `/mcp/` falls through to the web UI. |
| Transport | MCP Streamable HTTP. Responses are always `Content-Type: application/json` (never SSE). |
| Auth | `Authorization: Bearer <MUSTER_TOKEN>` (the session's own token). Missing/unknown token: `401`. The token identifies the **calling session**. |
| Origin | If an `Origin` header is present it must be loopback (`127.0.0.1`, `localhost`, `[::1]`, `tauri://`, `wry://`), else `403`. CLI clients send none. |
| `GET` / `DELETE` | `405 Method Not Allowed` (no standalone SSE stream, no sessions). |
| Notifications | `202 Accepted`, empty body. |
| Sessions | None. We never mint `Mcp-Session-Id`; a client-sent one is ignored. |

### Protocol versions (dual-era)

The server answers both eras of the spec on the same endpoint:

- **Modern `2026-07-28` (stateless):** chosen when a request carries
  `params._meta["io.modelcontextprotocol/protocolVersion"]`. Headers `MCP-Protocol-Version`,
  `Mcp-Method` and, for `tools/call`, `Mcp-Name` (plain, or `=?base64?…?=`) are required and
  must match the body, else `400` + `-32020 HeaderMismatch`. An unknown version gets `400` +
  `-32022` with `data.supported`. Methods: `server/discover`, `tools/list`, `tools/call`.
  Unknown methods get `404` + `-32601`. Results include `resultType: "complete"` and
  `_meta["io.modelcontextprotocol/serverInfo"]`; list results include `ttlMs` and `cacheScope`.
  Modern requests inside a JSON-RPC batch are refused, because their headers can't be checked.
  A `2026-07-28` `MCP-Protocol-Version` header on a body without `_meta` gets `-32020`.
- **Legacy `2025-11-25`, `2025-06-18`, `2025-03-26` (initialize-based):** `initialize` echoes
  the requested version if supported, else answers `2025-11-25`. It advertises
  `capabilities.tools` and `serverInfo.name = "muster"`. Also served: `notifications/initialized`
  (202), `ping`, `tools/list`, `tools/call`. JSON-RPC batches are accepted. An unknown
  `MCP-Protocol-Version` header on a non-initialize request gets `400`.

Every message must carry `"jsonrpc": "2.0"`, else `400` + `-32600`.

Verified against real clients (2026-09-23): **Claude Code 2.1.280** speaks the modern era
(`server/discover` probe, then `tools/list` and `tools/call`, all with `_meta` and the mirrored headers).
**Codex CLI 0.155.1** speaks legacy `2025-06-18` (`initialize`, `notifications/initialized`,
`tools/list`, `tools/call`, no session id).

Tool failures (bad arguments, guardrail refusals, unknown sessions) come back as normal results
with `isError: true` and a message the model can act on. An unknown tool name is a JSON-RPC
`-32602` error.

## How sessions get registered

The spawning code (agent adapters, `src/agents/*`) wires this server into each agent for that
session only, never in the user's global config. Every session has `MUSTER_URL` and
`MUSTER_TOKEN` in its environment. Suggested wiring:

- **Claude Code:** `claude --mcp-config <session_dir>/mcp.json` where the file contains
  ```json
  {"mcpServers": {"muster": {"type": "http", "url": "http://127.0.0.1:7433/mcp",
    "headers": {"Authorization": "Bearer <session token>"}}}}
  ```
  The tools then appear as `mcp__muster__<tool>`. Consider pre-allowing them with
  `--allowedTools mcp__muster__*` so read-only calls don't trigger permission prompts.
- **Codex CLI:** config overrides, e.g.
  `-c 'mcp_servers.muster.url="http://127.0.0.1:7433/mcp"'`
  `-c 'mcp_servers.muster.bearer_token_env_var="MUSTER_TOKEN"'`
  `-c 'mcp_servers.muster.tool_timeout_sec=1900'`.
  Raise `tool_timeout_sec`, because Codex's default tool timeout (about 60 s) would cut off
  `wait_for_state`.
- **Anything else:** POST JSON-RPC with the bearer header, as below.

```sh
curl -s -X POST "$MUSTER_URL/mcp" -H "Authorization: Bearer $MUSTER_TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}'
```

## Tools

All results are text. Structured results are pretty-printed JSON. "Caller" means the session
whose token made the request. By default every tool is limited to the caller's **workspace**
(see [Reach](#reach-shell-commands-and-other-workspaces)): lists leave out other workspaces,
and targets in them are refused.

| Tool | Arguments | Result |
|---|---|---|
| `whoami` | — | Caller session, cwd, channel (id, name, branch, base branch, cwd), workspace (id, name, path), spawn budget, `policy: {shell_commands, other_workspaces}`. |
| `list_channels` | — | Non-archived channels: id, name, workspace, cwd, branch, base_branch, `state` (aggregate), `sessions`, `live_sessions`. |
| `list_sessions` | `channel_id?` | All sessions (including exited): id, channel_id, kind, name, state, detail, state_since_ms, queued, parent_id, title, exit_code, is_me. |
| `spawn_agent` | `kind` (`claude`\|`codex`\|`shell`), `prompt?`, `name?`, `channel_id?` **or** `new_channel?: {name, worktree? = true, base?}` | New session (`parent_id` = caller) plus its channel. Default: caller's channel. `new_channel` is created in the caller's workspace. `kind: shell` with a `prompt` is refused unless shell commands are allowed. |
| `send_message` | `session_id`, `text`, `interrupt? = false` | `{delivered, queued, sent_at_ms}`. Goes through the injection queue (`inject::send`, `from = "session:<caller>"`). Shell targets are refused unless shell commands are allowed; when allowed, the text is preceded by a line clear (Ctrl-E Ctrl-U) so it doesn't append to a half-typed command. |
| `read_screen` | `session_id` | `[session N (kind) state=S]` header, then the visible screen. |
| `get_recent_output` | `session_id`, `lines? = 100` (max 2000) | Last N lines, ANSI stripped. |
| `wait_for_state` | `session_id`, `states?`, `timeout_seconds? = 300` (max 1800), `after_ms?` | `{state, detail, reached, timed_out, ended, waited_ms, queued, …}`. |
| `post_to_channel` | `text`, `channel_id?` (default: caller's) | Logs an `agent_post` event (`{session_id, text}`) on the human-visible timeline. |
| `get_diff` | `channel_id?` (default: caller's), `max_bytes? = 60000` (max 400000) | `worktree::diff(channel.cwd, channel.base_branch)`. Past `max_bytes` it is cut at a line boundary with a note listing the changed files (first 200). |

### wait_for_state semantics

Default `states`: `idle, needs_approval, asking_question, error, stuck, exited`, meaning
"finished or needs help". (`done` is accepted as an alias for `idle`.)

1. It returns at once if the session is already in one of the states, except for a
   **stale `idle`**. Only `idle` means "done", so only `idle` can be stale. Blocked states
   (`needs_approval`, `asking_question`, `error`, `stuck`) always count, because a queued
   message can't be delivered while the agent is blocked on them.
2. An `idle` state is stale when:
   - the session still has queued input (the queue is about to deliver), or
   - there was **activity** after the session went idle, less than 15 s ago. Activity means
     an MCP `send_message` to the session, or a delivery out of its injection queue (the
     daemon sees the queue length shrink, from any sender, human included).

   So "send, then wait" waits for the agent to actually take the message and finish, even
   when the message sat in the queue first. If no state change shows up within 15 s of the
   activity, the idle is accepted (the agent may have finished too fast to observe).
3. `after_ms` (unix ms): pass `send_message`'s `sent_at_ms`. A non-idle state must have been
   entered at or after that time. For `idle`, `after_ms` becomes the activity time in rule 2,
   replacing our own send marker, with the same 15 s grace. Queue deliveries still count.
   `after_ms = 0` turns off every staleness rule and accepts any matching state.
4. It otherwise waits on the hub's event bus (`SessionUpserted`) and re-checks at least once
   a second.
5. If the session exits while you wait for other states, the call returns `ended: "exited"`.
6. On timeout it returns normally with `timed_out: true` (not an error). A dropped HTTP
   request cancels the wait, and the audit log records it as `cancelled`.

## Guardrails

| Rule | Value |
|---|---|
| Spawn depth | A human-started session is depth 0. Its children are depth 1, and so on. Max depth **3**: a depth-3 session can't spawn. Computed by walking `parent_id` (cycle-safe), and also remembered at spawn time, so removing an ancestor can't shorten a lineage. (The remembered depths live in memory; after a daemon restart only the `parent_id` walk remains.) |
| Live children per caller | **8** (live sessions whose `parent_id` is the caller) |
| Live MCP-spawned sessions, daemon-wide | **24** (live sessions with any `parent_id`) |
| `send_message` rate | **20 per 60 s** sliding window, per caller. A send that fails (e.g. the PTY is gone) doesn't use up a slot. |
| `spawn_agent` rate | **10 per 10 minutes** sliding window, per caller. Refused or failed spawns don't use up a slot. |
| Agent-created channels | At most **10** non-archived channels created through `spawn_agent`'s `new_channel`, daemon-wide. Spawning into existing channels is not affected. (Tracked in memory: after a daemon restart, earlier channels no longer count.) |
| Shell commands | Off by default, see below. |
| Other workspaces | Off by default, see below. |
| `post_to_channel` rate | **30 per 60 s** per caller |
| Self-messaging | Refused (`session_id` == caller) |
| Message size | 100 000 chars (`send_message`), 20 000 chars (`post_to_channel`) |
| Permission prompts | There is **no approve tool**. `send_message` with `interrupt: true` is refused while the target is `needs_approval`. Plain sends are queued and delivered only once the agent is ready. A marked extension point (`guard::AGENTS_MAY_APPROVE`, see `src/mcp/guard.rs`) exists for a future human-enabled setting. |
| Audit | Every `tools/call` (including refused ones, unknown tools, and calls without a name) logs an `mcp_call` event in the caller's channel: `{session_id, tool, args, ok}`, plus `cancelled: true` if the client disconnected mid-call. Strings in args over 500 bytes are shortened; args over 4 KB total are replaced by a truncated string. Protocol-level rejections (bad auth, bad headers) are not tool calls and aren't logged. |

### Reach: shell commands and other workspaces

A message to a shell session, or a shell spawned with a prompt, runs an arbitrary command
outside the agent CLI's own permission prompts. And one agent driving sessions in an unrelated
workspace crosses a line the human drew by making separate workspaces. So by default the MCP
server refuses:

- `spawn_agent` with `kind: "shell"` and a `prompt` (a shell without a prompt is fine: it's for
  the human), and `send_message` to a `shell` session. Error text: `… The human can allow it by
  starting the muster daemon with MUSTER_MCP_ALLOW_SHELL=1.`
- any target outside the caller's workspace: `send_message`, `read_screen`,
  `get_recent_output`, `wait_for_state` (by session), `post_to_channel`, `get_diff`,
  `spawn_agent` (by `channel_id`). `list_channels` and `list_sessions` only show the caller's
  workspace. Error text: `… The human can allow it by starting the muster daemon with
  MUSTER_MCP_CROSS_WORKSPACE=1.`

Text from agents (`send_message` text, `spawn_agent` prompt) is typed into a terminal, so it
is cleaned first: ESC and all other control characters except newline and tab are removed (CR
becomes newline). Otherwise `ESC [ 201 ~` could end the bracketed paste and the rest would
arrive as raw keystrokes, e.g. answering a permission dialog. And because Claude Code and Codex
run input that starts with `!` as a shell command, such text is refused for them too unless
shell commands are allowed.

Only the human can turn these on, through the daemon's environment (`1`, `true`, `yes` or `on`),
for example `MUSTER_MCP_ALLOW_SHELL=1 muster daemon`. The daemon logs a warning at startup when
either is on. Agents can see the current setting in `whoami`'s `policy`.

`spawn_agent` calls run one at a time, so parallel requests can't overshoot the limits.
If `spawn_agent` created a new channel and the spawn then fails, the channel (and its
worktree) is archived again.
Spawn limits apply to MCP spawns only. Humans can start any number of sessions from the UI.

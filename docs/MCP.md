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

The spawning code (agent adapters, `src/adapters/*`) wires this server into each agent for that
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
| `whoami` | — | Caller session, cwd, channel (id, name, branch, base branch, cwd, `permissions`), workspace (id, name, path), spawn budget, `policy: {shell_commands, other_workspaces}`. |
| `list_channels` | — | Non-archived channels: id, name, workspace, cwd, branch, base_branch, `permissions`, `state` (aggregate), `sessions`, `live_sessions`. |
| `list_sessions` | `channel_id?` | All sessions (including exited): id, channel_id, kind, name, state, detail, state_since_ms, queued, parent_id, title, exit_code, is_me. |
| `spawn_session` (alias `spawn_agent`, its old name: still accepted, not listed) | `kind` (`claude`\|`codex`\|`shell`), `prompt?`, `name?`, `model?`, `effort?`, `args?: string[]`, `channel_id?` **or** `new_channel?: {name?, worktree? = true, base?}` | New session (`parent_id` = caller) plus its channel. Default: caller's channel. The new session is a tab in the caller's window. `new_channel` is created in the caller's workspace, in the caller's window (else its channel's), with the caller's channel's permission profile. Without a `name` it is auto-named like any unnamed channel (docs/PROTOCOL.md), usually from the agent's title or the `prompt`. Unlike `channel.create`, `worktree` defaults to `true` here, so parallel agents don't share a checkout. `kind: shell` with a `prompt` or any `args` is refused unless shell commands are allowed (arguments to a shell are a command too). `model`/`effort` become `--model`/`--effort` (claude) or `-m`/`-c model_reasoning_effort=` (codex); `args` are extra CLI arguments; all of them go through the agent-argument allowlist (docs/PROTOCOL.md, "Channel permissions"), so they can't widen the profile. |
| `send_message` | `session_id`, `text`, `interrupt? = false` | `{delivered, queued, sent_at_ms}`. Goes through the injection queue (`inject::send`, `from = "session:<caller>"`). Like any first prompt, it names an auto-named channel. Shell targets are refused unless shell commands are allowed; when allowed, the text is preceded by a line clear (Ctrl-E Ctrl-U) so it doesn't append to a half-typed command. |
| `read_screen` | `session_id` | `[session N (kind) state=S]` header, then the visible screen. |
| `get_recent_output` | `session_id`, `lines? = 100` (max 2000) | Last N lines, ANSI stripped. |
| `wait_for_state` | `session_id`, `states?`, `timeout_seconds? = 300` (max 1800), `after_ms?` | `{state, detail, reached, timed_out, ended, waited_ms, queued, …}`. |
| `post_to_channel` | `text`, `channel_id?` (default: caller's) | Logs an `agent_post` event (`{session_id, text}`) on the human-visible timeline. |
| `get_diff` | `channel_id?` (default: caller's), `max_bytes? = 60000` (max 400000) | `worktree::diff(channel.cwd, channel.base_branch)`. Past `max_bytes` it is cut at a line boundary with a note listing the changed files (first 200). |
| `list_agents` | — | `{agents: [{id, name, kind, model, effort, persona, permissions, accepts_origins, autostart, home_dir, channel_id, session_id, state, detail, resumable, created_at, is_me}], your_origin: {origin, hop}}`. All agents, whatever the caller's workspace. |
| `ask_agent` | `agent` (name, case-insensitive), `text`, `timeout_seconds? = 600` (max 1800) | `{agent, msg_id, session_id, reply, state, detail, timed_out, ended, origin, hop, sent_at_ms, started, waited_ms, note?, unattributed_output?}`. Starts the agent if it has no live session, sends `[muster msg <id> from <sender> origin=<o> hop=<n>]` plus the text through the injection queue (`from = "session:<caller>"`), then waits for **this message's** turn: until the agent reports a final message attributed to it (Stop hook, Codex notify), or, after the message was typed in, stops on a prompt, question or error (`reply: null` and a `note`), or exits. If the agent goes idle after it with no attributed final message (3 s grace), `reply` is null and `unattributed_output` holds its last 60 terminal lines, with a `note` that they may belong to another prompt. See "Agents" below. |

`whoami` also returns `agent` (the caller's agent name, or null) and `origin: {origin, hop}`.

### Panels

Panels always belong to the caller's **task**, even when cross-workspace access is enabled.
Only `get_diff` is app-visible (`_meta.ui.visibility: ["app", "model"]`); the other tools
are model-only. The bridge's `get_diff` accepts only `max_bytes`, using the panel's task.

| Tool | Arguments | Result |
|---|---|---|
| `panel_open` | `title`, `app` (`markdown`, `image`, `chart`, `table`, `pr_decision`, `pr_overview` or `custom`), `data?`, `html?`, `html_path?`, `csp?` | `{panel_id}` |
| `panel_update` | `panel_id`, `data?` (replaces the whole input), `html?`, `html_path?`, `title?` | `{}`; advances the revision |
| `panel_close` | `panel_id` | `{}` |
| `list_panels` | `{}` | `{panels: [...]}`; open panels in this task, with their latest `context` |
| `wait_for_panel_event` | `panel_id?`, `timeout_seconds? = 300` (0–1800) | `{events: [...], remaining, timed_out}`; consumes the returned events |
| `panel_event` | `panel_id`, `event_id` | `{event}`; the full event a panel prompt showed only as a placeholder; consumes it |

`markdown` takes `data: {markdown: "# Review"}`; the other built-ins are below. `custom` requires either inline `html` or
`html_path`, resolved inside the task cwd; symlinks escaping it are refused, and the path must
name a regular file (a FIFO or device is refused without waiting on it). HTML and JSON input
each have a 1 MiB limit; data must be an object. A task can have 16 open panels (128 in all);
closing one frees its slot and deletes its HTML and data. Open/update share the post rate
limit. `csp` uses MCP Apps fields (`resourceDomains`, `connectDomains`, `frameDomains`,
`baseUriDomains`): declared asset origins work, fetching and nested frames stay blocked.
Only loopback HTTP frame declarations are accepted; base URI overrides are refused.

A custom app is an MCP Apps (2026-01-26) page talking JSON-RPC to its parent frame. Until it
calls `ui/initialize` and then sends `ui/notifications/initialized`, the panel shows
"Loading"; after 10 s without it, the panel shows an error naming the stage that stalled, with
Reload. Input arrives as `ui/notifications/tool-input` (`params.arguments` is the panel's
`data`, again on every `panel_update`). A user action reaches the agent through `ui/message`,
accepted only straight after a click or key inside the panel's content (checked by the sandbox
proxy), at most 3 per user action (a 4th within about 5.5 s gets JSON-RPC error -32000), so
send one message per deliberate action; `_meta.muster.kind` names it.
A minimal app:

```html
<!doctype html><meta charset="utf-8"><button>Approve</button><script>
let n = 0; const calls = new Map();
const call = (method, params) => new Promise((ok) => { calls.set(++n, ok);
  parent.postMessage({jsonrpc: "2.0", id: n, method, params}, "*"); });
addEventListener("message", ({source, data: m}) => {
  if (source !== parent || m?.jsonrpc !== "2.0") return;
  if (m.id && calls.has(m.id)) { calls.get(m.id)(m.result); calls.delete(m.id); }
  if (m.method === "ui/notifications/tool-input") console.log(m.params.arguments);
  if (m.method === "ui/resource-teardown") parent.postMessage({jsonrpc: "2.0", id: m.id, result: {}}, "*");
});
call("ui/initialize", {protocolVersion: "2026-01-26", appInfo: {name: "approve", version: "1"},
  appCapabilities: {}}).then(() =>
  parent.postMessage({jsonrpc: "2.0", method: "ui/notifications/initialized", params: {}}, "*"));
document.querySelector("button").onclick = () => call("ui/message", {role: "user",
  content: {type: "text", text: "approved"}, _meta: {muster: {kind: "approve"}}});
</script>
```

Events contain `{id, panel_id, kind, payload, at}`; `id` stays the same if an event is
delivered later than first planned. The latest app context rides on the last event of each
panel in a result. The durable queue keeps 200 events per panel, adding an `overflow` event
(id 0) with the dropped count. A wait returns whole events, up to about 256 KiB, and how many
remain. A wait is bound to the caller's task and terminal run: it fails if the caller moves to
another task or restarts, and only then do events go to the idle opener again.

Without a wait, the idle opener receives a marked prompt through the durable injection queue,
at most 24 KiB of whole events in order (the app context is cut to 4 KiB in it; `list_panels`
has it all). Events that do not fit stay queued for the next prompt; one too long for any
prompt appears as a placeholder with its id, for `panel_event`. The timeline records the
prompt as `panel:<id>`; a user action starts a human-origin turn at hop 0. A prompt is typed
only while its opener is still in the panels' task and run: if the opener moves, exits or
restarts before the prompt is typed, its events go back to the panel queue. Once typing has
begun the prompt is never resent; if its run then ends, a notice says the events may not have
arrived. Clearing a session's prompt queue discards panel prompts in it. Shell, exited and
moved-to-another-task openers keep events queued. A human closing a tab queues `closed`;
agent `panel_close` does not. Archiving the task, including when its last session closes or
moves out, removes panels, files and events.

#### Built-in apps

Built-ins take only `data` (no `html`). Each follows the host theme and works by keyboard.
`panel_open` and `panel_update` refuse input whose basic shape is wrong, naming the field:
`image` (all of it, unknown fields included), `chart` (type, x values, series names and
lengths), `table` (column keys, rows as objects), `pr_decision` (`repo`, `number`,
`head_sha`, `base`, `head`, `url`, `title`, `author`, `summary`, `risk.level`, `checks`,
`changes`, `merge_method_default`, `status`, all of `media` and `actions`) and `pr_overview`
(`repo`, each PR's number, title, author, enums, counts, `url` and `head_sha`, `current`,
`sort`, `actions`). The app then checks the rest and shows any problem as a list inside the
panel; unknown fields of chart, table, pr_decision and pr_overview are only listed there as
warnings. Source: `web/src/panels/apps/`, built into `web/panels/<app>.html` by
`cd web && deno task panels`.

**`image`**: `{path?, url?, alt?, caption?}`, exactly one of `path` or `url`. `path` is a file
in the task worktree (the task cwd): Muster reads it when the panel opens or updates, refuses
`..` and symlink escapes, non-regular files and files over 8 MiB, accepts PNG, JPEG, GIF,
WebP and SVG (by content; SVG also needs the `.svg` extension), and stores it as a data URI
(`src`, `mime`, `bytes` are added by Muster; an agent may not pass them). SVG is shown with
`<img>`, so its scripts never run. `url` must be https without credentials; `panel_open` adds
its origin to `csp.resourceDomains`, and a later update must keep to an origin already
declared. The host must be a plain name or IP (no wildcards). Without `alt`, the caption or
"Image: <file name>" is the text alternative. Z or the button toggles zoom to fit / actual size.

```json
{"path": "shots/login.png", "alt": "Login page after the fix", "caption": "Chromium, 1280 px"}
```

**`chart`**: `{type, title?, x: {label?, values}, series: [{name, values}], y?: {label?, min?, max?}}`.
`type` is `bar`, `line`, `scatter` or `area`; `x.values` are 1–2000 numbers or strings
(numbers only for `scatter`; all-number x gives `line`/`area` a linear axis); 1–12 series,
each with one value per x value (a number, or `null` for a gap). Bars and areas include zero;
`y.min`/`y.max` fix the axis (with only one fixed, the other side follows the data, even if
the fixed bound lies beyond it). Series 1–8 get distinct colours; 9–12 reuse them with dashed
lines, hatched bars and square markers. Drawn as SVG with an accessible title and description, a
legend, hover and arrow-key tooltips, and a data table (hidden until "Show data table").

```json
{"type": "bar", "title": "CI minutes per day", "x": {"label": "Day", "values": ["Mon", "Tue", "Wed"]},
 "series": [{"name": "master", "values": [42, 55, 38]}, {"name": "PRs", "values": [80, 96, null]}],
 "y": {"label": "Minutes"}}
```

**`table`**: `{title?, columns: [{key, label?, align?}], rows: [{<key>: value}], sort?: {key, dir?}}`.
1–50 columns (`align`: `left`, `right` or `center`), up to 5000 rows whose values are
strings, numbers, booleans or null. Columns holding only numbers are right-aligned in
monospace. Header click sorts (text naturally, numbers numerically, empty cells last), the
filter box (`/`) matches every word in any column, and the header stays visible on scroll.
`sort.dir` is `asc` (default) or `desc`.

```json
{"title": "Slowest tests", "columns": [{"key": "test", "label": "Test"}, {"key": "seconds", "label": "Seconds"}],
 "rows": [{"test": "export_large_csv", "seconds": 27.05}, {"test": "session_survives_restart", "seconds": 12.4}],
 "sort": {"key": "seconds", "dir": "desc"}}
```

**`pr_decision`** ([AGENT_PANELS.md](AGENT_PANELS.md#pr-decision-end-to-end)):

| Field | |
|---|---|
| `repo`, `number`, `title`, `author`, `url` | `owner/name`, PR number, https PR URL |
| `base`, `head`, `head_sha` | branches, and the full 40-hex head commit (`headRefOid`) |
| `merge_method_default?`, `delete_branch_default?` | `squash` (default), `merge` or `rebase`; `false` |
| `summary` | what changed and why, plain text |
| `risk` | `{level: low\|medium\|high, reasons?: [string]}` |
| `checks` | `{state: passing\|failing\|pending\|none, items?: [{name, state: pass\|fail\|pending\|skipping\|cancel, url?}]}`, the checks of `head_sha`. With items, the badge comes from them (any `fail`: failing; else any `pending`: pending; else passing) and a contradicting `state` is a warning |
| `reviews?` | `{decision?: "APPROVED"…, items?: [{author, state}]}` |
| `changes` | `[{file, note, diff}]`: only the changes worth a look, each a unified diff (with or without the `diff --git` header; renames, mode changes and binary files show as a line) and the agent's note |
| `rest_note?`, `questions?` | a line about what is not shown; the agent's open questions |
| `answer?` | `{question, text}`: the agent's answer to an Ask, shown inline |
| `status?`, `status_detail?` | `open` (default), `merging`, `queued` (in a merge queue or set to auto-merge), `merged`, `failed` or `requested_changes`, shown as a banner and beside the actions; with `open`, a `status_detail` shows as a note (e.g. what the agent did for a custom action) |
| `media?` | up to 8 pictures and charts shown after the summary, before the diffs (below) |
| `actions?` | the user's own actions ([below](#the-users-own-actions)) |

Actions: **Merge** (M) with a method select and a delete-branch box shows the exact command
`gh pr merge <number> --repo <repo> --<method> [--delete-branch] --match-head-commit <head_sha>`
and sends only after Enter or "Confirm merge". The confirm arms only once the key that opened
it is released (a held or repeated key never confirms), and any update that changes `repo`,
`number`, `base`, `head_sha`, the checks state or `merge_method_default`, or any invalid
update, cancels it. Merge is disabled while checks fail unless Shift is held (Shift+M; a hint
says so beside it), and while the agent works (`merging`, `queued`, `merged`, or until the next
update after a sent decision). **Request changes** (R) and **Ask** (A) open a text box
(Ctrl+Enter sends); text that would make the event exceed the 16 KiB event limit cannot be
sent, and both are off once the PR is `merged`, `queued` or `requested_changes`. **Open on
GitHub** (O) uses the host browser. **Skip** (S) moves on without changing the PR; **Hold** (H)
puts it aside for later (off once it is `merged` or `queued`). Each sends one event, and both
are off until the agent's next update. The user's own actions follow them.

`media` items are `{kind: "image", path? | url?, alt?, caption?}` or `{kind: "chart", spec,
caption?}`. A picture follows the `image` app's rules: Muster reads a `path` (a file in the
task worktree, PNG, JPEG, GIF, WebP or SVG) into a data URI, a `url` must be https and its
origin joins `csp.resourceDomains` on `panel_open` (an update must keep to declared origins).
At most 8 items and 6 MiB of image files in all; the agent's own JSON keeps the 1 MiB limit.
A chart's `spec` is a `chart` app input, drawn by the same code. Pictures show as thumbnails
(one alone shows larger); a click or Enter enlarges one inside the panel, arrows step through
them, Esc or a click closes it.

Events are sent with `ui/message`; the decision is in the event's `kind`, and its fields in
`payload._meta.muster.data` (`payload.content.text` repeats it for people):

| `kind` | `payload._meta.muster.data` |
|---|---|
| `merge` | `{action: "merge", repo, number, base, method, delete_branch, head_sha}` |
| `request_changes` | `{action: "request_changes", repo, number, head_sha, body}` |
| `ask` | `{action: "ask", repo, number, question}` |
| `skip` | `{action: "skip", repo, number, head_sha}` |
| `hold` | `{action: "hold", repo, number, head_sha}` |
| `custom` | `{action: "custom", id, repo, number, head_sha}`, one of the user's own actions |

`--match-head-commit` binds the merge to the head only; the agent must also check that the
PR's base and repo still match the event before running the command (the PR reviewer flow
does).

```json
{"repo": "acme/api", "number": 812, "title": "Rate-limit failed logins per IP", "author": "dana",
 "url": "https://github.com/acme/api/pull/812", "base": "master", "head": "dana/login-limit",
 "head_sha": "3f9c2b7e41d0a8c6e5f4b3a2918d7c6b5a493827", "merge_method_default": "squash",
 "delete_branch_default": true, "summary": "Adds a per-IP limit to POST /login.",
 "risk": {"level": "medium", "reasons": ["Changes the login path"]},
 "checks": {"state": "passing", "items": [{"name": "test", "state": "pass"}]},
 "changes": [{"file": "src/auth/login.rs", "note": "Checked before the password hash.",
   "diff": "@@ -41,3 +41,6 @@\n     let ip = client_ip(&req);\n+    if let Some(wait) = app.limiter.blocked(ip).await? {\n+        return too_many(wait);\n+    }"}],
 "rest_note": "64 more lines of tests.", "questions": ["Is 5 per 10 minutes right?"]}
```

With pictures and a chart (the paths are files in the task worktree):

```json
{"media": [{"kind": "image", "path": ".muster-pr/812/1.png", "alt": "Login page after 5 failed attempts", "caption": "From the PR description"},
  {"kind": "chart", "caption": "Lines changed per area", "spec": {"type": "bar", "x": {"values": ["src/auth", "tests"]},
    "series": [{"name": "added", "values": [124, 64]}, {"name": "removed", "values": [9, 0]}]}}]}
```

**`pr_overview`**: every open PR of one repo as a card, for going through them with the agent.

| Field | |
|---|---|
| `repo` | `owner/name` |
| `title?` | heading (default "Open pull requests") |
| `prs` | up to 200 `{number, title, author, url?, created_at?, draft?, additions?, deletions?, files?, checks?, review?, labels?, risk?, summary?, status?, status_detail?, head_sha?}`; numbers unique |
| `prs[].checks?` | `passing`, `failing`, `pending` or `none` |
| `prs[].review?` | gh's `reviewDecision`: `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, `NONE` or `""` |
| `prs[].risk?`, `prs[].summary?` | the agent's triage: `low`, `medium` or `high`, and one line (at most 500 characters) |
| `prs[].status?` | the agent keeps it up to date: `new` (default), `reviewing`, `merged`, `queued`, `held`, `skipped`, `changes_requested` or `failed`; `status_detail` is a short note under the card |
| `current?` | the PR the agent is on (marked "Now"; the selection follows it) |
| `sort?` | the starting order: `risk` (riskiest first, default), `quick` (quick wins first: not a draft, checks passing, low risk, small) or `oldest` |
| `actions?` | the user's own actions ([below](#the-users-own-actions)) |

Cards show number, title, author, age, size (+/− lines, files), a draft flag, the checks,
review and risk badges, labels, the summary and the status. A select changes the sort and a
filter box (`/`, Esc clears) matches every word in the number, title, author, labels, summary
or status. PRs still to review come first, then decided ones (merged, queued, changes
requested), then held ones. Keys: J/K or ↓/↑ move, Enter or O opens the selected PR, H holds
it, N goes through them in order; a double-click opens. Readable from about 400 px wide.

| `kind` | `payload._meta.muster.data` |
|---|---|
| `open` | `{action: "open", repo, number}` |
| `hold` | `{action: "hold", repo, number}` |
| `next` | `{action: "next", repo, number, order}`: review `number` now, then the rest of `order` (the PRs still new or skipped, in the order the user sees, starting after `current`) |
| `custom` | `{action: "custom", id, repo, number, head_sha?}` for the selected PR (`head_sha` when its card has one) |

```json
{"repo": "acme/api", "current": 812, "prs": [
  {"number": 812, "title": "Rate-limit failed logins per IP", "author": "dana", "url": "https://github.com/acme/api/pull/812",
   "created_at": "2026-10-15T09:00:00Z", "additions": 182, "deletions": 12, "files": 6, "checks": "passing",
   "review": "REVIEW_REQUIRED", "labels": ["security"], "risk": "medium", "status": "reviewing",
   "summary": "Per-IP limit of 5 failed logins per 10 minutes on POST /login."},
  {"number": 815, "title": "Fix a typo in the README", "author": "lee", "additions": 1, "deletions": 1, "files": 1,
   "checks": "passing", "review": "APPROVED", "risk": "low", "summary": "One word in the install section."}]}
```

##### The user's own actions

Both PR apps take `actions: [{id, label, key?, description, outward?}]`, at most 8: buttons
for options the user asked the agent for ("a Needs QA button"). `id` is lowercase letters,
digits, `-` or `_` (at most 40), unique and not a built-in event name; `label` at most 40
characters; `description` (at most 300) says exactly what the agent will do and is shown as
the button's tooltip and in its confirm; `key` is one letter or digit, unique, and not one the apps use
(M R A O S H J K N; Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain
key never clashes). `outward: true` marks an action that changes GitHub or anything off this
machine (comments, labels, closing, pushing): its button (marked ↗) first shows the
description and needs Enter or "Confirm", armed only after the opening key is released, and
any change to the action or the PR cancels it. Each sends one `custom` event; an action is off
until the agent's next update. A custom action never merges: the agent merges only on a
pr_decision `merge` event.

```json
{"actions": [{"id": "needs-qa", "label": "Needs QA", "key": "q", "description": "Add the needs-qa label to this PR on GitHub.", "outward": true},
  {"id": "later", "label": "Label later", "key": "l", "description": "Note this PR in your list to label later; nothing changes on GitHub."}]}
```

The **PR reviewer** agent template (New agent) and the `muster-pr-review` skill (`muster skills
install`, docs/CLI.md) share the agent side: review one immutable
snapshot (head and base SHAs; diff and checks of that head), open this panel, wait for its
events, re-check head and base before merging, merge only on its Merge event, and report
`merged`, `queued` or `failed` from what GitHub says afterwards. For "help me review the open
PRs" they list the open PRs with `gh pr list`, triage each briefly, open a `pr_overview`, and
run that flow for each PR the user opens (or for each in turn on Next), updating its card
after each decision.

### Agents

A named agent (PLAN.md §10) is a persistent agent session with a home dir, its own tab and a
resumed conversation (the first pass called them bots). The envelope's sender label is the caller's agent name, else `session:<id>`.

**Provenance.** Every session has the origin and hop of the message it is handling
(`src/agents.rs`, runtime only). Anything it sends or spawns carries that origin one hop on (no
provenance: `human`, hop 0):

| Event | Effect on the target's provenance |
|---|---|
| A prompt from an agent is submitted (`send_message`, a `spawn_session` prompt, `ask_agent`, or the agent's `muster send/ask`) | Set to the sender's origin and hop, when the injector actually presses Enter, so a queued message never relabels the turn that is still running. |
| `spawn_session` (or `muster new` run by an agent) | The new session starts with the spawner's provenance and keeps it until it receives another attributed prompt (a parent moving on to human work doesn't make its helper human-rooted). |
| A prompt from the human through a client (UI, phone, `muster send` outside a session) is submitted | Cleared (human-rooted). |
| UserPromptSubmit without an envelope | Cleared only if the daemon saw human input for that session through a client connection (keystrokes, paste, `session.input` without `from_session`) since its last attributed delivery; otherwise kept. A hook never lowers provenance on its own. |
| UserPromptSubmit with an envelope | Counts only for an unanswered message muster delivered to that session; unknown, foreign or answered ids are ignored. |

An agent's reply is attributed per message: the message whose prompt was submitted (confirmed by the
envelope in UserPromptSubmit) owns the next final message; a prompt without an envelope in between
takes the turn, and its final message belongs to no agent message.

**CLI calls.** `muster send/keys/ask/new` pass `$MUSTER_SESSION_ID` as `from_session`. For an
agent session, `muster ask` runs this server's `ask_agent` and `muster send` its `send_message` as
that session (same guards, rate limits and audit event, with `via: "cli"` in the args); `muster
keys` and `muster new` carry its provenance and the Claude origin rule. A shell session is a human
typing in a shell tab, with that shell's provenance if an agent ever typed into it.

**Refusals are logged.** A message refused by the agent's policy (`ask_agent`'s permission
check, origin rule, hop limit or daily cap, and the same checks behind `muster ask`) is kept in
`agent_messages` with the reason and shown in the agents panel's log (`agent.messages`,
`agent_message` events). It is never delivered and never counts toward the cap. Bounded: per
sender at most 10 refusals per 10 minutes are stored (more are only counted, and the next stored
one says how many), the text is cut to 2 KB, and refused rows are pruned to the last 30 days and
the newest 1000.

**Old names.** `ask_bot` (argument `bot` or `agent`) and `list_bots`, the first pass's names
that older persona files still use, are accepted with the same guards but not listed. The
bots → agents move also rewrites exact occurrences of those names (and
`mcp__muster__ask_bot` / `mcp__muster__list_bots`) in agents' `CLAUDE.md` / `AGENTS.md`.

**Limits** (not closed): an agent that drops `MUSTER_SESSION_ID` and calls the API with the client
token is the human as far as the daemon can tell (the documented client-token limit, PROTOCOL.md
"Limits"). Provenance, active message attribution and prompt queues persist across backend
replacement. Surviving terminal processes retain their tokens and non-human origin. Hook
callbacks from new runs use a durable inbox while the backend is unavailable. Codex sessions with hooks off (`MUSTER_CODEX_HOOKS=0`) send no UserPromptSubmit, so a
prompt typed in their terminal never clears provenance (stricter, never looser). Replies need the
Stop hook or Codex notify; without them `ask_agent` returns the unattributed terminal tail.

Reach: agents live in their own "agents" workspace (`<data>/agents`). `list_agents` and `ask_agent` work
from every workspace: an agent is a shared service, asking runs no command anywhere, and the origin
rule, hop limit, daily cap and permission check apply. Every other tool keeps the workspace rule,
so agents can read each other's terminals but not reach the user's project workspaces (unless
`MUSTER_MCP_CROSS_WORKSPACE=1`), and project agents don't drive an agent's terminal directly.

### Permissions

Each channel has a permission profile the human sets in the UI (`channel.set_permissions`, see
PROTOCOL.md): `codex_sandbox` (`read_only`, `workspace_write`, `workspace_write_network`,
`full_access`) and `claude_mode` (`default`, `accept_edits`, `plan`, `bypass`). Every agent in
the channel starts with it, including ones spawned through MCP:

- `spawn_session` always uses the target channel's profile. It has no argument to ask for more
  (unknown arguments are ignored), and no tool changes a profile.
- An agent can't use a sibling channel's wider profile: `spawn_session` with a `channel_id` whose
  profile gives that `kind` more than the caller's own channel does (Codex: `codex_sandbox`,
  Claude: `claude_mode`, ordered `read_only < workspace_write < workspace_write_network <
  full_access` and `plan < default < accept_edits < bypass`) is refused, and so is
  `send_message` to an agent running in such a channel. Shells aren't governed by the profile.
  Spawning into or messaging an equal or narrower channel is fine.
- A `new_channel` created through `spawn_session` gets the **caller's** channel profile, never
  more (and never the defaults if the caller has less).
- `whoami` and `list_channels` show each channel's `permissions` as
  `{codex_sandbox, claude_mode, codex_flags, claude_permission_mode, set_by}`, so an agent knows
  whether a child could, say, reach the network before it spawns one. If a task needs more, the
  agent should ask the human to change the channel's permissions (or post to the channel).
- The profile is not a sandbox around muster itself: see "Limits" under "Channel permissions"
  in PROTOCOL.md (a networked or unsandboxed agent can read the client token).

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
| Spawn depth | A human-started session is depth 0. Its children are depth 1, and so on. Max depth **3**: a depth-3 session can't spawn. Computed by walking `parent_id` (cycle-safe), and also remembered at spawn time, so removing an ancestor can't shorten a lineage. Remembered parent depths persist before spawn, so backend replacement can recover a child even when its acknowledgement was lost. |
| Live children per caller | **8** (live sessions whose `parent_id` is the caller) |
| Live MCP-spawned sessions, daemon-wide | **24** (live sessions with any `parent_id`) |
| `send_message` rate | **20 per 60 s** sliding window, per caller. A refusal before durable queue acceptance doesn't use up a slot; retained or uncertain deliveries still count. |
| `spawn_session` rate | **10 per 10 minutes** sliding window, per caller. Refused or failed spawns don't use up a slot. |
| Agent-created channels | At most **10** non-archived channels created through `spawn_session`'s `new_channel`, daemon-wide. Spawning into existing channels is not affected. Channel origin is recorded in the same transaction as creation, so earlier open channels still count after backend replacement. |
| Shell commands | Off by default, see below. |
| Other workspaces | Off by default, see below. |
| `post_to_channel` rate | **30 per 60 s** per caller |
| Self-messaging | Refused (`session_id` == caller; `ask_agent` of the caller's own agent) |
| Agent origins | `ask_agent` is refused unless the agent's `accepts_origins` contains the message's origin. Defaults: Claude agents `human`; Codex agents `human`, `schedule`, `webhook`. A Claude session that isn't a named agent takes `human` only: `send_message` to one, or `spawn_session` of `kind: claude`, from a non-human chain is refused. |
| Agent hops | Max hop **4** (`guard::MAX_AGENT_HOPS`); a message from the human or a trigger is hop 0. |
| Agent daily cap | **200** messages delivered to one agent per rolling 24 hours, any origin (`guard::AGENT_DAILY_PROMPT_CAP`; counted from `agent_messages`, so it survives restarts, and re-checked in the same transaction as the insert, so concurrent asks over MCP and the CLI can't overshoot). |
| Agent messages | Only through `ask_agent`: `send_message` to an agent's session is refused. `ask_agent` shares `send_message`'s rate limit, size limit, text cleaning, `!` rule and permission-widening check, against the widest of the agent session's current tab and the agent's own tab (its profile before it has one). An agent only ever runs in its own tab (stored on the agent), never a same-named one. |
| Message size | 100 000 chars (`send_message`), 20 000 chars (`post_to_channel`) |
| Permission prompts | There is **no approve tool**. `send_message` with `interrupt: true` is refused while the target is `needs_approval`. Plain sends are queued and delivered only once the agent is ready. A marked extension point (`guard::AGENTS_MAY_APPROVE`, see `src/mcp/guard.rs`) exists for a future human-enabled setting. |
| Audit | Every `tools/call` (including refused ones, unknown tools, and calls without a name) logs an `mcp_call` event in the caller's channel: `{session_id, tool, args, ok}`, plus `cancelled: true` if the client disconnected mid-call. Strings in args over 500 bytes are shortened; args over 4 KB total are replaced by a truncated string. Protocol-level rejections (bad auth, bad headers) are not tool calls and aren't logged. |

### Reach: shell commands and other workspaces

A message to a shell session, or a shell spawned with a prompt, runs an arbitrary command
outside the agent CLI's own permission prompts. And one agent driving sessions in an unrelated
workspace crosses a line the human drew by making separate workspaces. So by default the MCP
server refuses:

- `spawn_session` with `kind: "shell"` and a `prompt` (a shell without a prompt is fine: it's for
  the human), and `send_message` to a `shell` session. Error text: `… The human can allow it by
  starting the muster daemon with MUSTER_MCP_ALLOW_SHELL=1.`
- any target outside the caller's workspace: `send_message`, `read_screen`,
  `get_recent_output`, `wait_for_state` (by session), `post_to_channel`, `get_diff`,
  `spawn_session` (by `channel_id`). `list_channels` and `list_sessions` only show the caller's
  workspace. Error text: `… The human can allow it by starting the muster daemon with
  MUSTER_MCP_CROSS_WORKSPACE=1.`

Text from agents (`send_message` text, `spawn_session` prompt) is typed into a terminal, so it
is cleaned first: ESC and all other control characters except newline and tab are removed (CR
becomes newline). Otherwise `ESC [ 201 ~` could end the bracketed paste and the rest would
arrive as raw keystrokes, e.g. answering a permission dialog. And because Claude Code and Codex
run input that starts with `!` as a shell command, such text is refused for them too unless
shell commands are allowed.

Only the human can turn these on, through the daemon's environment (`1`, `true`, `yes` or `on`),
for example `MUSTER_MCP_ALLOW_SHELL=1 muster daemon`. The daemon logs a warning at startup when
either is on. Agents can see the current setting in `whoami`'s `policy`.

`spawn_session` calls run one at a time, so parallel requests can't overshoot the limits.
If `spawn_session` created a new channel and the spawn then fails, the channel (and its
worktree) is archived again.
Spawn limits apply to MCP spawns only. Humans can start any number of sessions from the UI.

MCP guard state is durable application state. Rate-limit reservations are saved before
agent actions; a failed persistence write refuses the action. Backend replacement
restores rate windows, send/delivery markers, created-channel membership and remembered
spawn depths. This state is independent of terminal-host lifetime and protocol.

## Claude profile inheritance

`whoami` and `list_sessions` include the session's nullable `account_profile_id` (a Muster
configuration-directory reference, not verified account identity). A Claude child created
by `spawn_session` inherits its calling Claude session's pinned profile, including Default.
If the caller is not Claude, the target workspace default applies. This also holds when the
tool creates a new channel. Disabled/missing profiles fail without falling back to another
account. Named agents keep their own creation-time profile when started through `ask_agent`.
Profile registration and manual selection are available through the desktop/CLI, not MCP.

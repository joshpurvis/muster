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
| Transport | MCP Streamable HTTP. Replies use `application/json`. A pending tool call with a progress token can receive `text/event-stream`. |
| Auth | `Authorization: Bearer <MUSTER_TOKEN>` (the session's own token). Missing/unknown token: `401`. The token identifies the **calling session**. |
| Origin | If an `Origin` header is present it must be loopback (`127.0.0.1`, `localhost`, `[::1]`, `tauri://`, `wry://`), else `403`. CLI clients send none. |
| `GET` / `DELETE` | `405 Method Not Allowed` (no standalone SSE stream, no sessions). |
| Notifications | `202 Accepted`, empty body. |
| Sessions | None. We never mint `Mcp-Session-Id`; a client-sent one is ignored. |

### Progress during long waits

For a single `tools/call`, put a string or integer token in `params._meta.progressToken`
and accept both `application/json` and `text/event-stream`. A wait reports status changes
and sends a heartbeat every 30 seconds. This covers `wait_for_state`, `ask_agent`,
`request_session` while waiting for a reply, and `wait_for_panel_event`.

Each SSE `message` event carries one JSON-RPC message. A `notifications/progress`
notification echoes the token and gives an increasing `progress` number and a short
`message`. The last event carries the ordinary response with the original request ID,
then the stream closes. A call that finishes before the first update returns JSON.

Clients that omit the token receive one JSON response when the wait ends and may hit
their own idle timeout first. The tool's requested wait and overall time limit still apply.

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
- **Antigravity CLI.** Muster adds a private workspace (`--add-dir`) containing `.agents/mcp_config.json` and an `.agents/AGENTS.md` that tells the model to call Muster's tools through agy's `call_mcp_tool` (server `muster`), and allows `mcp(muster/*)` in the session's private settings, beside the user's own allow rules. See PROTOCOL.md, “Antigravity”.
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
| `whoami` | — | Caller session, cwd, channel (id, name, branch, base branch, cwd, `permissions`), workspace (id, name, path), spawn budget, `policy: {shell_commands, sharing_rules, may_start_sessions_in}`. `may_start_sessions_in`: the names of the workspaces besides its own that the caller's agent may start sessions in (empty for a session that isn't a named agent). |
| `list_channels` | — | Non-archived channels: id, name, workspace, cwd, branch, base_branch, `permissions`, `state` (aggregate), `sessions`, `live_sessions`. |
| `list_sessions` | `channel_id?` | All sessions (including exited): id, channel_id, kind, name, state, detail, state_since_ms, queued, quiet_waiting (quiet messages waiting for a later turn, see "Quiet messages" below), parent_id, title, exit_code, is_me, `link`, `mention` (see "Links" below). With `queued` above 0 also `queue_waits`: why those prompts are not typed yet, in plain words, or null when the next is about to be typed (docs/PROTOCOL.md `session.queue`, `waiting`). |
| `spawn_session` (alias `spawn_agent`, its old name: still accepted, not listed) | `kind` (`claude`\|`codex`\|`antigravity`\|`shell`), `prompt?`, `name?`, `model?`, `effort?`, `args?: string[]`, `channel_id?` **or** `new_channel?: {name?, workspace?, worktree? = true, base?}` | New session (`parent_id` = caller) plus its channel. Default: caller's channel. The new session is a tab in the caller's window. `new_channel` is created in the caller's workspace (or in the one `workspace` names, exactly, if the human lets the caller's agent start sessions there: see "Starting sessions in another workspace"), in the caller's window (else its channel's), with the caller's channel's permission profile. Without a `name` it is auto-named like any unnamed channel (docs/PROTOCOL.md), usually from the agent's title or the `prompt`. Unlike `channel.create`, `worktree` defaults to `true` here, so parallel agents don't share a checkout. `kind: shell` with a `prompt` or any `args` is refused unless shell commands are allowed (arguments to a shell are a command too). `model`/`effort` become `--model`/`--effort` (claude) or `-m`/`-c model_reasoning_effort=` (codex); `args` are extra CLI arguments; all of them go through the agent-argument allowlist (docs/PROTOCOL.md, "Channel permissions"), so they can't widen the profile. |
| `send_message` | `session_id`, `text`, `interrupt? = false`, `wake? = true` | `{delivered, queued, sent_at_ms}`. Goes through the injection queue (`inject::send`, `from = "session:<caller>"`). With `wake: false` it is kept for the session's next turn that can take it instead (`{session_id, quiet_id, wake: false, status: "waiting", note, sent_at_ms}`; Claude sessions only, not with `interrupt`; see "Quiet messages" below). A named agent's session is refused: it takes messages through `ask_agent`, or `tell_agent` for a note that doesn't need an answer. Like any first prompt, it names an auto-named channel. Shell targets are refused unless shell commands are allowed; when allowed, the text is preceded by a line clear (Ctrl-E Ctrl-U) so it doesn't append to a half-typed command. |
| `read_screen` | `session_id` | `[session N (kind) state=S]` header, then the visible screen. |
| `get_recent_output` | `session_id`, `lines? = 100` (max 2000) | Last N lines, ANSI stripped. |
| `wait_for_state` | `session_id`, `states?`, `timeout_seconds? = 300` (max 1800), `after_ms?` | `{state, detail, reached, timed_out, ended, waited_ms, queued, …}`. |
| `post_to_channel` | `text`, `channel_id?` (default: caller's) | Logs an `agent_post` event (`{session_id, text}`) on the human-visible timeline. |
| `get_diff` | `channel_id?` (default: caller's), `max_bytes? = 60000` (max 400000) | `worktree::diff(channel.cwd, channel.base_branch)`. Past `max_bytes` it is cut at a line boundary with a note listing the changed files (first 200). |
| `list_agents` | — | `{agents: [{id, name, kind, model, effort, persona, permissions, accepts_origins, autostart, home_dir, channel_id, session_id, state, detail, resumable, created_at, sleep_after_min, asleep, asleep_at, self_handoff, self_handoff_at, tools_up_front, lead, spawn_workspaces, is_me}], your_origin: {origin, hop}}`. `spawn_workspaces`: ids of the workspaces besides its own the agent may start sessions in ("Starting sessions in another workspace"). `lead`: a lead agent (see "Lead agents" under Agents); a lead's `your_origin` is then `lead` in a scheduled turn. All agents, whatever the caller's workspace. `asleep`: its process was stopped while idle (state `exited`, detail `asleep`); asking it wakes it, which re-reads its whole conversation unless a pending account move starts it in a new one (docs/PROTOCOL.md "Sleep and wake"). Listing never wakes anyone. |
| `ask_agent` | `agent` (name, case-insensitive), `text`, `timeout_seconds? = 600` (max 1800) | `{agent, msg_id, session_id, reply, state, detail, timed_out, ended, origin, hop, sent_at_ms, started, woke, woke_note?, waited_ms, note?, unattributed_output?}`. Starts the agent if it has no live session (`woke: true` when it was asleep: it restarted into its conversation first), sends `[muster msg <id> from <sender> origin=<o> hop=<n>]` plus the text through the injection queue (`from = "session:<caller>"`), then waits for **this message's** turn: until the agent reports a final message attributed to it (Stop hook, Codex notify), or, after the message was typed in, stops on a prompt, question or error (`reply: null` and a `note`), or exits. If the agent goes idle after it with no attributed final message (3 s grace), `reply` is null and `unattributed_output` holds its last 60 terminal lines, with a `note` that they may belong to another prompt. The agent's `reply` to this message while the ask waits ends it with that text as `reply`. On timeout a `note` says a later answer comes as a new message (or, sent with wake=false, at the start of the next turn). See "Agents" below. |
| `tell_agent` | `agent`, `text`, `wake` (required) | A message nobody waits for an answer to. `wake: true`: delivered now as `ask_agent`'s message is (the agent starts or wakes if needed), without waiting for the turn: `{agent, msg_id, wake, status: "delivered"\|"queued", session_id, woke, started, origin, hop, sent_at_ms, note}`. `wake: false`: kept for the agent's next turn that can take it (`{agent, msg_id, wake, status: "waiting", waiting, origin, hop, sent_at_ms, note}`); see "Quiet messages" below. Same checks as `ask_agent` either way. |
| `reply` | `msg_id`, `text`, `wake? = true` | `{msg_id, status, delivered, note, delivered_as, session_id, origin, hop}`. Answers a message delivered to the caller's agent after the turn that received it; see "Late replies" below. A waiting ask returns it only when it brings no origin the question's chain didn't have and is at most one hop past the question; otherwise it goes as a new message. A schedule's run whose turn left no final message takes it as its reply under the same rule (`status: "returned"`). With `wake: false`, unless a waiting ask takes it, the asker gets it at the start of its next turn that can take it instead of a new turn (refused, nothing stored, when the reply is rooted in two kinds of unattended work, or on `lead` work when an existing agent or a live session asked: send it with `wake: true`). Errors (nothing stored, no rate-limit slot used): the caller isn't a named agent, the message wasn't delivered to it (unknown, another agent's, refused, still queued), it already has a reply, empty or over 100 000 chars, starts with `!` (unless shell commands are allowed), or, with `wake: false`, over 6,000 characters (an emoji such as 😀 counts as two) or 150 lines. |
| `start_fresh` | `file` (absolute, or relative to the caller's working directory; at most 4096 bytes) | `{status: "waiting", file, note}`. A named Claude agent the human promoted asks for its own fresh start; see "Self handoff" below. Only ever for the caller itself (no target argument). Refused (and recorded in its tab's timeline) when the caller isn't a named agent, isn't promoted (the error says how the human grants it), is a Codex agent (not supported yet), already has a request waiting, started fresh less than 30 minutes ago, or the file doesn't exist, isn't a regular file, or lies outside its home folder and working directory once symlinks are resolved. Counts toward the per-minute post rate limit. |

`whoami` also returns `agent` (the caller's agent name, or null) and `origin: {origin, hop}`.

For `spawn_session` with `kind: "antigravity"`, the extra arguments are `--model`,
`--effort`, `--continue` (or `-c`) and `--conversation`. For example, pass
`"model": "claude-opus-5-5", "effort": "high"` (agy resolves it to `claude-opus-5-5-high`, and
refuses a level the model lacks). `agent.models` lists the models with their levels. The channel
supplies the execution mode.

### Links

When an agent mentions a note, panel, tab or agent to the human, it writes the title as a
Markdown link, never the bare number: `[Pairs to review](muster://note/91)`. In Muster's window
a click on it shows that note (docs/UI.md "Links"). Results that return one carry it ready-made:

- `link`: the address, `muster://note/<id>`, `muster://panel/<id>`, `muster://tab/<session id>` or
  `muster://agent/<name>`. It holds nothing else (no token, nothing private).
- `mention`: `[title](link)`, with the title on one line and its Markdown characters escaped.

They come with every session in `whoami`, `list_sessions` and `spawn_session`, `wait_for_state`, every agent in
`list_agents`, `panel_open` and every panel in `list_panels`, and every note result (`note_list`,
`note_read`, `note_write`, `note_append`, `note_open`). `muster link note|panel|tab|agent
<target>` prints one from the shell (docs/CLI.md). The server's `instructions` and every named
agent's CLAUDE.md or AGENTS.md ("Links", added to an older agent's file at its next start) tell
agents to use them. The link works in Muster's terminal, not inside panels or notes.

### Panels

Panels always belong to the caller's **task**, even when cross-workspace access is enabled.
Only `get_diff` is app-visible (`_meta.ui.visibility: ["app", "model"]`); the other tools
are model-only. The bridge's `get_diff` accepts only `max_bytes`, using the panel's task.

| Tool | Arguments | Result |
|---|---|---|
| `panel_open` | `title`, `app` (`markdown`, `image`, `chart`, `table`, `pr_decision`, `pr_overview`, `board`, `web_preview` or `custom`), `data?`, `html?`, `html_path?`, `csp?`, `kit?` (custom only, see [The kit](#the-kit-kit-true)), `notify_on_close?` (see below) | `{panel_id, link, mention, note?}`; `note` only for a `web_preview` with a problem |
| `panel_update` | `panel_id`, `data?` (replaces the whole input) or `set?` (edits to it, see [Changing part of the data](#changing-part-of-the-data-set)), `html?`, `html_path?`, `title?`, `kit?` (custom only; switching it reloads the app) | `{note?, added?}`; advances the revision |
| `panel_close` | `panel_id` | `{}` |
| `list_panels` | `{}` | `{panels: [...]}`; open panels in this task, with their latest `context`, each with `link` and `mention` |
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

### Changing part of the data (`set`)

`panel_update {panel_id, set: [{path, value}, ...]}` changes part of the stored data without
sending all of it, so marking one card of a long `pr_overview` merged is one short call:

```json
{"panel_id": 7, "set": [{"path": "prs[number=226].status", "value": "merged"},
                        {"path": "prs[number=226].status_detail", "value": "Merged at 14:02"}]}
```

- A path is field names joined by `.`, each optionally followed by `[field=value]` (the one
  list item whose `field` is that string, number or boolean) or `[index]` (from 0). Pick items
  by a unique field such as a PR's `number`. A value may be quoted as a JSON string, and must
  be when it holds `]` or `"`: `rows[id="a]b"]`, `rows[id="say \"hi\""]`.
- `value` (any JSON, `null` included) replaces what the path names. Every step must exist,
  except a final field name, which may be new: the result then lists that path in `added`, so
  a misspelt field (which the app would ignore) does not pass unnoticed.
- 1 to 50 edits, applied in order, all or none; each path at most 300 bytes and 16 steps. `set`
  and `data` cannot be combined.
- The edits are made on the stored data and commit only if no other data update landed
  since; otherwise they are made again on the newer data (up to 8 times, then the call is
  refused with "try again"), so an update is never overwritten. The result is checked as a
  whole `data` would be, so `set` can never store what `data` would refuse. Panel data is
  nested at most about 120 levels deep, `set` or not.
- A refusal names the edit and its path: ``set[1] `prs[number=999].status`: no item of `prs`
  has number=999``, or ``2 items of `prs` have author=dana``.
- Not for `image`, `pr_decision` or `web_preview`: they store files read from the worktree or
  a checked address, so they take the whole `data`.
- The app receives the full new data, exactly as after a whole update.

A custom app is an MCP Apps (2026-01-26) page talking JSON-RPC to its parent frame. Until it
calls `ui/initialize` and then sends `ui/notifications/initialized`, the panel shows
"Loading"; after 10 s without it, the panel shows an error naming the stage that stalled, with
Reload. Input arrives as `ui/notifications/tool-input` (`params.arguments` is the panel's
`data`, again on every `panel_update`). A user action reaches the agent through `ui/message`,
accepted only straight after a click or key inside the panel's content (checked by the sandbox
proxy), at most 3 per user action (a 4th within about 5.5 s gets JSON-RPC error -32000), so
send one message per deliberate action; `_meta.muster.kind` names it. With `kit: true`
Muster does all of this for the page (below, [The kit](#the-kit-kit-true)).
A minimal app without the kit:

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

Keys: the user can show any panel fullscreen (docs/UI.md, "Fullscreen"), and an Esc the app
leaves alone takes them back. An app that uses Esc itself (to close a dialog or clear a
selection) should call `preventDefault()` on that keydown; Esc typed in a field or with a
`<dialog>` open is never taken. Ctrl+= / Ctrl+- / Ctrl+0 and Ctrl+wheel never zoom the page
from inside a panel; the app still receives them (the built-in image app zooms its picture).

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
moved-to-another-task openers keep events queued.

Only a user action starts a panel prompt. A human closing a tab queues a `closed` event (agent
`panel_close` queues nothing), but on its own it does not prompt an idle opener: typed into an
agent that has been idle for hours, any prompt makes it read its whole conversation again with
its prompt cache expired, which costs real usage for news it rarely needs. The notice waits in
the queue: `wait_for_panel_event` returns it, and it comes along in the next panel prompt that a
real user action starts for that opener (it does not ride on other prompts, such as messages
from agents or the human). In that prompt user actions go first and notices take only the room
left, so a pile of notices never pushes a click out; an untyped prompt an older backend queued
with only notices goes back to the panel queue at startup or before typing. A closed panel drops
out of `list_panels`. Open a panel with
`notify_on_close: true` when closing it means something (a "pick one" panel where closing is the
answer "none"): then the `closed` event prompts the idle opener like a click. No built-in sets
it; closing a `pr_decision` or `pr_overview` panel is not a decision. The `overflow` notice
never starts a prompt either (it only accompanies real events). At startup, Muster forgets a
closed panel whose queue holds only such notices once it was closed more than 7 days ago.
Archiving the task, including when its last session closes or
moves out, removes panels, files and events.

#### The kit (`kit: true`)

A custom app opened with `kit: true` gets the Muster Kit in front of its HTML: the same
styles, components, chart renderer and fonts (Inter and JetBrains Mono, OFL) that the built-in
apps use, plus the handshake. Muster adds it when it serves the panel, so none of it counts
toward the 1 MiB, and it loads nothing from the network (the fonts are `data:` URIs; the panel
CSP allows `font-src data:` and nothing else changes: no `fetch`, no nested frames).
`panel_update {kit}` switches it on or off and reloads the app; it is refused for built-ins
(they already use it). Without `kit`, a custom app is exactly the agent's HTML, as before.

With the kit, do not call `ui/initialize` yourself: the kit does the handshake, follows the
host theme (`<html data-theme>`), and reports size changes. Everything else is optional:

```html
<main class="mk-page">
  <header class="mk-head">
    <div>
      <h1 class="mk-title">3 tests failed out of 1,216 <span class="mk-scope">billing-api · run 1482</span></h1>
      <p class="mk-takeaway">All three started with commit <code>9be04f1</code>.</p>
    </div>
    <button class="mk-btn primary" data-send="rerun_failed" data-busy="Starting…"><mk-icon name="refresh"></mk-icon>Re-run failed</button>
  </header>
  <div class="mk-grid" style="--min:300px">
    <section class="mk-card">
      <h2>Duration <span class="aside">last 20 runs</span></h2>
      <mk-stat label="This run" value="4m 12s" delta="−18 s" tone="good"></mk-stat>
      <mk-chart height="110"><script type="application/json">
        {"type": "line", "format": "s", "x": {"values": [1480, 1481, 1482]}, "series": [{"name": "s", "values": [266, 270, 252]}]}
      </script></mk-chart>
    </section>
  </div>
</main>
```

- Layout: `mk-page`, `mk-head`, `mk-title`, `mk-scope`, `mk-takeaway`, `mk-section` (with an
  `h2`), `mk-grid` (`--min` sets the narrowest column), `mk-wide` (the whole row), `mk-row`,
  `mk-stack`, `mk-card` (`tone-good|warn|bad`, `flat`), `mk-card-head`, `mk-split`, `mk-foot`,
  `mk-source`, `mk-note`.
- Tags: `<mk-stat label value unit delta tone note trend color>`, `<mk-chart height pin legend>`
  with the chart app's JSON inside, `<mk-spark values>`, `<mk-ring>` with
  `{segments, center, sub, label}` JSON, `<mk-meter label value max text of mark mark-label>`,
  `<mk-icon name>`. A tag with bad JSON shows its error in place; the rest still draws.
- Classes: `mk-stats`, `mk-badge` (`good warn bad info accent plain`), `mk-chip`, `mk-delta`,
  `mk-btn` (`primary danger ghost sm`), `mk-seg`, `mk-search`/`mk-input`, `mk-list`, `mk-icon`,
  `mk-avatar`, `mk-bar-cell`, `mk-steps` (`li[data-state=done|active|failed]`), `mk-timeline`,
  `mk-callout`, `mk-kv`, `mk-pre`, `mk-table`, `mk-empty`, `mk-error`, `mk-skel`.
- Buttons that talk to the agent: `data-send="kind"` plus any `data-*` sends the panel event
  `kind` with the other data attributes as its data (as strings; `mk.send` keeps JSON types),
  with no script. `data-confirm="Question?"` asks first (a second click or Enter sends; a key
  held down never confirms; Esc, leaving the button or 5 s cancels);
  `data-busy` and `data-sent` set what it says meanwhile; `data-text` is the sentence the agent
  reads. A sent button stays off until the next `panel_update` (or 30 s).
- Script: `mk.onData(fn)` (the panel's data now and on every update), `mk.send(kind, data,
  text?)` (from a click or key handler), `mk.chart(el, spec, opts)`, `mk.board(el, data)`,
  `mk.fmt(v, format)`, `mk.spark`, `mk.ring`, `mk.meter`, `mk.icon(name)`, `mk.icons()`,
  `mk.upgrade(root)` for tags added later.
- Colours: use the `--mk-*` tokens (`--mk-text`, `--mk-text-2`, `--mk-accent`, `--mk-good`,
  `--mk-c1`…`--mk-c8`, …), never fixed colours, so light and dark both work.

Source: `web/src/panels/kit/`, built into `web/panels/kit.css` and `kit.js`; examples in
`web/dev/panel_examples/`.

#### Built-in apps

Built-ins take only `data` (no `html`). Each follows the host theme and works by keyboard.
`panel_open` and `panel_update` refuse input that the app would refuse, naming the field:
every required field, type, enum, count and length limit below, with lengths counted in
characters (Unicode code points) the same way on both sides, so accepted input never turns
into an error screen in the panel. Unknown fields are refused in `image` input, in
`pr_decision` `media` items and in `board` input; in chart, table, pr_decision, pr_overview and `actions` they are
accepted and listed in the panel as warnings. Source: `web/src/panels/apps/`, built into `web/panels/<app>.html` by
`cd web && deno task panels`.

Every built-in draws with the Muster Kit (the "Briefing" look: the finding first, then the
evidence): the title with its scope after it, the agent's one-sentence takeaway, headline
numbers, the evidence, and a source line. They share the kit's colour tokens (text at 4.5:1 or
more on every surface in both themes, series colours at 3:1) and its two bundled fonts, which
Muster inlines when it serves the panel, so nothing loads from the network.

**`markdown`**: `{markdown}`. Set for reading (a 70-character measure): headings, paragraphs,
lists, `- [ ]`/`- [x]` checklists, `>` quotes (a soft callout), fenced code with its language
shown, tables (columns of numbers right-aligned), `---` rules, inline code, bold, italic and
http(s) links (opened in the system browser). The paragraph right under the `#` title reads
as a lead. A comment box under it sends the user's words to the agent as a `ui/message` event.

**`image`**: `{path?, url?, alt?, caption?}`, exactly one of `path` or `url`. `path` is a file
in the task worktree (the task cwd): Muster reads it when the panel opens or updates, refuses
`..` and symlink escapes, non-regular files and files over 8 MiB, accepts PNG, JPEG, GIF,
WebP and SVG (by content; SVG also needs the `.svg` extension), and stores it as a data URI
(`src`, `mime`, `bytes` are added by Muster; an agent may not pass them). SVG is shown with
`<img>`, so its scripts never run. `url` must be https without credentials; `panel_open` adds
its origin to `csp.resourceDomains`, and a later update must keep to an origin already
declared. The host must be a plain name or IP (no wildcards). Without `alt`, the caption or
"Image: <file name>" is the text alternative. The picture opens at Fit (shrunk to the panel,
never enlarged past actual size; an SVG with only a viewBox fills it); − and + (or the keys,
Ctrl+wheel and a pinch, around the pointer) zoom from the size on screen, 100% (1) is actual
pixels, 0 fits again, Z or a click switches Fit / 100% (twice the fitted size when
Fit already is 100%), and a picture bigger than the panel
scrolls and drags to pan.

```json
{"path": "shots/login.png", "alt": "Login page after the fix", "caption": "Chromium, 1280 px"}
```

**`chart`**: `{type, title?, x: {label?, values}, series: [{name, values, color?}], y?: {label?, min?, max?}}`,
plus the optional Briefing fields below. `type` is `bar`, `line`, `scatter` or `area`;
`x.values` are up to 2000 numbers or strings (numbers only for `scatter`; all-number x gives
`line`/`area` a linear axis); up to 12 series, each with one value per x value (a number, or
`null` for a gap). A chart with no values at all is valid: the panel says "No data to show"
with `empty` as the reason. Bars and areas include zero; `y.min`/`y.max` fix the axis (with
only one fixed, the other side follows the data, even if the fixed bound lies beyond it).
Series take Muster's eight series colours in order; past 8 series the first is highlighted and
the rest turn grey. Drawn by the kit as SVG (lines smoothed without overshooting, labelled at
their ends; value labels on bars when they fit; a missing bar shows "–") with an accessible
title and description, hover and arrow-key tooltips that name the point they compare with
("vs Thu"), a live region reading each point, legend toggles, `x.label` and `y.label` on the
axes, and a data table (visually hidden until "Data table"). It redraws at its new width when the panel is resized.

| Optional field | |
|---|---|
| `takeaway` | the one-sentence finding under the title (at most 600 characters; `**bold**` and `` `code` `` render) |
| `scope`, `source` | what the chart covers, after the title in a muted tone, and a footer line ("gh run list · 412 runs") |
| `format`, `xFormat` | `int`, `pct` (values are already percents: 0.34 means 0.34 %; tooltip changes show in points), `usd`, `ms`, `s` (clock ticks: 30 s, 1 m, 5 m) or `plain`; for the axis, labels and tooltip. Numbers compact to k, M, B and T with a true minus sign; whole-number x (run numbers, years) prints without separators by default |
| `highlight` | a series name: the others turn grey |
| `annotations` | up to 20 `{x, label}`; `x` must be one of `x.values`. A dashed marker with a label, or a ringed, named point on a scatter |
| `stacked` | `true` for `bar` and `area` only; negative values stack below zero |
| `labels` | `auto` (default), `values`, `end` or `none` |
| `trend` | `true` for `scatter` only: a least-squares line |
| `empty` | what to say when there is nothing to draw (at most 200 characters) |
| `stats` | up to 4 headline numbers above the chart, `{label, value, unit?, delta?, tone?: good\|bad\|flat, note?, trend?, color?}` (the Board's stats items). When they key every series (a stat whose `label` is the series name and whose `color` is its colour) they replace the legend; otherwise the legend stays |
| `pin` | the index of an x value whose tooltip shows while the pointer is away |
| `series[].color` | a kit colour token (`var(--mk-add)`, `var(--mk-c2)`) or a hex colour |

```json
{"type": "bar", "title": "CI minutes per day", "scope": "acme-store · week of Oct 13", "format": "int",
 "takeaway": "PR builds used **1.75×** the minutes of master builds.", "source": "gh run list · 412 runs",
 "x": {"label": "Day", "values": ["Mon", "Tue", "Wed"]},
 "series": [{"name": "master", "values": [42, 55, 38]}, {"name": "PRs", "values": [80, 96, null]}],
 "y": {"label": "Minutes"},
 "stats": [{"label": "PRs", "value": "503", "unit": "min", "delta": "+8%", "tone": "bad", "note": "vs last week", "color": "var(--mk-c2)"}]}
```

**`table`**: `{title?, scope?, takeaway?, source?, columns: [{key, label?, align?, format?, tones?}], rows: [{<key>: value}], sort?: {key, dir?}}`.
1–50 columns (`align`: `left`, `right` or `center`), up to 5000 rows whose values are
strings, numbers, booleans or null. Columns holding only numbers are right-aligned in
tabular mono figures. A column's `format` draws its cells as `bar` (the number with an inline
bar against the column's largest), `badge` (a tinted badge; `tones` maps a cell value to
`good`, `warn`, `bad`, `info`, `accent` or `plain`, at most 50 values) or `mono` (paths, ids).
Header click sorts (text naturally, numbers numerically, empty cells last), the filter box
(`/`) matches every word in any column (a filter that matches nothing says so, with Clear
filter), the header stays visible on scroll, and the user can switch Comfortable or Compact
rows. `sort.dir` is `asc` (default) or `desc`.

```json
{"title": "Flaky tests", "scope": "last 200 CI runs", "takeaway": "**search ranks by score** failed 1 run in 9.",
 "columns": [{"key": "test", "label": "Test"}, {"key": "failed", "label": "Failed", "format": "bar"},
   {"key": "status", "label": "Status", "format": "badge", "tones": {"Flaky": "warn", "Fixed": "good"}}],
 "rows": [{"test": "search ranks by score", "failed": 23, "status": "Flaky"}, {"test": "coupon and tax", "failed": 1, "status": "Fixed"}],
 "sort": {"key": "failed", "dir": "desc"}}
```

**`board`**: a dashboard from plain JSON. No HTML and no script come from the agent, so it is
the quickest and safest kind of panel for status pages and summaries. Muster refuses unknown
fields and block kinds here, naming them.

```json
{"title": "$4,480 spent so far this month", "scope": "acme cloud · October",
 "takeaway": "On track to finish about 6% under budget. Postgres grew the most.",
 "blocks": [
   {"kind": "stats", "span": "full", "items": [
     {"label": "Month to date", "value": "$4,480", "delta": "+4.1%", "tone": "bad", "note": "vs September"},
     {"label": "Per day", "value": "$224", "trend": [205, 212, 219, 216, 224]}]},
   {"kind": "chart", "span": "full", "title": "Spend per week", "spec": {"type": "bar", "stacked": true, "format": "usd",
     "x": {"values": ["Oct 6", "Oct 13", "Oct 20"]},
     "series": [{"name": "Postgres", "values": [651, 669, 681]}, {"name": "Compute", "values": [543, 539, 549]}]}},
   {"kind": "list", "title": "By service", "items": [{"title": "Postgres", "sub": "db-primary + 2 replicas", "bar": 0.43, "value": "$1,906"}]},
   {"kind": "meters", "title": "Budgets", "note": "The tick is where spend should be today.",
    "items": [{"label": "Whole account", "value": 4480, "max": 7400, "text": "$4,480", "of": "of $7,400", "mark": 4774}]},
   {"kind": "callout", "tone": "warn", "title": "Postgres is ahead of pace.", "text": "It ends the month about $50 over budget."},
   {"kind": "actions", "items": [{"id": "explain", "label": "Why did Postgres grow?", "primary": true, "icon": "sparkle"},
     {"id": "pause_replica", "label": "Pause replica 2", "danger": true, "confirm": "Pause db-replica-2 until Monday?", "data": {"replica": "db-replica-2"}}]}],
 "source": "billing export, 1–20 Oct"}
```

| Field | |
|---|---|
| `title`, `scope?`, `takeaway?`, `source?` | the title (required), what it covers, the one-sentence finding (`**bold**` renders), and a footer line |
| `min?` | narrowest column in px (160–1200, default 300) before the grid drops a column |
| `blocks` | 1–40 blocks, each `{kind, span?: "full", title?, aside?, …}`; `span: "full"` takes the whole row, otherwise one column. `kind` is one of: |
| `stats` | `items`: 1–8 `{label, value, unit?, delta?, tone?: good\|bad\|flat, note?, trend?: number[], color?}`. `tone` says whether the change is good news; a minus sign alone says nothing about that |
| `chart` | `spec`: exactly the chart app's input; `height?` 80–800 |
| `list` | `items`: 1–100 `{title, sub?, icon?, tone?: good\|warn\|bad\|info\|accent, badge?, bar?: 0–1, value?, color?}` |
| `meters` | `note?`; `items`: 1–20 `{label, value, max, text?, of?, mark?, markLabel?, tone?, color?}`. `mark` is a tick (a pace line, a limit); say what it means in `note` |
| `callout` | `text`, `tone?: good\|warn\|bad\|info`, `icon?` |
| `actions` | `items`: 1–8 `{id, label, primary?, danger?, confirm?, icon?, key?, data?}`. A click sends the panel event `kind: id` (letters, digits, `_`, `-`, `/`; not `closed`, `overflow` or `navigated`; unique) with `data` (at most 10 lowercase keys with string, number or boolean values) in `_meta.muster.data`. `data` arrives with its JSON types, and the event's text is `Pressed "<label>"`; an action whose event would pass the 16 KiB event limit is refused. `confirm` makes it two-step: the first click turns the button into that question, the second click or Enter sends (a key held down never confirms). A sent button stays off until the agent's next `panel_update`. `key` is one letter or digit that presses it. `primary` with `danger` draws a filled red button |

Icons (`icon`) are names from the kit's set (`check`, `alert`, `info`, `clock`, `refresh`,
`rocket`, `undo`, `sparkle`, `db`, `server`, `dollar`, `chart`, …; an unknown name draws "?"
and the panel lists the valid ones). Colours are kit tokens (`var(--mk-c1)` … `var(--mk-c8)`,
`var(--mk-good)`, `var(--mk-warn)`, `var(--mk-bad)`, `var(--mk-accent)`) or hex colours.

**`web_preview`** ([AGENT_PANELS.md](AGENT_PANELS.md#showing-web-pages)): `{url}`, the agent's
running local dev server, live beside its terminal. Only loopback http(s) is accepted:
`localhost` or `127.0.0.1`, any port except Muster's own (its window and its panel server).
Everything else is refused with an error that says to open the page in the user's
system browser instead, as are credentials (`user@`), other spellings of 127.0.0.1
(`127.1`, `2130706433`, `0177.0.0.1`), `0.0.0.0`, IPv6 literals such as `[::1]` (a browser
cannot hold a frame to an IPv6 address; `localhost` reaches a server listening on ::1 too), spaces, control and non-ASCII characters, and
backslashes; the URL is stored rebuilt from its checked parts (`http://LOCALHOST:5173` becomes
`http://localhost:5173/`). The panel shows the address, Reload and Open in browser (the system
browser in the native window), and frames the page through the panel server's `/preview` page
(docs/PROTOCOL.md), which holds the frame to the URL's origin: the page may navigate and
redirect within it, but a link or redirect to another origin (another port, a login site) is
blocked in the frame, and Open in browser is the way out. A popup or `target=_blank` link opens
in the system browser (native window) or a new tab. The native Linux window passes the desktop's
activation token so the browser can come forward if the desktop allows it (`misc:focus_on_activate`
on Hyprland). Every `panel_update` with data reloads the
page, even at the same URL; a title-only update does not (panels carry `data_revision`, the
revision of the last update with data).

The address in the bar is editable: the user types a local address and presses Enter (Escape
restores the current one). Muster completes what a person types (a bare port `3000` becomes
`http://localhost:3000/`, `localhost:3000/path` gets `http://`), then checks it with the same
rule as `panel_update`; a refused address shows the reason under the bar and keeps the text.
An accepted one is stored like an update with data (every window and a restart show it), and
the agent gets a `navigated` event, `{url, previous}` (both as stored), through
`wait_for_panel_event` or the idle panel prompt. The address and its event commit together, in
order, for every change: the apps' 30-events-a-minute limit does not apply to them (the
200-event queue bound does). Pressing Enter on the current address only
reloads and sends nothing. Agents still pass full URLs; only the bar completes short forms.

Muster checks the server when the panel opens or changes, and the result's `note` tells the
agent in one sentence when nothing answers, when the page refuses framing
(`X-Frame-Options`, CSP `frame-ancestors`), when it redirects to another origin, or when it sent
no answer in 3 s. The panel shows the same as a notice with Reload and Open in browser; while
nothing answers it checks every 2 s for 2 minutes and loads the page once the server is up. A
page still loading after 10 s gets a hint. These checks are best effort: they read the first
response head only (https servers are only connected to), and a page can still fail in the
browser in ways Muster cannot see (a self-signed certificate, a script error). Muster's
shortcuts do not work while focus is inside the page (it belongs to the dev server); click the
bar or another tab first. Third-party cookie rules may keep logins from sticking in the frame.

```json
{"url": "http://localhost:5173"}
```

**`pr_decision`** ([AGENT_PANELS.md](AGENT_PANELS.md#pr-decision-end-to-end)):

| Field | |
|---|---|
| `repo`, `number`, `title`, `author`, `url` | `owner/name`, PR number, https PR URL |
| `base`, `head`, `head_sha` | branches, and the full 40-hex head commit (`headRefOid`) |
| `merge_method_default?`, `delete_branch_default?` | `squash` (default), `merge` or `rebase`; `false` |
| `summary` | what changed and why, plain text |
| `verdict?`, `undo?` | one line each (at most 300 characters, no line breaks): the agent's call in one plain sentence ("Safe to merge: small bug fix, tests pass"), shown first and prominently, and how to back it out ("If this goes wrong: revert the merge commit"), shown under it |
| `risk` | `{level: low\|medium\|high, reasons?: [string]}` |
| `checks` | `{state: passing\|failing\|pending\|none, items?: [{name, state: pass\|fail\|pending\|skipping\|cancel, url?}]}`, the checks of `head_sha`. With items, the badge comes from them (any `fail`: failing; else any `pending`: pending; else passing) and a contradicting `state` is a warning |
| `reviews?` | `{decision?: "APPROVED"…, items?: [{author, state}]}` |
| `changes` | `[{file, note, diff}]`: only the changes worth a look, each a unified diff (with or without the `diff --git` header; renames, mode changes and binary files show as a line) and the agent's note |
| `rest_note?` | a line about what is not shown |
| `questions?` | up to 20 questions the user answers in place (below); a plain string is the old form, read as a `text` question with id `q1`, `q2`… by its place |
| `answer?` | `{question, text}`: the agent's answer to an Ask, shown inline |
| `status?`, `status_detail?` | `open` (default), `merging`, `queued` (in a merge queue or set to auto-merge), `merged`, `failed` or `requested_changes`, shown as a banner and beside the actions; with `open`, a `status_detail` shows as a note (e.g. what the agent did for a custom action) |
| `media?` | up to 8 pictures and charts, shown after the risk, checks and reviews and before the diffs (below) |
| `actions?` | the user's own actions ([below](#the-users-own-actions)) |

Questions are `{id, text, why?, kind, options?, recommended?, required?}`: `id` is lowercase
letters, digits, `-` or `_` (at most 40, unique); `text` at most 500 characters and `why` (one
line on what depends on the answer) at most 300; `kind` is `yes_no`, `choice` or `text`. A
`choice` has `options: [{label, value}]`, 2 to 9, labels at most 80 and unique values at most
100 characters; `yes_no` has its own Yes and No (values `yes`, `no`) and `text` takes no
options. `recommended` is one of the values (for `text`, a suggested answer of at most 1000
characters); it starts picked and is marked "Recommended". `required: true` makes Merge wait
until the question is answered (M then says which one and focuses it). They show in "The agent
asks", after the risk, checks and reviews and before the pictures and diffs: buttons for the
options (number keys 1–9 pick in the focused question, arrows step), a short text box for
`text` (at most 1000 characters), and Tab or Shift+Tab (Enter in a text box) moves between
questions. A button under the header ("Answer 2 questions", or "Review 3 answers" once all
have one) and the key G jump to the first unanswered question, so the questions are one step
away in a narrow panel too; the "n of m answered" count in the bar does the same. Required
questions are marked "Needed to merge" and carry `aria-required`. Answers stay a local draft,
kept across updates by id, until an event carries them.

The user's draft is the answers plus the toggle actions they switched on ([below](#the-users-own-actions)).
A line above the actions says what goes with the next decision. **Merge**, **Request
changes**, **Hold** and **Skip** carry the whole draft (`answers`, `options`) in their one
event; **Send answers** (Ctrl+Enter; labelled "Send options" or "Send answers and options" by
what it carries) sends it alone while the user is not deciding yet, and is off while a Merge
confirm is open (the draft goes with that merge). Each send is one message. Answers stay
after a send (the button then reads "Answers sent ✓" until one changes); options are one-shot
and switch off once sent. If the host refuses a send, the panel puts back only what that send
took (its options, the "sent" mark, a typed comment, the open confirm), and only if nothing
happened since: no update from the agent, no other send and no edit; otherwise it says so and
leaves the user's newer state alone. An option counts only while its action is still the one
the user switched on (and, if outward, confirmed). An event that would exceed the 16
KiB limit with the typed answers is not sent ("shorten your typed answers"). The draft is off
once the PR is `merged`, `queued` or `requested_changes`.

Actions: **Merge** (M) with a method select and a delete-branch box shows the exact command
`gh pr merge <number> --repo <repo> --<method> [--delete-branch] --match-head-commit <head_sha>`,
then everything that goes with it (each answer and each option with what the agent will do),
and sends only after Enter or "Confirm merge". The confirm arms only once the key that opened
it is released (a held or repeated key never confirms), and any update that changes `repo`,
`number`, `base`, `head_sha`, the checks state or `merge_method_default`, any change to the
questions (a new required one, a changed kind, option or text), to the answers or options or
to an option's action while it is open, or any invalid update, cancels it. Merge also checks
right before sending that every required question has an answer. The confirm shows each
question and answer in full. Merge is disabled while checks fail unless Shift is held (Shift+M; a hint
says so beside it), and while the agent works (`merging`, `queued`, `merged`, or until the next
update after a sent decision). **Request changes** (R) and **Ask** (A) open a text box
(Ctrl+Enter sends); text that would make the event exceed the 16 KiB event limit cannot be
sent, and both are off once the PR is `merged`, `queued` or `requested_changes`. **Open on
GitHub** (O, beside the title) uses the host browser. **Skip** (S) moves on without changing
the PR; **Hold** (H) puts it aside for later (off once it is `merged` or `queued`). Each sends
one event, and both are off until the agent's next update. The user's own actions follow in
their own group. `?` lists every key (the key hints are hidden in narrow panels). The sticky
action bar takes at most about a third of the panel (it scrolls if it needs more), except
while a confirm or text box is open.

`media` items are `{kind: "image", path, alt?, caption?}` or `{kind: "chart", spec,
caption?}`. A picture is a file in the task worktree (PNG, JPEG, GIF, WebP or SVG), read by
Muster like the `image` app's `path` into a data URI; media takes no URLs (the agent downloads
a picture first). At most 8 items and 6 MiB of image files in all, checked before each file is
read; the agent's own JSON keeps the 1 MiB limit. A chart's `spec` is a `chart` app input,
drawn by the same code. Pictures show as thumbnails (one alone shows larger); a click or Enter
enlarges one in a modal view: everything behind it is inert, Tab stays inside, an open Merge
or outward confirm is closed first, and an update that replaces the picture closes the view.
It zooms like the image app (Fit, − and +, 100%, Z, Ctrl+wheel; scroll or drag to pan), arrows step through the
pictures, Esc or Close returns to the thumbnail.

Events are sent with `ui/message`; the decision is in the event's `kind`, and its fields in
`payload._meta.muster.data` (`payload.content.text` repeats it for people):

| `kind` | `payload._meta.muster.data` |
|---|---|
| `merge` | `{action: "merge", repo, number, base, method, delete_branch, head_sha, answers, options}` |
| `request_changes` | `{action: "request_changes", repo, number, head_sha, body, answers, options}` |
| `answers` | `{action: "answers", repo, number, head_sha, answers, options}`: Send answers, nothing decided |
| `ask` | `{action: "ask", repo, number, question}` |
| `skip` | `{action: "skip", repo, number, head_sha, answers, options}` |
| `hold` | `{action: "hold", repo, number, head_sha, answers, options}` |
| `custom` | `{action: "custom", id, repo, number, head_sha}`, one of the user's own `now` actions |

`answers` is `{<question id>: <value>}` for every answered question: `"yes"` or `"no"`, a
choice's `value`, or the typed text (trimmed); the recommended answer counts until the user
changes it. `options` lists the ids of the toggle actions switched on, in the panel's order.
Both are always present (`{}` and `[]` when empty), and `payload.content.text` repeats them for
people (`Answers: [limit] No; …. Also do, for this PR: needs-qa (…).`; the merge text still
ends with the exact command).

`--match-head-commit` binds the merge to the head only; the agent must also check that the
PR's base and repo still match the event before running the command (the PR reviewer flow
does).

```json
{"repo": "acme/api", "number": 812, "title": "Rate-limit failed logins per IP", "author": "dana",
 "url": "https://github.com/acme/api/pull/812", "base": "master", "head": "dana/login-limit",
 "head_sha": "3f9c2b7e41d0a8c6e5f4b3a2918d7c6b5a493827", "merge_method_default": "squash",
 "delete_branch_default": true, "summary": "Adds a per-IP limit to POST /login.",
 "verdict": "Worth a careful look: it changes how login works, but the tests pass.",
 "undo": "If this goes wrong: revert the merge commit; no data needs fixing.",
 "risk": {"level": "medium", "reasons": ["Changes the login path"]},
 "checks": {"state": "passing", "items": [{"name": "test", "state": "pass"}]},
 "changes": [{"file": "src/auth/login.rs", "note": "Checked before the password hash.",
   "diff": "@@ -41,3 +41,6 @@\n     let ip = client_ip(&req);\n+    if let Some(wait) = app.limiter.blocked(ip).await? {\n+        return too_many(wait);\n+    }"}],
 "rest_note": "64 more lines of tests.",
 "questions": [{"id": "limit", "text": "Keep the limit at 5 failed logins per 10 minutes?",
   "why": "Support asked for 10 per hour last year; 5 is stricter.", "kind": "yes_no", "recommended": "yes"},
  {"id": "proxy", "text": "Which addresses should count as one visitor?", "kind": "choice",
   "options": [{"label": "Trust only our own proxies", "value": "ours"}, {"label": "Ignore proxies", "value": "none"}],
   "recommended": "ours"},
  {"id": "note", "text": "Anything to tell the author?", "kind": "text"}]}
```

The merge event that panel sends after the user picked No, kept the second recommendation,
typed a note and switched on Needs QA:

```json
{"action": "merge", "repo": "acme/api", "number": 812, "base": "master", "method": "squash", "delete_branch": true,
 "head_sha": "3f9c2b7e41d0a8c6e5f4b3a2918d7c6b5a493827",
 "answers": {"limit": "no", "proxy": "ours", "note": "Thanks, nice work"}, "options": ["needs-qa"]}
```

With pictures and a chart (the paths are files in the task worktree):

```json
{"media": [{"kind": "image", "path": ".muster-pr-k3Xq9TzA/1.img", "alt": "Login page after 5 failed attempts", "caption": "From the PR description"},
  {"kind": "chart", "caption": "Lines changed per area", "spec": {"type": "bar", "x": {"values": ["src/auth", "tests"]},
    "series": [{"name": "added", "values": [124, 64]}, {"name": "removed", "values": [9, 0]}]}}]}
```

**`pr_overview`**: every open PR of one repo as a card, for going through them with the agent.

| Field | |
|---|---|
| `repo` | `owner/name` |
| `title?` | heading (default "Open pull requests") |
| `takeaway?` | the agent's one-sentence finding under the heading (at most 600 characters; `**bold**` renders) |
| `prs` | up to 200 `{number, title, author, url?, created_at?, draft?, additions?, deletions?, files?, checks?, review?, labels?, risk?, summary?, status?, status_detail?, head_sha?, base?, quick_win?, verdict?, merge_method_default?, delete_branch_default?}`; numbers unique. `muster pr list` prints its PRs in this shape |
| `prs[].checks?` | `passing`, `failing`, `pending` or `none` |
| `prs[].review?` | gh's `reviewDecision`: `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, `NONE` or `""` |
| `prs[].risk?`, `prs[].summary?` | the agent's triage: `low`, `medium` or `high`, and one line (at most 500 characters) |
| `prs[].status?` | the agent keeps it up to date: `new` (default), `ready` (the agent has prepared its review; shown as "Review ready", whatever its verdict), `reviewing`, `merging`, `merged`, `queued`, `held`, `skipped`, `changes_requested` or `failed`; `status_detail` is a short note under the card |
| `prs[].quick_win?`, `prs[].verdict?` | `quick_win: true` only after the agent's full one-snapshot review found it small, green and low risk; such a card needs `head_sha` and `base` (from that snapshot), a `verdict` (one line, at most 300 characters), checks `passing` and not `draft`, and can be merged from the overview. `verdict` shows on any card |
| `prs[].base?`, `prs[].merge_method_default?`, `prs[].delete_branch_default?` | the branch it merges into, and the quick-win Merge's method (`squash` by default) and delete-branch flag (`false`) |
| `current?` | the PR the agent is on (marked "Now"); the selection follows it, unless the user is on a card, whose selection then stays |
| `sort?` | the starting order: `risk` (riskiest first, default), `quick` (quick wins first: cards marked `quick_win`, then not a draft, checks passing, low risk, small) or `oldest` |
| `actions?` | the user's own actions ([below](#the-users-own-actions)) |

Cards show number, title, author, age, size (+/− lines, files), a draft flag, the checks,
review and risk badges, labels, the summary and the status. A select changes the sort and a
filter box (`/`, Esc clears) matches every word in the number, title, author, labels, summary,
status, verdict or badges ("failing", "approved", "review required", "quick win"); a "Quick
wins (n)" button, shown when there are any, keeps only the quick wins. PRs still to review come first,
then decided ones (merged, queued, changes requested), then held ones; held and decided cards
use a quieter surface with full-contrast text. Keys: J/K or ↓/↑ move, Enter or O opens the
focused PR, M merges it if it is a quick win, H holds it, N goes through them in order, `?`
lists the keys; a double-click opens. Keys always act on the card that has focus. Readable
from about 400 px wide.

**Merge** on a quick win (M, or the "Merge #n" button for the selected card) works like
pr_decision's: it shows the exact command for the card's repo, number, `merge_method_default`,
`delete_branch_default` and `head_sha` with its verdict, and sends only after Enter or "Confirm
merge", armed once the opening key is released. Any change to that card (or the repo) cancels
the confirm. It is off unless the card is a quick win with checks passing, not a draft, and
`new`, `ready`, `skipped`, `held` or `failed`, and until the agent's next update after a send.

| `kind` | `payload._meta.muster.data` |
|---|---|
| `open` | `{action: "open", repo, number}` |
| `hold` | `{action: "hold", repo, number}` |
| `next` | `{action: "next", repo, number, order}`: review `number` now, then `order`, the PRs after it that are still new, ready or skipped, in the order the user sees (starting after `current`; it never repeats `number`) |
| `merge` | `{action: "merge", repo, number, base, method, delete_branch, head_sha, answers: {}, options: []}`, pr_decision's shape, for a quick-win card (the text ends with the exact command) |
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

Both PR apps take `actions: [{id, label, key?, description, outward, mode?}]`, at most 8:
buttons for options the user asked the agent for ("a Needs QA button"). `id` is lowercase letters,
digits, `-` or `_` (at most 40), unique and not a built-in event name; `label` at most 40
characters and not a built-in button's name (Merge, Request changes, Ask, Open, Open on
GitHub, Skip, Hold, Next, Confirm, Cancel, Send answers; compared trimmed and case-insensitive);
`description` (at most 300) says exactly what the agent will do and is shown as the button's
tooltip and in its confirm; `key` is one letter or digit, unique, and not one the apps use (M R
A O S H J K N G; Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain key
never clashes). `outward` is required: `true` for an action that changes GitHub or anything
off this machine (comments, labels, closing, pushing), `false` only for one that stays on it
(should an app ever get one without it, it treats it as `true`). An outward button (marked ↗)
first shows the description and needs Enter or "Confirm", armed only after the opening key is
released; any change to the action or the PR (its head, base or status) cancels it. The
actions show as their own group, "Your actions", apart from the built-in buttons; in a small
panel the group folds behind one button, and their keys work either way.

`mode` is `toggle` (default) or `now`. In pr_decision a toggle is an option: its button (or
key) switches it on or off (✓ when on), an outward one asks to confirm before it is switched
on (saying it goes with the next send), and the ids of those switched on ride along as
`options` with the next Merge, Request changes, Hold, Skip or Send answers, then switch off.
A toggle switched on is switched off again if the agent changes that action. A `now` action
sends one `custom` event at once and is off until the agent's next update. In pr_overview
every action sends a `custom` event at once for the selected PR, whatever its mode. A custom
action never merges: the agent merges only on a `merge` event.

```json
{"actions": [{"id": "needs-qa", "label": "Needs QA", "key": "q", "description": "Add the needs-qa label to this PR on GitHub.", "outward": true},
  {"id": "later", "label": "Label later", "key": "l", "description": "Note this PR in your list to label later; nothing changes on GitHub.", "outward": false},
  {"id": "ping", "label": "Ping the author", "description": "Post a comment asking the author for an update.", "outward": true, "mode": "now"}]}
```

The **PR reviewer** agent template (New agent) and the `muster-pr-review` skill (`muster skills
install`, docs/CLI.md) share the agent side: review one immutable
snapshot (`muster pr snapshot`, or the gh recipe on an older Muster: head and base SHAs; diff
and checks of that head), write everything in plain English with a verdict, an undo line and
questions the user can answer in place, open this panel, wait for its events, re-check head
and base before merging, merge only on a Merge event (from this panel, or from a quick-win
card), and report `merged`, `queued` or `failed` from what GitHub says afterwards. They act
on the `options` an event carries and take its `answers` in, never letting either change a
merge. For "help me review the open PRs" they list the open PRs with `muster pr list` (or `gh
pr list`), triage each briefly, open a `pr_overview`, and run that flow for each PR the user
opens (or for each in turn on Next), updating its card after each decision. While the user
reads, they prepare the next PRs (cards marked `ready`, small green ones as quick wins), and
for 6 or more PRs they may start helper sessions (a faster model) that only write draft
judgements to files; the agent treats those as untrusted, takes every fact from a snapshot
it runs itself, and never marks a quick win from a helper's work. They download pictures only from
GitHub's own image hosts (`muster pr snapshot` does this itself), with no PR text ever part of
a command, into a fresh private directory.

### Notes

Plain-text notes the human and every agent share; the human sees each as an editor tab
(docs/UI.md "Notes"). Unlike panels they are not task-scoped: every session sees every note.
Wire format and limits: docs/PROTOCOL.md "Notes".

| Tool | Arguments | Result |
|---|---|---|
| `note_list` | — | `{notes: [{id, title, rev, version, bytes, lines, preview, created_at, updated_at, updated_by, open, window_id, beside, link, mention}]}`, no text (`preview` is the first non-blank line). |
| `note_read` | `note_id` | `{note_id, title, rev, text, updated_at, updated_by, open, link, mention}`. |
| `note_write` | `text`, `title?`, `note_id?`, `rev?` | `{note_id, title, rev, link, mention}`. Without `note_id`: a new note (not shown until `note_open`). With it: replaces the whole text (and the title, if given); `rev` is required and must be the current one, else the write is refused (`changed since revision …`): read again, redo the change, write with the new rev. |
| `note_append` | `note_id`, `text` | `{note_id, title, rev, link, mention}`. Adds the text at the end, on a new line. No rev, never conflicts: use it while the human may be typing. |
| `note_open` | `note_id` | `{note_id, title, window_id, link, mention}`. Shows the note as a tab beside the caller's terminal (in its window, moving it from another one). Refused for a hidden agent tab. |

Each write and append records the caller as `updated_by` (`{kind: "agent", name, session_id}`;
the name is a named agent's, else `Claude (task name)`), which the editor shows as "edited by …".
`note_write` and `note_append` share the post rate limit (30 per minute with posts and panel
updates), also when an agent runs `muster note` from its session (those calls run as these tools). There is no delete tool: only the human deletes notes. The human's unsaved typing is
never lost to an agent's write: the editor merges it onto the newer revision (docs/PROTOCOL.md
"Notes"). The same calls from the shell: `muster note …` (docs/CLI.md).

### Agents

An Antigravity agent answers an `ask_agent` message with the `reply` tool and the
message ID, called through agy's `call_mcp_tool`. Its status callback carries no final message. An ask without an explicit
reply can return the terminal tail as `unattributed_output`. A schedule's run takes such a `reply` as its reply the same
way (docs/PROTOCOL.md, schedules).

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
| UserPromptSubmit with an envelope | Counts only for an unanswered message muster delivered to that session; unknown, foreign or answered ids are ignored. The envelope is the prompt's first line, or the first line inside one `<pasted_content …>` tag that wraps the whole prompt (how Claude Code hands its hook a long typed-in message); one deeper in the text never counts. |
| UserPromptSubmit that showed quiet messages | Raised to the stricter of the turn's and every shown message's (a non-human origin wins, then the higher hop), in the same transaction as the turn's attribution. A hop-3 message read during a turn the human started keeps the turn at hop 3. |

**Lead agents.** The human can make a Claude or Antigravity agent a lead agent (`lead`; Edit agent > More
options > "Lead agent", or `muster agent edit <name> --lead on` from a terminal outside Muster).
In a turn a reminder or a schedule started (origin `schedule`, or `lead` below), what a lead
agent's session sends or spawns goes as origin `lead` (`agents::sending_prov`): its
`ask_agent`, `tell_agent`, `send_message`, `reply` (and the same through `muster ask/send/reply/new`)
and `spawn_session`, with its own hop one on, and the sessions it starts begin with that
provenance. `lead` is taken like `human` wherever an origin is checked against an accept list or
the plain Claude rule (`Origin::accepted_as`), so a lead's reminder turn can start a Claude builder
or ask a Claude agent, and whatever that agent sends on carries `lead` too: the next hop behaves
as the human's. Everywhere else `lead` is still unattended work, like `schedule`: `set_reminder`,
`propose_schedule`, `record_answer` and `start_fresh` refuse it, a note edit made in it fires no
note trigger, it counts toward the receiver's unattended daily cap, and the OpenRouter spend caps
apply. So a builder reporting back to its lead can't turn the lead's turn into one that sets the
next reminder. The stricter origin still wins when chains meet (`human` < `lead` < `schedule` <
`webhook`). The hop still counts: a chain through a lead ends at hop 8 like any other. What is
lifted, and what isn't:
- the human's work stays `human`; a trigger's (`webhook`) or an add-on's work is never lifted, so
  outside triggers stay blocked from Claude agents even through a lead;
- keys (`muster keys`) reach a Claude session only from the human's own chain: never from scheduled
  work, `lead` included (raw input carries no route into the turn it starts);
- a message on `lead` work can't wait for a later turn, since only a `lead` turn could show it:
  `tell_agent`, `send_message`, and a `reply` to an existing agent or a live session (even one
  whose ask is still waiting) are refused with `wake: false` before anything is kept or counted; send it with
  `wake: true`. A `reply` the human or a schedule asked is shown in the message log (or taken as the
  schedule run's reply) either way;
- what reaches a lead is unchanged: its accepted origins decide what it takes.

An Antigravity lead works the same way: it calls these tools through `call_mcp_tool` (its persona
says how), and its lead section is the same text. An agent switched from Claude to Antigravity, or
back, stays a lead (see "Switching an agent's kind" in docs/PROTOCOL.md).
Codex agents can't be leads (refused; a flag written into the database by hand is ignored), and no
tool sets it: turning it on takes a click in Muster's window or the owner's terminal, the check
`tools_up_front` makes; anyone may turn it off, which is saved even when the persona file can't be
rewritten. A lead's persona file says it is one ("Lead agent"). An older backend reads a stored
`lead` origin column as `webhook` (the strictest it knows), but a late reply stored with `lead` in
its JSON drops out of its message log, and it refuses to start on runtime attribution state that
holds a `lead` provenance (an idle session keeps one until its next attributed prompt) or on a saved
prompt queue holding one: rolling back after lead work ran can need that state cleared first.

An agent's reply is attributed per message: the message whose prompt was submitted (confirmed by the
envelope in UserPromptSubmit) owns the next final message; a prompt without an envelope in between
takes the turn, and its final message belongs to no agent message.

**CLI calls.** `muster send/keys/ask/new` pass `$MUSTER_SESSION_ID` as `from_session`. For an
agent session, `muster ask` runs this server's `ask_agent` and `muster send` its `send_message` as
that session (same guards, rate limits and audit event, with `via: "cli"` in the args); `muster
keys` and `muster new` carry its provenance and the Claude origin rule. A shell session is a human
typing in a shell tab, with that shell's provenance if an agent ever typed into it.

**Late replies.** An agent can answer a message after the turn that received it, e.g. once a
long job finishes: it says so in that turn's reply, then calls `reply {msg_id, text}` (or runs
`muster reply <msg_id> TEXT` in its terminal, which calls this tool with its session's own token). Code:
`src/agent_reply.rs`.

- *Who:* only the named agent the message was addressed to (its current session, also after a
  restart), only for a message Muster typed into it (`delivered_at`, set when the injector
  presses Enter; never a refused or still-queued one), and once per message: a second `reply`
  is an error. To say more, it asks the sender with `ask_agent`.
- *Stored first:* the answer goes on the message's row (`agent_messages.answer`, JSON: text,
  time, route, status, note) before any delivery, so a backend reload never loses it; one a
  restart interrupted is delivered at the next start (an interrupted delivery to an agent is
  matched to the message that already carried it, never sent twice; one being typed into a
  plain session is marked "may not have arrived" rather than typed again).
- *Route:* the stricter of the message's chain and the replier's current provenance (a
  non-human origin wins, then the higher hop), one hop on. So a reply written while handling a
  scheduled job is unattended work, and a reply chain ends at hop 8 like any other. A lead agent's
  reply passes scheduled work on as `lead` (also in `origins`), its hop counted the same way. The reply
  also keeps every non-human origin it is rooted in (`origins`: a webhook-rooted question
  answered during scheduled work is both), and an asking agent must accept each of them; a
  Claude session takes none. A waiting ask takes the reply only when it brings no origin the
  question's chain didn't have; otherwise it goes the guarded way, as a new message.
- *The asker* is the agent that sent the message: if it was removed and a new agent took its
  name, the reply is kept in the log (`undelivered`), never sent to the newcomer.
- *Recovery:* the late replies left unfinished are read at daemon start, before the server takes
  requests. A carrying message that was recorded but never queued (the restart came in between)
  is dropped and sent again; one the spend cap refuses later in the asker's queue marks the
  reply `refused`. A delivery cut off by the backend shutting down stays `pending` for the next
  start.
- *How the asker gets it:*

  | Asker | What happens | Status |
  |---|---|---|
  | An ask still waiting for that message (`ask_agent`, `muster ask`, `agent.ask`) | It returns the text as its `reply` (it re-checks every second; after 30 s untaken, e.g. the ask was cancelled, the next row applies: `WAITER_MAX`). Only a reply that brings no origin the question's chain didn't have, and is no deeper in the chain than an answer in the question's own turn (one hop past it); any other goes as the next row says. | `returned` |
  | A named agent | A new agent message: `[muster msg <new id> from <replier> origin=<o> hop=<n>]`, then `Late reply to your message <id> ("<first line of the question>"):` and the text, with `reply_to` = the message answered. Through `agents::send_checked`: the asker's accepted origins, the hop limit, the daily and unattended caps, the OpenRouter spend caps, the injection queue; a stopped agent is started as for any message. A refusal is logged like any refused message. | `delivered` (`delivered_as`), else `refused` |
  | Another Claude, Codex or Antigravity session (live) | Typed in through its injection queue after `[muster reply to msg <id> from <replier> origin=<o> hop=<n>]` (no message row of its own, so not an envelope the agents' attribution acts on); the session takes the reply's origin and hop. A Claude session takes only `human`-origin replies (`send_message`'s rule). | `delivered` (`session_id`), else `refused` |
  | A schedule's, trigger's or "Then…" step's run, when the turn left no final message for it | The run's reply, under the first row's rule: in its wait it ends the run `replied`; after the run stopped waiting it is the run's late reply (a run that ended `no_reply`, `timed_out` or `blocked` shows `replied` with a `late reply` chip in its history; a schedule's own run, not a step, puts it in its note once). Any other reply goes as the next row says. | `returned` |
  | The human (a client, `muster ask` outside a session or in a shell tab), or a schedule | Shown in the sidebar's message log (the message's entry gets a `late reply` chip; its overlay shows the text and how it was delivered); a desktop notification when the human asked. | `shown` |
  | An agent since removed, a session that ended | Kept in the log only. | `undelivered` |

  With `wake: false` (`reply {msg_id, text, wake: false}`, `muster reply --no-wake`), the two
  rows that start a turn change: an asking named agent gets it as a quiet message (the same
  `Late reply to your message <id>` text, an agent message with `quiet` set; note "waits for
  <name>'s next turn that can take it, as message <n>"), and another Claude session gets it at
  the start of its next turn that can take it (a Codex session is not delivered to:
  `undelivered`). An ask still waiting takes it either way, under the first row's rule (no new
  origin, at most one hop past the question; `agent_reply::returnable`), and the human's and a schedule's rows
  are unchanged. On `lead` work a quiet reply to an existing agent or a live session is refused before it is
  stored, even while an ask waits for it (rare: the retry with `wake: true` still reaches that
  ask); one the human or a schedule asked is shown (or taken as the run's reply) as usual, and one whose asker is gone is kept
  as usual. Same checks as the waking way: for an asking agent, its accepted origins for the
  reply's origin and every origin it is rooted in, and the hop limit; for a plain session, the
  hop limit and the plain Claude rule on the reply's route; all before anything is kept. A reply
  rooted in two kinds of unattended work is refused with `wake: false` before it is stored (send
  it with `wake: true`, checked against both). The answer's text must meet the quiet size rule
  (6,000 characters, an emoji such as 😀 counting as two, and 150 lines), checked before it is stored and
  before the rate limit's slot is taken; the generated `Late reply to your message` line (always
  one line) gets room of its own. A quiet late reply to a plain session is linked to the message
  it answers (`late_reply_to`, one per session; an ordinary quiet note sent while handling that
  message is never mistaken for it), so a restart that cut its delivery off finds the carrying
  row, even once that session has ended, and reports what became of it: waiting or shown is
  `delivered`, dropped or expired is `undelivered` with the reason. A carrying agent message is
  read the same way.

  A reply to an asker is a reply to that asker's own question, so two rules for starting work
  don't apply: the workspace rule (an agent may answer a project session that asked it) and
  the permission-widening check (a plan-mode researcher may answer a wider orchestrator). All
  the others do, and `reply` shares `send_message`'s rate limit (20 per minute).
- *Seen in:* `agent.messages` and `agent_message` events (`answer` on the message answered;
  the carrying message's `reply_to` points back), `muster events` (`late reply: STATUS`).

**Quiet messages.** A report or a note doesn't need to start the receiver's turn. Sent with
`wake: false` (`tell_agent`, `reply`, `send_message`), a message waits, unseen, for the
receiver's next turn that can take it, whoever starts that turn (the human typing, another
message, a schedule, the prompt after a fresh start), and arrives as context at the start of
that turn, after the prompt. Nothing is typed into the receiver's input, and an asleep or
stopped agent stays that way. "The next turn that can take it" is not always the next turn: a
scheduled or webhook note waits for a turn of its own kind, and a note can be held (see "Which a
turn takes" below), so it may wait several turns, or expire unseen. Choose `wake: false` only
for what can wait. Code: `src/quiet.rs`.

- *What the receiver sees:* Claude Code's hook context for that turn (`additionalContext`),
  oldest first:

  ```
  Messages that waited for this turn. Other agents or sessions sent them without waking you; the human did not type them. To answer one, call reply with its msg_id and your text.
  [muster msg 812 from alpha origin=human hop=2] sent 6 min ago
    PR 31 is open, head abc1234. The report is in reports/auth.md.
  [muster msg 815 from beta origin=human hop=1] sent 2 min ago
    Late reply to your message 790 ("check the deploy"):
    deploy is green.
  ```

  An agent message keeps its envelope, so `reply` with its `msg_id` answers it (a shown quiet
  message counts as delivered; the last sentence of the first line is for named agents only). A
  message to a plain session has `[muster quiet message from <sender> origin=<o> hop=<n>]`
  instead. Every line of a message's text is indented, and every line break a reader could take
  for one (`\r`, U+0085, U+2028, U+2029) is made a plain line break first, so no line of it can
  pass for a header. A late reply's "Late reply to your message <id> (…):" line is always one
  line: line breaks in the quoted question become spaces. A turn takes at most 7,500 characters
  and 180 lines of them (Claude Code's limit for hook context is 8,000 characters and 200 lines,
  counted in UTF-16 code units as it counts them); the rest wait, under a last line "3 more
  messages are waiting; they come with a later turn." A message too large for any turn (the
  size rules keep this from happening) is passed over with a warning in the backend's log, never
  holding back the ones after it. A prompt starting with `/` (a slash command such as `/clear`)
  takes none.
- *How:* Muster's `UserPromptSubmit` hook (`muster hook claude UserPromptSubmit`) asks the
  backend for the session's waiting messages (`POST /quiet/take` with the session's own token
  and the prompt's message id when the prompt starts with a Muster envelope, at most 1 s) and
  prints them as the hook's JSON output. `take` judges the turn itself (below), picks the
  messages, marks them offered to the session's current run, and raises the session's
  provenance to their routes (the higher hop, their origin), all in one transaction and only
  while the token's run is still the session's current one: the turn never acts on them under
  its old route, two overlapping takes never offer one message twice, and a take from a run a
  restart replaced gets nothing. The payload the hook saves for the backend names them
  (`muster_quiet: {agent: [ids], session: [ids]}`); replaying it keeps them pending for that
  run. Every prompt callback of the run raises the provenance again to everything offered to the
  run and not shown yet (in case the prompt was the human's, or its callback arrived in another
  order). The run's next hook from the main agent (`PreToolUse`, `PostToolUse` or `Stop`: the
  turn ran; never a subagent's, whose hooks carry an `agent_id`) marks them shown. A new prompt
  with no such hook in between means the earlier one never ran (another `UserPromptSubmit` hook
  blocked it, and Claude Code discarded its context): its messages stay offered. Only a hook of
  the run they were offered to can confirm them. With the backend down (a reload) the hook
  prints nothing and they keep waiting; one offered but never confirmed is offered again after 2
  minutes (at once to a newer run, never to an older one), so a lost callback or a blocked
  prompt can show one twice, never lose it.
- *Which a turn takes:* a turn answering a message Muster delivered to this session (its
  prompt starts with that message's envelope) and not answered yet has that message's route;
  any other turn is the human's. A human's turn takes messages of the human's origin. A
  message's turn takes those and messages of its own origin, and only messages no deeper in a
  chain than it (hop no higher): its answer goes straight back to the asker with the message's
  route, so it never carries scheduled or triggered material into a human-rooted answer, or a
  deeper note into a shallower chain. One turn never mixes a schedule's and a webhook's work. A
  named agent takes only origins it still accepts (an origin taken away after a note was sent
  keeps it waiting). A message is held while the receiving process runs in a wider permission
  mode than the one its send was checked against (the tab restarted in Bypass, say), as the
  turn's own `UserPromptSubmit` payload reports it (`permission_mode`). A reported `auto` has
  width 3 regardless of the tab's configured mode. Only unknown modes such as `dontAsk` or
  missing modes use the tab's width. An agent on OpenRouter takes
  unattended ones only while the spend caps allow, read fresh only when the turn would take one,
  and waited for at most 400 ms: a slow reading leaves them waiting and never holds back the
  human's. The rest wait for a turn that can take them, or until they expire; the turn itself
  goes ahead either way.
- *Who takes them:* Claude sessions only, through that hook. A Codex receiver is refused with
  "<name> is a codex session, which can't take a message that waits for a later turn (only
  Claude sessions can). Send it with wake=true". `send_message` with `wake: false` and
  `interrupt: true` together is refused.
- *Guards:* the waking path's, every one, before anything is stored. `tell_agent` runs
  `ask_agent`'s checks (permission width, the agent's accepted origins, the hop limit, the daily
  and unattended caps in the insert's transaction, the OpenRouter spend caps with a fresh
  reading, the rate limit, text cleaning); every quiet message to a named agent checks the
  agent's accepted origins and the hop limit again where it is stored (`agents::quiet`), and
  keeps the permission mode `tell_agent`'s permission check ran against (a late reply: the one
  the agent's tabs have then). `send_message` keeps its own (reach, the
  Claude origin rule, permission width) and binds the message to the run and permission mode it
  checked; every quiet message to a plain session checks the hop limit and the plain Claude rule
  (the human's work only) on its final route where it is stored (`quiet::send_session`), a late
  reply's too. `reply` keeps its route and per-asker checks; a quiet reply rooted in two kinds of
  unattended work (a webhook's question answered during scheduled work) is refused before it is
  stored ("send it with wake=true"), since the waiting message carries one origin. Only the last
  step changes: stored instead of typed in. Refusals are logged like any refused message. A
  message refused for its size takes no rate limit slot.
- *Limits:* at most 6,000 characters (an emoji such as 😀 counts as two) and 150 lines per message (a late
  reply's generated first line is extra), and 20 waiting per receiver: the 21st is refused
  ("beta has 20 messages waiting already; send with wake=true, or use ask_agent if it must act
  now"). Characters are counted as Claude Code counts a hook's context, in UTF-16 code units:
  most letters count one, a character outside the Basic Multilingual Plane (😀) two, ☕ one, and
  an emoji built of several characters (👩‍💻) the sum of its parts (five). Lines are counted
  once every line break a reader could see (`\r\n`, `\r`, U+0085, U+2028, U+2029) is one. A
  message never shown within 7 days is marked expired (kept in the log, never shown): checked at
  daemon start, every hour and at each turn's start, and the tabs' counts follow. A
  message a turn has in hand (offered in the last 2 minutes) doesn't expire under it, and one
  that expired is never then marked shown.
- *Where they wait:* in the database, so a backend reload or restart keeps them. An agent's
  are kept by agent, not by session: they come with a later turn after a restart, a wake or a
  fresh start (`start_fresh`, `agent start --fresh`). A plain session's are for the run they
  were sent to: dropped when its process ends (or its tab is removed, or a new run starts, even
  while no backend ran: the next backend start drops them), and a session that has exited takes
  none. They are not prompts in the injection queue: they don't keep an agent awake (it may
  still go to sleep), and they never start a turn, so two agents can't keep each other busy with
  them.
- *Seen in:* the receiver's tab (an envelope badge with the count, `quiet_waiting` on the
  session; an asleep agent's tab shows it too, and messages sent before an agent's first start
  count once its tab opens), the sidebar's message log for named agents' messages ("waits
  for a turn", "shown", "expired"; `agent.messages` fields `quiet`, `shown_at`, `expired_at`),
  and the overlay of a message whose late reply went quietly, which shows where the carrying
  message stands from `answer.quiet_state` (`{state: "waiting"|"shown"|"expired"|"dropped", at?,
  reason?, msg?, to?, session?}`, an agent's or a plain session's carrier, whatever page of the
  log holds it), and `list_sessions`. Every Claude agent's generated `CLAUDE.md` says to use
  `tell_agent` for reports and `reply` with `wake: false` after a long job, and to leave wake on
  for Codex receivers (a Codex agent's `AGENTS.md` doesn't mention quiet messages).

**Message tools up front.** Claude Code defers MCP tools: a session sees only their names until
it looks one up with its tool search, which costs a step each time. An agent with "Load message
tools up front" (`tools_up_front`, off by default; Edit agent > More options, or `muster agent
edit NAME --tools-up-front on`) gets seven tools in its prompt from the start: `ask_agent`,
`tell_agent`, `reply`, `send_message`, `whoami`, `note_read`, `note_append`. Muster marks them
with `_meta["anthropic/alwaysLoad"]` in that agent's Claude sessions' `tools/list` (Claude Code
2.1.325 honours it per tool); a Codex agent can't have it (refused) and never gets the mark.
Like a self handoff grant, only the human turns it on (a click in Muster's window, or the CLI in
a terminal outside Muster; refused from inside any Muster session); `off` works anywhere. Measured on Claude Code 2.1.325 against Muster's server: about
2,200 more prompt tokens on every model call, mostly cache reads; all of Muster's tools would be
about 14,500. Applies from the agent's next start (Claude Code reads the list when it connects).

**Refusals are logged.** A message refused by the agent's policy (`ask_agent`'s permission
check, origin rule, hop limit, daily cap or unattended cap, and the same checks behind `muster ask`) is kept in
`agent_messages` with the reason and shown in the sidebar's message log (`agent.messages`,
`agent_message` events). It is never delivered and never counts toward the cap. Bounded: per
sender at most 10 refusals per 10 minutes are stored (more are only counted, and the next stored
one says how many), the text is cut to 2 KB, and refused rows are pruned to the last 30 days and
the newest 1000.

**Unattended cap.** On a chain a schedule (or later a trigger) started, every message counts
toward the receiving agent's unattended cap: at most 24 a day by default (the scheduling
settings), at any hop, on top of the 200-a-day cap. So an agent running a scheduled job can't
fan out unbounded work to others (docs/PROTOCOL.md "Schedules").

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

**Self handoff.** Long-running agents grow large conversations; only the human could clear one.
The human can promote a Claude agent to do it itself (`self_handoff`, off by default): "Can start
itself fresh" under More options in Edit agent, or `muster agent edit NAME --self-handoff on` in a
terminal outside Muster. Only the human: no tool changes it, and `agent.update` refuses to turn it
on from inside any Muster session (an agent's or a shell tab's, even one that dropped
`MUSTER_SESSION_ID`: the connection's program is checked like a window's, docs/PROTOCOL.md
`agent.update`), from a script's WebSocket or from a phone. Anyone may turn it off. Limit: a
program an agent detaches from its session (its parent exits, init adopts it) is caught by the
session's own control group only when Muster isolates sessions (the default with a systemd user
manager; not with `MUSTER_ISOLATION=off`). As for every agent rule (docs/PROTOCOL.md "Channel
permissions", Limits), an agent whose permissions let it act as the user outside its sandbox can
get round it, for example by editing Muster's database. A promoted
agent's `CLAUDE.md` gets a "Starting fresh" section saying how and when to use it. The agent writes
a handoff file in its home folder or working directory, calls `start_fresh` with its path (or runs
`muster start-fresh <path>`), and ends its turn. The call must come from the agent's own
processes (its Claude, or a command it runs): its token used by another program is refused, so
one agent can't reset another with a token read from its files. Muster then:

1. waits for that turn to end: idle, nothing queued, no text in its input box (the human writing to
   it), for 2 s (a request still waiting after 2 hours
   is given up; one whose agent is restarted, stopped, put to sleep, removed or no longer promoted
   is cancelled);
2. checks the file again, and the spend caps for unattended work on OpenRouter;
3. queues `/clear` and the prompt together, the prompt held at the head of the queue so a message
   that arrives meanwhile waits behind it, records the time (the rate limit: one per 30 minutes)
   and types `/clear` (Claude Code's own new conversation: the process, its background tasks and
   its tab stay), unless the right was taken back meanwhile;
4. waits up to 5 minutes for Claude's `SessionStart` hook with `source: "clear"` (else both are
   taken back unsent where they still wait, say behind a draft);
5. lets the prompt go, and records the handoff done once it is typed: `[muster self-handoff origin=<o> hop=<n>] You (<name>) asked Muster for this
   new conversation yourself at <time>, with the tool start_fresh; the human did not type this. …
   Read your handoff file <path> and your memory/ notes, then carry on from there.`

Both prompts carry the provenance of the turn that asked (no message id): a handoff asked while
handling scheduled or triggered work stays that work (the prompt says it is not a new request from
the human), and never becomes human-started. Every request, refusal, cancellation, failure and
finished handoff is a `system` event in the agent's tab (`self_handoff.stage`); a finished or
failed one is also a notice. "You cannot send a message to yourself" still holds for
`send_message`. Codex agents: not supported yet. A request waits in the backend's memory: a
backend restart forgets it (the agent can ask again), and settles what it left in the prompt
queue (an untyped `/clear` is dropped with its prompt; after a typed one the prompt goes).
Known gap: the file is checked when asked and again before `/clear`, not when the new
conversation reads it.

Reach: agents live in their own "agents" workspace (`<data>/agents`). `list_agents` and `ask_agent` work
from every workspace: an agent is a shared service, asking runs no command anywhere, and the origin
rule, hop limit, daily cap and permission check apply. Every other tool keeps the workspace rule,
so agents can read each other's terminals but not reach the user's project workspaces (unless
the human adds a directional sharing rule), and project agents don't drive an agent's terminal directly.

### Schedules

A named agent can schedule work for itself (docs/SCHEDULING_PLAN.md, Phase 3). The owner's rule:
an agent **proposes** and the human **approves** each proposal with a click in Needs you, at the top of Muster's sidebar
(only there: no command, tool or script approves); the one exception is a **one-off reminder for the agent itself,
5 minutes to 24 hours ahead**, which runs without approval. Code: `src/schedules/agent.rs`.

| Tool | Arguments | Result |
|---|---|---|
| `list_schedules` | — | `{schedules: [{schedule_id, agent, name, prompt, when, status, next_run, made_by, last_run}], takes_scheduled_work, limits}`: the schedules that run on the caller or that it made. `status` is `proposed` (waiting for the human), `active`, `paused`, `expired` or `done`; `next_run` is a local time. |
| `propose_schedule` | `prompt`, `reason`, exactly one of `cron` / `at` (`"YYYY-MM-DDTHH:MM"` or unix ms) / `in_minutes`; `agent?` (default: the caller), `name?`, `tz?`, `conversation?` (`continue`\|`fresh`), `quiet_hours?`, `daily_cap?`, `note_id?`, `replaces?` | `{schedule_id, status: "proposed", agent, when, estimate, note}`. A schedule that **doesn't run** until the human approves it. The human sees its prompt, timing, agent, the caller's `reason` and an estimate of its use (runs in the coming week after quiet hours and caps, what pays for them, the caps that bound them), and may edit or dismiss it. The agent it runs on must take scheduled work. Times only: no triggers, no "Then…" steps (those are the human's). A notice and desktop notification tell the human it waits. `replaces`: the id of a schedule this one replaces (it must exist; any schedule, not only the caller's): the card says "Approving pauses schedule <id> (<name>)", and the human's click on Approve pauses it in the same step, so the two never both run. Nothing is paused before that click; a dismissed proposal pauses nothing; if the replaced schedule was removed meanwhile, the approval goes ahead alone. |
| `set_reminder` | `prompt`, `in_minutes` (5 to 1440) **or** `at`; `name?`, `tz?` | `{schedule_id, status: "active", runs_at, note}`. A one-off schedule for the caller only, no approval: Muster sends `prompt` then, into its current conversation, as scheduled work (origin `schedule`). The caller must take scheduled work. For a Claude subscription agent, quiet hours stay on and a time inside them is refused. |
| `pause_schedule` | `schedule_id` | `{schedule_id, status, note}`. Pauses a schedule. A lead agent (in any turn), or any session (a named agent or not) whose current work is the human's (the human typed it, or it came along a chain the human or a lead started: origin `human` or `lead`), may pause any schedule; a non-lead's scheduled or triggered work pauses only a schedule that runs on the caller or that it made. Each pause shows a notice, "<who> paused schedule <id> (<name>)" (none when it was already paused). Only the human resumes. |
| `delete_schedule` | `schedule_id` | `{deleted}`. Deletes a schedule the caller made (a proposal, a reminder, an approved proposal) and its history. A schedule the human made can only be paused. |

Rules (every one is checked in the tool, whatever the client):

- **Only the human approves, resumes or widens.** No tool approves, resumes, edits or makes a
  schedule directly; `schedule.update`, `schedule.create`, resuming and the settings are refused
  from an agent's session and from a shell an agent has typed into (except the unattended cap
  and the quiet hours, which an agent changes with `muster schedule settings --cap N` or
  `--quiet 23:00-07:00`, either way), and `schedule.approve` is
  taken only from a click in Muster's window (a WebSocket of the UI's that holds a window, whose
  program is known and isn't inside any session: never the CLI, `/api/rpc` or a script;
  docs/PROTOCOL.md "Schedules"). A schedule made or widened anywhere else (the CLI without a
  session id, `/api/rpc`, a script) is held for that click like a proposal, and settings that
  loosen another limit (resuming all, spend caps, watched folders) are refused there. To change an approved schedule an agent proposes a new one.
  (Agents run as the owner's user, so this raises the bar rather than closing it:
  docs/SCHEDULING_PLAN.md "Approval and its limits".)
- **Not from unattended work.** A session handling scheduled or triggered work (any chain a
  schedule or trigger started: its provenance's origin isn't `human`) can't propose or set a
  reminder. So a reminder can't set the next one, and a schedule can't breed more schedules; the
  agent can again once the human writes to it. An ask from another agent on a human-rooted chain
  is the human's work.
- **Only named agents**, and a reminder only for the caller itself; a proposal may be for another
  agent that takes scheduled work. The one exception is `pause_schedule`, which any session may
  call under its rule above (owner, 27 Nov 2026: pausing only narrows, and the notice keeps it
  seen).
- **Limits per agent:** reminders at most 3 waiting, at least 15 minutes apart, 10 set per rolling
  24 hours (deleted ones count); proposals at most 3 waiting, 10 per rolling 24 hours (deleted or
  dismissed ones count); at most 5 live schedules it made (waiting, active, paused, or a one-off
  whose run is due but still waiting in line, which also counts as waiting and for the 15
  minutes). Calls are checked and recorded one at a time, so calls made at once (MCP and
  `muster schedule` together) can't overshoot a limit. The cron
  floor (every 15 minutes at most) applies as to the human's. Proposals and reminders share the
  post rate limit (30 per minute with note writes, panel updates and posts).
- **Once made (a reminder) or approved (a proposal), it is an ordinary schedule:** origin
  `schedule`, so the agent's accepted origins, the unattended cap (24 a day by default), the 200-a-day cap,
  the OpenRouter spend caps, quiet hours, a Claude subscription agent's 7-day expiry (from
  approval), the busy rule and the missed-run rule all apply when it runs.
- A proposal nobody approves is removed after 7 days; a reminder a day after it ran (its message
  stays in the agent's log). Every call is audited (`mcp_call`), and each schedule records who made
  it (`made_by`).

The same from an agent's shell: `muster schedule propose …` and `muster schedule remind …` inside
its session run `propose_schedule` and `set_reminder` as that agent, and `muster schedule pause`
and `muster schedule rm` run `pause_schedule` and `delete_schedule` (docs/CLI.md "Schedules").

### Questions for the human

An agent that needs the human posts a question and carries on: nothing waits on it, its turn
goes on and messages to it keep arriving (unlike an interview box, which holds every message to
the agent until it is answered). Muster keeps the question; the answer reaches the agent later as
a message. The human sees every open question in the pinned Questions tab of Muster's window
(docs/UI.md "Questions") and answers there with a click or in their own words; questions are also
listed with `list_questions` or `muster questions` (docs/CLI.md "Questions"), and a lead records
an answer the human typed in chat with `record_answer`. Code: `src/questions.rs`.

| Tool | Arguments | Result |
|---|---|---|
| `post_question` | `question`, `context`, `example`, `kind?` (`pick_one` default, `yes_no`, `pick_several`, `own_words`), `options?: [{text, why?}]`, `pick?` (a letter, or a list for `pick_several`), `if_no_answer`, `note_to_self`, `holding_up?`, `product?`, `area?` (`merge`, `money`, `public`, `customer_dns`, `other` default), `details?`, `pictures?: [{path, caption}]`, `links?: [{label, url}]` | `{number, question: "Q<n>", status: "open", answer_goes_to, note}`, at once. Refused, with every problem listed: no `context` (20 to 1000 characters), no concrete `example` (20 to 800, not the context again), no `if_no_answer` (15 to 500), no `note_to_self` (30 to 4000: what the asker will do with each answer and which files to reopen), a `question` that isn't one line of 10 to 300 characters; for every kind but `own_words`, fewer than 2 or more than 6 options (`yes_no`: exactly 2), no `pick`, a pick that isn't an option, a `pick_one` or `yes_no` pick that isn't option a (the pick goes first), or a picked option without a `why`. `own_words` (a paste, a value) takes no options and no pick. Options are lettered a to f in the order given. Pictures: up to 6, absolute paths (no control characters) of existing png, jpg, gif, webp or svg files, each with a caption; links: up to 4, each an http(s) address (opened in the browser) or a Muster link (`muster://panel/<id>`, `note/<id>`, `tab/<id>`, `agent/<name>`: shown in Muster's window). In `context`, `example` and option text, a `[label](url)` with either kind of address shows on the card as a link; other Markdown shows as written. The same question still open from the same asker is refused with its number; at most 20 open per asker and 200 in all. |
| `list_questions` | `number?`, `status?` (`open` default, `answered`, `withdrawn`, `all`), `mine?`, `limit?` (newest 200 at most) | `{questions: [{number, id, status, asker, passed_on_by, product, question, kind, options, pick, holding_up, if_no_answer, area, answer, withdrawn, asker_now}], shown, total}`, oldest first: the newest `limit` that match (`total`: how many match in all), of those the caller may read (`shown`; see "Who reads" below). `asker_now` (open ones): the asker's `state`, `running`, `asleep`, `cold` (its next message would re-read its whole conversation uncached), `idle_ms`, `new_conversation` (`{why, size_k, asleep, to}` when an answer now would start its recipient in a new conversation, its old one being cold and large, see "A cold, large conversation starts fresh" below; `to`: the lead's name when the answer goes to the lead of an asker that ended; else null). `answer`: `{picked, words, how, recorded_by, at, delivery}`. With `number`: that question whole (the asker's `note_to_self` and its ids included). `mine`: only the caller's (a named agent's across its restarts and fresh starts). |
| `withdraw_question` | `number`, `reason` | `{number, question, status: "withdrawn"}`. Only the asker, only while open. |
| `record_answer` | `number`, `words` (the human's answer, verbatim, at most 4000 characters), `pick?` | `{number, question, status: "answered", picked, delivery: {status, note, to, msg_id, session_id, at, fresh?}, delivered}` (`fresh: {why, replaces, run}`: the asker was started in a new conversation for it). See below. |

`post_question`, `withdraw_question` and `record_answer` share the post rate limit (30 per minute
with note writes, panel updates and posts); `record_answer` also counts toward the send limit (20
messages a minute with `send_message` and `ask_agent`). Every call is audited (`mcp_call`).

**Who reads.** A session sees the questions it asked, those of sessions it started, those asked
in its own workspace and in one whose output a sharing rule lets it read; a named agent (a lead
relays questions) sees all. The human's clients (`muster questions`) see all. So a question never
asks for a password, key or token.

**Numbers.** A question's number (Q1, Q2, …) is its row id and never changes or comes back, also
for a withdrawn one. `muster questions start-at N`, before the first question is posted, makes the
first one QN, so the numbers can carry on from a list kept elsewhere.

**Recording an answer.** A lead that the human answered in chat ("Q3 a") calls `record_answer`
with the human's words verbatim and the letters they chose. Allowed for any named agent, for the
session that started the asker, and for the asker itself (then nothing is sent: it has the
answer); and only while the caller handles what the human typed to it (no provenance: not
another agent's message, nor scheduled or triggered work), unless it started the asker. Refused,
with nothing stored, with letters that aren't the question's or more than one for
`pick_one`/`yes_no`, and when the answer couldn't reach the asker under the rules the caller's
own messages follow: for a named agent `ask_agent`'s (its accepted origins, the hop limit, the
daily cap, a tab with more permissions than the caller's channel); for a session
`send_message`'s (never a shell, never one that ended, another workspace only with a sharing
rule, never a channel with more permissions). Once the question was answered or withdrawn it is
refused; if the answer's delivery failed (`refused`, `undelivered`), the one who recorded it sends
it again by calling `record_answer` with the same words and pick. Muster can't check that the
human said it: the message says who recorded it and that Muster can't check it, and arrives as
that recorder's message, never as the human's own input.

**An answer from the Questions tab.** The human's own, given with a click in Muster's window
(docs/PROTOCOL.md "Questions", `question.answer`): it has no recorder, and goes after 8 seconds
(the human can take it back meanwhile, and later while it still waits in the asker's input queue)
along the same path as a recorded one, as the human's own message: `[muster msg <id> from human
origin=human hop=0]` to a named agent (or `[muster answer to question Q<n> from human
origin=human hop=0]` to a session), then `The human picked it in Muster's Questions tab (<time>).`
or `The human answered it in Muster's Questions tab (<time>), in these words: "<words>"`, and the
whole question with the asker's note as below. An answer the human took back never arrives; if
its message was already queued, it is taken out and marked refused ("taken back").

**Delivery.** The answer is stored first, then delivered:
- To the asking **named agent, by name**: through `ask_agent`'s path, as a message from the
  recorder (`[muster msg <id> from <recorder> origin=<o> hop=<n>]`), so the asker's accepted
  origins, the hop limit, the daily caps, the spend caps and the injection queue all apply. It
  reaches the agent's current session whatever happened since it asked (a restart, a fresh
  start); an asleep agent wakes and a stopped one starts, as for any message. A removed agent,
  or a newer agent that took its name, doesn't get it.
- Else to the **session that asked**, if it still runs, after a
  `[muster answer to question Q<n> from <recorder> origin=<o> hop=<n>]` header (no agent
  message row; a Claude session takes it only on a human-rooted chain).
- Else to the **session that started it** (its lead: by name when that is a named agent),
  saying the asker has ended. If the recorder is that lead, nothing is sent (`not_needed`).
A message for a named agent that was removed, and another made with its name, in the moment
before it is typed in is refused (as every agent message now is).

**A cold, large conversation starts fresh.** When the answer goes to a named Claude agent whose
conversation is cold (its prompt cache has likely expired by its lifetime in `prefs.cache_ttl`,
whether or not the cold warnings are shown: docs/PROTOCOL.md "Cold prompt cache") and at least
100,000 tokens (its last request's input, as its status line reported it for that conversation;
a report that arrives after a newer one, or without the run's API time to order it by, is
dropped), Muster starts the agent in a new
conversation first, the way Start fresh does (persona, memory and the start lock as usual), and
the answer is that conversation's first message: it is queued under the same hold of the start
lock, before any other message can start the agent. Every delivery check (accepted origins, hop
limit, daily and spend caps) runs first, so a refused answer leaves the conversation as it was.
Only while the agent rests (idle with an empty input queue and no sub-agent, background task or
just-typed turn under way, or asleep), with nothing else on its way to it, nobody typing into it
(a draft in its input box, or keys in the last two minutes), no reset or account move the owner
chose already due at its next start, no fresh start the agent asked for itself waiting (Self
handoff), and no prompt of the human's own
taken in by that agent in the last 24 hours, in whichever tab (typed at its input box, or sent as
the human's from a terminal or the window; messages and answers don't count). Whatever Muster can't tell (no size reported yet for that conversation, a
Codex agent, a status line Muster doesn't run) keeps the current conversation, as do warm or
smaller ones. The same function, with one more line for warm conversations of 200,000 tokens and
over, decides idle agent resets when the owner turns them on (docs/PROTOCOL.md "Idle agent
resets"). The message then starts with one line: `Muster started you in a new conversation
for this message, because the old one was cold and large, about 216k tokens and idle 3 h. Read
your memory/ notes first; the message below has everything it is about.`, and the answer's
delivery says so (`delivery.fresh: {why, replaces, run}`; its note starts `started <agent> in a new
conversation, because ...`). The decision is saved with the delivery, with the run it replaces, before the agent is
restarted, so a backend restart in between neither restarts it twice nor types the answer in
twice (a restart that finds the old run still there decides again, on the old conversation's
size and the human's prompts in it), and answers that had started a new conversation are
finished first, before late replies. A fresh start that fails before a new run takes over gives
the agent its old conversation back, and the answer goes into it, with no new-conversation line;
an answer refused after a backend restart cut its fresh start off gives it back too. Deciding,
restarting, giving back and queuing happen under one hold of the start lock. A second answer to the same agent waits for
the first and goes into the new conversation. The Questions tab's "After you answer" line comes
from the same rule (`asker_now.new_conversation`), naming the lead when the answer goes there.
An answer whose `into` is `fresh` (nothing sets it yet) gets a new conversation whatever the size,
under the same resting checks. Other messages (`ask_agent`, `tell_agent`, `send_message`, late replies, quiet messages,
schedules, reminders, pull request cards) always go into the current conversation.

The message starts `Answer to your question Q<n>: <letter>) <option>.`, says who recorded it and
quotes the words it recorded, then repeats the whole question (context, example, options with the
asker's pick, if no answer, holding up, details, links, pictures) and the asker's `note_to_self`
(headed as the asker's note, not the reader's, when it goes to the asker's lead), so an agent
whose conversation was restarted since can act on it. It is plain text: control characters are
removed before it is typed anywhere. Each delivery checks the reach rules again, with the
recorder's current session (a named recorder's, after its restarts); a recorder Muster can no
longer find refuses it rather than sending unchecked. A delivery that a backend restart
interrupted is finished at the next start: to a named agent, the message it was sent as is looked
up (under the sender label kept when it was recorded) and never sent twice; to a session, it
counts as delivered while still in that session's input queue and is sent again otherwise (if the
restart came right after it was typed in, the session sees it twice rather than never). One that
can't be delivered (`refused`, `undelivered`) is kept with its reason, `list_questions` shows it,
and its recorder can send it again.

Not yet (later steps of the Questions tab): one-key answers, Later and Skip, a lead passing a
session's question on in one call (`passed_on_by` is kept for it), defaults that answer a question
at a set time (`area` is kept: merges, money, public and customer DNS will never take one), the
human choosing per answer whether a cold asker gets it in its current conversation or a fresh
start (`into`; cold and large ones start fresh on their own, above), answers for one agent sent
together, and a one-line copy to the lead.

### Pull requests to merge

An agent that wants a pull request merged posts a card for it and carries on. The human sees every
waiting card in the pinned **PRs to merge** tab of Muster's window (docs/UI.md "PRs to merge") and
merges with a click there; Muster's own code does the merge, never a model. Code: `src/prs.rs`.

| Tool | Arguments | Result |
|---|---|---|
| `post_pr` | `repo` (OWNER/NAME on GitHub), `number`, `head_sha` (the full 40-character id of the commit to merge, as pushed), `summary`, `who_notices`, `tests`, `undo`, `after_merge`, `details?`, `pictures?: [{path, caption, side: "before" \| "after"}]` | `{card: "P<n>", repo, number, head_sha, status: "waiting", replaced, result_goes_to, note, left_out?}`, at once: posting never calls GitHub. Refused, with every problem listed: a repo that isn't OWNER/NAME, a number outside 1 to 2^53, a `head_sha` that isn't 40 hex digits, no `summary` (20 to 600 characters), `who_notices` (10 to 300), `tests` (10 to 1000), `undo` (10 to 500) or `after_merge` (5 to 500), unknown fields. Pictures: at most 6, absolute paths (no control characters, not a symlink) of png, jpg, gif, webp or svg files, an image by their content, 8 MiB each and 24 MiB in all (counting the pictures kept, not those left out), each with a one-line caption and its side; Muster copies them into its data directory (`prs/<card>/<post>/`) when the card is posted, so a later rebuild in the agent's folder can't change what the human sees. A picture more than 3 times as tall as it is wide (a full-page phone screenshot, 390 × 8000: fitted to the window it is a sliver too narrow to read) is left out, not refused: the card is posted with the rest, the picture keeps its place in the pairs (marked `too_tall` with its size; Muster keeps no copy, and the window shows a line saying so instead), and `left_out` lists each one (number, path, size) with what to post instead: one screen (a phone: about 390 × 844), or a few such crops. Its size comes from the file's header (PNG, JPEG with its EXIF rotation, GIF, WebP, or the `<svg>` tag's width and height, else its viewBox); a picture whose header gives no size is kept. A picture over 8 MiB is refused as before; its problem line adds "a full-page screenshot? post a crop of one screen". At most 20 open cards per poster and 100 in all. |
| `withdraw_pr` | `repo`, `number`, `reason` (5 to 500 characters) | `{card, repo, number, status: "withdrawn"}`. Only its poster (a named agent across its restarts and fresh starts, else the same session), or once that poster has ended (a session) or was removed (a named agent) what started it; only while the card is open and not being merged. A withdrawn card's pictures are deleted. |

Both share the post rate limit with `post_question` and the note and panel writes. Every call is
audited (`mcp_call`).

**One card per pull request.** A pull request has one open card (waiting, being merged, stale, or
changes asked; repo names ignore case). Posting the same repo and number again replaces it,
keeping its number: the new commit, words and pictures, and what Muster read of the old one starts
over. Only the card's poster posts it again (or, once that poster has ended or was removed, the session
or named agent that started it, which is where its messages went); nobody does while it is being
merged. The new post's pictures are written before it replaces the old one: if they can't be, nothing
changes.

**What Merge does.** Only a click in Muster's window merges (docs/PROTOCOL.md "Pull requests to
merge"): no MCP tool, CLI command or `/api/rpc` call does. On the click Muster reads the pull request
from GitHub again (`gh pr view <n> --repo <repo> --json …`) and refuses unless all four rules pass:
it is open, not a draft and auto-merge isn't on for it; GitHub's newest commit is `head_sha`; it
doesn't conflict with its base (GitHub still working that out doesn't refuse on its own); and no
GitHub check failed or is still running (a check run passes when completed with success, neutral or
skipped, a status when success; anything else counts as failed; no checks at all passes). A reply
missing any of those fields refuses too. It also refuses when the base branch isn't the one the
human saw, when GitHub knows the pull request by another repo name now, and when it merges through
a merge queue (GitHub's `isMergeQueueEnabled` for it, the question gh itself asks: there gh would
queue it or turn on auto-merge instead of merging, and GitHub would merge it later its own way) or
GitHub doesn't answer that. Then the one write: `gh pr merge <n> --repo=<repo> --merge
--match-head-commit=<head_sha>`: a merge commit, never squash or rebase, never `--auto`, `--admin` or
`--delete-branch` (GitHub still deletes the branch if the repo is set to delete merged branches);
GitHub itself refuses if the head moved in between. Then it reads the pull request back. `gh` runs
from Muster's own data directory with the human's GitHub login.

**Messages to the poster.** Each is stored on the card first, then delivered as an answer to a
question is (docs/MCP.md "Questions for the human", "Delivery"): to the posting agent by name, also
after a restart or a fresh start, else the session that posted, else its lead; a restart finishes
one on its way. They start:
- `Merge result for <repo> #<n>: merged.`: the commit merged, the merge commit, and the card's own
  `after_merge`, to do now.
- `Merge result for <repo> #<n>: not merged: <reason>.`: why (with the first line of GitHub's words
  when it said something, without gh's suggestions of other flags). Nothing was merged; don't merge it
  yourself; post the card again once it is fixed.
- `Merge result for <repo> #<n>: not known yet: <reason>.`: Muster asked GitHub to merge but couldn't
  confirm the result (gh timed out, the read back failed, or GitHub accepted it and left it open). It
  may still merge: don't push to the branch or merge it yourself; if Muster's next read finds it
  merged, the `merged` message follows.
- `Changed since you posted it:`: GitHub's newest commit isn't `head_sha` (the agent pushed again).
  Merge is off until the card is posted again with the new commit and new pictures. Sent once per
  post, with the poster's own provenance from when it posted (no click caused it), and only to the
  poster itself: never on to a lead once the poster has ended. The results of the human's click go
  as the human's own (origin human, hop 0). Each message's recipient and provenance are fixed when
  it is written, so a later post of the card doesn't redirect one still on its way.
- After a restart during a merge whose write had gone to GitHub, the poster gets the `not known yet`
  message (and `merged` once a read finds it merged); a merge cut off before the write isn't
  mentioned to it.
- `Changes asked on <repo> #<n>:`: the human's own words (sender `human`); the card left the list
  until it is posted again.

Each ends with the card's number, when and by whom it was posted, the commit, what it changes and its
`after_merge`, so an agent whose conversation was restarted since can act on it, and a line
`(Muster's note <k> on card P<n>.)`.

### Permissions

Each channel has a permission profile the human sets in the UI (`channel.set_permissions`, see
PROTOCOL.md): `codex_sandbox` (`read_only`, `workspace_write`, `workspace_write_network`,
`full_access`) and `claude_mode` (`plan`, `default`, `accept_edits`, `auto`, `bypass`), plus `antigravity_mode`
(ordered `plan < accept_edits < auto < full_access`). New profiles use `auto` for Claude and Antigravity.
Claude `default` uses the account's Claude Code setting. Every agent in the channel uses its
profile on spawn and restart, including agents spawned through MCP.

- `spawn_session` always uses the target channel's profile. It has no argument to ask for more
  (unknown arguments are ignored), and no tool changes a profile.
- An agent can't use a sibling channel's wider profile: `spawn_session` with a `channel_id` whose
  profile gives that `kind` more than the caller's own channel does (Codex: `codex_sandbox`,
  Claude: `claude_mode`, ordered `read_only < workspace_write < workspace_write_network <
  full_access` and `plan < default < accept_edits < auto < bypass`) is refused, and so is
  `send_message` to an agent running in such a channel. Shells aren't governed by the profile.
  Spawning into or messaging an equal or narrower channel is fine.
- A caller of another kind than the target is compared by its **own** mode too
  (`Permissions::exceeds_from`), since the other kinds' fields of its channel aren't shown on its
  form: reaching it is fine when either its channel's field for the target's kind is no narrower
  (as above: it could start such an agent in its own channel), or its own mode sits at least as
  high on one scale every kind shares: 0 changes nothing (Claude `plan`, Codex `read_only`,
  Antigravity `plan`). Level 1 covers Claude `default`. Level 2 covers Claude and Antigravity
  `accept_edits` and `auto`, plus Codex `workspace_write`. Level 3 covers Codex
  `workspace_write_network`. Level 4 covers Claude `bypass` and Codex or Antigravity `full_access`.
  For example, Claude `auto` and Codex `workspace_write` can message each other in either direction. The
  same kind is still compared field by field. Recording an answer (`record_answer`) follows the
  same rule.
- A `new_channel` created through `spawn_session` gets the **caller's** channel profile, never
  more (and never the defaults if the caller has less), also in another workspace
  (`new_channel.workspace`): never that workspace's defaults, even if they are wider.
- `whoami` and `list_channels` show each channel's `permissions` as
  `{codex_sandbox, claude_mode, antigravity_mode, codex_flags, claude_permission_mode, antigravity_flags, set_by}`, so an agent knows
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
| Live children per caller | **32** (live sessions whose `parent_id` is the caller; raised from 8 on 8 Nov 2026) |
| Live MCP-spawned sessions, daemon-wide | **32** (live sessions with any `parent_id`; raised from 24 with it, so one caller can reach its 32) |
| `send_message` rate | **20 per 60 s** sliding window, per caller. A refusal before durable queue acceptance doesn't use up a slot; retained or uncertain deliveries still count. |
| `spawn_session` rate | **10 per 10 minutes** sliding window, per caller. Refused or failed spawns don't use up a slot. |
| Agent-created channels | At most **10** non-archived channels created through `spawn_session`'s `new_channel`, daemon-wide. Spawning into existing channels is not affected. Channel origin is recorded in the same transaction as creation, so earlier open channels still count after backend replacement. |
| Shell commands | Off by default, see below. |
| Other workspaces | Off by default, see below. |
| `post_to_channel` rate | **30 per 60 s** per caller |
| Self-messaging | Refused (`session_id` == caller; `ask_agent` of the caller's own agent) |
| Agent origins | `ask_agent` is refused unless the agent's `accepts_origins` contains the message's origin. Defaults: Claude and Antigravity agents `human`; Codex agents `human`, `schedule`, `webhook`. A Claude session that isn't a named agent takes `human` only: `send_message` to one, or `spawn_session` of `kind: claude`, from a non-human chain is refused. `lead` (scheduled work a lead agent passed on) is taken like `human` in both, but never as keys or as a message that waits for a later turn (see "Lead agents"). |
| Agent hops | Max hop **8** (`guard::MAX_AGENT_HOPS`); a message from the human or a trigger is hop 0. A lead agent's messages count too, and the limit also holds for what reaches a plain session (`send_message`, a spawned session's start, keys, a reply typed in: `guard::check_hop`). |
| Agent daily cap | **200** messages delivered to one agent per rolling 24 hours, any origin (`guard::AGENT_DAILY_PROMPT_CAP`; counted from `agent_messages`, so it survives restarts, and re-checked in the same transaction as the insert, so concurrent asks over MCP and the CLI can't overshoot). |
| Agent messages | Only through `ask_agent` and `tell_agent`: `send_message` to an agent's session is refused. `ask_agent` shares `send_message`'s rate limit, size limit, text cleaning, `!` rule and permission-widening check, against the widest of the agent session's current tab and the agent's own tab (its profile before it has one). An agent only ever runs in its own tab (stored on the agent), never a same-named one. |
| Message size | 100 000 chars (`send_message`), 20 000 chars (`post_to_channel`); a quiet message (`wake: false`) 6,000 chars and 150 lines |
| Quiet messages | At most **20** waiting per receiver; expired unshown after **7 days**; Claude receivers only. Every check of the waking path applies first (see "Quiet messages"). |
| Schedules | An agent proposes; only the human approves (no tool approves, resumes or edits a schedule). One-off reminders for the caller itself, 5 minutes to 24 hours ahead, need no approval. Neither while handling scheduled or triggered work. Limits: see "Schedules" above. |
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
  workspace plus explicitly shared targets. The human grants access in Muster's sidebar (Channels ⋯) under
  Session communication, with separate messaging and output-reading permissions. A sharing rule
  lets sessions message or read each other; it never lets one start sessions in the other
  workspace (that takes the grant below).

### Starting sessions in another workspace

A lead agent lives in its own folder, so the sessions it starts there don't load a repository's
CLAUDE.md, AGENTS.md or `.claude` hooks. To start a builder inside the repository instead, the
human lets that agent start sessions in the repository's workspace: Edit agent (the agent's menu
in the sidebar) > More options > **May start sessions in**, one tick per workspace
(`Agent::spawn_workspaces`, `agent.update` `spawn_workspaces`).

- Only a click in Muster's window adds a workspace. The CLI (even from the owner's own terminal),
  a script, a paired device, and any call made from inside a Muster session are refused; they can
  only take workspaces off the list. No MCP tool changes it.
- The agent then calls `spawn_session` with `new_channel: {workspace: "<name>"}` (the exact name
  in the sidebar), or with a `channel_id` in that workspace. By default the new channel gets its
  own git worktree and branch in that repository (`worktree` defaults to `true`; ignored for a
  non-git workspace), and the child starts there, so the repository's instructions and hooks load.
- A `channel_id` there is accepted only for a channel that holds a session the agent started
  there (to retry in it); the human's own channels there are refused, grant or not.
- A new channel gets the caller's channel permission profile (see "Permissions"), never the
  target workspace's; a `channel_id` keeps that channel's, which can't exceed the caller's.
- A session started this way records the agent (`SessionInfo.delegated_by`), and so do the
  sessions it starts in turn. Such a session counts as one of the agent's own workspace for
  reach, wherever it runs: it reaches what the agent's other children reach, never the human's
  sessions in the workspace it runs in, and starts sessions there only while the grant holds.
- The agent can wait for, read, message and diff the sessions started under its grant (from any
  of its own sessions: a restart keeps that, since the record names the agent), and they show in
  its `list_sessions` and `list_channels`. The human's own sessions in that workspace, and other
  agents' sessions there, stay out of reach.
- Refused, with the setting named: another workspace without the grant; any session that isn't a
  named agent or started under one's grant; and a sharing rule alone. An unknown name is refused
  with the names the caller may use, and a blank one too. When two workspaces share a name, the
  one the caller may start sessions in is used; if that is still not one, it is refused.
- Taking a workspace off the list ends the reach and further starts there at once (also for the
  sessions started under the grant); the sessions already running keep running. `whoami` shows
  the current list as `policy.may_start_sessions_in`.
- The usual spawn limits apply (depth, live children, rate, agent-created channels).

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

### Session conversations and explicit sharing

Ordinary sessions support `request_session({session_id, text, timeout_seconds?})`.
The default timeout is 0 (return immediately); up to 1800 seconds waits for an
explicit answer. The recipient calls `reply_session({request_id, text})`, once.
`session_messages({request_id?, after_id?})` returns only the caller's conversations,
including request text, answer, timestamps and status: queued, delivered, answered,
failed or timed_out. A wait timeout does not cancel delivery or prevent a late reply.
These records persist across backend reloads. Up to 1000 requests are retained;
answered or failed entries are evicted first, never unresolved entries. This API
uses explicit replies rather than guessing which terminal output answered a request.
Existing `send_message` remains a fire-and-forget prompt (or, with `wake: false`, a quiet
message); named agents still use `ask_agent`, `tell_agent` and `reply`.

`list_sessions` includes `reachability` with `can_message`, `can_read_output`,
`message_reason` and `read_output_reason`. Discovering a session does not authorize
reading its output. Same-workspace access remains the default. The old
`MUSTER_MCP_CROSS_WORKSPACE` switch is retired; it no longer grants access. In Muster's sidebar (Channels ⋯ → Session communication),
**Session communication** lets the human add directional workspace grants,
optionally limited to one caller session, with separate messaging and reading
permissions. Only a verified local window can change grants. Named agents remain
global shared services under their existing origin and permission rules. Shared
notes retain their existing global scope.

`subscribe_channel({channel_id, enabled})` opts the caller into future posts.
`channel_updates({after_id?})` returns subscribed updates and `next_cursor`; pass
that cursor on the next poll. Reads recheck current sharing rules. Unsubscribing
stops access through this feed. Subscriptions do not inject prompts or wake agents,
so posts cannot automatically produce reply loops. Each session can subscribe to
32 channels. The feed retains 1000 posts globally and exposes
`oldest_available_id` so consumers can detect a retention gap. Own posts are omitted.

Session communication (Channels ⋯ in the sidebar) shows session conversation threads and their progress. Human RPC:
`communication.get` returns grants, messages and subscriptions; `communication.set`
accepts `{grants, expected_grants}` and rejects stale edits. Each grant contains
`from_workspace`, `to_workspace`, optional `from_session`, `message`, `read_output`.

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
| `list_agents` | — | `{agents: [{id, name, kind, model, effort, persona, permissions, accepts_origins, autostart, home_dir, channel_id, session_id, state, detail, resumable, created_at, sleep_after_min, asleep, asleep_at, is_me}], your_origin: {origin, hop}}`. All agents, whatever the caller's workspace. `asleep`: its process was stopped while idle (state `exited`, detail `asleep`); asking it wakes it, which re-reads its whole conversation (docs/PROTOCOL.md "Sleep and wake"). Listing never wakes anyone. |
| `ask_agent` | `agent` (name, case-insensitive), `text`, `timeout_seconds? = 600` (max 1800) | `{agent, msg_id, session_id, reply, state, detail, timed_out, ended, origin, hop, sent_at_ms, started, woke, woke_note?, waited_ms, note?, unattributed_output?}`. Starts the agent if it has no live session (`woke: true` when it was asleep: it restarted into its conversation first), sends `[muster msg <id> from <sender> origin=<o> hop=<n>]` plus the text through the injection queue (`from = "session:<caller>"`), then waits for **this message's** turn: until the agent reports a final message attributed to it (Stop hook, Codex notify), or, after the message was typed in, stops on a prompt, question or error (`reply: null` and a `note`), or exits. If the agent goes idle after it with no attributed final message (3 s grace), `reply` is null and `unattributed_output` holds its last 60 terminal lines, with a `note` that they may belong to another prompt. The agent's `reply` to this message while the ask waits ends it with that text as `reply`. On timeout a `note` says a later answer comes as a new message. See "Agents" below. |
| `reply` | `msg_id`, `text` | `{msg_id, status, delivered, note, delivered_as, session_id, origin, hop}`. Answers a message delivered to the caller's agent after the turn that received it; see "Late replies" below. Errors (nothing stored, no rate-limit slot used): the caller isn't a named agent, the message wasn't delivered to it (unknown, another agent's, refused, still queued), it already has a reply, empty or over 100 000 chars, or starts with `!` (unless shell commands are allowed). |

`whoami` also returns `agent` (the caller's agent name, or null) and `origin: {origin, hop}`.

### Panels

Panels always belong to the caller's **task**, even when cross-workspace access is enabled.
Only `get_diff` is app-visible (`_meta.ui.visibility: ["app", "model"]`); the other tools
are model-only. The bridge's `get_diff` accepts only `max_bytes`, using the panel's task.

| Tool | Arguments | Result |
|---|---|---|
| `panel_open` | `title`, `app` (`markdown`, `image`, `chart`, `table`, `pr_decision`, `pr_overview`, `board`, `web_preview` or `custom`), `data?`, `html?`, `html_path?`, `csp?`, `kit?` (custom only, see [The kit](#the-kit-kit-true)), `notify_on_close?` (see below) | `{panel_id, note?}`; `note` only for a `web_preview` with a problem |
| `panel_update` | `panel_id`, `data?` (replaces the whole input), `html?`, `html_path?`, `title?`, `kit?` (custom only; switching it reloads the app) | `{note?}`; advances the revision |
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
in the system browser (native window) or a new tab. Every `panel_update` with data reloads the
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
| `note_list` | — | `{notes: [{id, title, rev, version, bytes, lines, preview, created_at, updated_at, updated_by, open, window_id, beside}]}`, no text (`preview` is the first non-blank line). |
| `note_read` | `note_id` | `{note_id, title, rev, text, updated_at, updated_by, open}`. |
| `note_write` | `text`, `title?`, `note_id?`, `rev?` | `{note_id, title, rev}`. Without `note_id`: a new note (not shown until `note_open`). With it: replaces the whole text (and the title, if given); `rev` is required and must be the current one, else the write is refused (`changed since revision …`): read again, redo the change, write with the new rev. |
| `note_append` | `note_id`, `text` | `{note_id, rev}`. Adds the text at the end, on a new line. No rev, never conflicts: use it while the human may be typing. |
| `note_open` | `note_id` | `{note_id, window_id}`. Shows the note as a tab beside the caller's terminal (in its window, moving it from another one). Refused for a hidden agent tab. |

Each write and append records the caller as `updated_by` (`{kind: "agent", name, session_id}`;
the name is a named agent's, else `Claude (task name)`), which the editor shows as "edited by …".
`note_write` and `note_append` share the post rate limit (30 per minute with posts and panel
updates), also when an agent runs `muster note` from its session (those calls run as these tools). There is no delete tool: only the human deletes notes. The human's unsaved typing is
never lost to an agent's write: the editor merges it onto the newer revision (docs/PROTOCOL.md
"Notes"). The same calls from the shell: `muster note …` (docs/CLI.md).

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
  scheduled job is unattended work, and a reply chain ends at hop 4 like any other. The reply
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
  | An ask still waiting for that message (`ask_agent`, `muster ask`, `agent.ask`) | It returns the text as its `reply` (it re-checks every second; after 3 s untaken, e.g. the ask was cancelled, the next row applies). | `returned` |
  | A named agent | A new agent message: `[muster msg <new id> from <replier> origin=<o> hop=<n>]`, then `Late reply to your message <id> ("<first line of the question>"):` and the text, with `reply_to` = the message answered. Through `agents::send_checked`: the asker's accepted origins, the hop limit, the daily and unattended caps, the OpenRouter spend caps, the injection queue; a stopped agent is started as for any message. A refusal is logged like any refused message. | `delivered` (`delivered_as`), else `refused` |
  | Another Claude or Codex session (live) | Typed in through its injection queue after `[muster reply to msg <id> from <replier> origin=<o> hop=<n>]` (no message row of its own, so not an envelope the agents' attribution acts on); the session takes the reply's origin and hop. A Claude session takes only `human`-origin replies (`send_message`'s rule). | `delivered` (`session_id`), else `refused` |
  | The human (a client, `muster ask` outside a session or in a shell tab), or a schedule | Shown in the agents panel's message log (the message's entry gets a `late reply` chip; its overlay shows the text and how it was delivered); a desktop notification when the human asked. | `shown` |
  | An agent since removed, a session that ended | Kept in the log only. | `undelivered` |

  A reply to an asker is a reply to that asker's own question, so two rules for starting work
  don't apply: the workspace rule (an agent may answer a project session that asked it) and
  the permission-widening check (a plan-mode researcher may answer a wider orchestrator). All
  the others do, and `reply` shares `send_message`'s rate limit (20 per minute).
- *Seen in:* `agent.messages` and `agent_message` events (`answer` on the message answered;
  the carrying message's `reply_to` points back), `muster events` (`late reply: STATUS`).

**Refusals are logged.** A message refused by the agent's policy (`ask_agent`'s permission
check, origin rule, hop limit, daily cap or unattended cap, and the same checks behind `muster ask`) is kept in
`agent_messages` with the reason and shown in the agents panel's log (`agent.messages`,
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

Reach: agents live in their own "agents" workspace (`<data>/agents`). `list_agents` and `ask_agent` work
from every workspace: an agent is a shared service, asking runs no command anywhere, and the origin
rule, hop limit, daily cap and permission check apply. Every other tool keeps the workspace rule,
so agents can read each other's terminals but not reach the user's project workspaces (unless
`MUSTER_MCP_CROSS_WORKSPACE=1`), and project agents don't drive an agent's terminal directly.

### Schedules

A named agent can schedule work for itself (docs/SCHEDULING_PLAN.md, Phase 3). The owner's rule:
an agent **proposes** and the human **approves** each proposal with a click in the Agents panel
(only there: no command, tool or script approves); the one exception is a **one-off reminder for the agent itself,
5 minutes to 24 hours ahead**, which runs without approval. Code: `src/schedules/agent.rs`.

| Tool | Arguments | Result |
|---|---|---|
| `list_schedules` | — | `{schedules: [{schedule_id, agent, name, prompt, when, status, next_run, made_by, last_run}], takes_scheduled_work, limits}`: the schedules that run on the caller or that it made. `status` is `proposed` (waiting for the human), `active`, `paused`, `expired` or `done`; `next_run` is a local time. |
| `propose_schedule` | `prompt`, `reason`, exactly one of `cron` / `at` (`"YYYY-MM-DDTHH:MM"` or unix ms) / `in_minutes`; `agent?` (default: the caller), `name?`, `tz?`, `conversation?` (`continue`\|`fresh`), `quiet_hours?`, `daily_cap?`, `note_id?` | `{schedule_id, status: "proposed", agent, when, estimate, note}`. A schedule that **doesn't run** until the human approves it. The human sees its prompt, timing, agent, the caller's `reason` and an estimate of its use (runs in the coming week after quiet hours and caps, what pays for them, the caps that bound them), and may edit or dismiss it. The agent it runs on must take scheduled work. Times only: no triggers, no "Then…" steps (those are the human's). A notice and desktop notification tell the human it waits. |
| `set_reminder` | `prompt`, `in_minutes` (5 to 1440) **or** `at`; `name?`, `tz?` | `{schedule_id, status: "active", runs_at, note}`. A one-off schedule for the caller only, no approval: Muster sends `prompt` then, into its current conversation, as scheduled work (origin `schedule`). The caller must take scheduled work. For a Claude subscription agent, quiet hours stay on and a time inside them is refused. |
| `pause_schedule` | `schedule_id` | `{schedule_id, status}`. Pauses a schedule that runs on the caller or that it made. Only the human resumes. |
| `delete_schedule` | `schedule_id` | `{deleted}`. Deletes a schedule the caller made (a proposal, a reminder, an approved proposal) and its history. A schedule the human made can only be paused. |

Rules (every one is checked in the tool, whatever the client):

- **Only the human approves, resumes or widens.** No tool approves, resumes, edits or makes a
  schedule directly; `schedule.update`, `schedule.create`, resuming and the settings are refused
  from an agent's session and from a shell an agent has typed into, and `schedule.approve` is
  taken only from a click in Muster's window (a WebSocket of the UI's that holds a window, whose
  program is known and isn't inside any session: never the CLI, `/api/rpc` or a script;
  docs/PROTOCOL.md "Schedules"). A schedule made or widened anywhere else (the CLI without a
  session id, `/api/rpc`, a script) is held for that click like a proposal, and settings that
  loosen a limit are refused there. To change an approved schedule an agent proposes a new one.
  (Agents run as the owner's user, so this raises the bar rather than closing it:
  docs/SCHEDULING_PLAN.md "Approval and its limits".)
- **Not from unattended work.** A session handling scheduled or triggered work (any chain a
  schedule or trigger started: its provenance's origin isn't `human`) can't propose or set a
  reminder. So a reminder can't set the next one, and a schedule can't breed more schedules; the
  agent can again once the human writes to it. An ask from another agent on a human-rooted chain
  is the human's work.
- **Only named agents**, and a reminder only for the caller itself; a proposal may be for another
  agent that takes scheduled work.
- **Limits per agent:** reminders at most 3 waiting, at least 15 minutes apart, 10 set per rolling
  24 hours (deleted ones count); proposals at most 3 waiting, 10 per rolling 24 hours (deleted or
  dismissed ones count); at most 5 live schedules it made (waiting, active, paused, or a one-off
  whose run is due but still waiting in line, which also counts as waiting and for the 15
  minutes). Calls are checked and recorded one at a time, so calls made at once (MCP and
  `muster schedule` together) can't overshoot a limit. The cron
  floor (every 15 minutes at most) applies as to the human's. Proposals and reminders share the
  post rate limit (30 per minute with note writes, panel updates and posts).
- **Once made (a reminder) or approved (a proposal), it is an ordinary schedule:** origin
  `schedule`, so the agent's accepted origins, the unattended cap (24 a day), the 200-a-day cap,
  the OpenRouter spend caps, quiet hours, a Claude subscription agent's 7-day expiry (from
  approval), the busy rule and the missed-run rule all apply when it runs.
- A proposal nobody approves is removed after 7 days; a reminder a day after it ran (its message
  stays in the agent's log). Every call is audited (`mcp_call`), and each schedule records who made
  it (`made_by`).

The same from an agent's shell: `muster schedule propose …` and `muster schedule remind …` inside
its session run `propose_schedule` and `set_reminder` as that agent, and `muster schedule pause`
and `muster schedule rm` run `pause_schedule` and `delete_schedule` (docs/CLI.md "Schedules").

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

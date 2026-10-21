---
name: muster-panels
description: "Show things to the user in a Muster panel beside the terminal (an image, a chart, a table, Markdown, a dashboard, a live preview of the web app you are running locally, or a small interactive app of your own) and get their clicks back. Use when the user asks for a visual view: to see something in a panel, a chart or plot, a table view, an image or screenshot shown, or the app you are building, e.g. \"chart this\", \"put it in a table\", \"show the screenshot\", \"show this in a panel\", \"show me the app\". Not for ordinary explanations. Needs a Muster session; for pull request reviews use muster-pr-review."
---

# Muster panels

A panel is a tab beside your terminal that shows a small sandboxed web app. You give it data;
the user looks, and can click or type back to you.

## Only inside Muster

Panels need a Muster session: `$MUSTER_SESSION_ID` is set and the muster MCP tools
(`panel_open` and friends) are available. If not, tell the user that panels need Muster and
show the result as text instead. (Inside a session, `muster panel ...` on the command line does
the same as the tools.)

## Tools

- `panel_open {title, app, data?, html?, html_path?, csp?, kit?, notify_on_close?}` →
  `{panel_id}`. A refused call names the bad field: fix it and call again. The user closing a
  panel does not prompt you: its `closed` event comes with the next panel prompt or
  `wait_for_panel_event`. Pass `notify_on_close: true` only when closing it is an answer you
  must act on (closing a `pr_decision` panel is not a decision).
- `panel_update {panel_id, data?, title?, html?, html_path?, kit?}`: `data` replaces the whole
  input, so send every field; the app updates in place.
- `panel_close {panel_id}`, `list_panels {}`.
- `wait_for_panel_event {panel_id?, timeout_seconds?}` → `{events, remaining, timed_out}`;
  call again while `remaining > 0`. Without a wait, events reach you later as a prompt headed
  `[panel "Title" #id]`; fetch a placeholder event with `panel_event {panel_id, event_id}`.
- Limits: 1 MiB of data or HTML per panel, 16 open panels per task. Panels belong to your
  task and close with it.

## Which kind of panel

Every panel reads like a short note from you: **the title with what it covers, the finding in
one sentence, the headline numbers, then the evidence, then where it came from.** Pick the
lightest kind that fits:

1. **A built-in** when one fits the data: `chart`, `table`, `image`, `markdown`,
   `web_preview` (and the PR apps). Give a `chart` or `table` a `takeaway` (one sentence,
   `**bold**` allowed), a `scope` and a `source`. The others refuse those fields: an `image`
   carries its finding in `caption` (and `alt`), a `markdown` panel in its first lines, and a
   `web_preview` in its title.
2. **A board** (`app: "board"`) for a dashboard or status page: plain JSON, no HTML, no
   script. Headline numbers, charts, lists, budget meters, a callout and buttons that send you
   events. This is the default for anything with several parts.
3. **The kit** (`app: "custom"` with `kit: true`) only when you need your own layout (a
   pipeline, a ring, a timeline, a custom table). You write HTML; Muster adds the styles,
   components, fonts and the handshake.
4. Plain custom HTML (no `kit`) only when you must control everything, and then you write the
   MCP Apps handshake yourself (below).

## Built-in apps

Pass `app` and `data`; the full schemas and examples are in Muster's docs/MCP.md, "Built-in
apps", and in the `panel_open` tool description.

- `markdown`: `{markdown}`, set for reading (checklists, `>` quotes, code with its language,
  tables), with a comment box the user can send back.
- `image`: `{path | url, alt?, caption?}`. `path` is a PNG, JPEG, GIF, WebP or SVG file in
  your worktree (max 8 MiB); `url` must be https. Give `alt`, a one-line description.
- `chart`: `{type: bar|line|scatter|area, title?, x: {label?, values}, series: [{name, values}],
  y?: {label?, min?, max?}}`; one value per x value, `null` for a gap, up to 12 series. Optional:
  `takeaway`, `scope`, `source`, `format` (`int`, `pct` for values already in percent, `usd`,
  `ms`, `s`), `highlight` (one series in colour, the rest grey), `annotations` `[{x, label}]`
  (a deploy, a flag), `stacked` (bar, area), `labels` (`auto|values|end|none`), `trend`
  (scatter), `empty` (what to say when there is no data), `stats` (up to 4 headline numbers:
  `{label, value, unit?, delta?, tone: good|bad|flat, note?, color?}`; `tone` says whether the
  change is good news). For many series, highlight the one that matters.
- `table`: `{title?, scope?, takeaway?, source?, columns: [{key, label?, align?, format?, tones?}],
  rows: [{key: value}], sort?: {key, dir}}`; `format` is `bar` (inline bar for a number column),
  `badge` (with `tones: {"Flaky": "warn", "Fixed": "good"}`) or `mono` (paths). The user can
  sort, filter and switch density.
- `board`: see below.
- `web_preview`: `{url}`, your running local dev server, live (e.g. `http://localhost:5173`).
  See below.
- `pr_decision` and `pr_overview`: see the muster-pr-review skill.

A chart, for example:

```json
{"type": "line", "title": "p95 latency", "scope": "billing-api · 14 days", "format": "ms", "highlight": "p95",
 "takeaway": "Spiked to **262 ms** after the v2.4 deploy; the cache fix brought it to **158 ms**.",
 "x": {"values": ["Oct 12", "Oct 13", "Oct 14", "Oct 15"]},
 "series": [{"name": "p95", "values": [188, 240, 262, 158]}, {"name": "p50", "values": [61, 74, 79, 54]}],
 "annotations": [{"x": "Oct 13", "label": "v2.4"}], "source": "Grafana export"}
```

## A board

`{title, scope?, takeaway?, source?, min?, blocks: [...]}`. Each block is
`{kind, span?: "full", title?, aside?, ...}`: `span: "full"` takes the whole row, otherwise
blocks flow in columns at least `min` px wide (default 300) and drop to one column when narrow.

- `stats`: `items: [{label, value, unit?, delta?, tone?: good|bad|flat, note?, trend?: number[]}]`
- `chart`: `spec`: exactly the chart app's input; `height?`
- `list`: `items: [{title, sub?, icon?, tone?, badge?, bar?: 0–1, value?, color?}]`
- `meters`: `note?`, `items: [{label, value, max, text?, of?, mark?}]`; `mark` is a tick (a
  pace line, a limit): say what it means in `note`
- `callout`: `{tone?: good|warn|bad|info, title?, text}`
- `actions`: `items: [{id, label, primary?, danger?, confirm?, icon?, key?, data?}]`. A click
  sends you the panel event `kind: id` with `data` (JSON types kept; keep it well under 16 KiB); `confirm: "Roll back production?"` makes
  it two-step. The button stays off until your next `panel_update`, so answer each event with
  an update.

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
   {"kind": "meters", "title": "Budgets", "note": "The tick is where spend should be today.",
    "items": [{"label": "Whole account", "value": 4480, "max": 7400, "text": "$4,480", "of": "of $7,400", "mark": 4774}]},
   {"kind": "callout", "tone": "warn", "title": "Postgres is ahead of pace.", "text": "It ends the month about $50 over budget."},
   {"kind": "actions", "items": [{"id": "explain", "label": "Why did Postgres grow?", "primary": true, "icon": "sparkle"}]}],
 "source": "billing export, 1–20 Oct"}
```

Muster refuses a malformed board and names the field: fix it and call again.

## Previewing the web app you are building

When you run a dev server (Vite, Next, `deno task dev`, `python -m http.server`) and the user
wants to see the result, open `web_preview` with its local address and keep working: the page
updates as the server reloads, and the user has Reload and Open in browser.

- Only `http(s)://localhost` or `127.0.0.1` with a port is accepted (prefer `localhost`; for
  a server on `[::1]` use `localhost` too, IPv6 addresses are refused). For any other page (a deployed site, GitHub, docs), do not use a panel: open it
  in the user's system browser (`xdg-open URL` on Linux, `open URL` on macOS) or give the link.
- If the server prints `0.0.0.0`, use `localhost` with the same port.
- Read `note` in the result: it says when nothing answers yet (start the server; the panel
  loads by itself once it is up), when the page refuses to be framed (allow framing in the dev
  server's config, or use the browser), or when it redirects to another address (pass the
  final local URL). No `note` means the server answered.
- To show another page or port, `panel_update {panel_id, data: {url}}`; any update reloads the
  page. A link or redirect to another origin stays blocked inside the panel.
- The user can type another local address into the panel's bar. You then get a panel event
  `navigated` with `{url, previous}`: the user is now looking at `url`, so talk about that
  page (and use it in later updates) instead of moving them back.

## Your own app

Set `app: "custom"` and pass `html` (or `html_path`, a file in your worktree).

### With the kit (`kit: true`): start here

Muster puts the Muster Kit in front of your HTML: the same styles, components, charts and fonts
as the built-ins, and the handshake. None of it counts toward your 1 MiB. Do not call
`ui/initialize` yourself. Start from `<main class="mk-page">` with an `mk-head` (a title with an
`mk-scope`, then a one-sentence takeaway), put numbers in `<mk-stat>` and charts in `<mk-chart>`,
use `data-send` for buttons (`data-confirm` for anything that can't be undone), and never write
colours: use the `--mk-*` tokens.

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

- Layout: `mk-page`, `mk-head`, `mk-title`, `mk-scope`, `mk-takeaway`, `mk-section` (+ `h2`),
  `mk-grid` (`--min`), `mk-wide` (whole row), `mk-row`, `mk-stack`, `mk-card` (`tone-good|warn|bad`).
- Tags: `<mk-stat label value unit delta tone note trend>`, `<mk-chart>` with the chart app's
  JSON, `<mk-spark values>`, `<mk-ring>` (`{segments, center, sub}`), `<mk-meter label value max
  mark>`, `<mk-icon name>`.
- Classes: `mk-badge good|warn|bad|info`, `mk-chip`, `mk-btn primary|danger|ghost|sm`,
  `mk-list`, `mk-steps` (`li[data-state=done|active|failed]`), `mk-timeline`, `mk-callout`,
  `mk-kv`, `mk-pre`, `mk-table`, `mk-empty`.
- Buttons: `data-send="kind"` plus any `data-*` sends you the event `kind` with those as its
  data (strings); `data-confirm="Roll back production to v2.3.2?"` asks first. A sent button waits for
  your next `panel_update`.
- Script, if you need it: `mk.onData(fn)` (your data, now and on every update),
  `mk.send(kind, data)` from a click handler, `mk.chart(el, spec)`, `mk.board(el, data)`,
  `mk.fmt(v, "usd")`.

### Without the kit

Your HTML is an MCP Apps (2026-01-26) page that talks JSON-RPC to `parent` with `postMessage`:

1. Send the request `ui/initialize` with `{protocolVersion: "2026-01-26", appInfo: {name,
   version}, appCapabilities: {}}`, then the notification `ui/notifications/initialized`.
   Without this handshake the panel shows an error after 10 s.
2. Your data arrives as `ui/notifications/tool-input` (`params.arguments`), again after each
   `panel_update`. Answer `ui/resource-teardown` with an empty result.
3. Send the user's action with `ui/message` `{role: "user", content: {type: "text", text},
   _meta: {muster: {kind, data}}}`. It is accepted only straight after a real click or key
   inside the panel, at most 3 per user action (more get error -32000: show "try again"), so
   send one message per deliberate action, from the click or key handler itself.
4. Open links with `ui/open-link {url}` (http or https, also after a user action).
5. Theme: `hostContext.theme` (light or dark) in the initialize result and
   `ui/notifications/host-context-changed`.

Either way, the app runs sandboxed on its own origin: no `fetch`, no network, no nested frames. Inline
scripts and styles work. To load images, scripts, styles or fonts from a CDN, declare its
origin: `csp: {resourceDomains: ["https://cdn.example"]}`. Everything the app shows that came
from outside (issue text, web pages) must go in as text (`textContent`), never as HTML.

---
name: muster-panels
description: "Show things to the user in a Muster panel beside the terminal (an image, a chart, a table, Markdown, a live preview of the web app you are running locally, or a small interactive app of your own) and get their clicks back. Use when the user asks for a visual view: to see something in a panel, a chart or plot, a table view, an image or screenshot shown, or the app you are building, e.g. \"chart this\", \"put it in a table\", \"show the screenshot\", \"show this in a panel\", \"show me the app\". Not for ordinary explanations. Needs a Muster session; for pull request reviews use muster-pr-review."
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

- `panel_open {title, app, data?, html?, html_path?, csp?}` → `{panel_id}`. A refused call
  names the bad field: fix it and call again.
- `panel_update {panel_id, data?, title?, html?, html_path?}`: `data` replaces the whole
  input, so send every field; the app updates in place.
- `panel_close {panel_id}`, `list_panels {}`.
- `wait_for_panel_event {panel_id?, timeout_seconds?}` → `{events, remaining, timed_out}`;
  call again while `remaining > 0`. Without a wait, events reach you later as a prompt headed
  `[panel "Title" #id]`; fetch a placeholder event with `panel_event {panel_id, event_id}`.
- Limits: 1 MiB of data or HTML per panel, 16 open panels per task. Panels belong to your
  task and close with it.

## Built-in apps

Pass `app` and `data`; the full schemas and examples are in Muster's docs/MCP.md, "Built-in
apps", and in the `panel_open` tool description.

- `markdown`: `{markdown}`, with a comment box the user can send back.
- `image`: `{path | url, alt?, caption?}`. `path` is a PNG, JPEG, GIF, WebP or SVG file in
  your worktree (max 8 MiB); `url` must be https. Give `alt`, a one-line description.
- `chart`: `{type: bar|line|scatter|area, title?, x: {label?, values}, series: [{name, values}],
  y?: {label?, min?, max?}}`; one value per x value, `null` for a gap, up to 12 series.
- `table`: `{title?, columns: [{key, label?, align?}], rows: [{key: value}], sort?: {key, dir}}`;
  the user can sort and filter.
- `web_preview`: `{url}`, your running local dev server, live (e.g. `http://localhost:5173`).
  See below.
- `pr_decision` and `pr_overview`: see the muster-pr-review skill.

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

## Your own app

Set `app: "custom"` and pass `html` (or `html_path`, a file in your worktree). It is an MCP
Apps (2026-01-26) page that talks JSON-RPC to `parent` with `postMessage`:

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

The app runs sandboxed on its own origin: no `fetch`, no network, no nested frames. Inline
scripts and styles work. To load images, scripts, styles or fonts from a CDN, declare its
origin: `csp: {resourceDomains: ["https://cdn.example"]}`. Everything the app shows that came
from outside (issue text, web pages) must go in as text (`textContent`), never as HTML.

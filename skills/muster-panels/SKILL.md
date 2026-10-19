---
name: muster-panels
description: "Show things to the user in a Muster panel beside the terminal (an image, a chart, a table, Markdown, or a small interactive app of your own) and get their clicks back. Use when the user asks for a visual view: to see something in a panel, a chart or plot, a table view, or an image or screenshot shown, e.g. \"chart this\", \"put it in a table\", \"show the screenshot\", \"show this in a panel\". Not for ordinary explanations. Needs a Muster session; for pull request reviews use muster-pr-review."
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
- `pr_decision` and `pr_overview`: see the muster-pr-review skill.

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

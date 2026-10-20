# Agent panels

Status: core built on `feature/agent-panels`, 2026-10-18. The remainder of this document is
the approved broader plan; current tools and limits are in [MCP.md](MCP.md#panels).

Core includes durable panels/events, task-scoped MCP and CLI, tab placement/movement, the
two-frame MCP Apps bridge, native IPC capability, and the Markdown app. Built on
`feature/panel-apps`: the phase 1 `image`, `chart` and `table` apps, the phase 2
`pr_decision` app and the PR reviewer template (schemas in [MCP.md](MCP.md#built-in-apps)).
Built on `feature/pr-overview`: the `pr_overview` app (all open PRs of a repo), pictures and
charts in `pr_decision` (`media`), Skip and Hold, and the user's own actions in both
(["Open PRs, end to end"](#open-prs-end-to-end)). Built on `feature/pr-flow-ux`: questions
answered in place, one event carrying the answers and options, a verdict and undo line,
quick-win merging from the overview, and a skill that prepares PRs ahead
(["Fewer round trips"](#fewer-round-trips)).
Built on `feature/web-preview`, 2026-10-20: the `web_preview` app, a local dev server live
beside the agent's terminal (["Showing web pages"](#showing-web-pages); schema and limits in
[MCP.md](MCP.md#built-in-apps)). The native embedded browser stays deferred.
Built on `feature/preview-address`: the user can type a local address into the preview's bar;
it is checked like the agent's, stored, and reported to the agent as a `navigated` event.
Built on `feature/panel-design-b`, 2026-10-20: the "Briefing" look (direction B, picked by the
owner) for every built-in panel and the host's panel chrome, drawn by one shared kit
(`web/src/panels/kit/`) with Inter and JetBrains Mono bundled; optional takeaway, scope, source
and headline-number fields on chart and table, more chart options (formats, highlight,
annotations, stacking with negatives, trend, empty), a colour from Muster's own merge check on
the PR decision verdict; and the agent toolkit: the `board` app (a dashboard from JSON, no HTML)
and the kit for custom apps (`kit: true`; [MCP.md](MCP.md#the-kit-kit-true)).
The `diff` app, fetch approval and placement options remain later phases. Nested frames are
blocked for agent-written apps even for declared loopback dev servers: a child document's
independent CSP would otherwise bypass the no-fetch policy. `web_preview` does not reopen that:
it is a separate built-in that the UI draws itself, outside the sandbox proxy, framing only the
dev server's URL under a CSP held to that origin. `connectDomains` are stored without granting access.
Each panel data revision is treated as a fresh tool input/result invocation in the same frame;
the MCP Apps spec normally sends input once per invocation. The gesture rule is checked in the
sandbox proxy and was verified in Chromium and in WebKitGTK 2.54 (run under GTK's Broadway
backend): a tab click lets nothing through, one click inside the app lets at most 3 events through.

Agents should be able to show things next to their terminal: images, charts, a rich diff with
their notes on it, a live preview of the web app they are building, and interactive components
they write themselves. The user clicks, picks, comments or approves in the panel, and the agent
gets that as a message. This serves the premise in [PLAN.md](../PLAN.md): every feature should
reduce how often the human has to read a raw terminal.

The first use case is getting through pull requests quickly: the agent reads the PR and shows
only what the user needs to decide (what changed and why, the risk, the checks, the few changes
worth looking at). The user clicks Merge, and the agent runs `gh pr merge`.

## What a panel is

- **A tab.** A panel is a new kind of tab in the existing split/tab layout ([UI.md](UI.md)),
  next to terminal tabs. It can be split beside its agent's terminal, dragged to another group,
  moved to another window, zoomed or shown fullscreen (Alt+Shift+Z or the button in the strip
  above it; Esc the app leaves alone goes back, see [UI.md](UI.md)), with no new layout code. It belongs to a task (channel) and
  remembers which session opened it. "Side panel" is just the default placement: a new panel
  opens in a split to the right of its agent's terminal.
- **An app.** Every panel's content is a small web page in the [MCP Apps](https://modelcontextprotocol.io/extensions/apps)
  format (HTML served as `text/html;profile=mcp-app`, stable spec 2026-01-26, Apache-2.0 SDK
  `@modelcontextprotocol/ext-apps`). Muster's built-in panels (image, chart, Markdown, diff
  review) are apps Muster ships; an agent can also write its own. One runtime, one security
  model, and agent-written apps also work in other hosts (Claude, ChatGPT, VS Code, Cursor).
- **Muster is the host.** Claude Code and Codex are the MCP clients and render nothing. Muster
  is the MCP server they call and also the UI, so it renders the apps its own tools return.
  The spec does not cover a host that is not the model's client; the return path below is
  Muster's own.

## How an agent uses it

New MCP tools (docs/MCP.md), with matching `muster panel ...` CLI commands for scripts:

| Tool | What it does |
|---|---|
| `panel_open` | `{title, app, data?, placement?}` → `{panel_id}`. `app` is a built-in (`image`, `chart`, `markdown`, `table`, `diff`, `pr_decision`, `web_preview`) or the agent's own HTML (inline, or a file path in its worktree). `data` is the app's input (an image path, a Vega-Lite spec, a diff with notes). |
| `panel_update` | `{panel_id, data? , html?}`: new input, or a new version of the app. The panel updates in place and keeps its scroll and focus where the app allows. |
| `panel_close` | `{panel_id}` |
| `wait_for_panel_event` | `{panel_id?, timeout_seconds?}`: blocks until the user does something in a panel, like `wait_for_state`. Returns the events. |
| `list_panels` | The caller's task's panels. |

Built-ins come first because they make the common cases fast and consistent: an agent passes data
instead of writing UI every time. Agent-written apps cover everything else.

## Getting the user's actions back to the agent

A click in a panel becomes an event (`{panel_id, kind, payload}`, e.g. `approve_hunk`,
`comment`, `submit_review`, or an app's own `ui/message` text). Events queue per panel and reach
the agent one of two ways:

- If the agent is waiting in `wait_for_panel_event`, the call returns them. This works mid-turn:
  "open the review, then wait for the user's decisions".
- If it isn't, Muster delivers them as a prompt through the existing injection queue once the
  agent is idle, marked as coming from the panel: `[panel "PR #812 review"] approved 6 of 9
  hunks; comments on src/auth.rs:40 ...`. The queue already handles busy agents, restarts and
  backend reloads.

An app can only send an event in direct response to the user (a click or key in the panel). A
panel that renders a stranger's PR must not be able to type instructions to the agent on its own.

## Security

The panel's code is written by an agent, and may display text written by a stranger (a PR, an
issue, a web page). Both must stay contained.

- **Separate origin.** The UI on `127.0.0.1:<port>` holds the client token, which can do
  anything (start agents, change permissions, shut down). Panels run on a second origin: a small
  panel server on another port that serves only the sandbox proxy page (and the web preview's
  frame page) and the apps, with the
  CSP sent as an HTTP header. It holds no token and cannot reach the daemon's API. This is the
  two-iframe "sandbox proxy" model the MCP Apps spec requires of web hosts.
- **A narrow bridge.** Through `postMessage`, an app may: receive its input and updates, report
  its size, send user events (above), open a link in the system browser, and call the few Muster
  tools marked app-visible, scoped to its own task (e.g. read a file in the task's worktree,
  `get_diff`). Nothing that starts, stops, messages or reconfigures sessions.
- **Network.** By default (the MCP Apps default), an app loads nothing from the internet and
  cannot `fetch`. An app declares the domains it needs (`_meta.ui.csp`). Scripts, styles, fonts
  and images from declared domains (CDNs) load without asking. Fetching data (`connect-src`)
  from a declared domain needs the user to allow it once per panel; until then those requests
  are blocked and the panel shows what it asked for. Undeclared domains are always blocked.
- **The kit.** With `kit: true`, Muster puts kit.css and kit.js (and the fonts, as `data:`
  URIs) in front of a custom app's HTML when it serves it. It runs inside the same sandbox with
  the same CSP; the only CSP change for the kit is `font-src data:`, which reaches no host. The
  kit has no `fetch`, XHR or frames, and its buttons send events through the same gated
  `ui/message` as any app. The stored HTML stays the agent's own, so the 1 MiB limit is unchanged.
- **The native window.** WebKitGTK reports the top-level page's address for IPC messages from any
  frame (see the note in src/gui.rs), so today a framed page could close the window or toggle its
  title bar. Before panels ship, IPC must carry a capability that only the main page has.
- **Agents.** Opening a panel is an MCP call like any other, rate-limited per caller, and
  subject to the task's permission profile (a read-only agent can still open panels; it shows,
  it doesn't change anything).

## Showing web pages

- **Local dev servers work in a frame.** An agent building a web app can show it running in a
  `web_preview` panel (`http://localhost:5173`), live, beside its terminal. Most dev servers allow
  framing. (Built on `feature/web-preview`.) Only loopback http(s) URLs are accepted, never
  Muster's own ports, checked when the panel opens or changes and again when the frame's page
  is served. The panel server's `/preview` page frames the URL with a `frame-src` naming only
  its origin, so a framed page cannot navigate or redirect off its dev server (to a login site,
  another port or Muster itself); the user follows it with Open in browser. Muster's own pages
  refuse to be framed at all. Muster checks the server itself (is it up, does it refuse
  framing, does it redirect away) and tells both the agent (a `note` on `panel_open`) and the
  user (a notice in the panel), since a cross-origin frame gives the UI no signal.
- **Most logged-in sites refuse to be framed.** github.com sends `X-Frame-Options: deny`, and so
  do many others. A frame cannot show them.
- **A real embedded browser is possible, in the native window only, and deferred.** wry can put extra native
  webviews in the window (`build_gtk` with a `gtk::Fixed`, works on Wayland). Costs: it always
  draws above Muster's own HTML (menus and dialogs can't overlap it), its position must follow
  every layout change, it has its own cookie store to log into, and neither a browser tab nor the
  phone can have one. An agent could drive it (navigate, read, screenshot).
- **Everywhere else**, a page that can't be framed opens in the system browser.

For PR review none of this is needed: Muster renders the diff from data, which is faster and
lets the agent annotate it.

## PR decision, end to end

The goal is a decision in under a minute, not a line-by-line review.

1. "Look at PR 812." The agent runs `gh pr view 812 --json ...`, `gh pr diff 812` and
   `gh pr checks 812`, reads the changed code in its worktree, and decides what the user needs to
   see.
2. `panel_open {app: "pr_decision", title: "PR #812", data: {...}}`. The panel opens beside its
   terminal and shows, top to bottom:
   - the title, author, branch, and what will be merged into what;
   - a plain summary: what changed and why;
   - the agent's risk call (low / medium / high) and its reasons;
   - checks: passing, failing or pending, and review state;
   - only the changes worth looking at, as small diffs with the agent's note on each, and a
     count of the rest ("31 more lines of tests and renames");
   - the agent's open questions, if any, answered in place (see "Fewer round trips" below).
3. Actions, each one key: **Merge** (with the repo's merge method, e.g. squash, and whether to
   delete the branch), **Request changes** (with a comment), **Ask** (a question; the answer
   appears in the panel), **Open on GitHub** (system browser).
4. On Merge, the panel shows the exact command first, then the agent, waiting in
   `wait_for_panel_event`, runs it and reports the result in the panel:
   `gh pr merge 812 --repo owner/name --squash --delete-branch --match-head-commit <sha>`.
   `--match-head-commit` binds the merge to the commit the user saw: if anyone pushed to the PR
   after the panel opened, GitHub refuses the merge and the agent refreshes the panel instead.
   The Merge button is disabled while checks are failing, unless the user holds Shift to merge
   anyway.

Merging is outward-facing and hard to undo, so the rule for this app is strict: the agent merges
only on a Merge event from the panel, which only a user's click or key can send, and only with
the head commit shown in that panel. The agent's own CLI may still ask for permission to run
`gh`, depending on its permission profile; that is a second check, not a replacement.

Agents get these instructions two ways, from one source text (`skills/muster-pr-review/SKILL.md`):
the `muster-pr-review` skill (`muster skills install`), which any Claude Code or Codex session
loads when asked to "review PR 812", and the "PR reviewer" agent template, which is that text
under a short persona. A skill keeps the PR's text in the current agent's context; a fresh
template agent isolates it from other work. A `muster-panels` skill covers the other apps.
A full hunk-by-hunk `diff` app, for PRs that need real review, can follow.

## Open PRs, end to end

"Help me review the open PRs" (the user's words, 2026-10-19): the agent pulls pictures and
writes descriptions so the changes are quick to take in, and the user decides with a click:
merge, hold (put it aside to come back to later), or options they asked for at the start.

1. The agent lists the open PRs (`gh pr list --json ...`, shaped by `--jq` to the panel's
   fields), adds a risk guess and a one-line summary to each without reading diffs, and opens
   a `pr_overview` panel: one card per PR with its size, age, checks, review state, labels,
   risk and summary, sortable (riskiest, quick wins, oldest) and filterable.
2. The user opens a PR (Enter), or presses Next (N) to go through them in the order they see.
   The agent marks it `reviewing`, runs the one-snapshot review above and opens its
   `pr_decision` panel, now with `media` when it helps: images from the PR description, a
   screenshot of a UI change's preview, a small chart of lines changed per area. Pictures are
   files: the agent downloads them from GitHub's own image hosts only, with the URLs taken from
   the PR body by `grep` (never typed into a command), into a fresh private `mktemp -d`
   directory that it deletes afterwards. The risk, checks and reviews stay above the pictures.
3. The user merges, requests changes, skips (S) or holds (H). The agent updates the PR's card
   (`merged`, `queued`, `changes_requested`, `held`), closes the decision panel and returns to
   the overview, or, after Next, goes on to the next PR. Held PRs gather at the end.
4. Options the user asked for at the start ("also give me a Needs QA button") become
   `actions` in both panels, shown apart as "Your actions": a button (and key) with a plain
   description of what the agent will do. Every action says whether it is `outward` (changes
   GitHub or anything off the machine); those ask to confirm, showing that description. A
   label may not copy a built-in button's name. The agent does exactly that and reports it on
   the card.

The merge rules above are unchanged: only a Merge event merges, with the head commit the
panel showed; Skip, Hold, custom actions and the overview's other events never merge. Since
`feature/pr-flow-ux` a quick-win card in the overview can send that Merge event too (below).

## Fewer round trips

The user's feedback (2026-10-19): answering the agent's questions took too much friction (they
sat at the bottom, and the user had to open Ask and type them out), custom buttons went to the
agent one by one, each PR took minutes to build, and the questions were often so full of
jargon that the user had to ask what they meant. Decided:

1. **Questions answered in place.** `questions` are `{id, text, why?, kind: yes_no | choice |
   text, options?, recommended?, required?}` (a plain string still works, as a `text`
   question). Each shows as a small form after the risk, checks and reviews: buttons for the
   options with the recommended one picked and marked, number keys to pick, Tab to the next
   question, a short text box only for `text`. Answers are a local draft.
2. **One event carries everything.** Merge, Request changes, Hold and Skip send the draft's
   `answers` and `options` with them; Send answers (Ctrl+Enter) sends the draft alone. The
   user's own actions gain `mode`: `toggle` (default) options switch on and ride along with
   the next send, `now` actions send at once as before (outward ones still confirm first;
   an outward toggle confirms before it is switched on). Merge's confirm lists what goes
   with it, and a change to the draft cancels it. One message per send keeps every action
   within the 3-messages-per-click budget. Ask stays for the user's own questions.
3. **Plain English.** The skill requires plain, short, jargon-free text everywhere, one
   sentence per question with a one-line `why`, choices with a recommendation whenever
   possible, and "Writing for the user" examples.
4. **Verdict and undo first.** `verdict` (one sentence: "Safe to merge: small bug fix, tests
   pass") and `undo` ("If this goes wrong: revert the merge commit") open the panel.
5. **Quick wins.** After the full one-snapshot review of a PR finds it small, green and low
   risk, the agent marks its overview card `quick_win` with that snapshot's `head_sha`,
   `base` and a `verdict`. The card then has Merge, with the same confirm, exact command and
   event shape as pr_decision; any change to the card cancels the confirm, and the agent runs
   every pre-merge re-check. "Quick wins" filters and sorts them first.
6. **Speed.** The skill uses `muster pr list` and `muster pr snapshot` (one read-only command
   each instead of many `gh` calls; the gh recipe stays for older Muster), prepares the next
   PRs while the user reads (cards marked `ready`), and for 6 or more PRs may start helper
   sessions on a faster model that only write draft judgements to files. Those are one
   untrusted source: the main agent runs `muster pr snapshot` itself for every PR before
   using one, checks the draft against it, and marks quick wins only from its own snapshot and
   a complete diff (no truncated files, complete file list and checks); helpers never act on
   GitHub.

## Persistence

A panel lives under the data directory per task: the app, its latest input, and its event queue.
It survives window reloads and backend restarts, and closes with its task. Size limits per panel
(input and app) keep a runaway agent from filling the disk. Keeping a few earlier versions, so the
user can step back through what an agent showed, is cheap and can come later.

## Phone

Panels are web pages, so the phone PWA can show them too: review a PR from the phone. The phone's
device scopes ([ANDROID.md](ANDROID.md)) decide whether it may send events or only view.

## Phases

1. Panel tabs, the second origin and sandbox, `panel_open`/`update`/`close`, built-in `image`,
   `markdown`, `chart`, `table`. One way: the agent shows, the user looks. (Built; the apps
   on `feature/panel-apps`.)
2. Events back to the agent (`wait_for_panel_event` and the idle prompt), the `pr_decision` app,
   the PR reviewer template. This is the first use case, end to end. The `diff` app follows.
   (Built except `diff`; `pr_decision` and the template on `feature/panel-apps`.)
3. Agent-written apps: arbitrary HTML/JS, declared network access, app-visible tools.
4. `web_preview` for local dev servers (built on `feature/web-preview`); then, if wanted, the
   native embedded browser.
5. Panels on the phone.

## Decisions (user, 2026-10-18)

1. **Placement:** panels are tabs in the existing layout, opening in a split to the right of the
   agent's terminal, movable like any tab. (Not a fixed dock.)
2. **Web pages:** local dev-server previews in frames, and the system browser for everything
   else. The native embedded browser is deferred.
3. **Network for agent-written apps:** scripts, styles, fonts and images from declared domains
   load without asking; fetching data needs the user's OK once per panel.
4. **First use case:** the `pr_decision` app (show what's needed to decide, then Merge runs
   `gh pr merge --match-head-commit`), ahead of a full hunk-by-hunk review.

Also settled: MCP Apps as the panel format; events reach the agent through
`wait_for_panel_event` or, when it is idle, as a marked prompt; apps send events only on a user
action; the phone comes last.

## Known gaps

- `wait_for_panel_event` and `panel_event` delete the events they return as the result is
  built. If the backend dies, or the caller's connection drops, before the caller reads it,
  those events are lost. Chosen over redelivering events the agent may already have acted on.
- `web_preview` refuses IPv6 literals (`[::1]`): a CSP cannot name an IPv6 host, so such a
  frame could not be held to its dev server. `localhost` reaches a server on ::1 too.
- `web_preview` checks read only the first response head (https servers are only connected
  to), so a page can still fail in the frame for reasons the panel cannot name, and Muster's
  keyboard shortcuts do not reach a focused dev page.
- On macOS, `html_path` is checked by path before opening, and nothing checks the opened file
  afterwards, so swapping a parent directory for a symlink in between could copy a file from
  outside the task cwd into a panel. Linux checks the opened descriptor; Linux is the target.

- The Briefing look and the kit were checked in headless Chromium (the panel e2e, both themes,
  about 360 px and 1440 px wide). They use only features WebKitGTK 2.54 supports (ES2020, CSS
  custom properties, `color-mix()`, container queries, `ResizeObserver`, inline SVG; the
  `-webkit-` prefix for `backdrop-filter`). A WebKitGTK MiniBrowser run under Broadway
  confirmed the kit's script runs, container queries and `color-mix()` are supported and both
  `data:` fonts load; layout could not be judged there (Broadway reported a broken viewport), and
  the native window itself was not run on this branch.

## Tools

- `muster pr list` / `muster pr snapshot` (docs/CLI.md, "PR snapshots"): read-only, parallel GitHub fetches whose JSON fits `pr_overview` and `pr_decision` as is, so the agent adds only its judgement.

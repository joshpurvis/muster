# Muster UI spec (v2: split groups, each with its own tabs)

Goal: a great tiling terminal for agents, in the spirit of VS Code editor groups, kitty and
wezterm. A window is split into **groups**, and each group has its own **tab strip**. A tab is
one terminal. Tabs drag freely between groups, onto a group's edge (a new split), and to other
windows. One fuzzy Jump covers everything. There is no inbox, timeline, composer, diff panel or global
tab bar; the only sidebar is the optional agents panel (Alt+G). Attention comes from state dots, the approval banner, desktop
notifications and a "next that needs me" key.

Several windows may be open at once (one muster client each), so the window manager can tile
them. The daemon owns every terminal. Closing a window never kills anything, and reopening
restores that window's groups and tabs.

## Concepts

- **Window**: one client window, numbered 1, 2, 3... It owns a **layout**: a split tree whose
  leaves are groups (persisted by the daemon, `Window.layout`, see docs/PROTOCOL.md).
- **Group**: a leaf of the window's split tree, with its own tab strip and one visible tab. The
  layout JSON calls it a `pane` (`{type:"pane", id, tabs: session ids, active}`); v1's pane code
  and drag and drop carry over, now at window level.
- **Tab**: one **session** (a Claude, Codex or shell terminal). Each session belongs to one
  window (`SessionInfo.window_id`).
- **Task**: what the API calls a **channel**, no longer shown as its own UI. It holds a
  session's folder (`cwd`), optional git worktree and branch, permissions profile, port block
  and the auto-derived name. A new tab starts a new task; "new terminal here" (Alt+N, splits)
  starts another session in the focused tab's task, so a shell next to an agent shares its
  worktree. When a task's last session closes, the daemon closes the task (its worktree is kept
  unless removed explicitly).
- **Workspace**: a folder the daemon knows about. Not shown as UI.

## Visual language

Unchanged from v0. Dark by default with a light theme (`prefers-color-scheme` plus a manual
toggle). All colors are CSS custom properties on `:root`. State colors: starting gray
(pulsing), working `#4ea1ff`, needs_approval / asking_question `#f0a020`, idle `#3fb950`,
error `#f85149`, stuck `#e0823d`, exited `#6a6a76` (hollow dot). Inter 13px UI, JetBrains Mono
13px terminal, no web font downloads. Radii 6/10px, hairline borders, 120–160ms motion,
`prefers-reduced-motion` respected, inline SVG icons.

## Layout

```
┌ group ─────────────────────────────────────────┬ group ──────────────────────────────────┐
│ ● fix auth ⎇ ×│ ◐ fix auth · shell │ +         │ ● new api 🛡 │ ● logs │ +  ◫ ⊟ 🔍 ⋯      │
│ [approval banner when needed]                  │ codex                                   │
│ claude                                         │                                         │
│                                                ├ group ──────────────────────────────────┤
│                                                │ ● tests │ +                             │
│                                                │ shell                                   │
└ status line (22px, optional): ~/p/muster · ⎇ muster/fix-auth · 4 tabs  needs you: 1  ● connected ┘
```

### Group tab strips
- Every group has a tab strip (31px), always shown, even with one tab. Tabs show the state dot,
  the kind icon, the label, an optional `⎇` (worktree) and permissions icon (amber for network
  or accept edits, red for full access or bypass; both on every tab of the task), and `×` on
  hover.
- **Label**: the task's name (auto-derived, muted italics while it's only the folder name). When
  the task has several sessions, a secondary muted part names the session (`fix auth · shell`);
  the task part is truncated first.
  Tooltip: full name, folder, branch, state detail.
- The active tab of the focused group has an accent top bar; other groups' active tabs are
  muted. Unread activity shows a bold label.
- Double-click renames exactly what the tab shows. A tab showing one name edits that name: the
  task's, or the session's when its task is unknown. A tab showing `task · session` edits both,
  as two fields `[task] · [session]`, from any of the task's tabs. Each field has a small muted
  label before it, "Task" or "Tab" (a session is a tab), also with one field; in a very narrow
  strip (300px or less) the labels shrink but stay. The inputs are named "Task name" and "Tab
  name". The
  cursor starts in the task
  field on the task's main tab (its first agent session, else its first session), in the session
  field otherwise. Tab / Shift+Tab move between the fields, Enter renames whatever changed, Esc
  cancels, clicking away commits. Blank or unchanged fields are left alone. After Enter or Esc
  the keyboard goes back to the tab, or to the terminal when the rename came from its menu or the
  palette. While renaming, the tab keeps room for its fields; in a narrow strip it takes the whole
  strip. Middle-click
  closes. `+` at the end of each strip opens a new tab in that group (same folder as the group's active tab, no worktree, last agent kind); `▾` next to it:
  New tab…, New worktree tab, New terminal in this task, and Claude / Codex / Shell.
- The **top-right group's** strip ends with the window controls: Split right `◫`, Split down
  `⊟` (acting on the focused group), a chip naming the window (only with 2+ windows; click
  renames), Jump `🔍 Ctrl Space`, and `⋯` (Rename window…, Theme ▸, Status line, Terminal gets
  Alt keys, Jump focuses the other window, System title bar (native window only), Command
  palette, Keyboard shortcuts, then Reload window, Close window, Shut down Muster…), plus a
  "Reconnecting…" pill when disconnected. The top-right group is the one touching the window's
  right edge highest up; a zoomed group carries the controls; narrow strips keep only Jump and
  `⋯`.
- Strips scroll sideways past their width (wheel scrolls, active tab kept in view) with an
  `N ▾` overflow list. Roving tabindex: ←/→/Home/End, Enter focuses the terminal, F2 renames.
- Context menu: Rename (same as double-click), Permissions… (task), Copy path, Copy branch, New
  terminal in this task, Split right ▸ / Split down ▸ (Move this tab, or a new Claude / Codex /
  Shell in this task there), Move to window ▸ (existing windows plus
  "New window"), Restart, Interrupt, Close tab, and when it is the task's last session with a
  worktree, Close + remove worktree (confirmed in a dialog). Restart and Interrupt, here and in
  the terminal menu, ask first while an agent is mid-turn (working, stuck, needs approval or asking):
  "Claude is working" / "Codex is working", "Stop its current turn?" or "Restart it? Its
  current turn is lost and the program starts fresh.", with Cancel focused. Shells and idle
  agents don't ask, nor do the palette commands and shortcuts.

### Agent panel tabs

Agents open built-in apps (Markdown, Image, Chart, Table, PR decision, PR overview) or custom MCP Apps
beside their terminal; the strip above the frame names which ("Panel · Chart"). A new panel joins the group
to the opener's right, or creates that split. Its tab has a panel icon, title and close button;
dragging, splitting, moving between windows, keyboard tab selection and group zoom work as
for terminals. The menu includes Reload panel and Close panel. Closing sends `closed` to the
opening agent; archiving the task removes all its panels. The phone UI ignores panels.

Panel input updates retain the frame; changed HTML and Reload panel replace it. A panel's app
lives in one layer per window, drawn over its tab's place in the layout, so switching tabs,
zooming, splitting and dragging the tab to another group keep the app and anything typed in it.
Moving a panel to another window, reloading the window or a backend restart load the app again
from its last input: state the app kept only in the page (an unsent comment) is lost there.
Theme changes reach the app through the bridge. The built-in Markdown app renders text safely
and includes a comment field for sending a user event.
The other built-ins follow the theme and work by keyboard: Image toggles fit/actual size
(Z); Chart shows values on hover or with the arrow keys and has a "Show data table" link;
Table sorts by header click and filters (`/` focuses the box, Esc clears it); PR decision
has Merge (M), Request changes (R), Ask (A), Open on GitHub (O), Skip (S), Hold (H) and the
user's own actions (their key on the button, in a separate "Your actions" group that folds in
small panels), and its pictures enlarge on click or Enter in a modal view (Z for actual size,
drag to pan, arrows step, Esc closes); PR overview moves with J/K or the arrows, opens with
Enter (or O, or a double-click), holds with H, goes through the PRs in order with N, and
filters with `/`. In both, `?` lists the keys. An outward custom action (marked ↗) shows what
the agent will do and needs Enter or Confirm, like Merge. Merge first shows the
exact `gh pr merge` command and needs a fresh Enter or "Confirm merge" (a held key never
confirms; a change to the PR cancels the step); with failing checks it is disabled unless
Shift is held (Shift+M), and a hint beside it says so. Esc closes the confirm or a comment
box and returns to its button; Ctrl+Enter sends a comment or question. The latest action's
result shows above the action bar.

An app sends a message or opens a link only right after a real click or key inside it: the
sandbox proxy checks its own user activation (selecting the tab or focusing the panel does not
count) and passes at most 3 such requests per action; the host also rate-limits them. Links
open in the host browser.

While a panel has focus, Muster's navigation shortcuts still work: Jump (Ctrl+Space, Ctrl+K),
the palette (Ctrl+Shift+P), Alt+J, tab switching (Alt+1..9, Ctrl+Tab, Alt+[ / ]), Alt+Shift+[ / ],
Alt+arrows between groups, Alt+Z, Alt+G, Alt+/ and F1, and Alt+W, which closes that panel.
In the app's text fields, Alt+letters and Alt+arrows stay with the field, as in Muster's own.
Commands that act on an agent (approve, interrupt, new tabs) are not taken from a panel. Closing
a focused panel, or the agent closing it, hands the keyboard to the tab that takes its place.

A panel shows "Loading panel…" (with the app's first script error, if any) until the app
completes the MCP Apps handshake. After 10 s it shows what stalled (the sandbox did not load,
the app never sent `ui/initialize`, or it never sent `ui/notifications/initialized`) with the
app's last error and Reload panel. A load cut off by a dropped connection retries on reconnect.
Jump lists every panel by title (pulling one from another window), and the status line counts
panel tabs with terminals.
Saved layouts still use numbers: positive session IDs, panel tab IDs `-(panel_id + 1)`;
`-1` remains the internal empty-pane sentinel. Old session-only layouts need no migration.

### Dragging tabs
Pointer-based (not HTML5 DnD), with the v1 drop-zone overlay:
- Onto another group's body: left/right/top/bottom halves (outer 28%) split that group on that
  side with the tab in the new group; the center moves the tab into that group.
- Onto a group's tab strip: inserts at the marker (reorders within the same strip).
- Onto a group's own body edge (dragging the group's only or active tab): splits it; a group
  left empty by a drag closes.
- Onto the "→ Window N / → New window" targets that appear in the focused strip's right end
  while dragging: moves the session to that window (`session.set_window`), shown there (the
  daemon launches the window if no client shows it, `window.show`). A dropped tab becomes the
  active tab of its group, which takes focus.
- Esc, window blur or pointercancel abort. Drops that change nothing show no overlay.

### Groups
- Everything in v1 "Panes" carries over for groups: gutters (5px hit area, accent on hover,
  double-click equalizes), min sizes (splits never create a group smaller than ~240×120px;
  "Not enough room" toast), zoom (Alt+Z maximizes the focused group), focus ring and dim veil
  on unfocused groups, the reparented cached xterm instances, the exited-session bar, the
  approval banner (**Approve** Alt+A, **Always**, **Deny** Alt+Shift+A, "Jump to prompt"),
  and the right-click terminal menu. The right button is always Muster's: right-clicking a
  terminal opens this menu even when its program tracks the mouse (e.g. Codex), the program
  never gets a right-button press, release or drag (dropped where xterm's mouse reports become
  input, so also during a chord with another button and outside the terminal), and the
  browser's own menu never shows over a terminal (text fields keep theirs). Left and middle
  clicks, the wheel and motion still go to programs that track the mouse. The menu's Paste
  reads the clipboard; if the browser refuses, an error toast says to press Ctrl+Shift+V
  (which pastes without that permission). Failed copies show an error toast too. A zoomed
  group shows a "zoomed" badge in its strip (click restores).
- A window with no tabs shows the empty launcher (folder input with `fs.complete`, then
  Claude / Codex / Shell, keys 1 2 3), with Jump and `⋯` in its top-right corner. An empty group
  (only possible transiently) closes.
- The window layout is saved with `window.set_layout` (debounced 400ms) and reconciled on load
  and on every session change: sessions of this window that aren't in the layout join the
  focused group; unknown or foreign sessions are dropped; empty groups collapse (a focused group
  that empties hands focus to its nearest neighbour). A window whose `layout` is null (first
  run after v1, or a daemon without it) starts as one group holding its sessions in task order;
  v1's per-channel layouts are not migrated. Sessions this client creates (new tab, new
  terminal, split) are held out of the layout until their call returns and places them.

### Status line (optional, on by default, toggle in the palette)
22px, muted: the focused tab's task name (only once it has a real name, not just its folder's;
truncated with an ellipsis, the full name in the tooltip, click copies), its folder (truncated
from the left, click copies), branch (click copies) and the number of tabs in the window. Right:
"needs you: N" (same as Alt+J) and a connection dot.

**Typing latency monitor** (palette, off by default, not kept across reloads): a small overlay above the status line timing each key typed into a terminal by stage, from keydown to the next animation frame after the first output that follows it (an estimate: output is not matched to keys, and the real paint time is not measured), with Copy (plain-text report) and Clear (`web/src/latency_monitor.ts`).

### Updates
A banner in the toast stack (bottom right, above the toasts; it stays until acted on or
dismissed) offers one waiting update at a time (`hello.updates`, `updates_changed`; see
docs/PROTOCOL.md "Updates and window preferences"), in this order:
1. **Backend update ready**: the executable the backend was started from holds a new build.
   "Restart backend" (`daemon.reload` with the offered build's id, so a file rewritten since is
   refused; disabled while disconnected, never queued): terminals keep running and the window
   reconnects.
2. **UI update ready**: the installed UI release differs from the one this page loaded (after
   `muster ui-install`). "Reload" (Ctrl+Shift+R).
3. **Terminal host update**: a new `muster-terminal-host` is beside that executable. No button:
   installing it needs a full restart, which ends all sessions; a "Shut down Muster…" link opens
   the shutdown dialog.

When the backend and the UI both changed, the backend banner comes first and the UI banner shows
after the restart. The banner is hidden while a dialog is open, and has its own polite live
region (apart from the toasts' status region) that announces each update once. `×` dismisses that update (per viewer, in localStorage) until a different one
of the same kind arrives; the next waiting update shows meanwhile.

### Window and shutdown
- **Reload window** (Ctrl+Shift+R, palette, `⋯`): saves a pending layout change, then reloads.
  Plain Ctrl+R stays with the terminal.
- **Close window** (palette, `⋯`): closes this window only; its tabs stay in it for the next
  `muster`. The native window closes itself; a browser closes only windows it allows a page to
  close (one opened by a script or with a single history entry, as Muster's `--app` windows
  are), else the item is hidden and a refused close says so in a toast.
- **System title bar** (`⋯`, palette; native windows only): shows or hides the window
  decorations of every native window. Kept by the backend (`prefs.set`), so new windows are built
  with it; open windows switch at once. Windows opened by an older build don't have the switch.
- **Shut down Muster…** (`⋯`, palette, the terminal host banner): a dialog warning that it stops
  the backend and the terminal host and ends every session and agent, with how many sessions are
  running. Cancel is focused; "Shut down" (red) calls `daemon.shutdown`. Every window then shows
  "Muster is shut down" (an alert dialog that takes the focus) and stops reconnecting. Only
  `daemon_stopping` or the call's reply does that; a dropped connection just reconnects.

### Overlays
- **Jump** (Ctrl+Space or Ctrl+Shift+K anywhere, Ctrl+K outside terminals; Ctrl+Space again closes it): fuzzy search over every tab and
  session in **all windows**, and folders for "new tab here" (recent folders and known
  workspaces; choosing one opens a tab there with the last agent kind), and every named agent,
  stopped ones too (choosing one starts it and pulls its tab here; its session isn't listed a
  second time). Rows show the state dot,
  label, window (for other windows), folder and state detail; one row per tab (session).
  Sessions that need you come first, then recent ones, then this window's tabs in order. Plain queries also match commands (ranked
  last). `>` switches to commands (Ctrl+Shift+P opens with `>`), and every command lists its
  shortcut. Choosing a tab in another window **pulls it into this window's focused group**
  (`session.set_window`) by default. The setting "Jump focuses the other window" (palette or `⋯`,
  stored in localStorage) makes it call `window.raise` instead, falling back to pulling when
  that returns `raised: false`. Raising doesn't select the tab over there (there is no RPC for
  that).
- **New tab…** (Alt+Shift+N): opens in the focused group. Folder (path input with `fs.complete`, defaults to the focused
  tab's folder; recent folders as chips), agent segmented control (Claude · Codex · Shell ·
  remembers last; no "None": a tab is a session), worktree toggle (default **off**; disabled for a known non-git
  folder), base branch (only when worktree is on), optional name (blank = auto), optional first
  prompt (Claude and Codex), and the collapsed Permissions section from v0. Enter or
  Ctrl+Enter creates. Ctrl+Shift+N skips the dialog (see Worktrees).
- **Rename window…** (palette, `⋯`, the window chip): a small dialog; the title follows.
- **Permissions** dialog: unchanged from v0 (tab context menu, the tab's shield icon, palette).
- **New agent / Edit agent** and the **message** overlay: see "Agents panel".
- **Shortcuts sheet** (Alt+/ or F1). Context menus are custom and keyboard navigable. Toasts sit
  bottom-right. Modals trap focus. Never use `alert/confirm/prompt`.

### Claude profiles

**Settings and more → Claude profiles** registers existing Claude configuration directories,
renames their labels, and enables/disables them. Saved workspaces are not listed here. Setup and login
happen in the official Claude CLI, never in Muster. The dialog does not inspect authentication
or settings files. Directory bindings are fixed; disabling does not stop running sessions.

New Tab shows a Claude profile selector for Claude only. Its initial choice is explicit
**Default**, which preserves the ordinary inherited Claude environment, regardless of saved
CLI workspace defaults. Choose a registered profile for that new session. New tabs
and splits opened directly from a Claude tab inherit that tab's selection, including Default.
The tab tooltip identifies the profile; a named profile also appears in the optional status
line. These are user labels for directories, not verified account identities.

New Agent can pin a Claude profile. Edit Agent shows the fixed selection without allowing
it to change, so its saved conversation never accidentally resumes under another profile.
The account itself is verified with `/status` in Claude Code. Backend versions without
profile support do not show these controls.

## Agents panel

Named agents (PLAN.md §10) get a left dock, about 220px wide, in any window. **Alt+G** (or
"Toggle agents panel" in Jump and the palette) shows and hides it; each window remembers its own
state in its layout (`Window.layout.agents`, saved like zoom). You talk to an agent in its own
terminal tab; there is no chat view.

| Part | What |
|---|---|
| Header | "Agents", the count, `×` hides the panel. |
| Row | State dot (same colours as tabs; hollow when stopped), name, kind badge (CLAUDE / CODEX), and below it the state plus time in state (`idle · 3m`, `stopped`), with an `auto` marker for autostart agents. Hover: the persona's first line. |
| Click | Starts the agent when no process runs (`agent.start` with this window, so a new tab opens here), then shows its tab exactly as Jump does: pulled into this window's focused group when it is elsewhere. Also selects the row, which filters the log. Enter does the same; ↑/↓ move between rows. |
| Context menu (right-click, `⋯`, Shift+F10) | Edit…, Memory…, Start / Show, Start fresh (a new conversation, restarting a running one), Stop (`session.kill`), Autostart (checked when on), Remove… (the app's confirm dialog; its folder stays). |
| + New agent | Under the list: the New agent dialog. |
| Message log | Collapsible (the expanded state is a per-viewer pref), newest first: `from → to`, time, the message's first line, origin and hop chips (non-human origins in amber), a `replied` chip, and for a refused message a red row with the reason. A selected agent filters it (a `to <name> ×` chip clears the filter). "Older messages" pages `agent.messages`. Clicking an entry opens an overlay with the full message, its reply (or "no reply yet") and its route. |

Live updates come from `agent_upserted`, `agent_removed` and `agent_message` (the agent list and
the last 100 messages are loaded after each `hello`); states come from the agents' sessions.
Daemons without agents (`agent.list` is an unknown method) show a one-line note instead.

**New / Edit agent dialog** (+ New agent, the palette's "New agent…", a row's Edit…):

- New only: a template picker (Blank, Orchestrator, Researcher, Reviewer, PR reviewer, Background worker, from
  `agent.templates`) with the template's one-line description; picking one fills every field
  (a name you typed is kept). Name and kind (Claude · Codex).
- Model and effort (optional, effort suggests the CLI's levels), persona (monospace textarea,
  written to `CLAUDE.md` / `AGENTS.md`), accepted origins (Human, Schedule, Webhook checkboxes,
  at least one), the Permissions section showing only the control for the agent's kind, and an
  Autostart toggle.
- Ticking Schedule or Webhook on a Claude agent shows, in amber: "Anthropic's consumer terms are
  unclear about unattended use; scheduled work is safer on a Codex agent."
- Edit shows the kind badge, name and home folder, and a second tab, **Memory**: the files in
  the agent's `memory/` on the left (plus a field to create one by name), a plain-text editor
  on the right with Save (Ctrl+S). Switching files or closing with unsaved changes asks first
  (closing: a second Esc discards). Edit saves only changed fields (`agent.update`); model,
  effort and permissions apply from the agent's next start, which the toast says when it runs.
- Ctrl+Enter creates or saves. Creating opens the panel if it was closed.

## Naming (no names up front)

New tasks (tabs) are created **without a name**. The daemon names the task (`auto_named: true`)
and keeps the name updated until the human renames it:
1. an agent's terminal title (Claude Code and Codex set a task summary via OSC 0/2), cleaned
   of spinner glyphs and ignored when it's just the program name or a path;
2. else the first prompt sent to the tab (first few words);
3. else a shell session's title (e.g. the running command);
4. else the folder name (shown muted).
Renaming (double-click, menu, `muster rename`) sets `auto_named: false` for good.

## Worktrees

Off by default. **Ctrl+Shift+N** creates a worktree tab directly in the focused group: same
folder and agent as the focused tab, no dialog. **Alt+Shift+N** opens the New tab dialog, where worktree is a toggle. The
worktree folder is keyed by the tab's id, so it never moves. The branch starts as
`muster/<id>`. The daemon renames it once, to `muster/<slug of the first real name>`, and again
on each explicit rename, as long as muster created the branch and it has no upstream.

## Windows

- `muster` (no arguments) opens a new window every time. It **claims** the lowest-numbered
  window that has tabs but no client connected (so reopening after closing restores your
  tabs), else a new empty window. `muster open [path] [--window N]` opens a window on a folder
  (a new tab there) or reclaims window N.
- The window keeps its number in `sessionStorage`, so a reload reclaims the same window.
- `document.title` = `muster · <window name>` with a `(N) ` prefix when N sessions in this window
  need you. The native window mirrors the document title, and the WM helpers find windows by
  this title.
- Closing a window leaves its tabs (sessions) and layout attached to it. A released window with
  no sessions is deleted by the daemon.
- Sessions created outside a window (CLI `muster new`, MCP `spawn_session`) go to the spawning
  session's window, else the task's window, else the most recently focused window, and join
  that window's focused group.

## Keyboard map

App shortcuts use Alt and Ctrl+Shift, intercepted in the capture phase before xterm. Keys
agents and shells rely on (Alt+M, Alt+B, Alt+F) are not app shortcuts. In text fields, plain
Alt+letters and Alt+arrows are left alone. The setting **Terminal gets Alt keys** sends every
Alt shortcut except Alt+Shift+… to a focused terminal. Ctrl+Shift+T/N are browser-reserved in
Chromium `--app` mode, so there Alt+T and Alt+Shift+N are the reliable fallbacks.

| Keys | Action |
|---|---|
| Ctrl+Space or Ctrl+Shift+K (Ctrl+K outside terminals) | Jump (Ctrl+Space again closes it) |
| Ctrl+Shift+P | Command palette |
| Alt+T or Ctrl+Shift+T | New tab in the focused group (new task: same folder, last agent, no worktree) |
| Ctrl+Shift+N | New worktree tab in the focused group |
| Alt+Shift+N | New tab… dialog |
| Alt+N | New terminal in the focused tab's task, as a tab in the focused group (picker menu 1/2/3) |
| Alt+1…9 | Go to tab N of the focused group (hold Alt to see numbers) |
| Ctrl+Tab / Ctrl+Shift+Tab, Alt+] / Alt+[ | Next / previous tab in the focused group |
| Alt+Shift+[ / Alt+Shift+] | Move the tab left / right within its group |
| Ctrl+Alt+←↑→↓ | Move the tab to the neighbouring group in that direction (splitting at the window edge) |
| Alt+J | Jump to the next session that needs you (all windows; pulls its tab here, or raises its window with "Jump focuses the other window"). Order: most urgent state, then longest waiting; repeated presses cycle |
| Alt+D / Alt+Shift+D | Split the focused group right / down with a new shell in the focused tab's task (picker via menu) |
| Alt+W | Close the focused tab (inline confirm if working); an emptied group closes |
| Alt+←↑→↓ | Focus the group in that direction |
| Alt+Shift+←↑→↓ | Resize the focused group |
| Alt+Z | Zoom the focused group |
| Alt+G | Show / hide the agents panel (per window) |
| F2 (in a focused strip) | Rename what the tab shows (as double-click) |
| Alt+A / Alt+Shift+A | Approve / deny focused session's prompt |
| Ctrl+Shift+F | Find in terminal |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy / paste in terminal |
| Ctrl+= / Ctrl+- / Ctrl+0 | Terminal font size |
| Alt+/ or F1 | Shortcuts sheet |
| Ctrl+Shift+R | Reload the window (plain Ctrl+R goes to the terminal) |

## Behavior details
- Reconnect: an unobtrusive "Reconnecting…" pill with exponential backoff to 5s. On reconnect,
  call `hello`, re-claim the same window and re-attach every visible terminal. Terminals refuse
  input while disconnected, under a veil.
- Other windows' changes arrive as events. A tab moved away disappears (its terminals are
  detached with `session.detach` but stay cached for 5 minutes in case it comes back; showing
  it again re-attaches and the replay resets it), and a tab moved here appears.
- Only attach sessions shown in this window (a terminal attaches when its pane is first shown).
  Detach terminals of tabs that left.
- Every hello (first connect and reconnects) re-claims the window, then re-attaches. If
  `window_removed` names this client's window, it claims the same id again (created if
  missing).
- Daemons without windows (hello has no `windows`): every session is a tab of this window, in
  task `sort_order`; the layout is kept in memory only; window UI (window chip, Move to window,
  Jump focuses the other window, Rename window) is hidden; new tabs send the folder name as
  `name`, since such daemons require one. A failed `window.claim` degrades the same way.
- Daemons with windows but before v2 (sessions without `window_id`): a session is a tab of its
  task's window. Without `session.set_window` ("unknown method"), moving a tab to another window
  or pulling it here falls back to `channel.move`, which works only for a task's lone session
  (else an error toast). Without `window.set_layout` the layout isn't saved (a reload rebuilds
  one group). Closing a task's last tab also calls `channel.close_if_empty`, for daemons that
  don't close emptied tasks themselves.
- Desktop notifications (daemon, `notify.rs`) are unchanged.
- Per-viewer prefs in `localStorage` (try/catch): font size, theme, last agent kind, recent
  folders, Jump recency, status line on/off, "Terminal gets Alt keys", "Jump focuses the other
  window", the agents panel's message log expanded or not, the active tab per window, and the
  dismissed updates. The system title bar is shared by every window and kept by the backend.
- `window.focused` is called on window focus and right after a claim when the page has focus.
- Performance: batch output per animation frame per terminal. Never re-render the Preact tree on
  output bytes.

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
- Double-click renames (the task for its main tab, i.e. the task's first agent session or else
  its first session; the session otherwise; the context menu offers both). Middle-click
  closes. `+` at the end of each strip opens a new tab in that group (same folder as the group's active tab, no worktree, last agent kind); `▾` next to it:
  New tab…, New worktree tab, New terminal in this task, and Claude / Codex / Shell.
- The **top-right group's** strip ends with the window controls: Split right `◫`, Split down
  `⊟` (acting on the focused group), a chip naming the window (only with 2+ windows; click
  renames), Jump `🔍 Ctrl Space`, and `⋯` (Rename window…, Theme ▸, Status line, Terminal gets
  Alt keys, Jump focuses the other window, Command palette, Keyboard shortcuts), plus a
  "Reconnecting…" pill when disconnected. The top-right group is the one touching the window's
  right edge highest up; a zoomed group carries the controls; narrow strips keep only Jump and
  `⋯`.
- Strips scroll sideways past their width (wheel scrolls, active tab kept in view) with an
  `N ▾` overflow list. Roving tabindex: ←/→/Home/End, Enter focuses the terminal, F2 renames.
- Context menu: Rename tab, Rename task, Permissions… (task), Copy path, Copy branch, New
  terminal in this task, Split right ▸ / Split down ▸ (Move this tab, or a new Claude / Codex /
  Shell in this task there), Move to window ▸ (existing windows plus
  "New window"), Restart, Interrupt, Close tab, and when it is the task's last session with a
  worktree, Close + remove worktree (confirmed in a dialog).

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
  and the right-click group menu (Shift+right-click inside mouse-tracking programs). A zoomed
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
22px, muted: the focused tab's task folder (truncated from the left, click copies), branch
(click copies) and the number of tabs in the window. Right: "needs you: N" (same as Alt+J) and
a connection dot.

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

- New only: a template picker (Blank, Orchestrator, Researcher, Reviewer, Background worker, from
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
| F2 (in a focused strip) | Rename the tab (the task on its main tab) |
| Alt+A / Alt+Shift+A | Approve / deny focused session's prompt |
| Ctrl+Shift+F | Find in terminal |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy / paste in terminal |
| Ctrl+= / Ctrl+- / Ctrl+0 | Terminal font size |
| Alt+/ or F1 | Shortcuts sheet |

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
  window", the agents panel's message log expanded or not, and the active tab per window.
- `window.focused` is called on window focus and right after a claim when the page has focus.
- Performance: batch output per animation frame per terminal. Never re-render the Preact tree on
  output bytes.

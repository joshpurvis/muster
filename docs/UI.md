# Muster UI spec (v1: tabs and splits)

Goal: a great tiling terminal for agents, in the spirit of kitty/wezterm. Tabs across the top,
splits inside each tab, and one fuzzy jump for everything. There is no sidebar, inbox,
timeline, composer or diff panel. Attention comes from state dots, the approval banner,
desktop notifications and a "next that needs me" key.

Several windows may be open at once (one muster client each), so the window manager can tile
them. The daemon owns every terminal. Closing a window never kills anything, and reopening
restores that window's tabs.

## Concepts

- **Window**: one client window. It owns an ordered list of tabs, persisted by the daemon
  (`Window`, see docs/PROTOCOL.md). Windows are numbered 1, 2, 3...
- **Tab**: what the API calls a **channel**. It has a folder (`cwd`), an optional git worktree,
  a permissions profile, a split tree of panes and a name.
- **Pane**: a leaf of the split tree showing one terminal session. A pane can still hold several
  sessions as pane-tabs (drop a pane header onto another pane's center). Its tab strip only
  appears when it holds 2 or more.
- **Workspace**: a folder the daemon knows about. It's not shown as UI. New tabs pick a folder,
  and the frontend adds it as a workspace behind the scenes (`workspace.add` is idempotent by
  path).

## Visual language

Unchanged from v0. Dark by default with a light theme (`prefers-color-scheme` plus a manual
toggle). All colors are CSS custom properties on `:root`. State colors: starting gray
(pulsing), working `#4ea1ff`, needs_approval / asking_question `#f0a020`, idle `#3fb950`,
error `#f85149`, stuck `#e0823d`, exited `#6a6a76` (hollow dot). Inter 13px UI, JetBrains Mono
13px terminal, no web font downloads. Radii 6/10px, hairline borders, 120–160ms motion,
`prefers-reduced-motion` respected, inline SVG icons.

## Layout

```
┌ tab bar (34px) ─────────────────────────────────────────────────────────────────────────┐
│ ● fix auth bug ⎇ ×│ ◐ muster ×│ ● new api ⎇ 🛡 ×│  +  ▾        [🔍 Ctrl K]  ⋯ │
├──────────────────────────────────────────────────────────────────────────────────────────┤
│ ┌ pane ──────────────────────────────┬ pane ───────────────────────┐                     │
│ │ [approval banner when needed]      │                             │                     │
│ │ xterm                              │ xterm                       │                     │
│ │                                    ├─────────────────────────────┤                     │
│ │                                    │ xterm                       │                     │
│ └────────────────────────────────────┴─────────────────────────────┘                     │
└ status line (22px, optional): ~/p/muster · ⎇ muster/fix-auth-bug · 3 sessions  ● connected ┘
```

### Tab bar
- One tab per channel in this window, in the window's order. Each tab shows the aggregate state
  dot, the name and an optional `⎇` (worktree) icon. It shows a permissions icon when the
  profile is elevated: amber globe/shield for network or accept edits, red warning shield for
  full access or bypass. Tabs have a `×` on hover.
- An **unnamed** tab (`auto_named: true`) shows its derived name in normal text. Before
  anything was derived, it shows the folder name in muted italics.
- Active tab: `--bg-2` background joined to the content, with an accent top bar. Unread
  activity (state changed since you last viewed it) shows a bold name.
- Double-click renames inline. Middle-click closes. Drag reorders (`window.reorder_tabs`), and
  dragging a tab out of the bar onto the jump button or a "→ window N" entry moves it. The
  context menu has Rename, Permissions…, Copy path, Copy branch, Move to window ▸ (existing
  windows plus "New window"), Close tab and Close tab + remove worktree (only with a worktree).
- `+` makes a new tab (same folder as the active tab, no worktree, last agent kind). `▾` next
  to it opens a menu: New tab…, New worktree tab, and Claude / Codex / Shell.
- Past about 8 tabs the strip scrolls sideways (the wheel scrolls it and the active tab stays in
  view). A `N ▾` overflow button lists all tabs.
- Closing a tab (Alt+W on the last pane, `×`, menu) archives the channel. If an agent is
  working, the tab turns into an inline "Close? ⏎ / Esc" confirmation first. Closing never
  removes a worktree unless the "+ remove worktree" action is chosen.
- A window with no tabs shows the empty launcher (Claude / Codex / Shell, keys 1 2 3, folder
  picker).

### Panes
Everything in v0 "Panes" carries over: the layout tree, reconcile, min sizes, gutters, drag and
drop with drop-zone overlays, zoom, the reparented cached xterm instances, and the exited-session
bar. Differences:
- A pane shows a thin **pane header** (22px: kind icon, session name, state word, `×`) only
  when the tab has 2 or more panes. With one pane there is no chrome at all. Dragging the
  header works like dragging a v0 pane-tab.
- The pane tab strip only appears when the pane holds 2+ sessions.
- Focus: with several panes, the focused one has a faint accent ring and the others sit under a
  light dim veil.
- Approval banner above the terminal for needs_approval / asking_question, as in v0:
  **Approve** (Alt+A), **Always**, **Deny** (Alt+Shift+A), "Jump to prompt".
- Empty pane: centered launcher (Claude / Codex / Shell, keys 1 2 3).

### Status line (optional, on by default, toggle in the palette)
It sits at the bottom, 22px, muted: the active tab's cwd (truncated from the left, click
copies) and branch (click copies), plus the number of sessions. On the right: a "needs you: N"
chip, which is the same as Alt+J, and a connection dot.

### Overlays
- **Jump** (Ctrl+Shift+K anywhere, Ctrl+K outside terminals): fuzzy search over every tab and
  session in **all windows**, and folders for "new tab here". Rows show the state dot,
  name, tab, window number, cwd and state detail. Sessions that need you come first, then recent
  ones. `>` switches to commands (Ctrl+Shift+P opens with `>`), and every command lists its
  shortcut. Choosing something in another window **pulls that tab into this window**
  (`channel.move`) by default. The setting "Jump focuses the other window" (palette, stored in
  localStorage) makes it call `window.raise` instead, falling back to pulling when that returns
  `raised: false`.
- **New tab…** (Alt+Shift+N): folder (path input with `fs.complete`, defaults to the active
  tab's folder; recent folders listed), agent segmented control (Claude · Codex · Shell · None;
  remembers last), worktree toggle (default **off**; on when opened via Ctrl+Shift+N), base
  branch (only when worktree is on), optional name (blank = auto), optional first prompt, and
  the collapsed Permissions section from v0. Enter or Ctrl+Enter creates.
- **Permissions** dialog: unchanged from v0 (tab context menu, the tab's shield icon, palette).
- **Shortcuts sheet** (Alt+/ or F1). Context menus are custom and keyboard navigable. Toasts sit
  bottom-right. Modals trap focus. Never use `alert/confirm/prompt`.

## Naming (no names up front)

New tabs are created **without a name**. The daemon names them (`auto_named: true`) and keeps
the name updated until the human renames the tab:
1. an agent's terminal title (Claude Code and Codex set a task summary via OSC 0/2), cleaned
   of spinner glyphs and ignored when it's just the program name or a path;
2. else the first prompt sent to the tab (first few words);
3. else a shell session's title (e.g. the running command);
4. else the folder name (shown muted).
Renaming (double-click, menu, `muster rename`) sets `auto_named: false` for good.

## Worktrees

Off by default. **Ctrl+Shift+N** creates a worktree tab directly: same folder and agent as the
active tab, no dialog. **Alt+Shift+N** opens the New tab dialog, where worktree is a toggle. The
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
- Closing a window leaves its tabs attached to it. A released window with no tabs is deleted by
  the daemon.
- New tabs created outside a window (CLI `muster new`, MCP `spawn_agent` in a new channel)
  go to the spawning session's window, else the most recently focused window.

## Keyboard map

App shortcuts use Alt and Ctrl+Shift, intercepted in the capture phase before xterm. Keys
agents and shells rely on (Alt+M, Alt+B, Alt+F) are not app shortcuts. In text fields, plain
Alt+letters and Alt+arrows are left alone. The setting **Terminal gets Alt keys** sends every
Alt shortcut except Alt+Shift+… to a focused terminal. Ctrl+Shift+T/N are browser-reserved in
Chromium `--app` mode, so there Alt+T and Alt+Shift+N are the reliable fallbacks.

| Keys | Action |
|---|---|
| Ctrl+Shift+K (Ctrl+K outside terminals) | Jump |
| Ctrl+Shift+P | Command palette |
| Alt+T or Ctrl+Shift+T | New tab (same folder, last agent, no worktree) |
| Ctrl+Shift+N | New worktree tab (same folder, last agent) |
| Alt+Shift+N | New tab… dialog |
| Alt+1…9 | Go to tab N in this window (hold Alt to see numbers) |
| Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+Alt+→ / ← | Next / previous tab |
| Alt+Shift+[ / Alt+Shift+] | Move tab left / right |
| Alt+J | Jump to the next session that needs you (all windows; pulls its tab here) |
| Alt+D / Alt+Shift+D | Split right / down (new shell; picker via menu) |
| Alt+N | New session as a pane-tab in the focused pane |
| Alt+W | Close pane (closes the tab when it's the last pane; inline confirm if working) |
| Alt+←↑→↓ | Focus pane in direction |
| Alt+Shift+←↑→↓ | Resize focused pane |
| Alt+Z | Zoom focused pane |
| Alt+[ / Alt+] | Previous / next pane-tab in pane |
| Alt+A / Alt+Shift+A | Approve / deny focused session's prompt |
| Ctrl+Shift+F | Find in terminal |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy / paste in terminal |
| Ctrl+= / Ctrl+- / Ctrl+0 | Terminal font size |
| Alt+/ or F1 | Shortcuts sheet |

## Behavior details
- Reconnect: an unobtrusive "Reconnecting…" pill with exponential backoff to 5s. On reconnect,
  call `hello`, re-claim the same window and re-attach every visible terminal. Terminals refuse
  input while disconnected, under a veil.
- Other windows' changes arrive as events. A tab moved away disappears (its terminals stay
  cached for a while in case it comes back), and a tab moved here appears.
- Only attach sessions shown in this window. Detach terminals of tabs that left.
- Desktop notifications (daemon, `notify.rs`) are unchanged.
- Per-viewer prefs in `localStorage` (try/catch): font size, theme, last agent kind, recent
  folders, status line on/off, "Terminal gets Alt keys", "Jump focuses the other window".
- Performance: batch output per animation frame per terminal. Never re-render the Preact tree on
  output bytes.

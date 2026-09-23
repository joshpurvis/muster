# Muster UI spec (v0)

Goal: feels like Slack × Obsidian × a great terminal (kitty/wezterm). Dense but calm, fast,
every action discoverable by mouse AND keyboard, nothing surprising. Human attention is the
scarce resource: surface what needs the human; hide what doesn't.

## Visual language

- Dark by default, light theme via `prefers-color-scheme` + manual toggle (`data-theme` on `<html>`).
  All colors are CSS custom properties on `:root`.
- Dark palette (starting point, tune by eye):
  `--bg-0 #0f0f11` (rail/topbar) · `--bg-1 #151518` (sidebar) · `--bg-2 #1a1a1e` (main, terminal bg)
  `--bg-3 #222228` (hover/raised) · `--bg-4 #2a2a31` (active) · `--border #26262d` · `--border-strong #34343d`
  `--text #e4e4e8` · `--text-muted #9d9daa` · `--text-faint #6a6a76` · `--accent #8b7cf6` (violet) · `--accent-soft rgba(139,124,246,.14)`
- State colors (used everywhere a state appears: dots, pills, tab underline, inbox):
  starting gray (pulsing) · working `#4ea1ff` (soft pulse/spinner) · needs_approval & asking_question `#f0a020`
  · idle `#3fb950` · error `#f85149` · stuck `#e0823d` · exited `#6a6a76` (hollow dot).
- Type: UI `Inter, "SF Pro Text", system-ui, sans-serif` 13px; terminal
  `"JetBrains Mono", "Fira Code", "Cascadia Code", ui-monospace, monospace` 13px (user adjustable).
  No web font downloads (works offline).
- Radii 6px (controls) / 10px (overlays). 1px hairline borders; shadows only on overlays.
  Motion: 120–160ms ease-out; respect `prefers-reduced-motion`.
- Icons: inline SVG (Lucide-style strokes, 1.5px), no icon font.

## Layout

```
┌ topbar (36px): ◧ sidebar toggle · [🔍 Search or jump to…  Ctrl K] · inbox bell(3) · ⚙ ──────────┐
├ sidebar (resizable 180–420, collapsible) ┬ channel header (44px) ─────────────────────┬ right panel ┤
│  Inbox                               3   │ # fix-auth  ⎇ muster/fix-auth  ~/r/.. [+▾][⫽][⊟][☰][⋯] │ Timeline│Diff │
│  ▾ muster                            +   │ ┌ pane ───────────────┬ pane ─────────────┐│ (resizable, │
│    ● # fix-auth            (bold=unread) │ │ tabs: ● claude ×  + │ ● shell ×  +      ││  toggle)    │
│       ◦ claude  working                  │ │ [approval banner]   │                   ││             │
│       ◦ shell   idle                     │ │ xterm               │ xterm             ││             │
│    ● # new-api                         2 │ │                     ├───────────────────┤│             │
│  ▸ other-repo                            │ │                     │ ...               ││             │
│                                          │ └─────────────────────┴───────────────────┘│             │
│  + Add workspace          ● connected    │ composer: [→ claude ▾] Message…   queued 1 ⏎ │             │
└──────────────────────────────────────────┴──────────────────────────────────────────┴─────────────┘
```

### Sidebar
- "Inbox" row on top with count badge (count = sessions whose state `needs_human()`).
- One collapsible section per workspace (name, hover shows `+` new channel and `⋯`).
- Channel row: aggregate state dot, `#` + name, right side: queued/inbox count badge or spinner.
  Bold when unread (any timeline event newer than `last_read_at`). Active channel highlighted
  with `--bg-4` + accent left bar.
- The active channel expands to list its sessions (kind icon, name, state word in muted text).
  Clicking a session focuses its tab (activates it in its pane).
- Drag channels to reorder (`channel.reorder`). Double-click to rename inline. Right-click →
  context menu (Rename, New session ▸, Copy path, Mark read, Archive…).
- Footer: "+ Add workspace", connection status dot (green / amber "Reconnecting…").
- Aggregate state priority: needs_approval > asking_question > error > stuck > working > starting > idle > exited.

### Channel header
`# name` (double-click to rename), branch chip `⎇ branch` (click copies), cwd (muted, truncated
from the left, click copies), then icon buttons with tooltips showing shortcuts:
New session (split-button with Claude/Codex/Shell), Split right, Split down, Timeline, Diff, `⋯`
(Rename, Archive, Archive + remove worktree, Copy path).

### Panes (the core; must feel great)
- Layout is a tree persisted per channel via `channel.set_layout` (debounced 400ms):
  ```ts
  type Layout =
    | { type: "split"; dir: "row" | "col"; sizes: number[] /* fractions sum 1 */; children: Layout[] }
    | { type: "pane"; id: string; tabs: number[] /* session ids */; active: number | null };
  ```
  On load, reconcile with live sessions: drop unknown ids, append sessions not in any pane to the
  focused/first pane. Collapse empty panes (except the last) and single-child splits.
- Tab strip (30px): tabs show kind icon, name, state dot; close `×` on hover; middle-click closes;
  double-click renames; `+` at the end opens a small menu (Claude / Codex / Shell, keys 1/2/3).
  Active tab of the focused pane gets an accent underline; others muted.
- Gutters: 5px hit area, 1px visible line, accent on hover/drag, `col-resize`/`row-resize` cursor.
  Drag to resize with min pane size ~120px. Double-click gutter equalizes siblings.
- Drag & drop tabs (Obsidian-style): dragging a tab over a pane shows a translucent overlay for
  the drop zone: left/right/top/bottom half-edges create a split, center moves into that pane's
  tabs. Dropping within a tab strip reorders. Use pointer events (not HTML5 DnD) for smoothness.
- Zoom: maximize the focused pane (others hidden, not destroyed); indicator in the tab strip.
- Terminals: one `xterm.js` instance per session, created once and cached in a registry keyed by
  session id. Its DOM element is **reparented** when layout changes — never disposed on
  re-layout (avoids replay flicker). Addons: fit (ResizeObserver + rAF debounce), webgl (fallback
  on context loss), unicode11, web-links, search. Scrollback 10k. Only visible terminals send
  `session.resize`. xterm theme derived from CSS vars.
- Exited session: dim terminal + bottom bar "Exited (code N) · Restart (Enter) · Close".
- Approval banner (above terminal when state is needs_approval/asking_question): amber strip with
  `state_detail` text and buttons **Approve** (Alt+A), **Always**, **Deny** (Alt+Shift+A),
  plus "Jump to prompt". For asking_question: just "Answer in terminal" + Focus.
- Empty pane: centered launcher with three large buttons (Claude / Codex / Shell, keys 1 2 3).

### Composer (Slack message box, bottom of channel)
- Target chip on the left: which session receives the message (defaults to the focused pane's
  active agent session; dropdown lists channel sessions with state).
- Auto-growing textarea (max ~40% height). Enter = send (queues if agent busy),
  Shift+Enter = newline, Ctrl+Enter = send & interrupt. Up-arrow in empty box recalls last message.
- Shows "queued N" chip (from `SessionInfo.queued`) with a clear (×) action.
- Esc returns focus to the target's terminal. Alt+M focuses the composer.
- `/note text` posts a human note to the timeline instead of sending to the agent.

### Right panel (resizable, toggle)
- **Timeline** tab: Slack-like feed for the channel, grouped by minute/author. Human notes and
  prompts as messages ("You → claude"), agent posts (MCP `post_to_channel`) as messages with the
  session as author, state changes / session start/exit / MCP calls as compact system lines
  (collapsible runs). Infinite scroll back via `channel.timeline` `before`.
- **Diff** tab: `channel.diff`, rendered as a file list + unified hunks, +/− colored, refresh button.

### Inbox view (Alt+I, or click Inbox)
- Replaces the pane area. Cards sorted by urgency then age: channel · session · state pill · detail ·
  "2m ago", a live mini preview of the last ~8 screen lines (`session.screen`, refresh on state
  change), and actions: Approve / Always / Deny / Reply (inline input → `session.send`) / Open.
- Keyboard: j/k (or ↑/↓) move, `a` approve, `d` deny, `r` reply, Enter open.
- Empty state: "Nothing needs you. ✓" with muted illustration-free styling.

### Overlays
- **Quick switcher** (Ctrl+K when focus isn't in a terminal, Ctrl+Shift+K anywhere): fuzzy search
  over channels, sessions, workspaces; `>` prefix switches to commands (Ctrl+Shift+P opens with `>`).
  Every command lists its shortcut. Arrow keys + Enter, Esc closes. Recent items first.
- **New channel** (Alt+Shift+N): name (live slug preview + branch name), workspace select, agent
  segmented control (Claude · Codex · Shell · None; remembers last), worktree toggle (default on for
  git), base branch (optional), initial prompt textarea. Ctrl+Enter / Enter on name creates.
- **Add workspace**: path input with `fs.complete` suggestions (Tab completes), optional name.
- **Shortcuts sheet** (Alt+/ or F1).
- Context menus: custom (not native), keyboard navigable.
- Toasts bottom-right for `notice` events and RPC errors; auto-dismiss 4s, errors sticky-ish 8s.
- Confirm destructive actions inline (e.g. closing a working agent's tab turns the tab into
  "Kill? ⏎ / Esc") rather than modal dialogs where possible. Never use `alert/confirm/prompt`.

## Keyboard map

App shortcuts use Alt (Zellij/iTerm-style) and Ctrl+Shift; all are intercepted in the capture
phase before xterm sees them. Avoid browser-reserved combos (Ctrl+N/T/W, Ctrl+Shift+N/T/W/I/J).

| Keys | Action |
|---|---|
| Ctrl+Shift+K (Ctrl+K outside terminals) | Quick switcher |
| Ctrl+Shift+P | Command palette |
| Alt+Shift+N | New channel |
| Alt+N | New session tab in focused pane |
| Alt+D / Alt+Shift+D | Split right / split down (new shell by default; picker via menu) |
| Alt+W | Close tab (inline confirm if agent is working) |
| Alt+←↑→↓ | Focus pane in direction |
| Alt+Shift+←↑→↓ | Resize focused pane |
| Alt+Z | Zoom (maximize) focused pane |
| Alt+[ / Alt+] | Previous / next tab in pane |
| Alt+1…9 | Jump to channel N (sidebar order) |
| Ctrl+Alt+↑ / ↓ | Previous / next channel |
| Alt+I | Inbox |
| Alt+A / Alt+Shift+A | Approve / deny focused session's prompt |
| Alt+M | Focus composer |
| Alt+B | Toggle sidebar |
| Alt+L | Toggle right panel (timeline) · Alt+G diff |
| Ctrl+Shift+F | Find in terminal |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy / paste in terminal |
| Ctrl+= / Ctrl+- / Ctrl+0 | Terminal font size |
| Alt+/ or F1 | Shortcuts sheet |

## Behavior details
- Reconnect: on WS close show an unobtrusive "Reconnecting…" pill; exponential backoff to 5s;
  on reconnect call `hello` and re-attach every cached terminal (replay frame resets it).
- `document.title` = `(N) Muster` when N items need the human.
- Per-viewer prefs in `localStorage` (try/catch): sidebar width/collapsed, right panel width/tab/open,
  font size, theme, last agent kind, collapsed workspaces.
- Performance: batch incoming output per animation frame per terminal; never re-render the Preact
  tree on output bytes (terminals live outside the VDOM).

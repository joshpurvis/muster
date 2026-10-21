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
- **Tab**: one **session** (a Claude, Codex or shell terminal), or an agent panel or a note (see below). Each session belongs to one
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
  hover. The tab of a named agent whose current run is on OpenRouter (`SessionInfo.provider`, set when
  it starts) also has a small amber **OpenRouter** chip (its tooltip and accessible name:
  "OpenRouter · pay per use"): it bills per token. It stays until a restart applies another
  provider, and goes when the process exits. Billing text is at least 4.5:1 in both themes.
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
  "New window"), Restart, Interrupt, Close tab (a named agent's tab only hides: see "Agents panel"), and when it is the task's last session with a
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
The other built-ins follow the theme and work by keyboard: Image opens at Fit (shrunk to the
panel, never enlarged past actual size; an SVG with only a viewBox fills it) and zooms like an
image viewer: − / + (buttons, `-` / `+` or `=`, Ctrl+wheel or a pinch around the pointer) step
from the size on screen, 100% (`1`) is actual pixels, Fit (`0`) goes back, Z or a click
switches Fit / 100% (a picture already shown at 100% when fitted goes to twice the fitted
size instead, so the switch always zooms; again goes back to Fit), and a picture bigger than the panel scrolls and drags to pan (the zoom
shows bottom left and is announced once it settles; the browser's own zoom keys never zoom Muster from
inside a panel); Chart shows values on hover or with the arrow keys and has a "Show data table" link;
Table sorts by header click and filters (`/` focuses the box, Esc clears it); PR decision
shows the agent's verdict and undo line first, has Merge (M), Request changes (R), Ask (A),
Open on GitHub (O), Skip (S), Hold (H), Send answers (Ctrl+Enter) and the user's own actions
(their key on the button, in a separate "Your actions" group that folds in small panels;
toggle actions switch on or off and go with the next send), answers the agent's questions in
place (G or the button under the header jumps to them, number keys 1–9 pick in the focused
question, arrows step, Tab and Shift+Tab move between questions, the recommended answer starts
picked; a line above the actions says what goes with the next decision, and Send answers is
off while Merge is being confirmed), and its pictures enlarge on click or Enter in a modal view (zoom
as in Image, drag to pan, arrows step, Esc closes); PR overview moves with J/K or the
arrows, opens with Enter (or O, or a double-click), merges a quick win with M (after the same
confirm as Merge), holds with H, goes through the PRs in order with N, filters with `/`, and
can show only the quick wins. In both, `?` lists the keys. An outward custom action (marked ↗) shows what
the agent will do and needs Enter or Confirm, like Merge (a toggle one, before it is switched
on). Merge first shows the exact `gh pr merge` command and the answers and options that go
with it, and needs a fresh Enter or "Confirm merge" (a held key never confirms; a change to
the PR, or to the answers or options, cancels the step); with failing checks it is disabled unless
Shift is held (Shift+M), and a hint beside it says so. Esc closes the confirm or a comment
box and returns to its button; Ctrl+Enter sends a comment or question. The latest action's
result shows above the action bar.

An app sends a message or opens a link only right after a real click or key inside it: the
sandbox proxy checks its own user activation (selecting the tab or focusing the panel does not
count) and passes at most 3 such requests per action; the host also rate-limits them. Links
open in the host browser.

While a panel has focus, Muster's navigation shortcuts still work: Jump (Ctrl+Space, Ctrl+K),
the palette (Ctrl+Shift+P), Alt+J, tab switching (Alt+1..9, Ctrl+Tab, Alt+[ / ]), Alt+Shift+[ / ],
Alt+arrows between groups, Alt+Z, Alt+Shift+Z (fullscreen), Alt+G, Alt+/ and F1, and Alt+W,
which closes that panel. Esc reaches the app first; one it leaves alone (see "Fullscreen")
leaves a fullscreen panel.
In the app's text fields, Alt+letters and Alt+arrows stay with the field, as in Muster's own.
Commands that act on an agent (approve, interrupt, new tabs) are not taken from a panel. Closing
a focused panel, or the agent closing it, hands the keyboard to the tab that takes its place.

A panel shows "Loading panel…" (with the app's first script error, if any) until the app
completes the MCP Apps handshake. After 10 s it shows what stalled (the sandbox did not load,
the app never sent `ui/initialize`, or it never sent `ui/notifications/initialized`) with the
app's last error and Reload panel. A load cut off by a dropped connection retries on reconnect.
Jump lists every panel by title (pulling one from another window), and the status line counts
panel tabs with terminals.
Saved layouts still use numbers: positive session IDs, panel tab IDs `-(panel_id + 1)`, note tab IDs `-(2^40 + note_id)`;
`-1` remains the internal empty-pane sentinel. Old session-only layouts need no migration.

### Notes

A note is plain text that the human and every agent share (gedit-simple: no formatting, no
heavy editor). It is a tab like a terminal or a panel: it drags between groups, onto an edge
(a new split) and to other windows, splits, zooms, goes fullscreen, and Alt+1..9, Ctrl+Tab and
Alt+[ / ] select it. Agents read and write it through MCP (`note_read`, `note_append`, …,
docs/MCP.md "Notes") and `muster note` (docs/CLI.md); the human types in it.

- **Opening**: "New note" in the palette or in a strip's `▾` menu (a tab in that group, the
  keyboard in its editor). "Notes…" (palette, `▾`, a note tab's menu) lists every note, open or
  closed, newest first, with where it is (open here / in window N / closed), its first line, who
  edited it last and when: type to find one by its name or text, Enter opens it here (from
  closed, or pulled from another window), Ctrl+Enter makes a new note named after what was typed,
  Shift+Delete (or the row's bin) asks to delete it. Jump lists every note too, by its title and
  by its text (two letters or more: the daemon searches, `note.search`, and the matching line is shown). An agent's `note_open` shows
  the note in the group to its right, or a new split there, as a panel opens.
- **Editor**: a monospace `<textarea>` with line wrap on the terminal's background. Typing saves
  by itself half a second after it stops (Ctrl+S saves at once, and never opens the browser's
  "Save page"); leaving the editor, closing the tab, moving it to another window and reloading
  the window save first. A status line under it: Saved / Edited / Saving… / the reason a save
  failed (deleted, reconnecting, over 1 MB), "edited by you" or "edited by Claude (fix auth)"
  with how long ago, the caret's line and column, and the size. Every note tab of a group keeps
  its editor (hidden behind the visible tab), so switching tabs keeps the undo history (Ctrl+Z);
  moving the tab to another group or window, or someone else's change, starts it afresh.
- **Never lost**: typing not yet saved is kept in this browser until the daemon has it. After a
  reload, a closed window or a crash, the window's next page brings it back (a bar says so),
  merged onto the note as it is by then, and saves it. Reload window and Close window first wait
  for saves on their way; when typing can be neither saved nor kept, they ask first.
- **Live**: an agent's change appears at once in the open editor, with the caret and scroll kept
  where they were. Typing that isn't saved yet is merged with it, never lost: changes in
  different places both apply; changes to the same lines keep the human's and then the agent's
  version between marker lines, and a bar above the status line says so (docs/PROTOCOL.md
  "Notes", concurrent edits).
- **Tab**: note icon and title; double-click or F2 renames (one field, "Note"); `×`, middle-click
  and Alt+W (with focus on the strip, not in the text) close it. Closing keeps the note; only
  **Delete note…** (tab menu, palette "Delete this note…", the notes list) removes it, after the
  app's confirm dialog (Cancel focused). Menu: Rename, Split right / down, Zoom group,
  Fullscreen, Move to window ▸, All notes…, Close note, Delete note….
- **Keys**: in the editor, Muster's shortcuts that work in text fields still do (Ctrl+Space,
  Ctrl+K, Ctrl+Shift+P, Ctrl+Tab, Alt+Shift+…); Alt+letters and Alt+arrows stay with the text, as
  in any field; Esc stays too (fullscreen ends with Alt+Shift+Z or the Exit fullscreen button in
  the corner, as for a terminal). The notes list is a combobox: the field keeps focus and the
  arrow keys move the selected row, which screen readers announce.
- The status line's tab count includes notes. The phone UI has no notes (it receives none).

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
  group shows a "zoomed" badge in its strip (click restores). A zoomed group takes the whole
  window body, whatever its share of the split was.
- **Fullscreen** (Alt+Shift+Z, the `⤢` button at the right end of a panel's strip or of the web
  preview's bar, Fullscreen in the tab and terminal menus, and "Fullscreen tab" in the
  palette): one tab fills the whole window, with no tab strips, agents panel or status line. A
  panel keeps its strip, whose button turns into Exit fullscreen; a terminal gets a small
  Exit fullscreen button in its top-right corner. It acts on the group: switching tabs in it
  (Alt+1..9, Ctrl+Tab) stays fullscreen. Per window and in memory only: the saved layout never
  changes, so leaving it shows the layout exactly as it was, and a reload ends it. It also ends
  when focus moves to another group (Jump, Alt+J, Alt+arrows, a new split) or the group goes,
  and on Rename (its fields are in the strip). Works for terminals and panels alike. When the
  switch removes what had the keyboard (a tab in the hidden strip, the corner button), the
  keyboard goes to the tab shown, so a panel's own keys keep working.
  **Esc** goes back only when nothing else wants it:
  - never from a focused terminal (agents use Esc to interrupt, editors to leave a mode), nor
    from a text field; Alt+Shift+Z or the corner button go back from there;
  - an open menu, dialog, Jump, rename or "Close?" takes that Esc and closes first;
  - inside a panel's app, the app gets Esc first. The sandbox helper hands it over only when
    the app did not use it: not `preventDefault()`ed, not typed in a field, and no `<dialog>`
    open. So Esc closes a PR decision's picture or confirm, or clears a table's filter, before
    a second Esc leaves fullscreen. An agent's own app that uses Esc should call
    `preventDefault()`.
  - The web preview's page is another origin and its keys never reach Muster: with focus in
    the page, use the bar's button (Alt+Shift+Z works once focus is on the bar).
  An app's own `ui/request-display-mode` "fullscreen" still zooms its group (it reports
  `displayMode: "fullscreen"` in both cases); asking for "inline" also ends a fullscreen
  showing it. The phone UI has no panels or groups and is unchanged.
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

### Cold cache bar
An agent that has sat idle longer than its provider keeps the conversation cached re-reads the
whole conversation at full cost with its next message. When the active tab is such an agent
(Claude or Codex, never a shell; idle, waiting for approval or an answer, or in error; past its
`SessionInfo.cache_cold_at`, see docs/PROTOCOL.md "Cold prompt cache"), a thin, calm bar sits
right under its terminal, by the CLI's input box, in the exited bar's family (a hollow blue dot,
a light blue tint):

> Idle 1 h 20 m. The prompt cache has likely expired, so your next message re-reads the whole
> conversation at full cost. · **Start fresh** · ×

- The idle time is since the agent's last turn (its start or end), counted by this page's clock
  every 20 s. Hover the sentence for the lifetime that applies ("about 1 h after the last turn").
- **Start fresh** (named agents only): the agent's Start fresh, a new conversation, for when the
  old context isn't needed.
- **×** dismisses the bar for this idle period only (per viewer, kept in localStorage across
  reloads): once a later turn ends and goes cold too, it comes back.
- It never takes the keyboard: its buttons don't take focus when pressed and the terminal keeps
  it. A polite live region inside it says once per idle period (not on each tab switch, and
  without the ticking time) "<agent>: the prompt cache has likely expired, so the next message
  re-reads the whole conversation at full cost." It sits above the terminal's canvases, which can
  overhang it until xterm refits.
- The tab gets a small **cold** marker (after the OpenRouter badge), a tooltip line and ", prompt
  cache likely expired" in its accessible name; the agent's row in the Agents panel gets one
  beside its state (also in its accessible name). The markers stay while the bar is dismissed.

**Cache warning…** (`⋯` "Settings and more", with its state as the hint; palette "Cache warning
settings…"; backends with `prefs.cache_warn` only): a switch "Warn when an agent's cache has likely
expired" and the lifetime in minutes (1 to 1440) for Claude on a subscription (default 1 h),
Claude on OpenRouter or an API key (5 min), Codex on a ChatGPT plan (30 min) and Codex on
OpenRouter (10 min), each with its default and why under it. A Claude whose launch sets its own
cache switch follows that instead (docs/PROTOCOL.md "Cold prompt cache"); the bar's tooltip names
the lifetime that applies. Save sends only the fields edited in this dialog (compared with what
it opened with, so another window's change to a field left alone stays); every window follows at
once. Off hides the bar and every marker. Its body scrolls in a short window; Save stays on
screen.

Muster's own sends are noted too: an agent message, a scheduled run's message or panel events
typed into a cold agent are still delivered, and recorded as a **cold start**: a `cold start`
chip on the message in the Agents panel's message log, in its message overlay, and on the
schedule run that sent it (tooltip: how long it had been idle, and that it likely re-read the
conversation uncached: an estimate).

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
  second time), and every note, open or closed, by its title and text (see "Notes"). Rows show the state dot,
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
  Ctrl+Enter creates. Ctrl+Shift+N skips the dialog (see Worktrees). It has no model choice, so
  no provider choice either: running on OpenRouter is a named agent's setting (below).
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
| Header | "Agents", the count, `⋯` (New agent…, and with a backend that has schedules: New schedule…, Pause all schedules (checked while on), Scheduling settings…), `×` hides the panel. While Pause all is on, an amber line under it reads **All schedules paused · Resume**. |
| Row | State dot (same colours as tabs; hollow when stopped), name, kind badge (CLAUDE / CODEX), and below it the state plus time in state (`idle · 3m`, `stopped`), with an `auto` marker for autostart agents and a blue `cold` marker while its prompt cache has likely expired (see "Cold cache bar"). An agent on OpenRouter gets a third line, an amber **OpenRouter · pay per use** badge (it bills per token). The badge follows what the running process bills to: switched to Subscription while running on OpenRouter it reads **OpenRouter · until restart**, switched the other way **OpenRouter · from restart** (tooltip and accessible name say which); clicking such a running agent shows its tab and a toast that the new provider applies when it restarts (Start fresh, or Stop and start it). Hover: the persona's first line. |
| Click | Starts the agent when no process runs (`agent.start` with this window, so a new tab opens here), then shows its tab exactly as Jump does: pulled into this window's focused group when it is elsewhere or hidden (its tab was closed). Also selects the row, which filters the log. Enter does the same; ↑/↓ move between rows. |
| Closing its tab | Hides it, never stops it. Every way to close the tab does this: the tab's ×, Alt+W, middle-click, Close tab in the tab menu and in the terminal's right-click menu, and the Close button on the "Exited · Restart · Close" bar. No "Close?" confirm; a toast says it keeps running, with a **Show** button that brings it back (the toast names Alt+G only while Alt keys reach the app, not with "Terminal gets Alt keys" on). The daemon decides, not the window: `session.remove` of an agent's session hides it too, so closing before the agent list has loaded can't stop it. The session goes on in no window (messages still reach it; its row shows its live state), survives a backend reload or restart, and comes back with its scrollback on the toast's Show, a row click, Start / Show, Jump or Alt+J. Only Stop (and Start fresh, Remove…) stop it; Kill process in the terminal's right-click menu and `muster kill` still kill the process. Closing a window leaves its tabs as always. |
| Context menu (right-click, `⋯`, Shift+F10) | Edit…, Memory…, New schedule… (preselects this agent), Start / Show, Start fresh (a new conversation, restarting a running one), Stop (`session.kill`), Autostart (checked when on), Remove… (the app's confirm dialog: it stops, its tab closes; its folder stays; its schedules go too). |
| Schedules | Under the row of an agent that has any, on a thin guide line, soonest next run first. Each schedule: a clock, its name, then `next: Mon 09:00` (`today 14:00`, `tomorrow 09:00`, `3 Nov 09:00` beyond a week), `skips next: …` with Skip next on, a **PAUSED** badge when paused, `expired · Renew`, or `done` (a one-off that ran); on the right, the last run's outcome as a chip (replied green, failures red, skips grey, running blue). An amber `expires tomorrow · Renew` line the day before a Claude subscription agent's schedule expires; once `expires_at` passes the row reads `expired · Renew` by the panel's own clock, without waiting for the backend. While Pause all is on, next times are struck through (tooltip says why). Hover: its reading ("At 09:00, Monday to Friday"). ↑/↓ move through agent and schedule rows alike. |
| Schedule menu (right-click, `⋯`, Shift+F10) | Edit…, Run now (outside its timing, quiet hours and daily cap; the agent's own caps still apply; opens the history), Pause / Resume, Skip next run (its time as the hint) / Don't skip next run, Renew (only for schedules that expire: 7 more days), History (checked when shown), Remove… (the app's confirm dialog; its run history goes, the messages it sent stay in the log). |
| Run history | Click a schedule (or Enter, or History) to show its runs under it, newest first, 20 at a time ("Older runs" pages `schedule.runs` with `before`). Each line: a dot in the outcome's colour, `Mon 09:00 · replied · 4m` (the planned time, the outcome, how long it took; skips and refusals have no duration); under it a `catch-up` or `run now` chip and the reason for a skip, refusal, block or failure (`busy: the agent was busy for an hour`, `quiet hours (23:00–07:00)`). Failures red, skips grey. A run that sent a message opens it in the message overlay (its text and reply). Live from `schedule_run` (also while a page loads; pages merge in). After a reconnect, histories are thrown away and the open ones load again; a failed load says so with Try again. |
| + New agent | Under the list: the New agent dialog. |
| Message log | Collapsible (the expanded state is a per-viewer pref), newest first: `from → to`, time, the message's first line, origin and hop chips (non-human origins in amber), a `replied` chip, a `cold start` chip when it was typed into an agent whose cache had likely expired, and for a refused message a red row with the reason. A selected agent filters it (a `to <name> ×` chip clears the filter). "Older messages" pages `agent.messages`. Clicking an entry opens an overlay with the full message, its reply (or "no reply yet") and its route. |

Live updates come from `agent_upserted`, `agent_removed` and `agent_message` (the agent list and
the last 100 messages are loaded after each `hello`); states come from the agents' sessions.
Daemons without agents (`agent.list` is an unknown method) show a one-line note instead.
Schedules and their settings load after each `hello` too (`schedule.list`), then follow
`schedule_upserted`, `schedule_removed`, `schedule_run` and `schedule_settings`; a backend without
them (`schedule.list` is an unknown method) shows no schedule UI at all (no section, menu items,
palette commands or banner). The palette has New schedule…, Pause all schedules / Resume all
schedules (whichever applies) and Scheduling settings….

**Scheduling settings** (header `⋯`, palette): Pause all schedules (a switch; each schedule skips
its runs, recorded once, until resumed), quiet hours from / to (`HH:MM`, this computer's time zone,
named in the hint; they apply to schedules with quiet hours on, the default for Claude
subscription agents), and "Unattended messages per agent per day" (1–200, default 24: scheduled
and triggered messages, not the human's, over any 24 hours; past it the message is refused with
the reason). Save sends only what changed (`schedule.settings.set`).

**New / Edit schedule dialog** (header `⋯`, an agent's menu, the palette, a schedule's Edit…):

- Agent (New only; Edit shows it as fixed text): Codex and OpenRouter agents first ("Best for
  scheduled work"), then Claude subscription agents that accept the Schedule origin, then the
  rest, greyed out and never selectable, with a hint to tick Schedule in Edit agent. The agent
  passed in (an agent's New schedule…) is preselected if it can take scheduled work, else the
  first Codex or OpenRouter agent; a Claude subscription agent is never preselected for you. A
  Claude subscription agent shows the amber unattended-use warning and one line: its recurring
  schedules stop after 7 days unless renewed, and quiet hours are on by default. With no agent
  that takes scheduled work, the field says so and offers **New agent…** (opens the New agent
  dialog on the Background worker template).
- Prompt (monospace), with a hint: `{date}`, `{last_run}` and `{last_reply}` are filled in each
  run. Name (optional; blank: the prompt's first line).
- When: Every 15 minutes, Every 30 minutes, Every hour (at minute :MM), Every day at HH:MM,
  Weekdays at HH:MM (`MM HH * * 1-5`), Once a week on a day at HH:MM, Once at a date and time
  (`datetime-local`, named "in <zone>": a wall-clock time sent as `{kind:"once", local}` and
  read by the backend in the schedule's time zone; Edit shows a stored one-off in that zone),
  Custom (a cron line). Each change asks
  `schedule.preview` (debounced 250 ms; a newer answer always wins) and shows under it the
  reading, the next 3 runs (each `quiet hours: skipped` where it falls in them), "Stops after 7
  days unless you renew it" for a Claude subscription agent's recurring schedule, or the error
  (e.g. faster than every 15 minutes) in red. Edit maps a stored cron line back to its preset
  when it has one's shape, else Custom with the line as stored, and sends `when` only when a
  timing control changed what it describes (or a one-off's zone changed), so a `@daily` made
  with the CLI stays `@daily` and keeps Skip next when only the prompt is edited.
- Segmented choices are radio groups with one Tab stop: arrows, Home and End move and pick.
- If missed (Automatic · Run once when Muster is back · Skip; Automatic says what it does for
  this timing from the preview's `missed_default`), Conversation (Continue · Fresh each run:
  a new conversation each run, never while you're in a turn with it), Notify me (On failure ·
  Always · Never), Also put the reply in (Nothing · A note: an existing note's title from a list,
  or a new title, which makes that note), and "Skip runs in quiet hours (23:00–07:00)" (follows
  the agent's default until changed).
- More options (open in Edit when either is set): at most N runs a day (empty: no cap; Run now
  doesn't count) and the time zone (this browser's by default; 09:00 stays 09:00 across clock
  changes).
- Ctrl+Enter creates or saves; Esc closes. Create (`schedule.create`) opens the panel if it was
  closed and toasts the next run; Save sends only changed fields (`schedule.update`). The
  backend's refusals (an agent that doesn't take scheduled work, a bad cron line) show in the
  dialog.

**New / Edit agent dialog** (+ New agent, the palette's "New agent…", a row's Edit…):

- New only: a template picker (Blank, Orchestrator, Researcher, Reviewer, PR reviewer, Background worker, from
  `agent.templates`) with the template's one-line description; picking one fills every field
  (a name you typed is kept). Name and kind (Claude · Codex).
- **Provider** (shown when the backend supports it): Subscription · OpenRouter. Subscription is
  the CLI's own login, as before. Without `OPENROUTER_API_KEY` on the backend, OpenRouter is
  disabled and a hint under it says how to set it (`env OPENROUTER_API_KEY=… muster reload` from a
  shell that has the key; the key is read only when the backend starts; see docs/PROTOCOL.md
  "OpenRouter"). An agent already on OpenRouter keeps the choice and the hint says it won't start.
  With OpenRouter chosen, an amber note says it bills per token to the OpenRouter key, not the
  subscription, and that Claude Code is built for Anthropic's models (Codex: OpenAI's work
  best). Switching provider keeps the model only when the other list has it.
- Model on OpenRouter replaces the Model dropdown: a search box over OpenRouter's models
  (`openrouter.models`: tool use only), then a list grouped by vendor with the CLI's own vendor
  first (Anthropic for Claude, OpenAI for Codex), each row the model's name, id, price per million
  tokens in / out and context size; Default first for Claude (Claude Code's choice through
  OpenRouter's latest Anthropic aliases), Custom… last (any id). Every word typed must match the
  id or the name; ↓ goes from the search box into the list, ↑/↓/Home/End move in it, Enter in the
  box picks the first match. The list is one Tab stop (the chosen model): the dialog's focus trap
  skips elements with `tabindex="-1"`, so Tab never walks the rows. Under it: how many models and where from, or, with no list, why it
  couldn't load and that Custom… still works. A Codex agent on OpenRouter has no Default: Create
  stays off until a model is chosen.
- Model and Effort dropdowns for the agent's kind (`agent.models`): Default (the CLI's
  choice), the CLI's models (Claude's aliases such as `opus`; Codex's model ids) or levels, and
  Custom…, which shows a text field for any other value. A saved value the list doesn't have
  shows as Custom with its text, and Custom stays chosen (also with its field cleared) until
  another entry is picked. Effort lists the chosen model's levels where they differ from the
  CLI's; changing the model puts a listed level it doesn't offer back to Default (a custom one
  stays). Switching kind puts a model or level the new kind doesn't list back to Default. Then persona (monospace textarea, written to `CLAUDE.md` / `AGENTS.md`), accepted origins (Human, Schedule, Webhook checkboxes,
  at least one), the Permissions section showing only the control for the agent's kind, and an
  Autostart toggle.
- Ticking Schedule or Webhook on a Claude agent shows, in amber: "Anthropic's consumer terms are
  unclear about unattended use; scheduled work is safer on a Codex agent."
- Edit shows the kind badge, name and home folder, and a second tab, **Memory**: the files in
  the agent's `memory/` on the left, under an **Add files…** button and above a field to create
  a note by name, and a plain-text editor on the right with Delete and Save (Ctrl+S). Switching
  files or closing with unsaved changes asks first (closing: a second Esc discards).
  - Empty `memory/`: the right side explains it (the agent's notebook; its `CLAUDE.md` /
    `AGENTS.md` tells it to read these files when a conversation starts and to keep its notes
    there) and offers **Add files…** and **New note** (focuses the name field).
  - **Add files…** opens the browser's file picker (`<input type=file multiple>`; the native
    window shows the system's); dropping files anywhere on the tab does the same (a dashed
    outline while dragging). Each file goes through `agent.memory.add`: text files only (UTF-8,
    no NUL characters), at most 256 KB, a plain visible name (no folders, nothing starting with
    `.`); others are skipped with the reason. Nothing is replaced: a name that is taken gets a
    number (`notes.md` → `notes-2.md`). A note above the editor lists what was added (and
    renamed) and skipped, and says that the agent reads memory/ when a new conversation starts,
    so a running or resumed conversation needs to be asked to read memory/. Files picked or
    dropped while a batch is still being added join it (the outline says so), never dropped.
    The first added file opens, unless the open file has unsaved edits or the human edits or
    opens something before its text arrives.
  - **Delete** asks in a bar under the file's name (an `alertdialog` its buttons are described
    by; Cancel focused; Cancel or Esc answers no and puts focus back on Delete) and removes the
    file (`agent.memory.delete`; focus then goes to Add files…); a note never saved is just
    discarded. A delete that finishes after another file was opened or edited leaves that work
    alone. The question is in the tab, not the app's confirm dialog, which would replace the
    Edit dialog and lose its unsaved Settings.
  - Files are named exactly as listed (an agent's `notes.md ` is not `notes.md`); a new note's
    name can't have invisible characters or spaces around a folder name.
- Edit saves only changed fields (`agent.update`); provider, model, effort and permissions apply
  from the agent's next start, which the toast says when it runs. Edit's title shows the
  OpenRouter badge for an agent on it.
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
| Alt+W | Close the focused tab (inline confirm if working; a named agent's tab is hidden, its agent keeps running); an emptied group closes |
| Alt+←↑→↓ | Focus the group in that direction |
| Alt+Shift+←↑→↓ | Resize the focused group |
| Alt+Z | Zoom the focused group |
| Alt+Shift+Z | Fullscreen: the focused tab fills the window (again, or Esc outside terminals and text fields, goes back) |
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

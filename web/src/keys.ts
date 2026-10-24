// One command table drives keyboard shortcuts, the command palette and the shortcuts sheet.
// App shortcuts are matched in the capture phase, before xterm or inputs see them.

import * as A from "./actions.ts";
import {
  agentsSupported,
  altHeld,
  confirmClose,
  daemonRestarting,
  daemonStopped,
  focusedSession,
  focusedTabId,
  focusedTask,
  fullscreen,
  hereIds,
  overlay,
  renaming,
  prefs,
  schedulesSupported,
  scheduleSettings,
  setPref,
  toast,
  windowId,
  windowPrefs,
  windowsSupported,
} from "./state.ts";
import { hasTerminalFocus, setShortcutMatcher } from "./terms.ts";
import { noteId } from "./layout/tree.ts";
import { notesSupported } from "./notes.ts";
import { canCloseWindow, isNative } from "./native.ts";
import { latencyMonitorOn, setLatencyMonitor } from "./latency_monitor.ts";
import { isMac } from "./util.ts";
import type { KeyChord, PanelKey } from "./panels/bridge.ts";

export interface KeySpec {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
  code: string;
}

export interface Command {
  id: string;
  title: string;
  group: string;
  /** Display + match strings like "Alt+Shift+N". First is primary. */
  keys?: string[];
  run: () => void;
  /** Hidden from the palette (still bound). */
  hidden?: boolean;
  enabled?: () => boolean;
}

export interface KeyCtx {
  inTerminal: boolean;
  inInput: boolean;
}

const CODE_ALIASES: Record<string, string> = {
  "[": "BracketLeft",
  "]": "BracketRight",
  "/": "Slash",
  "=": "Equal",
  "-": "Minus",
  "0": "Digit0",
  "←": "ArrowLeft",
  "→": "ArrowRight",
  "↑": "ArrowUp",
  "↓": "ArrowDown",
  Left: "ArrowLeft",
  Right: "ArrowRight",
  Up: "ArrowUp",
  Down: "ArrowDown",
  Enter: "Enter",
  Esc: "Escape",
};

export function parseKey(s: string): KeySpec {
  const parts = s.split("+");
  const k: KeySpec = { ctrl: false, alt: false, shift: false, meta: false, code: "" };
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (p === "" && i === parts.length - 1) {
      k.code = "Equal"; // "Ctrl++"
      continue;
    }
    if (i < parts.length - 1) {
      if (p === "Ctrl") k.ctrl = true;
      else if (p === "Alt") k.alt = true;
      else if (p === "Shift") k.shift = true;
      else if (p === "Meta" || p === "Cmd") k.meta = true;
      continue;
    }
    if (CODE_ALIASES[p]) k.code = CODE_ALIASES[p];
    else if (/^[A-Z]$/.test(p)) k.code = "Key" + p;
    else if (/^[0-9]$/.test(p)) k.code = "Digit" + p;
    else k.code = p; // F1, Tab, ...
  }
  return k;
}

export function matches(k: KeySpec, e: KeyboardEvent): boolean {
  if (e.ctrlKey !== k.ctrl || e.altKey !== k.alt || e.shiftKey !== k.shift || e.metaKey !== k.meta) {
    return false;
  }
  if (e.code === k.code) return true;
  // Numpad/alternate layouts: fall back to key for digits and symbols.
  if (k.code === "Equal" && (e.key === "=" || e.key === "+")) return true;
  if (k.code === "Minus" && e.key === "-") return true;
  return false;
}

/** Pretty label for display (Mac glyphs on macOS). */
export function keyLabel(s: string): string[] {
  const parts = s.split("+").filter((p, i, a) => p !== "" || i === a.length - 1);
  return parts.map((p) => {
    if (p === "") return "+";
    if (isMac) {
      if (p === "Alt") return "⌥";
      if (p === "Ctrl") return "⌃";
      if (p === "Shift") return "⇧";
      if (p === "Meta" || p === "Cmd") return "⌘";
    }
    if (p === "Left") return "←";
    if (p === "Right") return "→";
    if (p === "Up") return "↑";
    if (p === "Down") return "↓";
    return p;
  });
}

const hasTab = () => focusedTabId.value !== null;
const hasTerminal = () => focusedSession.value !== null;
/** The focused tab's note id, when it is a note. */
const focusedNote = () => (focusedTabId.value === null ? null : noteId(focusedTabId.value));

function onOff(on: boolean, onText: string, offText: string) {
  toast("info", on ? onText : offText);
}

export const commands: Command[] = [
  // Navigation
  {
    id: "jump",
    title: "Jump to a tab, session or folder",
    group: "Navigate",
    // Ctrl+Space is free in shells and agent CLIs (it only sends NUL); it also closes Jump.
    keys: ["Ctrl+Space", "Ctrl+Shift+K", "Ctrl+K"],
    run: () => (overlay.value?.kind === "jump" ? (overlay.value = null) : A.openJump()),
  },
  {
    id: "palette",
    title: "Command palette",
    group: "Navigate",
    keys: ["Ctrl+Shift+P"],
    run: () => A.openJump(">"),
  },
  {
    id: "next-needs-you",
    title: "Next session that needs you",
    group: "Navigate",
    keys: ["Alt+J"],
    run: () => A.nextNeedsYou(),
  },
  {
    id: "next-tab",
    title: "Next tab in group",
    group: "Navigate",
    keys: ["Ctrl+Tab", "Alt+]"],
    run: () => A.tabStep(1),
  },
  {
    id: "prev-tab",
    title: "Previous tab in group",
    group: "Navigate",
    keys: ["Ctrl+Shift+Tab", "Alt+["],
    run: () => A.tabStep(-1),
  },
  ...Array.from({ length: 9 }, (_, i): Command => ({
    id: `tab-${i + 1}`,
    title: `Go to tab ${i + 1} of the group`,
    group: "Navigate",
    keys: [`Alt+${i + 1}`],
    hidden: true,
    run: () => A.gotoTabIndex(i),
  })),

  // Tabs
  {
    id: "new-tab",
    title: "New tab (same folder)",
    group: "Tab",
    keys: ["Alt+T", "Ctrl+Shift+T"],
    run: () => A.newTabHere(),
  },
  {
    id: "new-worktree-tab",
    title: "New worktree tab",
    group: "Tab",
    keys: ["Ctrl+Shift+N"],
    run: () => A.newTabHere({ worktree: true }),
  },
  {
    id: "new-tab-dialog",
    title: "New tab…",
    group: "Tab",
    keys: ["Alt+Shift+N"],
    run: () => A.newTabDialog(),
  },
  {
    id: "new-terminal",
    title: "New terminal in this task…",
    group: "Tab",
    keys: ["Alt+N"],
    run: () => A.spawnPicker(),
    enabled: hasTab,
  },
  {
    id: "new-claude",
    title: "New Claude in this task",
    group: "Tab",
    run: () => A.spawnHere("claude"),
    enabled: hasTab,
  },
  {
    id: "new-codex",
    title: "New Codex in this task",
    group: "Tab",
    run: () => A.spawnHere("codex"),
    enabled: hasTab,
  },
  {
    id: "new-shell",
    title: "New shell in this task",
    group: "Tab",
    run: () => A.spawnHere("shell"),
    enabled: hasTab,
  },
  {
    id: "move-tab-left",
    title: "Move tab left",
    group: "Tab",
    keys: ["Alt+Shift+["],
    run: () => A.shiftFocusedTab(-1),
    enabled: hasTab,
  },
  {
    id: "move-tab-right",
    title: "Move tab right",
    group: "Tab",
    keys: ["Alt+Shift+]"],
    run: () => A.shiftFocusedTab(1),
    enabled: hasTab,
  },
  ...(["Left", "Right", "Up", "Down"] as const).map((d): Command => ({
    id: `move-tab-${d.toLowerCase()}`,
    title: `Move tab to the group ${d === "Left" || d === "Right" ? "to the " : ""}${
      { Left: "left", Right: "right", Up: "above", Down: "below" }[d]
    }`,
    group: "Tab",
    keys: [`Ctrl+Alt+${d}`],
    run: () => A.moveFocusedTab(d.toLowerCase() as "left"),
    enabled: hasTab,
  })),
  {
    id: "rename-tab",
    title: "Rename",
    group: "Tab",
    enabled: () => hasTerminal() || focusedNote() !== null,
    run: () => {
      const s = focusedSession.value;
      const note = focusedNote();
      if (s) A.startRename(s.id, "terminal");
      else if (note !== null) A.startNoteRename(note);
    },
  },

  // Notes (docs/UI.md "Notes")
  {
    id: "new-note",
    title: "New note",
    group: "Notes",
    enabled: () => notesSupported.value && windowId.value !== null,
    run: () => A.newNote(),
  },
  {
    id: "notes",
    title: "Notes… (reopen, find, delete)",
    group: "Notes",
    enabled: () => notesSupported.value,
    run: () => A.openNotesList(),
  },
  {
    id: "delete-note",
    title: "Delete this note…",
    group: "Notes",
    enabled: () => focusedNote() !== null,
    run: () => {
      const note = focusedNote();
      if (note !== null) A.deleteNoteAsk(note);
    },
  },
  {
    id: "tab-permissions",
    title: "Task permissions…",
    group: "Tab",
    enabled: hasTab,
    run: () => focusedTask.value && A.openPermissions(focusedTask.value.id),
  },
  {
    id: "tab-new-window",
    title: "Move tab to a new window",
    group: "Tab",
    enabled: () => hasTab() && windowsSupported.value && windowId.value !== null,
    run: () => { if (focusedTabId.value !== null) A.moveTabToWindow(focusedTabId.value, "new"); },
  },
  {
    id: "close-tab",
    title: "Close tab",
    group: "Tab",
    keys: ["Alt+W"],
    enabled: hasTab,
    run: () => A.closeFocused(),
  },
  {
    id: "restart",
    title: "Restart tab's process",
    group: "Tab",
    run: () => {
      const s = focusedSession.value;
      if (s) A.restartSession(s.id);
    },
    enabled: hasTerminal,
  },
  {
    id: "interrupt",
    title: "Interrupt (Esc to agent)",
    group: "Tab",
    run: () => {
      const s = focusedSession.value;
      if (s) A.interruptSession(s.id);
    },
    enabled: hasTerminal,
  },
  {
    id: "approve",
    title: "Approve prompt",
    group: "Tab",
    keys: ["Alt+A"],
    run: () => A.approveFocused("approve"),
  },
  {
    id: "deny",
    title: "Deny prompt",
    group: "Tab",
    keys: ["Alt+Shift+A"],
    run: () => A.approveFocused("deny"),
  },
  {
    id: "rename-window",
    title: "Rename window…",
    group: "Window",
    enabled: () => windowId.value !== null,
    run: () => (overlay.value = { kind: "rename_window" }),
  },
  {
    id: "reload-window",
    title: "Reload window",
    group: "Window",
    // Plain Ctrl+R stays with the terminal (shell history search).
    keys: ["Ctrl+Shift+R"],
    run: () => A.reloadWindow(),
  },
  {
    id: "close-window",
    title: "Close window",
    group: "Window",
    enabled: canCloseWindow,
    run: () => A.closeWindow(),
  },
  {
    id: "title-bar",
    get title() {
      return `System title bar: ${windowPrefs.value?.decorations !== false ? "on (turn off)" : "off (turn on)"}`;
    },
    group: "Window",
    enabled: () => isNative && windowPrefs.value !== null,
    run: () => A.setDecorations(windowPrefs.value?.decorations === false),
  },
  {
    id: "cache_settings",
    title: "Cache warning settings…",
    group: "Window",
    enabled: () => windowPrefs.value?.cache_warn !== undefined,
    run: () => (overlay.value = { kind: "cache_settings" }),
  },
  {
    id: "shell",
    title: "Shell for new terminals…",
    group: "Window",
    enabled: () => windowPrefs.value !== null,
    run: () => (overlay.value = { kind: "shell" }),
  },
  {
    id: "restart-muster",
    title: "Restart Muster…",
    group: "Window",
    run: () => (overlay.value = { kind: "restart" }),
  },
  {
    id: "shutdown",
    title: "Shut down Muster…",
    group: "Window",
    run: () => (overlay.value = { kind: "shutdown" }),
  },

  // Agents
  {
    id: "agents-panel",
    title: "Toggle agents panel",
    group: "Agents",
    keys: ["Alt+G"],
    run: () => A.toggleAgentsPanel(),
  },
  {
    id: "new-agent",
    title: "New agent…",
    group: "Agents",
    enabled: () => agentsSupported.value,
    run: () => A.newAgentDialog(),
  },
  {
    id: "new-schedule",
    title: "New schedule…",
    group: "Agents",
    enabled: () => schedulesSupported.value,
    run: () => A.newScheduleDialog(),
  },
  {
    id: "pause-schedules",
    title: "Pause all schedules",
    group: "Agents",
    enabled: () => schedulesSupported.value && !scheduleSettings.value?.paused,
    run: () => A.setAllSchedulesPaused(true),
  },
  {
    id: "resume-schedules",
    title: "Resume all schedules",
    group: "Agents",
    enabled: () => schedulesSupported.value && !!scheduleSettings.value?.paused,
    run: () => A.setAllSchedulesPaused(false),
  },
  {
    id: "schedule-settings",
    title: "Scheduling settings…",
    group: "Agents",
    enabled: () => schedulesSupported.value,
    run: () => A.scheduleSettingsDialog(),
  },

  // Groups
  {
    id: "split-right",
    title: "Split right (new shell in this task)",
    group: "Groups",
    keys: ["Alt+D"],
    run: () => A.splitFocused("right"),
    enabled: hasTab,
  },
  {
    id: "split-down",
    title: "Split down (new shell in this task)",
    group: "Groups",
    keys: ["Alt+Shift+D"],
    run: () => A.splitFocused("bottom"),
    enabled: hasTab,
  },
  { id: "zoom", title: "Zoom group", group: "Groups", keys: ["Alt+Z"], run: () => A.zoomToggle() },
  {
    id: "fullscreen",
    get title() {
      return fullscreen.value === null ? "Fullscreen tab (fills the window)" : "Exit fullscreen";
    },
    group: "Groups",
    // Alt+Shift+…, so it also reaches the app with "Terminal gets Alt keys" on.
    keys: ["Alt+Shift+Z"],
    run: () => A.toggleFullscreen(),
    enabled: () => hasTab() || fullscreen.value !== null,
  },
  { id: "equalize", title: "Equalize splits", group: "Groups", run: () => A.equalizeAll() },
  ...(["Left", "Right", "Up", "Down"] as const).map((d): Command => ({
    id: `focus-${d.toLowerCase()}`,
    title: `Focus group ${d.toLowerCase()}`,
    group: "Groups",
    keys: [`Alt+${d}`],
    hidden: true,
    run: () => A.focusDir(d.toLowerCase() as "left"),
  })),
  ...(["Left", "Right", "Up", "Down"] as const).map((d): Command => ({
    id: `resize-${d.toLowerCase()}`,
    title: `Resize group ${d.toLowerCase()}`,
    group: "Groups",
    keys: [`Alt+Shift+${d}`],
    hidden: true,
    run: () => A.resizeDir(d.toLowerCase() as "left"),
  })),

  // View
  { id: "find", title: "Find in terminal", group: "View", keys: ["Ctrl+Shift+F"], run: () => A.openFind() },
  {
    id: "font-up",
    title: "Terminal font bigger",
    group: "View",
    keys: ["Ctrl+="],
    run: () => A.setFontSize(prefs.value.fontSize + 1),
  },
  {
    id: "font-down",
    title: "Terminal font smaller",
    group: "View",
    keys: ["Ctrl+-"],
    run: () => A.setFontSize(prefs.value.fontSize - 1),
  },
  {
    id: "font-reset",
    title: "Terminal font reset",
    group: "View",
    keys: ["Ctrl+0"],
    run: () => A.setFontSize(13),
  },
  { id: "theme", title: "Cycle theme (system / dark / light)", group: "View", run: () => A.cycleTheme() },
  {
    id: "status-line",
    get title() {
      return `Status line: ${prefs.value.statusLine ? "on (turn off)" : "off (turn on)"}`;
    },
    group: "View",
    run: () => A.toggleStatusLine(),
  },
  {
    id: "alt-to-terminal",
    get title() {
      return `Terminal gets Alt keys: ${prefs.value.altToTerminal ? "on (turn off)" : "off (turn on)"}`;
    },
    group: "View",
    run: () => {
      const on = !prefs.value.altToTerminal;
      setPref("altToTerminal", on);
      onOff(
        on,
        "Alt keys now go to the terminal. App shortcuts: Alt+Shift+… and Ctrl+Shift+…",
        "Alt shortcuts work in terminals again",
      );
    },
  },
  {
    id: "jump-raises",
    get title() {
      return `Jump focuses the other window: ${prefs.value.jumpRaises ? "on (turn off)" : "off (turn on)"}`;
    },
    group: "View",
    enabled: () => windowsSupported.value,
    run: () => {
      const on = !prefs.value.jumpRaises;
      setPref("jumpRaises", on);
      onOff(
        on,
        "Jump now focuses the window a tab is in (and pulls the tab here when it can't)",
        "Jump pulls tabs from other windows into this one",
      );
    },
  },
  {
    id: "latency-monitor",
    get title() {
      return `Typing latency monitor: ${latencyMonitorOn() ? "on (turn off)" : "off (turn on)"}`;
    },
    group: "View",
    run: () => setLatencyMonitor(!latencyMonitorOn()),
  },
  {
    id: "shortcuts",
    title: "Keyboard shortcuts",
    group: "Help",
    keys: ["Alt+/", "F1"],
    run: () => (overlay.value = overlay.value?.kind === "shortcuts" ? null : { kind: "shortcuts" }),
  },
];

// Ctrl+K only when focus isn't in a terminal (shells use it for kill-line).
const WHEN: Record<string, (ctx: KeyCtx, key: string) => boolean> = {
  jump: (ctx, key) => key !== "Ctrl+K" || !ctx.inTerminal,
};

interface Binding {
  key: string;
  spec: KeySpec;
  cmd: Command;
}
const bindings: Binding[] = commands.flatMap((cmd) =>
  (cmd.keys ?? []).map((key) => ({ key, spec: parseKey(key), cmd }))
);

/** Terminal focus counts as "not an input": xterm's helper textarea is an implementation detail. */
function ctxOf(e: KeyboardEvent): KeyCtx {
  const t = e.target as HTMLElement | null;
  const inTerminal = hasTerminalFocus() !== null || !!t?.closest?.(".term-el");
  const inInput = !inTerminal && !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" ||
    t.tagName === "SELECT" || t.isContentEditable);
  return { inTerminal, inInput };
}

/** Alt+letter / Alt+arrow: word motion and editing keys in text fields, readline and fish. */
function isTextEditingAlt(k: KeySpec): boolean {
  return k.alt && !k.shift && !k.ctrl && !k.meta && /^(Key[A-Z]|Arrow)/.test(k.code);
}

export function findBinding(e: KeyboardEvent, ctx: KeyCtx = ctxOf(e)): Binding | null {
  if (e.type !== "keydown") return null;
  for (const b of bindings) {
    if (!matches(b.spec, e)) continue;
    // Let text fields keep Alt+letters and Alt+arrows (word motion, etc.).
    if (ctx.inInput && isTextEditingAlt(b.spec)) continue;
    // Opt-in: a focused terminal gets every Alt key except Alt+Shift+….
    if (ctx.inTerminal && prefs.value.altToTerminal && b.spec.alt && !b.spec.shift) continue;
    const w = WHEN[b.cmd.id];
    if (w && !w(ctx, b.key)) continue;
    return b;
  }
  return null;
}

/** Keys the app claims while an overlay is open (the overlay handles its own keys). */
function overlayAllows(b: Binding): boolean {
  // Muster is restarting or shut down: no shortcut may act (one would reach a later backend, or
  // none). The screen shown then has nothing a shortcut drives.
  if (daemonRestarting.value || daemonStopped.value) return false;
  const o = overlay.value;
  if (!o) return true;
  // Toggle-style commands still work from inside overlays.
  return ["jump", "palette", "shortcuts"].includes(b.cmd.id);
}

/** A pending "Close?" only takes keys while its question is on screen in this window. */
function pendingHere(sid: number): boolean {
  return hereIds.value.has(sid);
}

// Shortcuts that also work while a panel's app has focus (its frame swallows keys; the sandbox
// helper hands these to us). Navigation only: an app can fake these messages, so nothing here
// starts, approves, interrupts or types into anything. Close closes that panel itself.
const PANEL_COMMANDS = new Set([
  "jump", "palette", "next-needs-you", "next-tab", "prev-tab", "move-tab-left", "move-tab-right",
  "close-tab", "agents-panel", "zoom", "fullscreen", "shortcuts", ...Array.from({ length: 9 }, (_, i) => `tab-${i + 1}`),
  ...["left", "right", "up", "down"].map((d) => `focus-${d}`),
]);
const panelBindings = () => bindings.filter((b) => PANEL_COMMANDS.has(b.cmd.id));

/** The chords a panel's sandbox should hand to the host (see web/panels/proxy.html). */
export function panelChords(): KeyChord[] {
  return panelBindings().map(({ spec }) => ({ ...spec, editAlt: isTextEditingAlt(spec) }));
}

/** A shortcut pressed inside panel tab `tab`'s app (the bridge checked the panel has focus). */
export function runPanelKey(tab: number, k: PanelKey) {
  // An Esc the app did not use (the sandbox helper hands over only those) leaves fullscreen.
  if (k.code === "Escape" && !k.ctrl && !k.alt && !k.shift && !k.meta) {
    // Only this focused panel's key reaches here (PanelView), and fullscreen always shows the
    // focused group.
    if (!overlay.value) A.exitFullscreen();
    return;
  }
  const e = new KeyboardEvent("keydown", {
    code: k.code, key: k.key, ctrlKey: k.ctrl, altKey: k.alt, shiftKey: k.shift, metaKey: k.meta,
  });
  const b = findBinding(e, { inTerminal: false, inInput: k.inInput });
  if (!b || !PANEL_COMMANDS.has(b.cmd.id) || !overlayAllows(b)) return;
  if (b.cmd.id === "close-tab") A.closeTab(tab);
  else if (!b.cmd.enabled || b.cmd.enabled()) b.cmd.run();
}

export function installKeys() {
  setShortcutMatcher((e) => {
    const b = findBinding(e);
    return !!b && overlayAllows(b);
  });
  installFullscreenEscape();
  // Clicking anywhere but the "Close?" question itself cancels it.
  addEventListener(
    "pointerdown",
    (e) => {
      if (confirmClose.value === null) return;
      if (!(e.target as HTMLElement)?.closest?.(".confirm-close")) confirmClose.value = null;
    },
    { capture: true },
  );
  installAltHeld();
  addEventListener(
    "keydown",
    (e) => {
      // Restarting or shut down: no key acts on the app (see `overlayAllows`).
      if (daemonRestarting.value || daemonStopped.value) return;
      // Inline "Close? ⏎ / Esc". Enter/Esc must not reach the terminal (Enter would submit the
      // agent's prompt), so terminal focus counts here.
      const pending = confirmClose.value;
      const here = pending !== null && pendingHere(pending);
      if (pending !== null && !here) confirmClose.value = null;
      if (
        pending && here && !overlay.value && !ctxOf(e).inInput && !e.isComposing &&
        !document.querySelector(".menu")
      ) {
        const bare = !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey;
        const modifierOnly = ["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"].includes(e.key);
        // Anything but a bare Enter/Esc means the user moved on: drop the question, pass the key.
        if (!modifierOnly && !(bare && (e.key === "Enter" || e.key === "Escape"))) {
          confirmClose.value = null;
        } else if (e.key === "Enter") {
          e.preventDefault();
          e.stopPropagation();
          confirmClose.value = null;
          A.closeTab(pending, true);
          return;
        } else if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          confirmClose.value = null;
          return;
        }
      }
      const b = findBinding(e);
      if (!b || !overlayAllows(b)) return;
      if (b.cmd.enabled && !b.cmd.enabled()) {
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      const o = overlay.value;
      if (o?.kind === "jump" && (b.cmd.id === "jump" || b.cmd.id === "palette")) {
        const wantCmd = b.cmd.id === "palette";
        if (wantCmd === o.initial.startsWith(">")) {
          overlay.value = null;
          A.focusActiveTerminalSoon();
          return;
        }
      }
      if (o && b.cmd.id !== "shortcuts") overlay.value = null;
      b.cmd.run();
    },
    { capture: true },
  );
}

/** Esc leaves fullscreen once everything else has had it. A menu, dialog, rename or inline
 * "Close?" open when it was pressed keeps it (they close first), and so do a focused terminal
 * and text fields: agents, editors and fields use Esc. Inside a panel's app, the sandbox
 * helper hands over only an Esc the app left alone (runPanelKey). */
function installFullscreenEscape() {
  let busy = false;
  // Before anything closes on this Esc (registered ahead of the shortcut listener).
  addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      busy = !!overlay.value || !!document.querySelector(".menu") || renaming.value !== null || confirmClose.value !== null;
    }
  }, { capture: true });
  addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || busy || e.defaultPrevented || e.isComposing || fullscreen.value === null) return;
    if (e.ctrlKey || e.altKey || e.shiftKey || e.metaKey) return;
    const ctx = ctxOf(e);
    if (ctx.inTerminal || ctx.inInput) return;
    e.preventDefault();
    A.exitFullscreen();
  });
}

/** Holding Alt alone for a moment shows the focused group's Alt+1..9 tab numbers. */
function installAltHeld() {
  let timer = 0;
  const clear = () => {
    clearTimeout(timer);
    timer = 0;
    if (altHeld.value) altHeld.value = false;
  };
  addEventListener("keydown", (e) => {
    if (e.key === "Alt" && !e.ctrlKey && !e.shiftKey && !e.metaKey && !e.repeat) {
      clearTimeout(timer);
      timer = setTimeout(() => (altHeld.value = true), 350);
    } else if (e.key !== "Alt") clear();
  }, { capture: true });
  addEventListener("keyup", (e) => {
    if (e.key === "Alt" || !e.altKey) clear();
  }, { capture: true });
  addEventListener("blur", clear);
  addEventListener("pointerdown", clear, { capture: true });
}

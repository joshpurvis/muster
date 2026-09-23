// One command table drives keyboard shortcuts, the command palette and the shortcuts sheet.
// App shortcuts are matched in the capture phase, before xterm or inputs see them.

import * as A from "./actions.ts";
import {
  activeChannel,
  altHeld,
  confirmClose,
  overlay,
  type PendingClose,
  prefs,
  renaming,
  sessions,
  setPref,
  tabIdSet,
  toast,
  windowId,
  windowsSupported,
} from "./state.ts";
import { hasTerminalFocus, setShortcutMatcher } from "./terms.ts";
import { isMac } from "./util.ts";

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

const hasTab = () => activeChannel.value !== null;

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
    title: "Next tab",
    group: "Navigate",
    keys: ["Ctrl+Tab", "Ctrl+Alt+Right"],
    run: () => A.stepTab(1),
  },
  {
    id: "prev-tab",
    title: "Previous tab",
    group: "Navigate",
    keys: ["Ctrl+Shift+Tab", "Ctrl+Alt+Left"],
    run: () => A.stepTab(-1),
  },
  ...Array.from({ length: 9 }, (_, i): Command => ({
    id: `tab-${i + 1}`,
    title: `Go to tab ${i + 1}`,
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
    id: "move-tab-left",
    title: "Move tab left",
    group: "Tab",
    keys: ["Alt+Shift+["],
    run: () => A.moveActiveTab(-1),
    enabled: hasTab,
  },
  {
    id: "move-tab-right",
    title: "Move tab right",
    group: "Tab",
    keys: ["Alt+Shift+]"],
    run: () => A.moveActiveTab(1),
    enabled: hasTab,
  },
  {
    id: "rename-tab",
    title: "Rename tab",
    group: "Tab",
    enabled: hasTab,
    run: () => {
      const c = activeChannel.value;
      if (c) renaming.value = { kind: "channel", id: c.id };
    },
  },
  {
    id: "tab-permissions",
    title: "Tab permissions…",
    group: "Tab",
    enabled: hasTab,
    run: () => activeChannel.value && A.openPermissions(activeChannel.value.id),
  },
  {
    id: "tab-new-window",
    title: "Move tab to a new window",
    group: "Tab",
    enabled: () => hasTab() && windowsSupported.value && windowId.value !== null,
    run: () => activeChannel.value && A.moveTabToWindow(activeChannel.value.id, "new"),
  },
  {
    id: "close-tab",
    title: "Close tab",
    group: "Tab",
    enabled: hasTab,
    run: () => activeChannel.value && A.closeTab(activeChannel.value.id),
  },
  {
    id: "rename-window",
    title: "Rename window…",
    group: "Tab",
    enabled: () => windowId.value !== null,
    run: () => (overlay.value = { kind: "rename_window" }),
  },

  // Sessions
  {
    id: "new-session",
    title: "New session in pane…",
    group: "Session",
    keys: ["Alt+N"],
    run: () => A.spawnPicker(),
    enabled: hasTab,
  },
  {
    id: "new-claude",
    title: "New Claude session in pane",
    group: "Session",
    run: () => A.spawnHere("claude"),
    enabled: hasTab,
  },
  {
    id: "new-codex",
    title: "New Codex session in pane",
    group: "Session",
    run: () => A.spawnHere("codex"),
    enabled: hasTab,
  },
  {
    id: "new-shell",
    title: "New shell in pane",
    group: "Session",
    run: () => A.spawnHere("shell"),
    enabled: hasTab,
  },
  {
    id: "close-pane",
    title: "Close pane",
    group: "Session",
    keys: ["Alt+W"],
    run: () => A.closeFocused(),
    enabled: hasTab,
  },
  {
    id: "restart",
    title: "Restart session",
    group: "Session",
    run: () => {
      const s = A.focusedSession();
      if (s) A.restartSession(s.id);
    },
    enabled: () => !!A.focusedSession(),
  },
  {
    id: "interrupt",
    title: "Interrupt session (Esc to agent)",
    group: "Session",
    run: () => {
      const s = A.focusedSession();
      if (s) A.interruptSession(s.id);
    },
    enabled: () => !!A.focusedSession(),
  },
  {
    id: "approve",
    title: "Approve prompt",
    group: "Session",
    keys: ["Alt+A"],
    run: () => A.approveFocused("approve"),
  },
  {
    id: "deny",
    title: "Deny prompt",
    group: "Session",
    keys: ["Alt+Shift+A"],
    run: () => A.approveFocused("deny"),
  },

  // Panes
  {
    id: "split-right",
    title: "Split right (new shell)",
    group: "Panes",
    keys: ["Alt+D"],
    run: () => A.splitFocused("right"),
    enabled: hasTab,
  },
  {
    id: "split-down",
    title: "Split down (new shell)",
    group: "Panes",
    keys: ["Alt+Shift+D"],
    run: () => A.splitFocused("bottom"),
    enabled: hasTab,
  },
  { id: "zoom", title: "Zoom pane", group: "Panes", keys: ["Alt+Z"], run: () => A.zoomToggle() },
  { id: "equalize", title: "Equalize splits", group: "Panes", run: () => A.equalizeAll() },
  {
    id: "prev-pane-tab",
    title: "Previous session in pane",
    group: "Panes",
    keys: ["Alt+["],
    run: () => A.paneTabStep(-1),
  },
  {
    id: "next-pane-tab",
    title: "Next session in pane",
    group: "Panes",
    keys: ["Alt+]"],
    run: () => A.paneTabStep(1),
  },
  ...(["Left", "Right", "Up", "Down"] as const).map((d): Command => ({
    id: `focus-${d.toLowerCase()}`,
    title: `Focus pane ${d.toLowerCase()}`,
    group: "Panes",
    keys: [`Alt+${d}`],
    hidden: true,
    run: () => A.focusDir(d.toLowerCase() as "left"),
  })),
  ...(["Left", "Right", "Up", "Down"] as const).map((d): Command => ({
    id: `resize-${d.toLowerCase()}`,
    title: `Resize pane ${d.toLowerCase()}`,
    group: "Panes",
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

export function findBinding(e: KeyboardEvent): Binding | null {
  if (e.type !== "keydown") return null;
  const ctx = ctxOf(e);
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
  const o = overlay.value;
  if (!o) return true;
  // Toggle-style commands still work from inside overlays.
  return ["jump", "palette", "shortcuts"].includes(b.cmd.id);
}

/** A pending "Close?" only takes keys while its question is on screen in this window. */
function pendingHere(p: PendingClose): boolean {
  if (p.kind === "channel") return tabIdSet.value.has(p.id);
  const s = sessions.value.get(p.id);
  return !!s && s.channel_id === activeChannel.value?.id;
}

export function installKeys() {
  setShortcutMatcher((e) => {
    const b = findBinding(e);
    return !!b && overlayAllows(b);
  });
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
          if (pending.kind === "session") A.closeSession(pending.id, true);
          else A.closeTab(pending.id, true);
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

/** Holding Alt alone for a moment shows the Alt+1..9 tab numbers. */
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

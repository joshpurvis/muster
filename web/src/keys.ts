// One command table drives keyboard shortcuts, the command palette and the shortcuts sheet.
// App shortcuts are matched in the capture phase, before xterm or inputs see them.

import * as A from "./actions.ts";
import { activeChannel, activeChannelId, confirmClose, overlay, prefs, view } from "./state.ts";
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
  /** Only match the key when this returns true. */
  when?: (ctx: KeyCtx) => boolean;
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

const inChannel = () => view.value === "channel" && activeChannelId.value !== null;

export const commands: Command[] = [
  // Navigation
  {
    id: "switcher",
    title: "Quick switcher",
    group: "Navigate",
    keys: ["Ctrl+Shift+K", "Ctrl+K"],
    run: () => (overlay.value = { kind: "switcher", initial: "" }),
  },
  {
    id: "palette",
    title: "Command palette",
    group: "Navigate",
    keys: ["Ctrl+Shift+P"],
    run: () => (overlay.value = { kind: "switcher", initial: ">" }),
  },
  { id: "inbox", title: "Toggle inbox", group: "Navigate", keys: ["Alt+I"], run: () => A.openInbox() },
  {
    id: "prev-channel",
    title: "Previous channel",
    group: "Navigate",
    keys: ["Ctrl+Alt+Up"],
    run: () => A.stepChannel(-1),
  },
  {
    id: "next-channel",
    title: "Next channel",
    group: "Navigate",
    keys: ["Ctrl+Alt+Down"],
    run: () => A.stepChannel(1),
  },
  ...Array.from({ length: 9 }, (_, i): Command => ({
    id: `channel-${i + 1}`,
    title: `Jump to channel ${i + 1}`,
    group: "Navigate",
    keys: [`Alt+${i + 1}`],
    hidden: true,
    run: () => A.jumpChannel(i),
  })),
  {
    id: "composer",
    title: "Focus composer",
    group: "Navigate",
    keys: ["Alt+M"],
    run: () => A.focusComposer(),
  },

  // Channels & sessions
  {
    id: "new-channel",
    title: "New channel…",
    group: "Channel",
    keys: ["Alt+Shift+N"],
    run: () => A.newChannelDialog(activeChannel.value?.workspace_id),
  },
  {
    id: "add-workspace",
    title: "Add workspace…",
    group: "Channel",
    run: () => (overlay.value = { kind: "add_workspace" }),
  },
  {
    id: "new-session",
    title: "New session tab…",
    group: "Session",
    keys: ["Alt+N"],
    run: () => A.spawnPicker(),
    enabled: inChannel,
  },
  {
    id: "new-claude",
    title: "New Claude session",
    group: "Session",
    run: () => A.spawnHere("claude"),
    enabled: inChannel,
  },
  {
    id: "new-codex",
    title: "New Codex session",
    group: "Session",
    run: () => A.spawnHere("codex"),
    enabled: inChannel,
  },
  {
    id: "new-shell",
    title: "New shell",
    group: "Session",
    run: () => A.spawnHere("shell"),
    enabled: inChannel,
  },
  {
    id: "close-tab",
    title: "Close tab",
    group: "Session",
    keys: ["Alt+W"],
    run: () => A.closeFocused(),
    enabled: inChannel,
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
    enabled: inChannel,
  },
  {
    id: "split-down",
    title: "Split down (new shell)",
    group: "Panes",
    keys: ["Alt+Shift+D"],
    run: () => A.splitFocused("bottom"),
    enabled: inChannel,
  },
  { id: "zoom", title: "Zoom pane", group: "Panes", keys: ["Alt+Z"], run: () => A.zoomToggle() },
  { id: "equalize", title: "Equalize splits", group: "Panes", run: () => A.equalizeAll() },
  { id: "prev-tab", title: "Previous tab", group: "Panes", keys: ["Alt+["], run: () => A.tabStep(-1) },
  { id: "next-tab", title: "Next tab", group: "Panes", keys: ["Alt+]"], run: () => A.tabStep(1) },
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
  { id: "sidebar", title: "Toggle sidebar", group: "View", keys: ["Alt+B"], run: () => A.toggleSidebar() },
  {
    id: "timeline",
    title: "Toggle timeline",
    group: "View",
    keys: ["Alt+L"],
    run: () => A.toggleRight("timeline"),
  },
  { id: "diff", title: "Toggle diff", group: "View", keys: ["Alt+G"], run: () => A.toggleRight("diff") },
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
    id: "shortcuts",
    title: "Keyboard shortcuts",
    group: "Help",
    keys: ["Alt+/", "F1"],
    run: () => (overlay.value = overlay.value?.kind === "shortcuts" ? null : { kind: "shortcuts" }),
  },
];

// Ctrl+K only when focus isn't in a terminal (shells use it for kill-line).
const WHEN: Record<string, (ctx: KeyCtx, key: string) => boolean> = {
  switcher: (ctx, key) => key !== "Ctrl+K" || !ctx.inTerminal,
};

interface Binding {
  key: string;
  spec: KeySpec;
  cmd: Command;
}
const bindings: Binding[] = commands.flatMap((cmd) =>
  (cmd.keys ?? []).map((key) => ({ key, spec: parseKey(key), cmd }))
);

function ctxOf(e: KeyboardEvent): KeyCtx {
  const t = e.target as HTMLElement | null;
  const inTerminal = hasTerminalFocus() !== null || !!t?.closest?.(".term-el");
  const inInput = !inTerminal && !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" ||
    t.isContentEditable);
  return { inTerminal, inInput };
}

export function findBinding(e: KeyboardEvent): Binding | null {
  if (e.type !== "keydown") return null;
  const ctx = ctxOf(e);
  for (const b of bindings) {
    if (!matches(b.spec, e)) continue;
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
  return ["switcher", "palette", "shortcuts"].includes(b.cmd.id);
}

export function installKeys() {
  setShortcutMatcher((e) => {
    const b = findBinding(e);
    return !!b && overlayAllows(b);
  });
  addEventListener(
    "keydown",
    (e) => {
      // Inline "Kill? ⏎ / Esc" confirmation for closing a busy tab.
      const pendingClose = confirmClose.value !== null ? A.sessionInActiveChannel(confirmClose.value) : false;
      if (confirmClose.value !== null && !pendingClose) confirmClose.value = null;
      if (pendingClose && !overlay.value && !(e.target as HTMLElement)?.closest?.("input, textarea")) {
        if (e.key === "Enter") {
          e.preventDefault();
          e.stopPropagation();
          A.closeSession(confirmClose.value!, true);
          return;
        }
        if (e.key === "Escape") {
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
      if (o?.kind === "switcher" && (b.cmd.id === "switcher" || b.cmd.id === "palette")) {
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

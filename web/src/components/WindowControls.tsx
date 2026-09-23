// The window's controls, at the right end of the top-right group's strip: Split right / down,
// the window chip (2+ windows), Jump and the ⋯ menu, plus a "Reconnecting…" pill. While a tab is
// dragged, the focused strip shows "→ Window N / → New window" drop targets (MoveTargets).
import { useRef } from "preact/hooks";
import { focusedSession, overlay, prefs, setPref, windowId, windows, windowsSupported } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { Kbd } from "./common.tsx";
import { type MenuItem, openMenuAt } from "./Menu.tsx";
import { IconMore, IconSearch, IconSplitDown, IconSplitRight } from "../icons.tsx";
import { windowLabel } from "../model.ts";
import { cx } from "../util.ts";

const THEME_NAME = { system: "Match system", dark: "Dark", light: "Light" } as const;

function moreMenu(): MenuItem[] {
  const p = prefs.value;
  return [
    ...(windowId.value !== null
      ? [{ label: "Rename window…", run: () => (overlay.value = { kind: "rename_window" }) }, "sep" as const]
      : []),
    {
      label: "Theme",
      hint: THEME_NAME[p.theme],
      submenu: (["system", "dark", "light"] as const).map((t): MenuItem => ({
        label: THEME_NAME[t],
        checked: p.theme === t,
        run: () => setPref("theme", t),
      })),
    },
    { label: "Status line", checked: p.statusLine, run: () => A.toggleStatusLine() },
    {
      label: "Terminal gets Alt keys",
      checked: p.altToTerminal,
      run: () => setPref("altToTerminal", !p.altToTerminal),
    },
    ...(windowsSupported.value
      ? [{
        label: "Jump focuses the other window",
        checked: p.jumpRaises,
        run: () => setPref("jumpRaises", !p.jumpRaises),
      }]
      : []),
    "sep",
    { label: "Command palette", keys: "Ctrl+Shift+P", run: () => A.openJump(">") },
    { label: "Keyboard shortcuts", keys: "Alt+/", run: () => (overlay.value = { kind: "shortcuts" }) },
  ];
}

/** "Move to window ▸" items for tab `sid`: the other windows, then a new one. */
export function moveMenu(sid: number): MenuItem[] {
  const others = windows.value.filter((w) => w.id !== windowId.value);
  return [
    ...others.map((w): MenuItem => ({
      label: windowLabel(w),
      hint: w.open ? undefined : "closed",
      run: () => A.moveTabToWindow(sid, w.id),
    })),
    ...(others.length ? ["sep" as const] : []),
    { label: "New window", run: () => A.moveTabToWindow(sid, "new") },
  ];
}

export function WindowControls({ empty = false }: { empty?: boolean }) {
  const menuRef = useRef<HTMLButtonElement>(null);
  const status = connStatus.value;
  const me = windows.value.find((w) => w.id === windowId.value);
  const showWin = windowsSupported.value && !!me && windows.value.length > 1;
  const canSplit = !empty && !!focusedSession.value;
  return (
    <div class="win-controls">
      {status === "reconnecting" && (
        <span class="reconnect-pill" role="status">
          <span class="conn-dot" /> Reconnecting…
        </span>
      )}
      {canSplit && (
        <div class="win-split" role="group" aria-label="Split the focused group">
          <button
            type="button"
            class="strip-btn win-split-btn"
            data-tip="Split right"
            data-keys="Alt+D"
            aria-label="Split right"
            onPointerDown={(e) => e.stopPropagation()}
            onClick={() => A.splitFocused("right")}
          >
            <IconSplitRight size={14} />
          </button>
          <button
            type="button"
            class="strip-btn win-split-btn"
            data-tip="Split down"
            data-keys="Alt+Shift+D"
            aria-label="Split down"
            onPointerDown={(e) => e.stopPropagation()}
            onClick={() => A.splitFocused("bottom")}
          >
            <IconSplitDown size={14} />
          </button>
        </div>
      )}
      {showWin && me && (
        <button
          type="button"
          class="win-chip"
          data-tip="This window · click to rename"
          onClick={() => (overlay.value = { kind: "rename_window" })}
        >
          {windowLabel(me)}
        </button>
      )}
      <button
        type="button"
        class="jump-btn"
        data-tip="Jump to a tab or folder, in any window"
        onClick={() => A.openJump()}
      >
        <IconSearch size={13} />
        <span class="jump-label">Jump</span>
        <Kbd k="Ctrl+Space" />
      </button>
      <button
        ref={menuRef}
        type="button"
        class="strip-btn win-more"
        data-tip="Settings and more"
        aria-label="Settings and more"
        onClick={() => openMenuAt(menuRef.current!, moreMenu(), { alignRight: true, minWidth: 240 })}
      >
        <IconMore size={16} />
      </button>
    </div>
  );
}

/** Drop targets shown in the focused strip while a tab is dragged. */
export function MoveTargets({ over }: { over: number | "new" | null }) {
  if (!windowsSupported.value || windowId.value === null) return null;
  const others = windows.value.filter((w) => w.id !== windowId.value);
  return (
    <div class="move-targets">
      {others.map((w) => (
        <span key={w.id} class={cx("move-target", over === w.id && "over")} data-move-to={w.id}>
          → {windowLabel(w)}
        </span>
      ))}
      <span class={cx("move-target", over === "new" && "over")} data-move-to="new">
        → New window
      </span>
    </div>
  );
}

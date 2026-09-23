// Top tab bar: one tab per channel in this window. Drag reorders (window.reorder_tabs); drag
// onto a "→ Window N" target or the Jump button moves a tab to another window; drag onto the
// active tab's panes merges it there as a split (or pane-tabs).
import { signal } from "@preact/signals";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { Channel } from "../types.ts";
import {
  activeChannel,
  altHeld,
  confirmClose,
  overlay,
  prefs,
  renaming,
  sessionsByChannel,
  setPref,
  tabs,
  unread,
  windowId,
  windows,
  windowsSupported,
  workspaceById,
} from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { InlineEdit, Kbd, StateDot } from "./common.tsx";
import { type MenuItem, openMenu, openMenuAt } from "./Menu.tsx";
import { PermBadge } from "./Permissions.tsx";
import {
  IconBranch,
  IconChevronDown,
  IconCodex,
  IconMore,
  IconPlus,
  IconSearch,
  IconShield,
  IconSpark,
  IconTerminal,
  IconX,
} from "../icons.tsx";
import { aggregateState, isNoopSlot, tabLabel, windowLabel } from "../model.ts";
import { cx } from "../util.ts";
import { computeTarget, DropMarker, type Target } from "../layout/dnd.tsx";

// ---------------- drag ----------------

type TabDropTarget = { kind: "window"; id: number } | { kind: "new" } | { kind: "jump" };

interface TabDrag {
  id: number;
  label: string;
  x: number;
  y: number;
  /** Insertion slot in the tab strip, when over it. */
  slot: number | null;
  line: { left: number; top: number; height: number } | null;
  target: TabDropTarget | null;
  /** Over the active tab's panes: where the dragged tab's sessions would land. */
  pane: Target | null;
}

export const tabDrag = signal<TabDrag | null>(null);

const THRESHOLD = 5;

function startTabDrag(e: PointerEvent, c: Channel, label: string, onClick: () => void) {
  if (e.button !== 0) return;
  const sx = e.clientX;
  const sy = e.clientY;
  let started = false;
  const move = (ev: PointerEvent) => {
    if (!started) {
      if (Math.hypot(ev.clientX - sx, ev.clientY - sy) < THRESHOLD) return;
      started = true;
      document.body.classList.add("dragging");
    }
    ev.preventDefault();
    tabDrag.value = {
      id: c.id,
      label,
      x: ev.clientX,
      y: ev.clientY,
      ...dropAt(ev.clientX, ev.clientY, c.id),
    };
  };
  const cleanup = () => {
    removeEventListener("pointermove", move, true);
    removeEventListener("pointerup", up, true);
    removeEventListener("pointercancel", cancel, true);
    removeEventListener("keydown", key, true);
    removeEventListener("blur", cancel);
    document.body.classList.remove("dragging");
  };
  const up = () => {
    cleanup();
    const d = tabDrag.value;
    tabDrag.value = null;
    if (!started) {
      onClick();
      return;
    }
    if (!d) return;
    const into = activeChannel.value?.id;
    if (d.pane && into !== undefined && into !== c.id) {
      A.mergeTab(c.id, into, d.pane.paneId, d.pane.zone, d.pane.index);
    } else if (d.target?.kind === "window") A.moveTabToWindow(c.id, d.target.id);
    else if (d.target?.kind === "new") A.moveTabToWindow(c.id, "new");
    else if (d.target?.kind === "jump") {
      const btn = document.querySelector(".jump-btn");
      if (btn) openMenuAt(btn, moveMenu(c), { minWidth: 200 });
    } else if (d.slot !== null) A.dropTabAt(c.id, d.slot);
  };
  const cancel = () => {
    cleanup();
    tabDrag.value = null;
  };
  const key = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      cancel();
    }
  };
  addEventListener("pointermove", move, true);
  addEventListener("pointerup", up, true);
  addEventListener("pointercancel", cancel, true);
  addEventListener("keydown", key, true);
  addEventListener("blur", cancel);
}

function dropAt(x: number, y: number, id: number): Pick<TabDrag, "slot" | "line" | "target" | "pane"> {
  const none = { slot: null, line: null, target: null, pane: null };
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  if (!el) return none;
  // Off the bar, over the active tab's panes (not the dragged tab's own): merge there.
  const into = activeChannel.value;
  if (!el.closest(".tabbar") && el.closest(".panes") && into && into.id !== id) {
    if (!sessionsByChannel.value.get(id)?.length) return none;
    return { ...none, pane: computeTarget(x, y, into.id, null) };
  }
  const mv = el.closest("[data-move-to]") as HTMLElement | null;
  if (mv) {
    const v = mv.dataset.moveTo!;
    return { ...none, target: v === "new" ? { kind: "new" } : { kind: "window", id: Number(v) } };
  }
  if (el.closest(".jump-btn") && windowsSupported.value) return { ...none, target: { kind: "jump" } };
  const bar = el.closest(".tabbar");
  if (!bar) return none;
  const strip = bar.querySelector<HTMLElement>(".tb-tabs");
  if (!strip) return none;
  const els = [...strip.querySelectorAll<HTMLElement>("[data-tab-id]")];
  let slot = els.length;
  for (let i = 0; i < els.length; i++) {
    const r = els[i].getBoundingClientRect();
    if (x < r.left + r.width / 2) {
      slot = i;
      break;
    }
  }
  const ids = els.map((t) => Number(t.dataset.tabId));
  if (isNoopSlot(ids, id, slot)) return none;
  const sr = strip.getBoundingClientRect();
  let lx: number;
  if (!els.length) lx = sr.left + 4;
  else if (slot < els.length) lx = els[slot].getBoundingClientRect().left;
  else lx = els[els.length - 1].getBoundingClientRect().right;
  lx = Math.max(sr.left + 1, Math.min(sr.right - 1, lx));
  return { slot, line: { left: lx - 1, top: sr.top + 5, height: sr.height - 10 }, target: null, pane: null };
}

export function TabDragOverlay() {
  const d = tabDrag.value;
  if (!d) return null;
  return (
    <>
      <DropMarker t={d.pane} />
      {d.line && (
        <div
          class="drop-line"
          style={{ left: d.line.left + "px", top: d.line.top + "px", height: d.line.height + "px" }}
        />
      )}
      <div
        class="drag-ghost"
        style={{ left: Math.min(d.x + 12, innerWidth - 200) + "px", top: d.y + 12 + "px" }}
      >
        <span>{d.label}</span>
      </div>
    </>
  );
}

// ---------------- menus ----------------

function moveMenu(c: Channel): MenuItem[] {
  const others = windows.value.filter((w) => w.id !== windowId.value);
  return [
    { header: "Move tab to" },
    ...others.map((w): MenuItem => ({
      label: windowLabel(w),
      hint: w.open ? undefined : "closed",
      run: () => A.moveTabToWindow(c.id, w.id),
    })),
    ...(others.length ? ["sep" as const] : []),
    { label: "New window", icon: <IconPlus size={14} />, run: () => A.moveTabToWindow(c.id, "new") },
  ];
}

function tabMenu(c: Channel): MenuItem[] {
  return [
    { label: "Rename", run: () => (renaming.value = { kind: "channel", id: c.id }) },
    { label: "Permissions…", icon: <IconShield size={14} />, run: () => A.openPermissions(c.id) },
    "sep",
    { label: "Copy path", run: () => A.copy(c.cwd, "Path copied") },
    ...(c.branch
      ? [{
        label: "Copy branch",
        icon: <IconBranch size={14} />,
        run: () => A.copy(c.branch!, "Branch copied"),
      }]
      : []),
    ...(windowsSupported.value && windowId.value !== null
      ? ["sep" as const, { label: "Move to window", submenu: moveMenu(c) }]
      : []),
    "sep",
    { label: "Close tab", danger: true, run: () => A.closeTab(c.id) },
    ...(c.owns_worktree
      ? [{ label: "Close tab + remove worktree…", danger: true, run: () => A.closeTabRemoveWorktree(c) }]
      : []),
  ];
}

function newMenu(): MenuItem[] {
  return [
    { label: "New tab…", keys: "Alt+Shift+N", run: () => A.newTabDialog() },
    {
      label: "New worktree tab",
      icon: <IconBranch size={14} />,
      keys: "Ctrl+Shift+N",
      run: () => A.newTabHere({ worktree: true }),
    },
    "sep",
    { header: "New tab here with" },
    {
      label: "Claude",
      icon: <IconSpark size={14} />,
      accel: "1",
      run: () => A.newTabHere({ agent: "claude" }),
    },
    {
      label: "Codex",
      icon: <IconCodex size={14} />,
      accel: "2",
      run: () => A.newTabHere({ agent: "codex" }),
    },
    {
      label: "Shell",
      icon: <IconTerminal size={14} />,
      accel: "3",
      run: () => A.newTabHere({ agent: "shell" }),
    },
  ];
}

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

// ---------------- components ----------------

export function TabBar() {
  const list = tabs.value;
  const active = activeChannel.value;
  const tabsRef = useRef<HTMLDivElement>(null);
  const moreRef = useRef<HTMLButtonElement>(null);
  const plusMoreRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLButtonElement>(null);
  const [overflow, setOverflow] = useState(false);
  const d = tabDrag.value;
  const status = connStatus.value;

  const measure = () => {
    const el = tabsRef.current;
    if (el) setOverflow(el.scrollWidth > el.clientWidth + 1 && el.childElementCount > 1);
  };
  useLayoutEffect(measure);
  useEffect(() => {
    const el = tabsRef.current;
    if (!el) return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Keep the active tab in view.
  useLayoutEffect(() => {
    const el = tabsRef.current;
    const t = el?.querySelector<HTMLElement>(".ttab.active");
    if (!el || !t) return;
    const r = t.getBoundingClientRect();
    const box = el.getBoundingClientRect();
    if (r.left < box.left) el.scrollLeft -= box.left - r.left + 12;
    else if (r.right > box.right) el.scrollLeft += r.right - box.right + 12;
  }, [active?.id, list.length, overflow]);

  const onWheel = (e: WheelEvent) => {
    const el = tabsRef.current;
    if (!el || el.scrollWidth <= el.clientWidth) return;
    if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
      e.preventDefault();
      el.scrollLeft += e.deltaY;
    }
  };

  const openAll = () => {
    openMenuAt(
      moreRef.current!,
      [
        { header: `${list.length} tabs` },
        ...list.map((c, i): MenuItem => ({
          label: tabLabel(c, workspaceById.value.get(c.workspace_id)).text,
          icon: <StateDot state={aggregateState(sessionsByChannel.value.get(c.id))} />,
          keys: i < 9 ? `Alt+${i + 1}` : undefined,
          checked: c.id === active?.id,
          run: () => A.selectTab(c.id),
        })),
      ],
      { alignRight: false, minWidth: 240 },
    );
  };

  const otherWindows = windowsSupported.value ? windows.value.filter((w) => w.id !== windowId.value) : [];
  const showWin = windowsSupported.value && windowId.value !== null && windows.value.length > 1;
  const me = windows.value.find((w) => w.id === windowId.value);

  return (
    <header class={cx("tabbar", d && "tab-dragging")}>
      <div
        class="tb-tabs"
        role="tablist"
        aria-label="Tabs"
        ref={tabsRef}
        onWheel={onWheel}
        onKeyDown={(e) => {
          // Roving focus: ←/→ (Home/End) select tabs, Enter goes to the terminal.
          if (e.altKey || e.ctrlKey || e.metaKey || !(e.target as HTMLElement).matches?.(".ttab")) return;
          const i = list.findIndex((c) => c.id === active?.id);
          let next = -1;
          if (e.key === "ArrowRight") next = (i + 1) % list.length;
          else if (e.key === "ArrowLeft") next = (i - 1 + list.length) % list.length;
          else if (e.key === "Home") next = 0;
          else if (e.key === "End") next = list.length - 1;
          else if (e.key === "Enter") {
            e.preventDefault();
            A.focusActiveTerminalSoon();
            return;
          } else if (e.key === "F2" && active) {
            e.preventDefault();
            renaming.value = { kind: "channel", id: active.id };
            return;
          }
          if (next < 0 || !list[next]) return;
          e.preventDefault();
          A.selectTab(list[next].id, false);
          requestAnimationFrame(() =>
            tabsRef.current?.querySelector<HTMLElement>(`[data-tab-id="${list[next].id}"]`)?.focus()
          );
        }}
      >
        {list.map((c, i) => <TopTab key={c.id} c={c} index={i} active={c.id === active?.id} />)}
      </div>
      {overflow && (
        <button
          ref={moreRef}
          type="button"
          class="tb-btn tb-more"
          data-tip="All tabs"
          aria-label="All tabs"
          onClick={openAll}
        >
          <span class="tab-more-count">{list.length}</span>
          <IconChevronDown size={12} />
        </button>
      )}
      <div class="tb-new">
        <button
          type="button"
          class="tb-btn"
          data-tip="New tab (same folder)"
          data-keys="Alt+T"
          aria-label="New tab"
          onClick={() => A.newTabHere()}
        >
          <IconPlus size={15} />
        </button>
        <button
          ref={plusMoreRef}
          type="button"
          class="tb-btn tb-btn-narrow"
          data-tip="More ways to open a tab"
          aria-label="New tab options"
          onClick={() => openMenuAt(plusMoreRef.current!, newMenu(), { alignRight: false, minWidth: 220 })}
        >
          <IconChevronDown size={12} />
        </button>
      </div>
      {d && windowsSupported.value && (
        <div class="tb-move-targets">
          {otherWindows.map((w) => (
            <span
              key={w.id}
              class={cx("tb-move", d.target?.kind === "window" && d.target.id === w.id && "over")}
              data-move-to={w.id}
            >
              → {windowLabel(w)}
            </span>
          ))}
          <span class={cx("tb-move", d.target?.kind === "new" && "over")} data-move-to="new">
            → New window
          </span>
        </div>
      )}
      <div
        class="tb-space"
        onDblClick={() => A.newTabHere()}
      />
      {status === "reconnecting" && (
        <span class="reconnect-pill" role="status">
          <span class="conn-dot" /> Reconnecting…
        </span>
      )}
      {showWin && me && (
        <button
          type="button"
          class="tb-win"
          data-tip="This window · click to rename"
          onClick={() => (overlay.value = { kind: "rename_window" })}
        >
          {windowLabel(me)}
        </button>
      )}
      <button
        type="button"
        class={cx("jump-btn", d?.target?.kind === "jump" && "over")}
        data-tip={d ? "Drop to move to a window" : "Jump to a tab, session or folder"}
        onClick={() => A.openJump()}
      >
        <IconSearch size={13} />
        <span>Jump</span>
        <Kbd k="Ctrl+Space" />
      </button>
      <button
        ref={menuRef}
        type="button"
        class="tb-btn"
        data-tip="Settings and more"
        aria-label="Settings and more"
        onClick={() => openMenuAt(menuRef.current!, moreMenu(), { alignRight: true, minWidth: 240 })}
      >
        <IconMore size={16} />
      </button>
    </header>
  );
}

function TopTab({ c, index, active }: { c: Channel; index: number; active: boolean }) {
  const ws = workspaceById.value.get(c.workspace_id);
  const label = tabLabel(c, ws);
  const state = aggregateState(sessionsByChannel.value.get(c.id));
  const r = renaming.value;
  const isRenaming = r?.kind === "channel" && r.id === c.id;
  const cc = confirmClose.value;
  const dragging = tabDrag.value?.id === c.id;
  const isUnread = !active && unread.value.has(c.id);

  if (cc?.kind === "channel" && cc.id === c.id) {
    return (
      <div class="ttab confirm confirm-close" data-tab-id={c.id} role="tab">
        <span class="confirm-q">Close {label.text}?</span>
        <button type="button" class="confirm-yes" onClick={() => A.closeTab(c.id, true)}>
          Close <kbd>⏎</kbd>
        </button>
        <button type="button" class="confirm-no" onClick={() => (confirmClose.value = null)}>
          <kbd>Esc</kbd>
        </button>
      </div>
    );
  }

  const tip = [label.text, c.branch ? `⎇ ${c.branch}` : null, c.cwd].filter(Boolean).join(" · ");
  return (
    <div
      class={cx("ttab", active && "active", isUnread && "unread", dragging && "dragging")}
      role="tab"
      aria-selected={active}
      tabIndex={active ? 0 : -1}
      aria-label={label.text}
      data-tab-id={c.id}
      data-tip={tip}
      onPointerDown={(e) => {
        if (isRenaming || (e.target as HTMLElement).closest("button")) return;
        startTabDrag(e, c, label.text, () => A.selectTab(c.id));
      }}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          A.closeTab(c.id);
        }
      }}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
      onDblClick={(e) => {
        if (!(e.target as HTMLElement).closest("button")) renaming.value = { kind: "channel", id: c.id };
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        openMenu({ x: e.clientX, y: e.clientY, items: tabMenu(c) });
      }}
    >
      {altHeld.value && index < 9 ? <span class="ttab-num">{index + 1}</span> : <StateDot state={state} />}
      {isRenaming
        ? (
          <InlineEdit
            value={label.text}
            class="tab-edit"
            onCommit={(v) => {
              renaming.value = null;
              A.renameChannel(c.id, v);
            }}
            onCancel={() => (renaming.value = null)}
          />
        )
        : <span class={cx("ttab-name", label.muted && "placeholder")}>{label.text}</span>}
      {c.owns_worktree && (
        <span class="ttab-wt" aria-label="Worktree">
          <IconBranch size={12} />
        </span>
      )}
      <PermBadge c={c} />
      <button
        type="button"
        class="tab-close"
        aria-label={`Close ${label.text}`}
        data-tip="Close tab"
        onClick={(e) => {
          e.stopPropagation();
          A.closeTab(c.id);
        }}
      >
        <IconX size={12} />
      </button>
    </div>
  );
}

// One group: its tab strip (always shown; a tab is one session) + the active tab's terminal
// (+ approval banner, exited bar, find bar). The top-right group's strip also carries the
// window controls.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { panelId, panelTab, type PaneNode } from "./tree.ts";
import { PanelSlot, reloadPanel } from "../panels/PanelView.tsx";
import {
  accountProfiles,
  altHeld,
  channelById,
  confirmClose,
  meta,
  renaming,
  sessions,
  panels,
  sessionsByChannel,
  unread,
  windowId,
  windowsSupported,
  workspaceById,
} from "../state.ts";
import type { Channel, Panel, SessionInfo, SessionKind } from "../types.ts";
import { profileName } from "../account_profiles.ts";
import { STATE_LABEL } from "../types.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import * as terms from "../terms.ts";
import { drag, startTabDrag } from "./dnd.tsx";
import { InlineEdit, StateDot } from "../components/common.tsx";
import { type MenuItem, openMenu, openMenuAt } from "../components/Menu.tsx";
import { PermBadge } from "../components/Permissions.tsx";
import { moveMenu, MoveTargets, WindowControls } from "../components/WindowControls.tsx";
import {
  IconBranch,
  IconCheck,
  IconChevronDown,
  IconCodex,
  IconMaximize,
  IconMinimize,
  IconPlus,
  IconPanel,
  IconRestart,
  IconShield,
  IconSpark,
  IconSplitDown,
  IconSplitRight,
  IconTerminal,
  IconX,
  KindIcon,
} from "../icons.tsx";
import { cx, tildify } from "../util.ts";
import { renameInputs, type TabLabel, tabLabel, tabText } from "../model.ts";

export function spawnMenuItems(onPick: (k: SessionKind) => void): MenuItem[] {
  return [
    { label: "Claude", icon: <IconSpark size={14} />, accel: "1", run: () => onPick("claude") },
    { label: "Codex", icon: <IconCodex size={14} />, accel: "2", run: () => onPick("codex") },
    { label: "Shell", icon: <IconTerminal size={14} />, accel: "3", run: () => onPick("shell") },
  ];
}

/** The label of tab `s` (its task's name, plus its own when the task has several sessions). */
export function labelOf(s: SessionInfo): TabLabel {
  const c = channelById.value.get(s.channel_id);
  const ws = c ? workspaceById.value.get(c.workspace_id) : undefined;
  return tabLabel(s, c, ws, (sessionsByChannel.value.get(s.channel_id) ?? []).length);
}

/** "Split right ▸" / "Split down ▸": move this tab into a new group, or a new terminal there. */
function splitMenu(
  s: SessionInfo,
  side: "right" | "bottom",
  paneId: string,
  tabsInGroup: number,
): MenuItem[] {
  return [
    {
      label: "Move this tab",
      disabled: tabsInGroup < 2,
      run: () => A.splitWithTab(s.id, side),
    },
    "sep",
    { header: "New terminal in this task" },
    ...spawnMenuItems((k) => A.spawn(s.channel_id, k, { type: "split", side, group: paneId })),
  ];
}

function tabMenu(s: SessionInfo, c: Channel | undefined, paneId: string, tabsInGroup: number): MenuItem[] {
  const last = (sessionsByChannel.value.get(s.channel_id) ?? []).every((x) => x.id === s.id);
  const items: MenuItem[] = [
    { label: "Rename", keys: "F2", run: () => A.startRename(s.id) },
  ];
  if (c) {
    items.push(
      { label: "Permissions…", icon: <IconShield size={14} />, run: () => A.openPermissions(c.id) },
      "sep",
      { label: "Copy path", run: () => A.copy(s.live_cwd || s.cwd || c.cwd, "Path copied") },
    );
    if (c.branch) {
      items.push({
        label: "Copy branch",
        icon: <IconBranch size={14} />,
        run: () => A.copy(c.branch!, "Branch copied"),
      });
    }
  }
  items.push(
    "sep",
    {
      label: "New terminal in this task",
      icon: <IconPlus size={14} />,
      keys: "Alt+N",
      submenu: spawnMenuItems((k) => A.spawn(s.channel_id, k, { type: "tab", group: paneId })),
    },
    {
      label: "Split right",
      icon: <IconSplitRight size={14} />,
      submenu: splitMenu(s, "right", paneId, tabsInGroup),
    },
    {
      label: "Split down",
      icon: <IconSplitDown size={14} />,
      submenu: splitMenu(s, "bottom", paneId, tabsInGroup),
    },
  );
  items.push({ label: "Fullscreen", icon: <IconMaximize size={14} />, keys: "Alt+Shift+Z", run: () => A.toggleFullscreen(s.id) });
  if (windowsSupported.value && windowId.value !== null) {
    items.push({ label: "Move to window", submenu: moveMenu(s.id) });
  }
  items.push(
    "sep",
    {
      label: "Restart",
      icon: <IconRestart size={14} />,
      run: () => A.restartOrInterruptAsk(s.id, "restart"),
    },
    {
      label: "Interrupt",
      disabled: s.state === "exited",
      run: () => A.restartOrInterruptAsk(s.id, "interrupt"),
    },
    { label: "Review queued prompts", disabled: s.queued === 0, run: () => A.reviewQueue(s.id) },
    "sep",
    { label: "Close tab", keys: "Alt+W", danger: true, run: () => A.closeTab(s.id) },
  );
  if (c && last && c.owns_worktree) {
    items.push({ label: "Close + remove worktree…", danger: true, run: () => A.closeTaskRemoveWorktree(c) });
  }
  return items;
}

/** Right-click menu of a group's terminal (also over programs that track the mouse). */
function bodyMenu(
  paneId: string,
  s: SessionInfo,
  multi: boolean,
  zoomed: boolean,
  tabsInGroup: number,
  full: boolean,
): MenuItem[] {
  const term = terms.terminal(s.id);
  const sel = term?.getSelection() ?? "";
  const items: MenuItem[] = [
    { label: "Copy", keys: "Ctrl+Shift+C", disabled: !sel, run: () => A.copy(sel) },
    { label: "Paste", keys: "Ctrl+Shift+V", run: () => A.pasteClipboard(s.id) },
    { label: "Find…", keys: "Ctrl+Shift+F", run: () => (A.findOpen.value = { sid: s.id }) },
    "sep",
    {
      label: "New terminal in this task",
      icon: <IconPlus size={14} />,
      keys: "Alt+N",
      submenu: spawnMenuItems((k) => A.spawn(s.channel_id, k, { type: "tab", group: paneId })),
    },
    {
      label: "Split right",
      icon: <IconSplitRight size={14} />,
      submenu: splitMenu(s, "right", paneId, tabsInGroup),
    },
    {
      label: "Split down",
      icon: <IconSplitDown size={14} />,
      submenu: splitMenu(s, "bottom", paneId, tabsInGroup),
    },
  ];
  if ((multi || zoomed) && !full) {
    items.push({
      label: zoomed ? "Restore layout" : "Zoom group",
      icon: zoomed ? <IconMinimize size={14} /> : <IconMaximize size={14} />,
      keys: "Alt+Z",
      run: () => {
        A.focusGroup(paneId);
        A.zoomToggle();
      },
    });
  }
  items.push({
    label: full ? "Exit fullscreen" : "Fullscreen",
    icon: full ? <IconMinimize size={14} /> : <IconMaximize size={14} />,
    keys: "Alt+Shift+Z",
    run: () => A.toggleFullscreen(s.id),
  });
  items.push(
    "sep",
    { label: "Rename", run: () => A.startRename(s.id, "terminal") },
    {
      label: "Restart",
      icon: <IconRestart size={14} />,
      run: () => A.restartOrInterruptAsk(s.id, "restart"),
    },
    {
      label: "Interrupt",
      disabled: s.state === "exited",
      run: () => A.restartOrInterruptAsk(s.id, "interrupt"),
    },
    { label: "Review queued prompts", disabled: s.queued === 0, run: () => A.reviewQueue(s.id) },
    { label: "Kill process", danger: true, disabled: s.state === "exited", run: () => A.killSession(s.id) },
    { label: "Close tab", keys: "Alt+W", danger: true, run: () => A.closeTab(s.id) },
  );
  return items;
}

export function Group(
  { pane, focused, zoomed, fullscreen, multi, controls }: {
    pane: PaneNode;
    focused: boolean;
    zoomed: boolean;
    /** This group's visible tab fills the window (no strip). */
    fullscreen: boolean;
    /** The window has 2+ groups. */
    multi: boolean;
    /** This strip carries the window controls (the top-right group). */
    controls: boolean;
  },
) {
  const all = sessions.value;
  const tabs = pane.tabs.filter((id) => all.has(id) || panels.value.has(panelId(id) ?? -1));
  const active = pane.active !== null ? all.get(pane.active) ?? null : null;
  const activePanel = pane.active !== null ? panels.value.get(panelId(pane.active) ?? -1) : undefined;
  const find = A.findOpen.value;
  const dragging = drag.value;

  const onBodyKey = (e: KeyboardEvent) => {
    if (!active || active.state !== "exited") return;
    if (!(e.target as HTMLElement).closest?.(".term-host") || e.isComposing) return;
    if (e.key === "Enter" && !e.altKey && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      e.stopPropagation();
      A.restartSession(active.id);
    }
  };

  const onContext = (e: MouseEvent) => {
    if (!active || (e.target as HTMLElement).closest?.(".approval, .exited-bar, .input-paused-bar, .findbar, input")) return;
    // Never the browser's own menu over a terminal. The right button is Muster's even over a
    // program that tracks the mouse; terms.ts keeps its reports from the program.
    e.preventDefault();
    A.focusGroup(pane.id);
    openMenu({ x: e.clientX, y: e.clientY, items: bodyMenu(pane.id, active, multi, zoomed, tabs.length, fullscreen) });
  };

  return (
    <div
      class={cx("pane", focused && "focused", zoomed && "zoomed", fullscreen && "fullscreen", dragging && "drop-target")}
      data-pane={pane.id}
      onPointerDownCapture={() => A.focusGroup(pane.id)}
    >
      {!fullscreen && <TabStrip pane={pane} tabs={tabs} focused={focused} zoomed={zoomed} controls={controls} />}
      {fullscreen && active && (
        // Terminals keep Esc (agents and editors use it), so a terminal shown fullscreen gets
        // this way back; a panel has the same button in the strip above it.
        <button
          type="button"
          class="fs-exit"
          data-tip="Exit fullscreen"
          data-keys="Alt+Shift+Z"
          aria-label="Exit fullscreen"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => {
            A.exitFullscreen();
            A.focusActiveTerminalSoon();
          }}
        >
          <IconMinimize size={13} />
        </button>
      )}
      <div
        class={cx("pane-body", active?.state === "exited" && "exited")}
        data-pane-body={pane.id}
        onKeyDownCapture={onBodyKey}
        onContextMenu={onContext}
      >
        {active && (active.state === "needs_approval" || active.state === "asking_question") && (
          <ApprovalBanner s={active} />
        )}
        {active && find?.sid === active.id && <FindBar sid={active.id} />}
        {active && <TermHost sid={active.id} />}
        {activePanel && <PanelSlot key={activePanel.id} id={activePanel.id} />}
        {active && active.input_interrupted && active.state !== "exited" && <InputPausedBar s={active} />}
        {active && active.state === "exited" && <ExitedBar s={active} />}
        {active && connStatus.value !== "open" && (
          <div class="term-veil" role="status">
            <span>
              <span class="conn-dot" /> Disconnected · reconnecting. Typing is paused.
            </span>
          </div>
        )}
      </div>
    </div>
  );
}

function TermHost({ sid }: { sid: number }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const host = ref.current!;
    terms.mount(sid, host);
    return () => terms.unmount(sid, host);
  }, [sid]);
  return <div class="term-host" ref={ref} />;
}

function newTabMenu(paneId: string): MenuItem[] {
  return [
    { label: "New tab…", keys: "Alt+Shift+N", run: () => A.newTabDialog() },
    {
      label: "New worktree tab",
      icon: <IconBranch size={14} />,
      keys: "Ctrl+Shift+N",
      run: () => {
        A.focusGroup(paneId);
        A.newTabHere({ worktree: true, group: paneId });
      },
    },
    {
      label: "New terminal in this task",
      keys: "Alt+N",
      run: () => {
        A.focusGroup(paneId);
        A.spawnPicker();
      },
    },
    "sep",
    { header: "New tab here with" },
    ...spawnMenuItems((k) => A.newTabHere({ agent: k, group: paneId })),
  ];
}

function TabStrip(
  { pane, tabs, focused, zoomed, controls }: {
    pane: PaneNode;
    tabs: number[];
    focused: boolean;
    zoomed: boolean;
    controls: boolean;
  },
) {
  const tabsRef = useRef<HTMLDivElement>(null);
  const moreRef = useRef<HTMLButtonElement>(null);
  const plusMoreRef = useRef<HTMLButtonElement>(null);
  const [overflow, setOverflow] = useState(false);
  const d = drag.value;

  // Overflow detection: re-measure on every render (tabs added/renamed) and on resize.
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

  // Keep the active tab visible when it changes (keyboard cycling, reveal, new tab).
  useLayoutEffect(() => {
    const el = tabsRef.current;
    const t = el?.querySelector<HTMLElement>(".tab.active");
    if (!el || !t) return;
    const r = t.getBoundingClientRect();
    const box = el.getBoundingClientRect();
    if (r.left < box.left) el.scrollLeft -= box.left - r.left + 8;
    else if (r.right > box.right) el.scrollLeft += r.right - box.right + 8;
  }, [pane.active, tabs.length, overflow]);

  const onWheel = (e: WheelEvent) => {
    const el = tabsRef.current;
    if (!el || el.scrollWidth <= el.clientWidth) return;
    // Vertical wheels scroll the strip sideways.
    if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
      e.preventDefault();
      el.scrollLeft += e.deltaY;
    }
  };

  const openAll = () => {
    openMenuAt(
      moreRef.current!,
      [
        { header: `${tabs.length} tabs in this group` },
        ...tabs.map((id, i): MenuItem => ({
          label: panels.value.get(panelId(id) ?? -1)?.title ?? tabText(labelOf(sessions.value.get(id)!)),
          icon: panelId(id) !== null ? <IconPanel size={13} /> : <StateDot state={sessions.value.get(id)!.state} />,
          keys: focused && i < 9 ? `Alt+${i + 1}` : undefined,
          checked: id === pane.active,
          run: () => A.revealSession(id),
        })),
      ],
      { alignRight: false, minWidth: 240 },
    );
  };

  // Roving focus: ←/→ (Home/End) select tabs, Enter goes to the terminal, F2 renames.
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.altKey || e.ctrlKey || e.metaKey || !(e.target as HTMLElement).matches?.(".tab")) return;
    const i = tabs.indexOf(pane.active ?? 0);
    let next = -1;
    if (e.key === "ArrowRight") next = (i + 1) % tabs.length;
    else if (e.key === "ArrowLeft") next = (i - 1 + tabs.length) % tabs.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = tabs.length - 1;
    else if (e.key === "Enter") {
      e.preventDefault();
      A.focusActiveTerminalSoon();
      return;
    } else if (e.key === "F2" && pane.active !== null) {
      e.preventDefault();
      A.startRename(pane.active);
      return;
    }
    if (next < 0 || !tabs[next]) return;
    e.preventDefault();
    A.revealSession(tabs[next], false);
    requestAnimationFrame(() =>
      tabsRef.current?.querySelector<HTMLElement>(`[data-tab-sid="${tabs[next]}"]`)?.focus()
    );
  };

  return (
    <div
      class={cx("tabstrip", focused && "focused", overflow && "overflowing", controls && "has-controls")}
      data-tabstrip={pane.id}
    >
      <div
        class="tabs"
        role="tablist"
        aria-label="Tabs"
        ref={tabsRef}
        onWheel={onWheel}
        onKeyDown={onKeyDown}
      >
        {tabs.map((id, i) => {
          const panel = panels.value.get(panelId(id) ?? -1);
          if (panel) return <PanelTab key={id} panel={panel} index={i} paneId={pane.id} tabsInGroup={tabs.length} active={id === pane.active} focused={focused} />;
          const s = sessions.value.get(id)!;
          return (
          <GroupTab
            key={s.id}
            paneId={pane.id}
            s={s}
            index={i}
            tabsInGroup={tabs.length}
            active={s.id === pane.active}
            groupFocused={focused}
          />
          );
        })}
      </div>
      {overflow && (
        <button
          ref={moreRef}
          type="button"
          class="tab-more"
          data-tip="All tabs in this group"
          aria-label="All tabs in this group"
          onClick={openAll}
        >
          <span class="tab-more-count">{tabs.length}</span>
          <IconChevronDown size={12} />
        </button>
      )}
      <div class="tab-new">
        <button
          type="button"
          class="tab-add"
          data-tip="New tab (same folder)"
          data-keys="Alt+T"
          aria-label="New tab"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => {
            A.focusGroup(pane.id);
            A.newTabHere({ group: pane.id });
          }}
        >
          <IconPlus size={14} />
        </button>
        <button
          ref={plusMoreRef}
          type="button"
          class="tab-add tab-add-more"
          data-tip="More ways to open a tab"
          aria-label="New tab options"
          onClick={() =>
            openMenuAt(plusMoreRef.current!, newTabMenu(pane.id), { alignRight: false, minWidth: 230 })}
        >
          <IconChevronDown size={12} />
        </button>
      </div>
      <div class="strip-spacer" />
      {d && focused && <MoveTargets over={d.win} />}
      {zoomed && (
        <button
          type="button"
          class="zoom-badge"
          data-tip="Restore layout"
          data-keys="Alt+Z"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => A.zoomToggle()}
        >
          zoomed
        </button>
      )}
      {controls && <WindowControls />}
    </div>
  );
}

function PanelTab({ panel, paneId, index, tabsInGroup, active, focused }: {
  panel: Panel; paneId: string; index: number; tabsInGroup: number; active: boolean; focused: boolean;
}) {
  const id = panelTab(panel.id);
  const menu: MenuItem[] = [
    { label: "Reload panel", icon: <IconRestart size={14} />, run: () => reloadPanel(panel.id) },
    { label: "Split right", disabled: tabsInGroup < 2, run: () => A.splitWithTab(id, "right") },
    { label: "Split down", disabled: tabsInGroup < 2, run: () => A.splitWithTab(id, "bottom") },
    { label: "Zoom group", keys: "Alt+Z", run: () => { A.focusGroup(paneId); A.zoomToggle(); } },
    { label: "Fullscreen", icon: <IconMaximize size={14} />, keys: "Alt+Shift+Z", run: () => A.toggleFullscreen(id) },
    ...(windowsSupported.value ? [{ label: "Move to window", submenu: moveMenu(id) }] : []),
    "sep",
    { label: "Close panel", keys: "Alt+W", run: () => A.closeTab(id) },
  ];
  return (
    <div class={cx("tab", "panel-tab", active && "active", active && focused && "focused", drag.value?.sid === id && "dragging")}
      role="tab" aria-selected={active} aria-label={panel.title} tabIndex={active ? 0 : -1}
      data-tab-sid={id} data-tip={`${panel.title} · Panel`}
      onPointerDown={(e) => {
        if (!(e.target as HTMLElement).closest("button")) startTabDrag(e, { id, kind: "panel" }, panel.title, () => A.revealSession(id));
      }}
      onAuxClick={(e) => { if (e.button === 1) { e.preventDefault(); A.closeTab(id); } }}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
      onContextMenu={(e) => { e.preventDefault(); openMenu({ x: e.clientX, y: e.clientY, items: menu }); }}>
      {altHeld.value && focused && index < 9 && <span class="tab-num">{index + 1}</span>}
      <IconPanel size={14} />
      <span class="tab-name"><span class="tab-task">{panel.title}</span></span>
      <button type="button" class="tab-close" aria-label={`Close ${panel.title}`} data-tip="Close panel"
        onClick={(e) => { e.stopPropagation(); A.closeTab(id); }}><IconX size={12} /></button>
    </div>
  );
}

function GroupTab(
  { paneId, s, index, tabsInGroup, active, groupFocused }: {
    paneId: string;
    s: SessionInfo;
    index: number;
    tabsInGroup: number;
    active: boolean;
    groupFocused: boolean;
  },
) {
  const c = channelById.value.get(s.channel_id);
  const label = labelOf(s);
  const r = renaming.value;
  const isRenaming = r?.sid === s.id;
  const dragging = drag.value?.sid === s.id;
  const isUnread = !(active && groupFocused) && !!c && unread.value.has(c.id);

  if (confirmClose.value === s.id) {
    return (
      <div class="tab confirm confirm-close" data-tab-sid={s.id} role="tab">
        <span class="confirm-q">Close {tabText(label)}?</span>
        <button type="button" class="confirm-yes" onClick={() => A.closeTab(s.id, true)}>
          Close <kbd>⏎</kbd>
        </button>
        <button type="button" class="confirm-no" onClick={() => (confirmClose.value = null)}>
          <kbd>Esc</kbd>
        </button>
      </div>
    );
  }

  const home = meta.value.home;
  const tip = [
    tabText(label),
    s.kind === "claude" ? `Claude profile: ${profileName(accountProfiles.value, s.account_profile_id)}` : null,
    c ? tildify(c.cwd, home) : null,
    c?.branch ? `⎇ ${c.branch}` : null,
    s.state_detail || STATE_LABEL[s.state],
  ].filter(Boolean).join(" · ");
  const edit = isRenaming ? renameInputs(r!.fields) : null;
  return (
    <div
      class={cx(
        "tab",
        active && "active",
        active && groupFocused && "focused",
        isUnread && "unread",
        dragging && "dragging",
        edit && "renaming",
        edit && edit.length > 1 && "renaming-two",
        "st-" + s.state,
      )}
      role="tab"
      aria-selected={active}
      aria-label={tabText(label)}
      tabIndex={active ? 0 : -1}
      data-tab-sid={s.id}
      data-tip={tip}
      onPointerDown={(e) => {
        if (isRenaming || (e.target as HTMLElement).closest("button")) return;
        startTabDrag(e, s, tabText(label), () => A.revealSession(s.id));
      }}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          A.closeTab(s.id);
        }
      }}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
      onDblClick={(e) => {
        if (!(e.target as HTMLElement).closest("button")) A.startRename(s.id);
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        openMenu({ x: e.clientX, y: e.clientY, items: tabMenu(s, c, paneId, tabsInGroup) });
      }}
    >
      {altHeld.value && groupFocused && index < 9
        ? <span class="tab-num">{index + 1}</span>
        : <StateDot state={s.state} />}
      <KindIcon kind={s.kind} size={13} />
      {edit
        ? (
          <InlineEdit
            fields={edit.map((f) => ({
              value: f.value,
              // In the UI a session is a tab.
              label: f.key === "task" ? "Task name" : "Tab name",
              tag: f.key === "task" ? "Task" : "Tab",
            }))}
            focus={Math.max(0, edit.findIndex((f) => f.key === r!.fields.focus))}
            class="tab-edit"
            onCommit={(v, byKey) =>
              A.commitRename(r!, Object.fromEntries(edit.map((f, i) => [f.key, v[i]])), byKey)}
            onCancel={(byKey) => A.endRename(r!, byKey)}
          />
        )
        : (
          <span class="tab-name">
            <span class={cx("tab-task", label.muted && "placeholder")}>{label.text}</span>
            {label.sub && <span class="tab-sub">· {label.sub}</span>}
            {!!s.subagents && (
              <span class="tab-subagents" title={`${s.subagents} subagent${s.subagents === 1 ? "" : "s"} running`}>
                +{s.subagents}
              </span>
            )}
          </span>
        )}
      {c?.owns_worktree && (
        <span class="tab-wt" aria-label="Worktree">
          <IconBranch size={12} />
        </span>
      )}
      {c && <PermBadge c={c} />}
      {s.queued > 0 && <span class="tab-queued" data-tip={`${s.queued} queued`}>{s.queued}</span>}
      <button
        type="button"
        class="tab-close"
        aria-label={`Close ${tabText(label)}`}
        data-tip="Close tab"
        data-keys="Alt+W"
        onClick={(e) => {
          e.stopPropagation();
          A.closeTab(s.id);
        }}
      >
        <IconX size={12} />
      </button>
    </div>
  );
}

function ApprovalBanner({ s }: { s: SessionInfo }) {
  const asking = s.state === "asking_question";
  const toPrompt = () => {
    terms.scrollToBottom(s.id);
    terms.focusTerminal(s.id);
  };
  return (
    <div class={cx("approval", asking && "asking")} role="alert">
      <span class="approval-dot" />
      <div class="approval-text">
        <strong>{asking ? "Question" : "Needs approval"}</strong>
        <span class="approval-detail">
          {s.state_detail || (asking ? "The agent is asking something." : "Permission requested.")}
        </span>
      </div>
      <div class="approval-actions">
        {asking
          ? (
            <button type="button" class="btn btn-sm" onClick={toPrompt}>
              Answer in terminal
            </button>
          )
          : (
            <>
              <button
                type="button"
                class="btn btn-sm btn-approve"
                data-tip="Approve"
                data-keys="Alt+A"
                onClick={() => A.approve(s.id, "approve")}
              >
                <IconCheck size={13} /> Approve
              </button>
              <button
                type="button"
                class="btn btn-sm"
                data-tip="Approve and don't ask again for this"
                onClick={() => A.approve(s.id, "approve_always")}
              >
                Always
              </button>
              <button
                type="button"
                class="btn btn-sm btn-deny"
                data-tip="Deny"
                data-keys="Alt+Shift+A"
                onClick={() => A.approve(s.id, "deny")}
              >
                Deny
              </button>
              <button type="button" class="btn btn-sm btn-ghost" onClick={toPrompt}>
                Jump to prompt
              </button>
            </>
          )}
      </div>
    </div>
  );
}

/** Typing is paused after an input interruption. The button is never focused for the user:
 * a fast typist's next Enter or Space must not continue without them reading this. */
function InputPausedBar({ s }: { s: SessionInfo }) {
  return (
    <div class="input-paused-bar" role="alert">
      <span class="exited-dot" />
      <span>Typing is paused: some input may not have reached this terminal. Check the screen first.</span>
      <button type="button" class="link-btn" onClick={() => A.continueInput(s.id)}>
        Continue typing
      </button>
    </div>
  );
}

function ExitedBar({ s }: { s: SessionInfo }) {
  return (
    <div class="exited-bar">
      <span class="exited-dot" />
      <span>
        Exited{s.exit_code !== null ? ` (code ${s.exit_code})` : ""}
      </span>
      <span class="sep">·</span>
      <button type="button" class="link-btn" onClick={() => A.restartSession(s.id)}>
        Restart <kbd>⏎</kbd>
      </button>
      <span class="sep">·</span>
      <button type="button" class="link-btn" onClick={() => A.closeTab(s.id, true)}>
        Close
      </button>
    </div>
  );
}

/** Centered Claude / Codex / Shell picker (keys 1 2 3). */
export function LaunchButtons(
  { pick, last }: { pick: (k: SessionKind) => void; last: string },
) {
  const opts: { k: SessionKind; icon: preact.ComponentChildren; desc: string; name: string }[] = [
    { k: "claude", icon: <IconSpark size={22} />, desc: "Claude Code agent", name: "Claude" },
    { k: "codex", icon: <IconCodex size={22} />, desc: "Codex CLI agent", name: "Codex" },
    { k: "shell", icon: <IconTerminal size={22} />, desc: "Plain shell", name: "Shell" },
  ];
  return (
    <div class="launcher-opts">
      {opts.map((o, i) => (
        <button
          type="button"
          class={cx("launch-btn", o.k === last && "last")}
          onClick={() => pick(o.k)}
        >
          <span class="launch-icon">{o.icon}</span>
          <span class="launch-name">{o.name}</span>
          <span class="launch-desc">{o.desc}</span>
          <kbd class="launch-key">{i + 1}</kbd>
        </button>
      ))}
    </div>
  );
}

function FindBar({ sid }: { sid: number }) {
  const ref = useRef<HTMLInputElement>(null);
  const [q, setQ] = useState("");
  const [res, setRes] = useState<{ i: number; n: number } | null>(null);
  useLayoutEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  useEffect(() => terms.onSearchResults(sid, (i, n) => setRes({ i, n })), [sid]);
  const close = () => {
    terms.searchClear(sid);
    A.findOpen.value = null;
    terms.focusTerminal(sid);
  };
  return (
    <div class="findbar">
      <input
        ref={ref}
        value={q}
        placeholder="Find in terminal"
        spellcheck={false}
        onInput={(e) => {
          const v = (e.target as HTMLInputElement).value;
          setQ(v);
          if (v) terms.searchNext(sid, v);
          else {
            terms.searchClear(sid);
            setRes(null);
          }
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            if (q) terms.searchNext(sid, q, e.shiftKey);
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            close();
          }
        }}
      />
      <span class="find-count">
        {q ? (res ? (res.n ? `${res.i + 1}/${res.n}` : "No results") : "") : ""}
      </span>
      <button
        type="button"
        class="strip-btn"
        aria-label="Previous"
        onClick={() => q && terms.searchNext(sid, q, true)}
      >
        <IconChevronDown size={14} class="flip" />
      </button>
      <button type="button" class="strip-btn" aria-label="Next" onClick={() => q && terms.searchNext(sid, q)}>
        <IconChevronDown size={14} />
      </button>
      <button type="button" class="strip-btn" aria-label="Close find" onClick={close}>
        <IconX size={13} />
      </button>
    </div>
  );
}

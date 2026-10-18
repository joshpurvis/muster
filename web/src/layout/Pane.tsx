// One group: its tab strip (always shown; a tab is one session) + the active tab's terminal
// (+ approval banner, exited bar, find bar). The top-right group's strip also carries the
// window controls.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { PaneNode } from "./tree.ts";
import {
  altHeld,
  channelById,
  confirmClose,
  meta,
  renaming,
  sessions,
  sessionsByChannel,
  unread,
  windowId,
  windowsSupported,
  workspaceById,
} from "../state.ts";
import type { Channel, SessionInfo, SessionKind } from "../types.ts";
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
import { type TabLabel, tabLabel, tabText } from "../model.ts";

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
    { label: "Rename tab", keys: "F2", run: () => A.renameIn("session", s.id) },
  ];
  if (c) {
    items.push(
      { label: "Rename task", run: () => A.renameIn("channel", s.id) },
      { label: "Permissions…", icon: <IconShield size={14} />, run: () => A.openPermissions(c.id) },
      "sep",
      { label: "Copy path", run: () => A.copy(s.cwd || c.cwd, "Path copied") },
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
  if (windowsSupported.value && windowId.value !== null) {
    items.push({ label: "Move to window", submenu: moveMenu(s.id) });
  }
  items.push(
    "sep",
    { label: "Restart", icon: <IconRestart size={14} />, run: () => A.restartSession(s.id) },
    { label: "Interrupt", disabled: s.state === "exited", run: () => A.interruptSession(s.id) },
    { label: "Review queued prompts", disabled: s.queued === 0, run: () => A.reviewQueue(s.id) },
    "sep",
    { label: "Close tab", keys: "Alt+W", danger: true, run: () => A.closeTab(s.id) },
  );
  if (c && last && c.owns_worktree) {
    items.push({ label: "Close + remove worktree…", danger: true, run: () => A.closeTaskRemoveWorktree(c) });
  }
  return items;
}

/** Right-click menu of a group's terminal (Shift+right-click inside mouse-tracking programs). */
function bodyMenu(
  paneId: string,
  s: SessionInfo,
  multi: boolean,
  zoomed: boolean,
  tabsInGroup: number,
): MenuItem[] {
  const term = terms.terminal(s.id);
  const sel = term?.getSelection() ?? "";
  const items: MenuItem[] = [
    { label: "Copy", keys: "Ctrl+Shift+C", disabled: !sel, run: () => A.copy(sel) },
    {
      label: "Paste",
      keys: "Ctrl+Shift+V",
      disabled: !navigator.clipboard?.readText,
      run: () => {
        navigator.clipboard.readText().then((t) => t && term?.paste(t)).catch(() => {});
        terms.focusTerminal(s.id);
      },
    },
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
  if (multi || zoomed) {
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
  items.push(
    "sep",
    { label: "Rename tab", run: () => A.renameIn("session", s.id) },
    { label: "Restart", icon: <IconRestart size={14} />, run: () => A.restartSession(s.id) },
    { label: "Interrupt", disabled: s.state === "exited", run: () => A.interruptSession(s.id) },
    { label: "Review queued prompts", disabled: s.queued === 0, run: () => A.reviewQueue(s.id) },
    { label: "Kill process", danger: true, disabled: s.state === "exited", run: () => A.killSession(s.id) },
    { label: "Close tab", keys: "Alt+W", danger: true, run: () => A.closeTab(s.id) },
  );
  return items;
}

export function Group(
  { pane, focused, zoomed, multi, controls }: {
    pane: PaneNode;
    focused: boolean;
    zoomed: boolean;
    /** The window has 2+ groups. */
    multi: boolean;
    /** This strip carries the window controls (the top-right group). */
    controls: boolean;
  },
) {
  const all = sessions.value;
  const tabs = pane.tabs.map((id) => all.get(id)).filter(Boolean) as SessionInfo[];
  const active = pane.active !== null ? all.get(pane.active) ?? null : null;
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
    // Programs that track the mouse get the right button; Shift+right-click still opens ours.
    const t = terms.terminal(active.id);
    if (t && t.modes.mouseTrackingMode !== "none" && !e.shiftKey) return;
    e.preventDefault();
    A.focusGroup(pane.id);
    openMenu({ x: e.clientX, y: e.clientY, items: bodyMenu(pane.id, active, multi, zoomed, tabs.length) });
  };

  return (
    <div
      class={cx("pane", focused && "focused", zoomed && "zoomed", dragging && "drop-target")}
      data-pane={pane.id}
      onPointerDownCapture={() => A.focusGroup(pane.id)}
    >
      <TabStrip pane={pane} tabs={tabs} focused={focused} zoomed={zoomed} controls={controls} />
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
    tabs: SessionInfo[];
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
        ...tabs.map((s, i): MenuItem => ({
          label: tabText(labelOf(s)),
          icon: <StateDot state={s.state} />,
          keys: focused && i < 9 ? `Alt+${i + 1}` : undefined,
          checked: s.id === pane.active,
          run: () => A.revealSession(s.id),
        })),
      ],
      { alignRight: false, minWidth: 240 },
    );
  };

  // Roving focus: ←/→ (Home/End) select tabs, Enter goes to the terminal, F2 renames.
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.altKey || e.ctrlKey || e.metaKey || !(e.target as HTMLElement).matches?.(".tab")) return;
    const i = tabs.findIndex((s) => s.id === pane.active);
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
    A.revealSession(tabs[next].id, false);
    requestAnimationFrame(() =>
      tabsRef.current?.querySelector<HTMLElement>(`[data-tab-sid="${tabs[next].id}"]`)?.focus()
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
        {tabs.map((s, i) => (
          <GroupTab
            key={s.id}
            paneId={pane.id}
            s={s}
            index={i}
            tabsInGroup={tabs.length}
            active={s.id === pane.active}
            groupFocused={focused}
          />
        ))}
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
    c ? tildify(c.cwd, home) : null,
    c?.branch ? `⎇ ${c.branch}` : null,
    s.state_detail || STATE_LABEL[s.state],
  ].filter(Boolean).join(" · ");
  const editValue = r?.kind === "channel" ? label.text : s.name;
  return (
    <div
      class={cx(
        "tab",
        active && "active",
        active && groupFocused && "focused",
        isUnread && "unread",
        dragging && "dragging",
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
      {isRenaming
        ? (
          <InlineEdit
            value={editValue}
            class="tab-edit"
            onCommit={(v) => {
              renaming.value = null;
              if (r?.kind === "channel") A.renameChannel(r.id, v);
              else A.renameSession(s.id, v);
            }}
            onCancel={() => (renaming.value = null)}
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

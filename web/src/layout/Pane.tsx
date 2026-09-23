// One pane: optional chrome (a pane-tab strip when it holds 2+ sessions, else a thin header
// when the tab has 2+ panes, else nothing) + terminal host (+ approval banner, exited bar, find
// bar, launcher).
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { PaneNode } from "./tree.ts";
import { chromeOf, moveTab } from "./tree.ts";
import {
  channelById,
  confirmClose,
  prefs,
  renaming,
  sessions,
  updateLayout,
  workspaceById,
} from "../state.ts";
import type { SessionInfo, SessionKind } from "../types.ts";
import { KIND_LABEL, STATE_LABEL } from "../types.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import * as terms from "../terms.ts";
import { drag, startTabDrag } from "./dnd.tsx";
import { InlineEdit, StateDot } from "../components/common.tsx";
import { type MenuItem, openMenu, openMenuAt } from "../components/Menu.tsx";
import {
  IconCheck,
  IconChevronDown,
  IconCodex,
  IconMaximize,
  IconMinimize,
  IconPlus,
  IconRestart,
  IconSpark,
  IconSplitDown,
  IconSplitRight,
  IconTerminal,
  IconX,
  KindIcon,
} from "../icons.tsx";
import { cx } from "../util.ts";
import { tabLabel } from "../model.ts";

export function spawnMenuItems(
  onPick: (k: SessionKind) => void,
): MenuItem[] {
  return [
    { label: "Claude", icon: <IconSpark size={14} />, accel: "1", run: () => onPick("claude") },
    { label: "Codex", icon: <IconCodex size={14} />, accel: "2", run: () => onPick("codex") },
    { label: "Shell", icon: <IconTerminal size={14} />, accel: "3", run: () => onPick("shell") },
  ];
}

/** Right-click menu of a pane (the only pane chrome when a tab has one pane and one session). */
function paneMenu(
  ch: number,
  paneId: string,
  s: SessionInfo | null,
  multi: boolean,
  zoomed: boolean,
): MenuItem[] {
  const term = s ? terms.terminal(s.id) : null;
  const sel = term?.getSelection() ?? "";
  const items: MenuItem[] = [];
  if (s) {
    items.push(
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
    );
  }
  items.push(
    {
      label: "New session here",
      icon: <IconPlus size={14} />,
      submenu: spawnMenuItems((k) => A.spawn(ch, k, { type: "tab", paneId })),
    },
    {
      label: "Split right",
      icon: <IconSplitRight size={14} />,
      submenu: spawnMenuItems((k) => A.spawn(ch, k, { type: "split", side: "right", paneId })),
    },
    {
      label: "Split down",
      icon: <IconSplitDown size={14} />,
      submenu: spawnMenuItems((k) => A.spawn(ch, k, { type: "split", side: "bottom", paneId })),
    },
  );
  if (multi) {
    items.push({
      label: zoomed ? "Restore layout" : "Zoom pane",
      icon: zoomed ? <IconMinimize size={14} /> : <IconMaximize size={14} />,
      keys: "Alt+Z",
      run: () => {
        A.focusPane(ch, paneId);
        A.zoomToggle();
      },
    });
  }
  if (s) {
    items.push(
      "sep",
      { label: "Rename session", run: () => (renaming.value = { kind: "session", id: s.id }) },
      { label: "Restart", icon: <IconRestart size={14} />, run: () => A.restartSession(s.id) },
      { label: "Interrupt", disabled: s.state === "exited", run: () => A.interruptSession(s.id) },
      { label: "Kill process", danger: true, disabled: s.state === "exited", run: () => A.killSession(s.id) },
      { label: "Close pane", keys: "Alt+W", danger: true, run: () => A.closeSession(s.id) },
    );
  }
  return items;
}

export function Pane(
  { ch, pane, focused, zoomed, multi }: {
    ch: number;
    pane: PaneNode;
    focused: boolean;
    zoomed: boolean;
    /** The tab has 2+ panes. */
    multi: boolean;
  },
) {
  const all = sessions.value;
  const tabs = pane.tabs.map((id) => all.get(id)).filter(Boolean) as SessionInfo[];
  const active = pane.active !== null ? all.get(pane.active) ?? null : null;
  const find = A.findOpen.value;
  const dragging = drag.value;
  const chrome = chromeOf(tabs.length, multi ? 2 : 1);

  const onBodyKey = (e: KeyboardEvent) => {
    if (!active || active.state !== "exited") return;
    if (!(e.target as HTMLElement).closest?.(".term-host, .launcher") || e.isComposing) return;
    if (e.key === "Enter" && !e.altKey && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      e.stopPropagation();
      A.restartSession(active.id);
    }
  };

  const onContext = (e: MouseEvent) => {
    if ((e.target as HTMLElement).closest?.(".approval, .exited-bar, .findbar, input")) return;
    // Programs that track the mouse get the right button; Shift+right-click still opens ours.
    const t = active ? terms.terminal(active.id) : null;
    if (t && t.modes.mouseTrackingMode !== "none" && !e.shiftKey) return;
    e.preventDefault();
    A.focusPane(ch, pane.id);
    openMenu({ x: e.clientX, y: e.clientY, items: paneMenu(ch, pane.id, active, multi, zoomed) });
  };

  return (
    <div
      class={cx("pane", focused && "focused", zoomed && "zoomed", dragging && "drop-target")}
      data-pane={pane.id}
      onPointerDownCapture={() => A.focusPane(ch, pane.id)}
    >
      {chrome === "strip"
        ? <TabStrip ch={ch} pane={pane} tabs={tabs} focused={focused} zoomed={zoomed} multi={multi} />
        : chrome === "head" && active
        ? <PaneHeader ch={ch} paneId={pane.id} s={active} focused={focused} zoomed={zoomed} />
        : null}
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
        {active ? <TermHost sid={active.id} /> : <Launcher ch={ch} paneId={pane.id} focused={focused} />}
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

/** Split / zoom buttons, shown while the pane is hovered. */
function PaneActions(
  { ch, paneId, zoomed, multi }: { ch: number; paneId: string; zoomed: boolean; multi: boolean },
) {
  return (
    <div class="pane-actions">
      {zoomed && (
        <span class="zoom-badge" data-tip="Pane is zoomed" data-keys="Alt+Z">
          zoomed
        </span>
      )}
      <button
        type="button"
        class="strip-btn"
        data-tip="Split right"
        data-keys="Alt+D"
        aria-label="Split right"
        onClick={() => A.spawn(ch, "shell", { type: "split", side: "right", paneId })}
        onPointerDown={(e) => {
          e.stopPropagation();
          A.focusPane(ch, paneId);
        }}
      >
        <IconSplitRight size={14} />
      </button>
      <button
        type="button"
        class="strip-btn"
        data-tip="Split down"
        data-keys="Alt+Shift+D"
        aria-label="Split down"
        onClick={() => A.spawn(ch, "shell", { type: "split", side: "bottom", paneId })}
        onPointerDown={(e) => {
          e.stopPropagation();
          A.focusPane(ch, paneId);
        }}
      >
        <IconSplitDown size={14} />
      </button>
      {(multi || zoomed) && (
        <button
          type="button"
          class={cx("strip-btn", zoomed && "on")}
          data-tip={zoomed ? "Restore layout" : "Zoom pane"}
          data-keys="Alt+Z"
          aria-label={zoomed ? "Restore layout" : "Zoom pane"}
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => {
            A.focusPane(ch, paneId);
            A.zoomToggle();
          }}
        >
          {zoomed ? <IconMinimize size={14} /> : <IconMaximize size={14} />}
        </button>
      )}
    </div>
  );
}

/** The thin header of a one-session pane in a tab with several panes. Drag it to move the
 * session (like a pane-tab); double-click renames. */
function PaneHeader(
  { ch, paneId, s, focused, zoomed }: {
    ch: number;
    paneId: string;
    s: SessionInfo;
    focused: boolean;
    zoomed: boolean;
  },
) {
  const r = renaming.value;
  const isRenaming = r?.kind === "session" && r.id === s.id;
  const cc = confirmClose.value;
  const dragging = drag.value?.sid === s.id;

  if (cc?.kind === "session" && cc.id === s.id) {
    return (
      <div class="pane-head confirm confirm-close" data-pane-head={paneId}>
        <span class="confirm-q">Close {s.name}?</span>
        <button type="button" class="confirm-yes" onClick={() => A.closeSession(s.id, true)}>
          Close <kbd>⏎</kbd>
        </button>
        <button type="button" class="confirm-no" onClick={() => (confirmClose.value = null)}>
          <kbd>Esc</kbd>
        </button>
      </div>
    );
  }

  const tip = [s.title, s.state_detail].filter(Boolean).join(" · ");
  return (
    <div
      class={cx("pane-head", focused && "focused", dragging && "dragging")}
      data-pane-head={paneId}
      data-tip={tip ? `${s.name} — ${tip}` : undefined}
      onPointerDown={(e) => {
        if (isRenaming || (e.target as HTMLElement).closest("button")) return;
        startTabDrag(e, ch, s, () => {
          A.focusPane(ch, paneId);
          A.focusActiveTerminalSoon();
        });
      }}
      onDblClick={(e) => {
        if (!(e.target as HTMLElement).closest("button")) renaming.value = { kind: "session", id: s.id };
      }}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          A.closeSession(s.id);
        }
      }}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
    >
      <KindIcon kind={s.kind} size={12} />
      {isRenaming
        ? (
          <InlineEdit
            value={s.name}
            class="tab-edit"
            onCommit={(v) => {
              renaming.value = null;
              A.renameSession(s.id, v);
            }}
            onCancel={() => (renaming.value = null)}
          />
        )
        : <span class="pane-head-name">{s.name}</span>}
      <span class={cx("pane-head-state", "st-" + s.state)}>{STATE_LABEL[s.state]}</span>
      {s.queued > 0 && <span class="tab-queued" data-tip={`${s.queued} queued`}>{s.queued}</span>}
      <span class="strip-spacer" />
      <PaneActions ch={ch} paneId={paneId} zoomed={zoomed} multi />
      <button
        type="button"
        class="strip-btn pane-head-close"
        aria-label={`Close ${s.name}`}
        data-tip="Close pane"
        data-keys="Alt+W"
        onPointerDown={(e) => e.stopPropagation()}
        onClick={() => A.closeSession(s.id)}
      >
        <IconX size={12} />
      </button>
    </div>
  );
}

function TabStrip(
  { ch, pane, tabs, focused, zoomed, multi }: {
    ch: number;
    pane: PaneNode;
    tabs: SessionInfo[];
    focused: boolean;
    zoomed: boolean;
    multi: boolean;
  },
) {
  const plusRef = useRef<HTMLButtonElement>(null);
  const tabsRef = useRef<HTMLDivElement>(null);
  const moreRef = useRef<HTMLButtonElement>(null);
  const [overflow, setOverflow] = useState(false);

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
        { header: `${tabs.length} sessions in this pane` },
        ...tabs.map((s): MenuItem => ({
          label: s.name,
          icon: <KindIcon kind={s.kind} size={13} />,
          hint: STATE_LABEL[s.state],
          checked: s.id === pane.active,
          run: () => A.activateSessionTab(ch, s.id),
        })),
      ],
      { alignRight: false, minWidth: 220 },
    );
  };

  const openPlus = () => {
    openMenuAt(
      plusRef.current!,
      [
        { header: "New session in this pane" },
        ...spawnMenuItems((k) => A.spawn(ch, k, { type: "tab", paneId: pane.id })),
      ],
      { alignRight: false },
    );
  };
  return (
    <div class={cx("tabstrip", focused && "focused", overflow && "overflowing")} data-tabstrip={pane.id}>
      <div class="tabs" role="tablist" ref={tabsRef} onWheel={onWheel}>
        {tabs.map((s) => (
          <PaneTab
            key={s.id}
            ch={ch}
            paneId={pane.id}
            s={s}
            active={s.id === pane.active}
            paneFocused={focused}
          />
        ))}
      </div>
      {overflow && (
        <button
          ref={moreRef}
          type="button"
          class="tab-more"
          data-tip="All sessions in this pane"
          aria-label="All sessions in this pane"
          onClick={openAll}
        >
          <span class="tab-more-count">{tabs.length}</span>
          <IconChevronDown size={12} />
        </button>
      )}
      <button
        ref={plusRef}
        type="button"
        class="tab-add"
        data-tip="New session"
        data-keys="Alt+N"
        aria-label="New session"
        onClick={openPlus}
      >
        <IconPlus size={14} />
      </button>
      <div class="strip-spacer" />
      <PaneActions ch={ch} paneId={pane.id} zoomed={zoomed} multi={multi} />
    </div>
  );
}

function PaneTab(
  { ch, paneId, s, active, paneFocused }: {
    ch: number;
    paneId: string;
    s: SessionInfo;
    active: boolean;
    paneFocused: boolean;
  },
) {
  const r = renaming.value;
  const isRenaming = r?.kind === "session" && r.id === s.id;
  const cc = confirmClose.value;
  const dragging = drag.value?.sid === s.id;

  const menuItems = (): MenuItem[] => [
    { label: "Rename", run: () => (renaming.value = { kind: "session", id: s.id }) },
    { label: "Restart", icon: <IconRestart size={14} />, run: () => A.restartSession(s.id) },
    { label: "Interrupt", disabled: s.state === "exited", run: () => A.interruptSession(s.id) },
    "sep",
    {
      label: "Move to split right",
      icon: <IconSplitRight size={14} />,
      run: () =>
        A.canSplit(ch, "right", paneId) && updateLayout(ch, (l) => moveTab(l, s.id, paneId, "right")),
    },
    {
      label: "Move to split down",
      icon: <IconSplitDown size={14} />,
      run: () =>
        A.canSplit(ch, "bottom", paneId) && updateLayout(ch, (l) => moveTab(l, s.id, paneId, "bottom")),
    },
    "sep",
    { label: "Copy working directory", run: () => A.copy(s.cwd, "Path copied") },
    "sep",
    {
      label: "Kill process",
      danger: true,
      disabled: s.state === "exited",
      run: () => A.killSession(s.id),
    },
    { label: "Close", keys: "Alt+W", danger: true, run: () => A.closeSession(s.id) },
  ];

  if (cc?.kind === "session" && cc.id === s.id) {
    return (
      <div class="tab confirm confirm-close" data-tab-sid={s.id} role="tab">
        <span class="confirm-q">Close {s.name}?</span>
        <button type="button" class="confirm-yes" onClick={() => A.closeSession(s.id, true)}>
          Close <kbd>⏎</kbd>
        </button>
        <button type="button" class="confirm-no" onClick={() => (confirmClose.value = null)}>
          <kbd>Esc</kbd>
        </button>
      </div>
    );
  }

  const tip = [s.title, s.state_detail].filter(Boolean).join(" · ");
  return (
    <div
      class={cx(
        "tab",
        active && "active",
        active && paneFocused && "focused",
        dragging && "dragging",
        "st-" + s.state,
      )}
      role="tab"
      aria-selected={active}
      data-tab-sid={s.id}
      data-tip={tip ? `${s.name} — ${tip}` : undefined}
      onPointerDown={(e) => {
        if (isRenaming) return;
        if ((e.target as HTMLElement).closest(".tab-close")) return;
        startTabDrag(e, ch, s, () => A.activateSessionTab(ch, s.id));
      }}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          A.closeSession(s.id);
        }
      }}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
      onDblClick={() => (renaming.value = { kind: "session", id: s.id })}
      onContextMenu={(e) => {
        e.preventDefault();
        openMenu({ x: e.clientX, y: e.clientY, items: menuItems() });
      }}
    >
      <StateDot state={s.state} />
      <KindIcon kind={s.kind} size={13} />
      {isRenaming
        ? (
          <InlineEdit
            value={s.name}
            class="tab-edit"
            onCommit={(v) => {
              renaming.value = null;
              A.renameSession(s.id, v);
            }}
            onCancel={() => (renaming.value = null)}
          />
        )
        : <span class="tab-name">{s.name}</span>}
      {s.queued > 0 && <span class="tab-queued" data-tip={`${s.queued} queued`}>{s.queued}</span>}
      <button
        type="button"
        class="tab-close"
        aria-label={`Close ${s.name}`}
        data-tip="Close"
        data-keys="Alt+W"
        onClick={(e) => {
          e.stopPropagation();
          A.closeSession(s.id);
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
      <button type="button" class="link-btn" onClick={() => A.closeSession(s.id, true)}>
        Close
      </button>
    </div>
  );
}

/** Centered Claude / Codex / Shell picker (keys 1 2 3). */
export function LaunchButtons(
  { pick, last }: { pick: (k: SessionKind) => void; last: string },
) {
  const opts: { k: SessionKind; icon: preact.ComponentChildren; desc: string }[] = [
    { k: "claude", icon: <IconSpark size={22} />, desc: "Claude Code agent" },
    { k: "codex", icon: <IconCodex size={22} />, desc: "Codex CLI agent" },
    { k: "shell", icon: <IconTerminal size={22} />, desc: "Plain shell" },
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
          <span class="launch-name">{KIND_LABEL[o.k]}</span>
          <span class="launch-desc">{o.desc}</span>
          <kbd class="launch-key">{i + 1}</kbd>
        </button>
      ))}
    </div>
  );
}

const KINDS: SessionKind[] = ["claude", "codex", "shell"];

function Launcher({ ch, paneId, focused }: { ch: number; paneId: string; focused: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const c = channelById.value.get(ch);
  const label = c ? tabLabel(c, workspaceById.value.get(c.workspace_id)).text : "";
  useEffect(() => {
    if (focused && document.activeElement === document.body) ref.current?.focus();
  }, [focused]);
  const pick = (k: SessionKind) => A.spawn(ch, k, { type: "tab", paneId });
  return (
    <div
      ref={ref}
      class="launcher"
      tabIndex={0}
      data-launcher={paneId}
      onKeyDown={(e) => {
        if (e.altKey || e.ctrlKey || e.metaKey) return;
        const i = ["1", "2", "3"].indexOf(e.key);
        if (i >= 0) {
          e.preventDefault();
          pick(KINDS[i]);
        }
      }}
    >
      <div class="launcher-title">
        Start a session{label
          ? (
            <>
              {" "}in <b>{label}</b>
            </>
          )
          : null}
      </div>
      <div class="launcher-sub">Runs in {c ? <code>{c.cwd}</code> : "the tab's folder"}</div>
      <LaunchButtons pick={pick} last={prefs.value.lastAgent} />
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

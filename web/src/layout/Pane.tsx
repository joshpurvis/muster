// One pane: tab strip + terminal host (+ approval banner, exited bar, find bar, launcher).
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { PaneNode } from "./tree.ts";
import { moveTab } from "./tree.ts";
import { channelById, confirmClose, layouts, prefs, renaming, sessions, updateLayout } from "../state.ts";
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

export function spawnMenuItems(
  onPick: (k: SessionKind) => void,
): MenuItem[] {
  return [
    { label: "Claude", icon: <IconSpark size={14} />, accel: "1", run: () => onPick("claude") },
    { label: "Codex", icon: <IconCodex size={14} />, accel: "2", run: () => onPick("codex") },
    { label: "Shell", icon: <IconTerminal size={14} />, accel: "3", run: () => onPick("shell") },
  ];
}

export function Pane(
  { ch, pane, focused, zoomed }: { ch: number; pane: PaneNode; focused: boolean; zoomed: boolean },
) {
  const all = sessions.value;
  const tabs = pane.tabs.map((id) => all.get(id)).filter(Boolean) as SessionInfo[];
  const active = pane.active !== null ? all.get(pane.active) ?? null : null;
  const multi = (() => {
    const l = layouts.value.get(ch);
    return !!l && l.root.type === "split";
  })();
  const find = A.findOpen.value;
  const dragging = drag.value;

  const onBodyKey = (e: KeyboardEvent) => {
    if (!active || active.state !== "exited") return;
    if (!(e.target as HTMLElement).closest?.(".term-host, .launcher") || e.isComposing) return;
    if (e.key === "Enter" && !e.altKey && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      e.stopPropagation();
      A.restartSession(active.id);
    }
  };

  return (
    <div
      class={cx("pane", focused && "focused", zoomed && "zoomed", dragging && "drop-target")}
      data-pane={pane.id}
      onPointerDownCapture={() => A.focusPane(ch, pane.id)}
    >
      <TabStrip ch={ch} pane={pane} tabs={tabs} focused={focused} zoomed={zoomed} multi={multi} />
      <div
        class={cx("pane-body", active?.state === "exited" && "exited")}
        data-pane-body={pane.id}
        onKeyDownCapture={onBodyKey}
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
        { header: `${tabs.length} tabs in this pane` },
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
          <Tab
            key={s.id}
            ch={ch}
            paneId={pane.id}
            s={s}
            active={s.id === pane.active}
            paneFocused={focused}
            single={tabs.length === 1}
          />
        ))}
      </div>
      {overflow && tabs.length > 1 && (
        <button
          ref={moreRef}
          type="button"
          class="tab-more"
          data-tip="All tabs"
          aria-label="All tabs"
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
      <div class="tabstrip-actions">
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
          onClick={() => A.spawn(ch, "shell", { type: "split", side: "right", paneId: pane.id })}
          onPointerDown={() => A.focusPane(ch, pane.id)}
        >
          <IconSplitRight size={14} />
        </button>
        <button
          type="button"
          class="strip-btn"
          data-tip="Split down"
          data-keys="Alt+Shift+D"
          aria-label="Split down"
          onClick={() => A.spawn(ch, "shell", { type: "split", side: "bottom", paneId: pane.id })}
          onPointerDown={() => A.focusPane(ch, pane.id)}
        >
          <IconSplitDown size={14} />
        </button>
        {(multi || zoomed) && (
          <button
            type="button"
            class={cx("strip-btn", zoomed && "on")}
            data-tip={zoomed ? "Restore layout" : "Zoom pane"}
            data-keys="Alt+Z"
            aria-label="Zoom"
            onClick={() => {
              A.focusPane(ch, pane.id);
              A.zoomToggle();
            }}
          >
            {zoomed ? <IconMinimize size={14} /> : <IconMaximize size={14} />}
          </button>
        )}
      </div>
    </div>
  );
}

function Tab(
  { ch, paneId, s, active, paneFocused, single }: {
    ch: number;
    paneId: string;
    s: SessionInfo;
    active: boolean;
    paneFocused: boolean;
    /** The pane's only tab (can't be split off its own pane). */
    single: boolean;
  },
) {
  const r = renaming.value;
  const isRenaming = r?.kind === "session" && r.id === s.id && r.where === "tab";
  const confirming = confirmClose.value === s.id;
  const dragging = drag.value?.sid === s.id;

  const menuItems = (): MenuItem[] => [
    {
      label: "Rename",
      run: () => (renaming.value = { kind: "session", id: s.id, where: "tab" }),
    },
    { label: "Restart", icon: <IconRestart size={14} />, run: () => A.restartSession(s.id) },
    { label: "Interrupt", disabled: s.state === "exited", run: () => A.interruptSession(s.id) },
    "sep",
    {
      label: "Move to split right",
      icon: <IconSplitRight size={14} />,
      disabled: single,
      run: () =>
        A.canSplit(ch, "right", paneId) && updateLayout(ch, (l) => moveTab(l, s.id, paneId, "right")),
    },
    {
      label: "Move to split down",
      icon: <IconSplitDown size={14} />,
      disabled: single,
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

  if (confirming) {
    return (
      <div class="tab confirm" data-tab-sid={s.id} role="tab">
        <span class="confirm-q">Kill {s.name}?</span>
        <button
          type="button"
          class="confirm-yes"
          onClick={() => A.closeSession(s.id, true)}
        >
          Kill <kbd>⏎</kbd>
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
      onDblClick={() => (renaming.value = { kind: "session", id: s.id, where: "tab" })}
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
            <button
              type="button"
              class="btn btn-sm"
              onClick={() => {
                terms.scrollToBottom(s.id);
                terms.focusTerminal(s.id);
              }}
            >
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
              <button
                type="button"
                class="btn btn-sm btn-ghost"
                onClick={() => {
                  terms.scrollToBottom(s.id);
                  terms.focusTerminal(s.id);
                }}
              >
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

function Launcher({ ch, paneId, focused }: { ch: number; paneId: string; focused: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const c = channelById.value.get(ch);
  const last = prefs.value.lastAgent;
  useEffect(() => {
    if (focused && document.activeElement === document.body) ref.current?.focus();
  }, [focused]);
  const pick = (k: SessionKind) => A.spawn(ch, k, { type: "tab", paneId });
  const opts: { k: SessionKind; icon: preact.ComponentChildren; desc: string }[] = [
    { k: "claude", icon: <IconSpark size={22} />, desc: "Claude Code agent" },
    { k: "codex", icon: <IconCodex size={22} />, desc: "Codex CLI agent" },
    { k: "shell", icon: <IconTerminal size={22} />, desc: "Plain shell" },
  ];
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
          pick(opts[i].k);
        }
      }}
    >
      <div class="launcher-title">
        Start a session{c
          ? (
            <>
              {" "}in <b>#{c.name}</b>
            </>
          )
          : null}
      </div>
      <div class="launcher-sub">Runs in {c ? <code>{c.cwd}</code> : "the channel directory"}</div>
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

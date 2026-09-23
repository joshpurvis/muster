// Left sidebar: inbox row, workspaces with channels (and the active channel's sessions).
import { useRef, useState } from "preact/hooks";
import {
  activeChannelId,
  aggregateState,
  altHeld,
  channelOrder,
  channels,
  channelsByWorkspace,
  inboxItems,
  layouts,
  overlay,
  prefs,
  renaming,
  sessionsByChannel,
  setPref,
  unread,
  view,
  workspaces,
} from "../state.ts";
import { connStatus } from "../rpc.ts";
import type { Channel, SessionInfo, Workspace } from "../types.ts";
import { STATE_LABEL } from "../types.ts";
import * as A from "../actions.ts";
import { InlineEdit, Spinner, StateDot } from "./common.tsx";
import { type MenuItem, openMenu, openMenuAt } from "./Menu.tsx";
import { spawnMenuItems } from "../layout/Pane.tsx";
import {
  IconArchive,
  IconChevron,
  IconCopy,
  IconInbox,
  IconMore,
  IconPencil,
  IconPlus,
  KindIcon,
} from "../icons.tsx";
import { cx } from "../util.ts";
import { findPane } from "../layout/tree.ts";

export function Sidebar() {
  const n = inboxItems.value.length;
  const ws = workspaces.value;
  return (
    <nav class="sidebar" aria-label="Channels">
      <div class="sidebar-scroll">
        <button
          type="button"
          class={cx("side-row inbox-row", view.value === "inbox" && "active")}
          onClick={A.openInbox}
          data-tip="Everything that needs you"
          data-keys="Alt+I"
        >
          <IconInbox size={15} />
          <span class="side-label">Inbox</span>
          {n > 0 && <span class="badge badge-attn">{n}</span>}
        </button>
        <div class="side-gap" />
        {ws.map((w) => <WorkspaceSection key={w.id} w={w} />)}
        {ws.length === 0 && (
          <div class="side-empty">
            Add a repo or folder to start creating channels.
          </div>
        )}
      </div>
      <div class="sidebar-footer">
        <button
          type="button"
          class="side-footer-btn"
          onClick={() => (overlay.value = { kind: "add_workspace" })}
        >
          <IconPlus size={14} /> Add workspace
        </button>
        <ConnDot />
      </div>
    </nav>
  );
}

function ConnDot() {
  const s = connStatus.value;
  const label = s === "open" ? "Connected" : s === "no_token" ? "No token" : "Reconnecting…";
  return (
    <span class={cx("conn", "conn-" + s)} data-tip={label} role="status" aria-label={label}>
      <span class="conn-dot" />
    </span>
  );
}

function WorkspaceSection({ w }: { w: Workspace }) {
  const collapsed = prefs.value.collapsedWorkspaces.includes(w.id);
  const list = channelsByWorkspace.value.get(w.id) ?? [];
  const moreRef = useRef<HTMLButtonElement>(null);
  const toggle = () => {
    const cur = prefs.value.collapsedWorkspaces;
    setPref(
      "collapsedWorkspaces",
      collapsed ? cur.filter((x) => x !== w.id) : [...cur, w.id],
    );
  };
  const wsMenu = (): MenuItem[] => [
    {
      label: "New channel…",
      keys: "Alt+Shift+N",
      icon: <IconPlus size={14} />,
      run: () => A.newChannelDialog(w.id),
    },
    { label: "Copy path", icon: <IconCopy size={14} />, run: () => A.copy(w.path, "Path copied") },
    "sep",
    {
      label: "Remove workspace…",
      danger: true,
      run: () =>
        overlay.value = {
          kind: "confirm",
          title: `Remove ${w.name}?`,
          body:
            "Its channels are archived and their sessions are stopped. Worktrees and files on disk are kept.",
          confirmLabel: "Remove workspace",
          danger: true,
          run: () => A.removeWorkspace(w.id),
        },
    },
  ];
  // Collapsed sections still show channels that need attention.
  const sbc = sessionsByChannel.value;
  const visible = collapsed
    ? list.filter((c) => c.id === activeChannelId.value || needs(sbc.get(c.id)))
    : list;
  return (
    <section class="ws">
      <div
        class="ws-head"
        onClick={toggle}
        onContextMenu={(e) => {
          e.preventDefault();
          openMenu({ x: e.clientX, y: e.clientY, items: wsMenu() });
        }}
        data-tip={w.path}
      >
        <IconChevron size={12} class={cx("ws-chev", !collapsed && "open")} />
        <span class="ws-name">{w.name}</span>
        <span class="ws-actions">
          <button
            type="button"
            class="mini-btn"
            aria-label="New channel"
            data-tip="New channel"
            data-keys="Alt+Shift+N"
            onClick={(e) => {
              e.stopPropagation();
              A.newChannelDialog(w.id);
            }}
          >
            <IconPlus size={13} />
          </button>
          <button
            type="button"
            class="mini-btn"
            ref={moreRef}
            aria-label="Workspace menu"
            onClick={(e) => {
              e.stopPropagation();
              openMenuAt(moreRef.current!, wsMenu(), { alignRight: false });
            }}
          >
            <IconMore size={13} />
          </button>
        </span>
      </div>
      <ChannelList w={w} list={visible} reorderable={!collapsed} />
      {!collapsed && list.length === 0 && (
        <button
          type="button"
          class="side-row ghost-row"
          onClick={() => A.newChannelDialog(w.id)}
        >
          <IconPlus size={13} /> <span class="side-label">New channel</span>
        </button>
      )}
    </section>
  );
}

function needs(list: SessionInfo[] | undefined) {
  return !!list?.some((s) => ["needs_approval", "asking_question", "error", "stuck"].includes(s.state));
}

/** Channel rows with pointer-based drag-to-reorder. */
function ChannelList(
  { w, list, reorderable }: { w: Workspace; list: Channel[]; reorderable: boolean },
) {
  const ref = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<{ id: number; index: number; y: number } | null>(null);

  const startDrag = (e: PointerEvent, c: Channel) => {
    if (e.button !== 0 || !reorderable) return;
    const sy = e.clientY;
    let started = false;
    let index = -1;
    const move = (ev: PointerEvent) => {
      if (!started && Math.abs(ev.clientY - sy) < 5) return;
      if (!started) {
        started = true;
        document.body.classList.add("dragging");
      }
      // Measure the channel rows themselves (the active one's wrapper also holds its sessions).
      const rows = [...ref.current!.querySelectorAll<HTMLElement>("[data-ch-row] > .ch-row")];
      index = rows.length;
      for (let i = 0; i < rows.length; i++) {
        const r = rows[i].getBoundingClientRect();
        if (ev.clientY < r.top + r.height / 2) {
          index = i;
          break;
        }
      }
      const y = index < rows.length
        ? rows[index].getBoundingClientRect().top
        : rows[rows.length - 1].getBoundingClientRect().bottom;
      setDrag({ id: c.id, index, y: y - ref.current!.getBoundingClientRect().top });
    };
    const stop = () => {
      removeEventListener("pointermove", move, true);
      removeEventListener("pointerup", up, true);
      removeEventListener("pointercancel", cancel, true);
      removeEventListener("blur", cancel);
      document.body.classList.remove("dragging");
      setDrag(null);
    };
    const cancel = () => stop();
    const up = () => {
      stop();
      if (!started) return;
      // Suppress the click that follows a drag.
      const swallow = (ev: Event) => ev.stopPropagation();
      addEventListener("click", swallow, { capture: true, once: true });
      setTimeout(() => removeEventListener("click", swallow, true), 0);
      const ids = list.map((x) => x.id);
      const from = ids.indexOf(c.id);
      if (from < 0 || index < 0) return;
      ids.splice(from, 1);
      ids.splice(index > from ? index - 1 : index, 0, c.id);
      if (ids.join() === list.map((x) => x.id).join()) return;
      // Optimistic: renumber sort_order locally.
      const order = new Map(ids.map((id, i) => [id, i]));
      channels.value = channels.value.map((x) =>
        order.has(x.id) ? { ...x, sort_order: order.get(x.id)! } : x
      );
      // Send the full order across workspaces so the daemon has a total order.
      const all = workspaces.value.flatMap((ws) =>
        ws.id === w.id ? ids : (channelsByWorkspace.value.get(ws.id) ?? []).map((x) => x.id)
      );
      A.reorderChannels(all);
    };
    addEventListener("pointermove", move, true);
    addEventListener("pointerup", up, true);
    addEventListener("pointercancel", cancel, true);
    addEventListener("blur", cancel);
  };

  return (
    <div class="ch-list" ref={ref}>
      {list.map((c) => (
        <ChannelRow
          key={c.id}
          c={c}
          dragging={drag?.id === c.id}
          onPointerDown={(e) => startDrag(e, c)}
        />
      ))}
      {drag && <div class="ch-drop-line" style={{ top: drag.y - 1 + "px" }} />}
    </div>
  );
}

function ChannelRow(
  { c, dragging, onPointerDown }: {
    c: Channel;
    dragging: boolean;
    onPointerDown: (e: PointerEvent) => void;
  },
) {
  const active = activeChannelId.value === c.id && view.value === "channel";
  const list = sessionsByChannel.value.get(c.id);
  const agg = aggregateState(list);
  const isUnread = unread.value.has(c.id) && !active;
  const attn =
    (list ?? []).filter((s) => ["needs_approval", "asking_question", "error", "stuck"].includes(s.state))
      .length;
  const queued = (list ?? []).reduce((n, s) => n + s.queued, 0);
  const working = agg === "working" || agg === "starting";
  const r = renaming.value;
  const isRenaming = r?.kind === "channel" && r.id === c.id && r.where === "sidebar";
  const num = altHeld.value ? channelOrder.value.findIndex((x) => x.id === c.id) + 1 : 0;

  const items = (): MenuItem[] => [
    {
      label: "Rename",
      icon: <IconPencil size={14} />,
      run: () => (renaming.value = { kind: "channel", id: c.id, where: "sidebar" }),
    },
    {
      label: "New session",
      icon: <IconPlus size={14} />,
      submenu: spawnMenuItems((k) => {
        A.selectChannel(c.id, false);
        A.spawn(c.id, k, { type: "tab" });
      }),
    },
    { label: "Copy path", icon: <IconCopy size={14} />, run: () => A.copy(c.cwd, "Path copied") },
    ...(c.branch ? [{ label: "Copy branch name", run: () => A.copy(c.branch!, "Branch copied") }] : []),
    { label: "Mark read", disabled: !unread.value.has(c.id), run: () => A.markChannelRead(c.id) },
    "sep",
    { label: "Archive…", icon: <IconArchive size={14} />, danger: true, run: () => confirmArchive(c, false) },
    ...(c.owns_worktree
      ? [{ label: "Archive + remove worktree…", danger: true, run: () => confirmArchive(c, true) }]
      : []),
  ];

  return (
    <div data-ch-row={c.id}>
      <div
        class={cx("side-row ch-row", active && "active", isUnread && "unread", dragging && "dragging")}
        role="button"
        tabIndex={-1}
        onPointerDown={onPointerDown}
        onClick={() => A.selectChannel(c.id)}
        onDblClick={() => (renaming.value = { kind: "channel", id: c.id, where: "sidebar" })}
        onContextMenu={(e) => {
          e.preventDefault();
          openMenu({ x: e.clientX, y: e.clientY, items: items() });
        }}
      >
        <StateDot state={agg} />
        <span class="ch-hash">#</span>
        {isRenaming
          ? (
            <InlineEdit
              value={c.name}
              onCommit={(v) => {
                renaming.value = null;
                A.renameChannel(c.id, v);
              }}
              onCancel={() => (renaming.value = null)}
            />
          )
          : <span class="side-label">{c.name}</span>}
        <span class="ch-right">
          {num > 0 && num <= 9 ? <kbd class="ch-num" aria-label={`Alt+${num}`}>{num}</kbd> : (
            <>
              {attn > 0 && <span class="badge badge-attn">{attn}</span>}
              {attn === 0 && queued > 0 && <span class="badge" data-tip={`${queued} queued`}>{queued}</span>}
              {attn === 0 && queued === 0 && working && <Spinner size={11} />}
            </>
          )}
        </span>
      </div>
      {active && list && list.length > 0 && <SessionRows ch={c.id} list={list} />}
    </div>
  );
}

function SessionRows({ ch, list }: { ch: number; list: SessionInfo[] }) {
  const l = layouts.value.get(ch);
  const focusedSid = l ? findPane(l.root, l.focus)?.active ?? null : null;
  return (
    <div class="sess-list">
      {list.map((s) => (
        <button
          type="button"
          key={s.id}
          class={cx("sess-row", s.id === focusedSid && "focused")}
          onClick={() => A.activateSessionTab(ch, s.id)}
          data-tip={s.state_detail ?? undefined}
        >
          <KindIcon kind={s.kind} size={12} />
          <span class="sess-name">{s.name}</span>
          <span class={cx("sess-state", "st-" + s.state)}>{STATE_LABEL[s.state]}</span>
        </button>
      ))}
    </div>
  );
}

export function confirmArchive(c: Channel, removeWorktree: boolean) {
  overlay.value = {
    kind: "confirm",
    title: removeWorktree ? `Archive #${c.name} and remove its worktree?` : `Archive #${c.name}?`,
    body: removeWorktree
      ? `Stops its sessions and deletes the worktree at ${c.cwd}. Uncommitted changes there are lost. The branch${
        c.branch ? ` ${c.branch}` : ""
      } is kept.`
      : "Stops its sessions and hides the channel. The worktree and branch stay on disk.",
    confirmLabel: removeWorktree ? "Archive and remove" : "Archive",
    danger: true,
    run: () => A.archiveChannel(c.id, removeWorktree),
  };
}

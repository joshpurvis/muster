import { useRef, useState } from "preact/hooks";
import {
  activeChannelId, agents, agentsPanelOpen, channelLayout, channels, channelsPanelOpen,
  cacheNow, meta, sessionsByChannel, toast, unread, updateSidebar, windowChannels, windowPrefs, workspaceById,
} from "../state.ts";
import { CACHE_DOT_LABEL, channelCacheDot } from "../cache.ts";
import { needsHuman, type Channel } from "../types.ts";
import { aggregateState } from "../model.ts";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { sidebarRatio } from "../layout/channels.ts";
import { allSessions } from "../layout/tree.ts";
import { drag } from "../layout/dnd.tsx";
import { IconAgents, IconChannels, IconChevron, IconMore, IconPlus } from "../icons.tsx";
import { cx, tildify } from "../util.ts";
import { InlineEdit, StateDot } from "./common.tsx";
import { contextMenu, openMenuAt, type MenuItem } from "./Menu.tsx";
import { AgentsPanel } from "./Agents.tsx";
import { agentsHeaderItems } from "./Schedules.tsx";

function channelViewItems(): MenuItem[] {
  const side = channelLayout.peek()!.sidebar;
  return [
    { label: side.channels ? "Hide channels" : "Show channels", run: A.toggleChannelsPanel },
    { label: "Hide sidebar", keys: "Alt+G", run: A.toggleSidebar },
  ];
}

/** Channels and global agents share the dock, but have independent scrolling and collapse. */
export function Sidebar() {
  const ref = useRef<HTMLElement>(null);
  const side = channelLayout.value?.sidebar;
  if (!side) return null;
  const both = side.channels && side.agents;
  const resize = (e: PointerEvent) => {
    if (e.button !== 0 || !ref.current) return;
    e.preventDefault();
    const rect = ref.current.getBoundingClientRect();
    const move = (ev: PointerEvent) => updateSidebar({ split: sidebarRatio((ev.clientY - rect.top) / rect.height) });
    const done = () => {
      removeEventListener("pointermove", move);
      removeEventListener("pointerup", done);
      removeEventListener("pointercancel", done);
      removeEventListener("blur", done);
      document.body.classList.remove("resizing-row");
    };
    document.body.classList.add("resizing-row");
    addEventListener("pointermove", move);
    addEventListener("pointerup", done);
    addEventListener("pointercancel", done);
    addEventListener("blur", done);
  };
  return (
    <aside ref={ref} class="sidebar" aria-label="Channels and agents"
      style={{ display: side.hidden ? "none" : undefined,
        gridTemplateRows: both ? `minmax(80px, ${side.split}fr) 5px minmax(100px, ${1 - side.split}fr)`
        : `${side.channels ? "minmax(0, 1fr)" : "var(--tab-h)"} 1px ${side.agents ? "minmax(0, 1fr)" : "var(--tab-h)"}` }}>
      <ChannelsPanel />
      <div class={cx("sidebar-divider", both && "resizable")}
        role="separator" aria-label="Resize channels and agents" aria-orientation="horizontal"
        aria-valuemin={20} aria-valuemax={80} aria-valuenow={Math.round(side.split * 100)}
        tabIndex={both ? 0 : -1} onPointerDown={both ? resize : undefined}
        onKeyDown={(e) => {
          if (!both || !["ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
          e.preventDefault();
          updateSidebar({ split: sidebarRatio(e.key === "Home" ? 0.2 : e.key === "End" ? 0.8 :
            side.split + (e.key === "ArrowDown" ? 0.05 : -0.05)) });
        }} />
      {agentsPanelOpen.value ? <AgentsPanel /> : (
        <button type="button" class="sidebar-collapsed" onClick={A.toggleAgentsPanel} aria-expanded="false"
          {...contextMenu(agentsHeaderItems)}>
          <IconChevron size={12} /><IconAgents size={14} /><span>Agents</span>
          <span class="ap-count">{agents.value.length}</span>
        </button>
      )}
    </aside>
  );
}

/** One flat list of the window's channels, in their saved order. */
function ChannelsPanel() {
  const open = channelsPanelOpen.value;
  const list = windowChannels.value;
  const fallback = channelLayout.value?.layouts[0];
  const headerItems = (): MenuItem[] => [
    { label: "New channel…", keys: "Alt+Shift+N", run: () => A.newTabDialog() },
    "sep", ...channelViewItems(),
  ];
  return (
    <section class="channels-panel" aria-label="Channels" {...contextMenu(headerItems)}>
      <div class="ap-head">
        <button type="button" class="sidebar-heading" aria-expanded={open} onClick={A.toggleChannelsPanel}>
          <IconChevron size={12} class={open ? "open" : ""} /><IconChannels size={14} /><span>Channels</span>
          <span class="ap-count">{list.length + (fallback ? 1 : 0)}</span>
        </button>
        <button type="button" class="ap-icon-btn" aria-label="New channel" data-tip="New channel" data-keys="Alt+Shift+N"
          onClick={() => A.newTabDialog()}><IconPlus size={13} /></button>
      </div>
      {open && <nav class="channels-scroll" aria-label="Channels" onKeyDown={(e) => {
        if (!(e.target as HTMLElement).matches(".channel-select") || !["ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
        e.preventDefault();
        const rows = [...e.currentTarget.querySelectorAll<HTMLButtonElement>(".channel-select")];
        const i = rows.indexOf(e.target as HTMLButtonElement);
        rows[e.key === "Home" ? 0 : e.key === "End" ? rows.length - 1 :
          Math.max(0, Math.min(rows.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))]?.focus();
      }}>
        {list.map((c) => <ChannelRow key={c.id} channel={c} context={qualifier(c, list)} />)}
        {fallback && <button type="button" class={cx("channel-select channel-fallback", activeChannelId.value === 0 && "active")}
          {...contextMenu(() => [
            { label: "New note", run: () => { A.openChannel(0, false); A.newNote(); } },
            { label: "All notes…", run: A.openNotesList },
            "sep",
            ...channelViewItems(),
          ])}
          aria-current={activeChannelId.value === 0 ? "page" : undefined} onClick={() => A.openChannel(0)}>
          <IconChannels size={13} /><span>Notes</span><span class="channel-count">{allSessions(fallback.root).length}</span>
        </button>}
        {!list.length && !fallback && <div class="ap-empty">
          Keep a project's terminals, notes and panels together.
          <button type="button" class="side-create" onClick={() => A.newTabDialog()}><IconPlus size={12} /> New channel</button>
        </div>}
      </nav>}
    </section>
  );
}

/** Without project headings, channels sharing a name show what tells them apart. */
function qualifier(c: Channel, list: Channel[]): string | null {
  const same = list.filter((x) => x.name === c.name);
  if (same.length < 2) return null;
  if (c.branch && same.filter((x) => x.branch === c.branch).length === 1) return c.branch;
  if (same.some((x) => x.cwd !== c.cwd)) return tildify(c.cwd, meta.value.home);
  return null;
}

function ChannelRow({ channel: c, context }: { channel: Channel; context: string | null }) {
  const [editing, setEditing] = useState(false);
  const items = sessionsByChannel.value.get(c.id) ?? [];
  const attention = items.filter((s) => needsHuman(s.state)).length;
  const state = aggregateState(items);
  const cache = state === "idle"
    ? channelCacheDot(items, cacheNow(), windowPrefs.value?.cache_warn !== false) : null;
  const active = activeChannelId.value === c.id;
  const count = channelLayout.value?.layouts[c.id] ? allSessions(channelLayout.value.layouts[c.id].root).length : 0;
  const d = drag.value;
  const menu = (): MenuItem[] => [
    { label: "Rename", run: () => setEditing(true) },
    { label: "New tab", run: () => { A.openChannel(c.id, false); A.newTabHere(); } },
    { label: "New note", run: () => { A.openChannel(c.id, false); A.newNote(); } },
    { label: "Permissions…", run: () => A.openPermissions(c.id) },
    { label: "Copy folder path", run: () => A.copy(c.cwd, "Path copied") },
    { label: "New channel in this project…",
      run: () => A.newTabDialog(false, workspaceById.peek().get(c.workspace_id)?.path ?? c.cwd) },
    "sep",
    { label: "Move up", disabled: !canReorder(c, -1), run: () => reorder(c, -1) },
    { label: "Move down", disabled: !canReorder(c, 1), run: () => reorder(c, 1) },
    "sep",
    { label: "Archive channel…", danger: true, run: () => A.archiveChannelAsk(c) },
    "sep", ...channelViewItems(),
  ];
  return <div class={cx("channel-row", active && "active", unread.value.has(c.id) && !active && "unread",
    d?.channel === c.id && "channel-drop")} data-channel-id={c.id}
    {...contextMenu(menu)}>
    {editing ? <div class="channel-rename"><InlineEdit fields={[{ value: c.name, label: "Channel name" }]}
      onCommit={([name]) => { setEditing(false); A.renameChannel(c.id, name); }} onCancel={() => setEditing(false)} /></div> :
      <button type="button" class="channel-select" aria-current={active ? "page" : undefined}
        data-tip={`${c.name} · ${tildify(c.cwd, meta.value.home)}${c.branch ? ` · ${c.branch}` : ""}${attention ? ` · ${attention} need you` : ""}${cache ? ` · ${CACHE_DOT_LABEL[cache]}` : ""}`}
        onClick={() => A.openChannel(c.id)} onDblClick={() => setEditing(true)}>
        <StateDot state={state} cache={cache} />
        {context ? <span class="channel-label"><span class="channel-name">{c.name}</span>
          <span class="channel-context">{context}</span></span> : <span class="channel-name">{c.name}</span>}
        {attention > 0 ? <span class="channel-attention" aria-label={`${attention} need you`}>{attention}</span> :
          <span class="channel-count" aria-label={`${count} tabs`}>{count}</span>}
      </button>}
    <button type="button" class="ap-icon-btn channel-more" aria-label={`Menu for ${c.name}`} aria-haspopup="menu"
      onClick={(e) => openMenuAt(e.currentTarget, menu(), { alignRight: false })}><IconMore size={12} /></button>
  </div>;
}

function canReorder(c: Channel, delta: number) {
  const list = windowChannels.peek();
  const i = list.findIndex((x) => x.id === c.id);
  return i + delta >= 0 && i + delta < list.length;
}

async function reorder(c: Channel, delta: number) {
  if (!canReorder(c, delta)) return;
  const all = [...channels.peek()].sort((a, b) => a.sort_order - b.sort_order || a.id - b.id);
  const shown = windowChannels.peek();
  const neighbour = shown[shown.findIndex((x) => x.id === c.id) + delta];
  const a = all.findIndex((x) => x.id === c.id), b = all.findIndex((x) => x.id === neighbour.id);
  [all[a], all[b]] = [all[b], all[a]];
  try {
    await rpc.call("channel.reorder", { ids: all.map((x) => x.id) });
    const order = new Map(all.map((x, i) => [x.id, i]));
    channels.value = channels.peek().map((x) => ({ ...x, sort_order: order.get(x.id) ?? x.sort_order }));
  } catch (e) {
    toast("error", `Couldn't reorder channels: ${e instanceof Error ? e.message : e}`);
  }
}

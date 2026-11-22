import { useRef, useState } from "preact/hooks";
import {
  activeChannelId, agents, agentsSupported, channelLayout, channels, channelsPanelOpen, cacheNow, hereIds, meta, overlay,
  prefs, schedules, schedulesSupported, sessionsByChannel, toast, unread, updateSidebar, windowChannels, windowPrefs,
  workspaceById,
} from "../state.ts";
import { CACHE_DOT_LABEL, channelCacheDot } from "../cache.ts";
import { needsHuman, type Channel } from "../types.ts";
import { aggregateState } from "../model.ts";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { SIDEBAR_MAX, SIDEBAR_MIN, sidebarWidth } from "../layout/channels.ts";
import { allSessions } from "../layout/tree.ts";
import { drag } from "../layout/dnd.tsx";
import { IconChannels, IconChevron, IconClock, IconMore, IconNote, IconPlus, IconShieldAlert, IconSpark } from "../icons.tsx";
import { notes, notesSupported } from "../notes.ts";
import { cardKey, otherName, rowChip, type RowChip as Chip, sidebarRows, type SidebarRow } from "../sidebar.ts";
import { cx, tildify } from "../util.ts";
import { InlineEdit, StateDot } from "./common.tsx";
import { contextMenu, openMenuAt, type MenuItem } from "./Menu.tsx";
import { Feed, useNow } from "./Agents.tsx";
import { UsageCard } from "./AccountProfiles.tsx";
import { NeedsYou, scheduleHeaderItems, SchedulesPausedNotice } from "./Schedules.tsx";
import {
  AgentCard, agentCard, AgentDot, agentFacts, closeCard, hoverCard, leaveCard, openAgentRow, openCardByKey,
  rowMenuItems,
} from "./AgentCard.tsx";

/** From this width the rows show each agent's state and time, cold, and its schedule count. */
const WIDE = 300;

function channelViewItems(): MenuItem[] {
  const side = channelLayout.peek()!.sidebar;
  return [
    { label: side.channels ? "Hide channels" : "Show channels", keys: "Alt+Shift+G", run: A.toggleChannelsPanel },
    { label: "Hide sidebar", keys: "Alt+G", run: A.toggleSidebar },
  ];
}

/** The Channels header's ⋯ (and its right-click): new channel, agent and schedule, the schedules'
 * pause and settings, session communication, and hiding. */
function headerItems(): MenuItem[] {
  const ag = agentsSupported.value;
  const sch = scheduleHeaderItems();
  return [
    { label: "New channel…", keys: "Alt+Shift+N", run: () => A.newTabDialog() },
    ...(ag ? ["sep" as const, { label: "New agent…", run: () => A.newAgentDialog() }] : []),
    ...sch,
    ...(ag ? [{ label: "Session communication…", run: () => (overlay.value = { kind: "communication" }) }] : []),
    "sep",
    ...channelViewItems(),
  ];
}

/** One list of channels; an agent lives in its channel's row. Messages and Usage sit under it. */
export function Sidebar() {
  const ref = useRef<HTMLElement>(null);
  const side = channelLayout.value?.sidebar;
  const now = useNow();
  if (!side) return null;
  const resize = (e: PointerEvent) => {
    if (e.button !== 0 || !ref.current) return;
    e.preventDefault();
    const left = ref.current.getBoundingClientRect().left;
    const move = (ev: PointerEvent) => updateSidebar({ width: sidebarWidth(ev.clientX - left) });
    const done = () => {
      removeEventListener("pointermove", move);
      removeEventListener("pointerup", done);
      removeEventListener("pointercancel", done);
      removeEventListener("blur", done);
      document.body.classList.remove("resizing-col");
    };
    closeCard();
    document.body.classList.add("resizing-col");
    addEventListener("pointermove", move);
    addEventListener("pointerup", done);
    addEventListener("pointercancel", done);
    addEventListener("blur", done);
  };
  return (
    <aside ref={ref} class={cx("sidebar", side.width >= WIDE && "wide")} aria-label="Channels and agents"
      style={{ display: side.hidden ? "none" : undefined, "--sidebar-w": `${side.width}px` }}>
      <ChannelsPanel now={now} />
      {agentsSupported.value && <Feed now={now} />}
      <UsageCard />
      <div class="sidebar-edge" role="separator" aria-label="Sidebar width" aria-orientation="vertical"
        aria-valuemin={SIDEBAR_MIN} aria-valuemax={SIDEBAR_MAX} aria-valuenow={side.width} tabIndex={0}
        data-tip="Drag to widen: wider rows show each agent's state and schedules"
        onPointerDown={resize} onDblClick={() => updateSidebar({ width: sidebarWidth(NaN) })}
        onKeyDown={(e) => {
          if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return;
          e.preventDefault();
          updateSidebar({ width: e.key === "Home" ? SIDEBAR_MIN : e.key === "End" ? SIDEBAR_MAX :
            sidebarWidth(side.width + (e.key === "ArrowRight" ? 20 : -20)) });
        }} />
    </aside>
  );
}

/** The window's channels in their saved order, each agent in its channel's row, with the
 * "Needs you" box above them while anything waits for the human. */
function ChannelsPanel({ now }: { now: number }) {
  const open = channelsPanelOpen.value;
  const list = windowChannels.value;
  const rows = sidebarRows(list, agentsSupported.value ? agents.value : []);
  const fallback = channelLayout.value?.layouts[0];
  return (
    <section class="channels-panel" aria-label="Channels" {...contextMenu(headerItems)}>
      <div class="ap-head">
        <button type="button" class="sidebar-heading" aria-expanded={open} onClick={A.toggleChannelsPanel}>
          <IconChevron size={12} class={open ? "open" : ""} /><IconChannels size={14} /><span>Channels</span>
          <span class="ap-count">{list.length}</span>
        </button>
        <button type="button" class="ap-icon-btn" aria-label="New channel" data-tip="New channel" data-keys="Alt+Shift+N"
          onClick={() => A.newTabDialog()}><IconPlus size={13} /></button>
        <button type="button" class="ap-icon-btn ap-head-more" aria-label="Channels menu" aria-haspopup="menu"
          data-tip={agentsSupported.value ? "New agent or schedule, pause all, settings" : "New channel"}
          onClick={(e) => openMenuAt(e.currentTarget, headerItems(), { minWidth: 210, alignRight: false })}>
          <IconMore size={13} />
        </button>
      </div>
      {open && <SchedulesPausedNotice />}
      {open && <div class="channels-scroll">
        <NeedsYou now={now} openAgent={openCardByKey} />
        <nav aria-label="Channels" onKeyDown={(e) => {
          const t = e.target as HTMLElement;
          if (!t.matches(".channel-select")) return;
          const id = Number(t.closest<HTMLElement>(".channel-row")?.dataset.agentId);
          if (id && cardKey(e.key, "row") === "open") {
            e.preventDefault();
            return openCardByKey(id);
          }
          if (e.key === "Escape" && agentCard.peek()) return closeCard();
          if (!["ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
          e.preventDefault();
          const all = [...e.currentTarget.querySelectorAll<HTMLButtonElement>(".channel-select")];
          const i = all.indexOf(t as HTMLButtonElement);
          all[e.key === "Home" ? 0 : e.key === "End" ? all.length - 1 :
            Math.max(0, Math.min(all.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))]?.focus();
        }}>
          {rows.map((r) => <Row key={r.key} row={r} context={r.channel ? qualifier(r.channel, list) : null} now={now} />)}
          {!rows.length && <div class="ap-empty">
            Keep a project's terminals, notes and panels together.
            <button type="button" class="side-create" onClick={() => A.newTabDialog()}><IconPlus size={12} /> New channel</button>
          </div>}
          {/* Opens the notes list (every note, newest first, type to filter), not the old Notes
              workspace, whose tabs only held notes that had no channel when channels came in. */}
          {notesSupported.value && <button type="button" class={cx("channel-select channel-fallback", activeChannelId.value === 0 && "active")}
            {...contextMenu(() => [
              { label: "New note", run: () => A.newNote() },
              ...(fallback ? [{ label: "Show unfiled note tabs", run: () => A.openChannel(0) }] : []),
              "sep",
              ...channelViewItems(),
            ])}
            aria-haspopup="dialog" data-tip="Find and open any note" onClick={A.openNotesList}>
            <IconNote size={13} /><span>Notes</span><span class="channel-count">{notes.value.size}</span>
          </button>}
        </nav>
      </div>}
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

function channelItems(c: Channel, rename: () => void): MenuItem[] {
  return [
    { label: "Rename", run: rename },
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
  ];
}

/** A channel (with its agent, if one owns it), or an agent whose channel isn't in this window. */
function Row({ row, context, now }: { row: SidebarRow; context: string | null; now: number }) {
  const { channel: c, agent: a } = row;
  const [editing, setEditing] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const items = c ? sessionsByChannel.value.get(c.id) ?? [] : [];
  const attention = items.filter((s) => needsHuman(s.state)).length;
  const state = aggregateState(items);
  const cache = state === "idle"
    ? channelCacheDot(items, cacheNow(), windowPrefs.value?.cache_warn !== false) : null;
  const active = !!c && activeChannelId.value === c.id;
  const count = c && channelLayout.value?.layouts[c.id] ? allSessions(channelLayout.value.layouts[c.id].root).length : 0;
  const d = drag.value;
  const own = c ? channelItems(c, () => setEditing(true)) : [];
  const menu = (): MenuItem[] => a
    ? rowMenuItems(a, c, [...own, ...(own.length ? ["sep" as const] : []), ...channelViewItems()])
    : [...own, "sep", ...channelViewItems()];
  const cardOpen = !!a && agentCard.value?.agent === a.id;
  const facts = a ? agentFacts(a, now) : null;
  const chip = a && schedulesSupported.value ? rowChip(a.id, schedules.value, prefs.value.hiddenRuns) : null;
  const other = otherName(c, a);
  const name = c?.name ?? a!.name;
  const label = a
    ? [
      `${name}${other ? `, agent ${other}` : ", agent"}`,
      facts!.status,
      facts!.cold ? "prompt cache likely expired" : null,
      c ? (attention ? `${attention} need you` : `${count} ${count === 1 ? "tab" : "tabs"}`) : "no channel in this window",
      chip && chip.tone !== "plain" ? chip.label : null,
    ].filter(Boolean).join(", ")
    : undefined;
  const open = () => a ? openAgentRow(a, c, hereIds.peek()) : A.openChannel(c!.id);
  return <div ref={ref} class={cx("channel-row", active && "active", c && unread.value.has(c.id) && !active && "unread",
    c && d?.channel === c.id && "channel-drop", a && "agent-row", cardOpen && "card-open")}
    data-channel-id={c?.id} data-agent-id={a?.id} data-agent={a?.name}
    onMouseEnter={a ? () => hoverCard(a.id) : undefined} onMouseLeave={a ? () => leaveCard(a.id) : undefined}
    {...contextMenu(menu)}>
    {editing && c ? <div class="channel-rename"><InlineEdit fields={[{ value: c.name, label: "Channel name" }]}
      onCommit={([n]) => { setEditing(false); A.renameChannel(c.id, n); }} onCancel={() => setEditing(false)} /></div> :
      <button type="button" class="channel-select" aria-current={active ? "page" : undefined}
        aria-label={label}
        aria-haspopup={a ? "dialog" : undefined} aria-expanded={a ? cardOpen : undefined}
        aria-controls={cardOpen ? `agent-card-${a!.id}` : undefined} aria-keyshortcuts={a ? "ArrowRight" : undefined}
        data-tip={a ? undefined :
          `${c!.name} · ${tildify(c!.cwd, meta.value.home)}${c!.branch ? ` · ${c!.branch}` : ""}${attention ? ` · ${attention} need you` : ""}${cache ? ` · ${CACHE_DOT_LABEL[cache]}` : ""}`}
        onClick={open} onDblClick={c ? () => setEditing(true) : undefined}>
        {a ? <AgentDot a={a} /> : <StateDot state={state} cache={cache} />}
        {context ? <span class="channel-label"><span class="channel-name">{name}</span>
          <span class="channel-context">{context}</span></span> :
          <span class="channel-name">{name}{other && <span class="channel-agent"> {other}</span>}</span>}
        {facts && <span class="row-wide row-state">{facts.status}</span>}
        {facts?.cold && <span class="row-wide ap-cold row-cold">cold</span>}
        {chip && <RowChip chip={chip} onClick={() => openCardByKey(a!.id)} />}
        {!c ? null : attention > 0 ? <span class="channel-attention" aria-label={`${attention} need you`}>{attention}</span> :
          <span class="channel-count" aria-label={`${count} tabs`}>{count}</span>}
      </button>}
    <button type="button" class="ap-icon-btn channel-more" aria-label={`Menu for ${name}`} aria-haspopup="menu"
      onClick={(e) => openMenuAt(e.currentTarget, menu(), { alignRight: false, minWidth: a ? 200 : undefined })}><IconMore size={12} /></button>
    {cardOpen && <AgentCard a={a!} channel={c} tabs={count} menuItems={menu} anchor={ref} now={now} />}
  </div>;
}

/** The row's chip: amber while a schedule waits for approval, red while a run failed (always
 * shown); otherwise a clock with the schedule count (wide sidebar only). */
function RowChip({ chip, onClick }: { chip: Chip; onClick: () => void }) {
  return (
    <span class={cx("row-chip", `row-chip-${chip.tone}`, chip.tone === "plain" && "row-wide")} data-tip={chip.label}
      aria-hidden="true"
      onClick={(e) => {
        // The chip sits in the row's button: it opens the card instead of the channel.
        e.stopPropagation();
        onClick();
      }}>
      {chip.tone === "bad" ? <IconShieldAlert size={10} /> : chip.tone === "wait" ? <IconSpark size={10} /> : <IconClock size={10} />}
      {chip.count}
    </span>
  );
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

// A named agent in the sidebar (docs/UI.md "Agents in the sidebar"): its row's dot and status,
// its one menu (the agent's actions, then its channel's), and the card that opens beside the row
// on hover or with → on the focused row: state, tags, quick actions, its schedules and folder.
import { signal } from "@preact/signals";
import { useEffect, useLayoutEffect, useRef } from "preact/hooks";
import {
  accountProfiles,
  cacheNow,
  meta,
  scheduleSettings,
  schedules,
  schedulesSupported,
  sessions,
  windowPrefs,
} from "../state.ts";
import * as A from "../actions.ts";
import { agentBilling, agentStatus, ago, firstLine, OPENROUTER_BADGE, pendingMove } from "../agents.ts";
import { cacheDot, CACHE_DOT_LABEL, idleText, isCold, wakeCold } from "../cache.ts";
import { sortSchedules } from "../schedules.ts";
import { cardKey } from "../sidebar.ts";
import type { Agent, Channel } from "../types.ts";
import { KIND_LABEL } from "../types.ts";
import { cx, tildify } from "../util.ts";
import { SessionDot, StateDot } from "./common.tsx";
import { menu, openMenu, openMenuAt, type MenuItem } from "./Menu.tsx";
import { ScheduleList } from "./Schedules.tsx";
import { IconMore } from "../icons.tsx";

/** The open card: whose, and whether the keyboard opened it (focus is in it; leaving the row
 * with the pointer doesn't close it). One at a time. */
export const agentCard = signal<{ agent: number; byKey: boolean } | null>(null);

/** The agent's live facts, as its row and card show them. */
export function agentFacts(a: Agent, now: number) {
  const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
  const st = agentStatus(a, s);
  // Asleep: its next message re-reads the conversation uncached once the cache has expired.
  const cold = !!s && (st.asleep ? wakeCold(s, cacheNow()) : isCold(s, cacheNow()));
  const dot = s && st.live ? cacheDot(s, cacheNow(), windowPrefs.value?.cache_warn !== false) : null;
  const time = st.since !== null ? ago(st.since, now) : null;
  return { s, st, cold, dot, status: time ? `${st.label} · ${time}` : st.label };
}

/** Today's agent dot: its session's state and cache colour, half filled asleep, hollow stopped. */
export function AgentDot({ a }: { a: Agent }) {
  const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
  const st = agentStatus(a, s);
  return s && st.live ? <SessionDot s={s} /> : <StateDot state={st.state} class={st.asleep ? "dot-asleep" : undefined} />;
}

/** The agent's menu items (Edit… to Remove…): a row's menu starts with these. */
export function agentMenuItems(a: Agent): MenuItem[] {
  const live = A.agentLive(a);
  const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
  const asleep = agentStatus(a, s).asleep;
  const cold = asleep && wakeCold(s, cacheNow());
  return [
    { label: "Edit…", run: () => A.editAgent(a.name) },
    { label: "Memory…", run: () => A.editAgent(a.name, "memory") },
    ...(A.canSwitchAccount(a) ? [{ label: "Switch account…", run: () => A.switchAgentAccount(a.name) }] : []),
    ...(schedulesSupported.value ? [{ label: "New schedule…", run: () => A.newScheduleDialog(a.name) }] : []),
    ...A.pendingMoveItems(a),
    "sep",
    ...(asleep
      ? [
        { label: "Show", run: () => A.openAgent(a.name) },
        { label: "Wake", hint: cold ? "re-reads it all" : undefined, run: () => A.wakeAgent(a) },
      ]
      : [{ label: live ? "Show" : "Start", run: () => A.openAgent(a.name) }]),
    {
      label: "Start fresh",
      hint: a.kind === "claude" ? "new conversation" : undefined,
      run: () => A.openAgent(a.name, true),
    },
    // Sleep needs a saved conversation to wake into; the daemon says why otherwise.
    ...(asleep ? [] : [{ label: "Sleep", disabled: !live || !a.resumable, run: () => A.sleepAgent(a) }]),
    { label: "Stop", disabled: !live, run: () => A.stopAgent(a) },
    { label: "Autostart", checked: a.autostart, run: () => A.setAgentAutostart(a, !a.autostart) },
    "sep",
    { label: "Remove…", danger: true, run: () => A.removeAgent(a) },
  ];
}

/** One menu for an agent's row: a heading with its names, its own items, then its channel's. */
export function rowMenuItems(a: Agent, channel: Channel | null, channelItems: MenuItem[]): MenuItem[] {
  const title = channel && channel.name !== a.name ? `${channel.name} · agent ${a.name}` : `Agent ${a.name}`;
  return [
    { header: title },
    ...agentMenuItems(a),
    ...(channelItems.length ? ["sep" as const, { header: channel ? "Channel" : "Sidebar" }, ...channelItems] : []),
  ];
}

/** The row's explanation of a waiting move (its tooltip, and its screen-reader description). */
function moveTip(move: NonNullable<ReturnType<typeof pendingMove>>): string {
  if (!move.running) return move.off ? `${move.text}. ${move.why}` : `${move.text}: a new conversation there.`;
  const why = move.restart ? "Restart now is in its menu." : move.why;
  return `${move.text}. Until then it keeps running on ${move.from}. ${why}`;
}

/** Click on an agent's row: its channel, as any channel; an agent that isn't running starts (as
 * Show does), and one whose tab was closed or is in another window comes here. */
export function openAgentRow(a: Agent, channel: Channel | null, here: ReadonlySet<number>) {
  const s = a.session_id !== null ? sessions.peek().get(a.session_id) : undefined;
  const st = agentStatus(a, s);
  if (channel && a.session_id !== null && here.has(a.session_id) && (st.live || st.asleep)) {
    return A.openChannel(channel.id);
  }
  A.openAgent(a.name);
}

const OPEN_MS = 350;
const SWITCH_MS = 120;
const CLOSE_MS = 280;
let openTimer = 0;
let closeTimer = 0;

/** Pointer over a row or its card: open after a short pause (at once when another card is open). */
export function hoverCard(agent: number) {
  clearTimeout(closeTimer);
  const cur = agentCard.peek();
  if (cur?.agent === agent) return;
  clearTimeout(openTimer);
  // A card the keyboard opened stays until the keyboard (or a click elsewhere) closes it.
  if (cur?.byKey) return;
  openTimer = setTimeout(() => (agentCard.value = { agent, byKey: false }), cur ? SWITCH_MS : OPEN_MS);
}

/** Pointer left the row and its card: close soon, unless the keyboard opened it or one of its
 * menus is open (the pointer is in the menu). */
export function leaveCard(agent: number) {
  clearTimeout(openTimer);
  clearTimeout(closeTimer);
  const check = () => {
    const cur = agentCard.peek();
    if (!cur || cur.agent !== agent || cur.byKey) return;
    if (menu.peek()) {
      closeTimer = setTimeout(check, CLOSE_MS);
      return;
    }
    agentCard.value = null;
  };
  closeTimer = setTimeout(check, CLOSE_MS);
}

export function closeCard(focusRow = false) {
  const cur = agentCard.peek();
  clearTimeout(openTimer);
  clearTimeout(closeTimer);
  if (!cur) return;
  agentCard.value = null;
  if (focusRow) rowButton(cur.agent)?.focus();
}

/** Open an agent's card from the keyboard (→ on its row, or a Needs you item): focus goes in. */
export function openCardByKey(agent: number) {
  clearTimeout(openTimer);
  clearTimeout(closeTimer);
  const row = rowButton(agent);
  row?.scrollIntoView({ block: "nearest" });
  agentCard.value = { agent, byKey: true };
}

function rowButton(agent: number) {
  return document.querySelector<HTMLElement>(`.channel-row[data-agent-id="${agent}"] .channel-select`);
}

/** The card beside an agent's row. `anchor` is the row it belongs to. */
export function AgentCard(
  { a, channel, tabs, menuItems, anchor, now }: {
    a: Agent;
    channel: Channel | null;
    tabs: number;
    menuItems: () => MenuItem[];
    anchor: { current: HTMLElement | null };
    now: number;
  },
) {
  const ref = useRef<HTMLDivElement>(null);
  const byKey = agentCard.value?.byKey ?? false;
  const { s, st, cold, dot, status } = agentFacts(a, now);
  const billing = agentBilling(a, s);
  const spent = billing.badge ? scheduleSettings.value?.spend?.agents.find((x) => x.id === a.id)?.spent : undefined;
  const move = pendingMove(a, s, accountProfiles.value);
  const persona = firstLine(a.persona);
  const list = schedulesSupported.value ? sortSchedules(schedules.value.filter((x) => x.agent_id === a.id)) : [];
  const live = A.agentLive(a);
  const folder = tildify(channel?.cwd ?? a.home_dir, meta.value.home);
  const id = `agent-card-${a.id}`;

  // Beside the row, inside the window: below the row's top, moved up when it would run off.
  useLayoutEffect(() => {
    const el = ref.current, row = anchor.current;
    if (!el || !row) return;
    const place = () => {
      const r = row.getBoundingClientRect();
      const side = row.closest(".sidebar")?.getBoundingClientRect();
      el.style.left = `${Math.round((side?.right ?? r.right) + 6)}px`;
      const h = el.offsetHeight;
      el.style.top = `${Math.round(Math.max(8, Math.min(r.top - 6, innerHeight - h - 8)))}px`;
    };
    place();
    const ro = new ResizeObserver(place);
    ro.observe(el);
    return () => ro.disconnect();
  });

  // Opened by the keyboard: focus its first button.
  useEffect(() => {
    if (byKey) ref.current?.querySelector<HTMLElement>("button")?.focus();
  }, [byKey]);

  // The row scrolled, the window lost focus or was resized: the card would float away; close it.
  useEffect(() => {
    const scroller = anchor.current?.closest(".channels-scroll");
    const close = () => closeCard();
    scroller?.addEventListener("scroll", close, { passive: true });
    addEventListener("resize", close);
    return () => {
      scroller?.removeEventListener("scroll", close);
      removeEventListener("resize", close);
    };
  }, []);

  const more = (e: Event) => openMenuAt(e.currentTarget as Element, menuItems(), { minWidth: 200, alignRight: false });
  const showLabel = st.asleep || live ? "Show" : "Start";
  return (
    <div
      ref={ref}
      id={id}
      class="agent-card"
      role="dialog"
      aria-label={`${a.name}: state, actions and schedules`}
      onKeyDown={(e) => {
        const t = e.target as HTMLElement;
        const act = cardKey(e.key, "card", t.tagName === "BUTTON");
        if (act === "close") {
          e.preventDefault();
          e.stopPropagation();
          closeCard(true);
        }
      }}
      onFocusOut={(e) => {
        // Focus left the card and its row (Tab past its last button, a click elsewhere): close.
        // A dialog or menu it opened (a run's message, Edit…) keeps it, so focus comes back to it.
        const to = e.relatedTarget as HTMLElement | null;
        if (!to || ref.current?.contains(to) || anchor.current?.contains(to) || menu.peek()) return;
        if (to.closest?.(".modal-backdrop, .menu-layer")) return;
        if (agentCard.peek()?.byKey) closeCard();
      }}
      onContextMenu={(e) => {
        // The card's own rows have their menus; empty space gets the row's.
        if ((e.target as HTMLElement).closest(".sch-row, .sch-prop, button, pre")) return;
        e.preventDefault();
        e.stopPropagation();
        openMenu({ x: e.clientX, y: e.clientY, items: menuItems(), minWidth: 200 });
      }}
    >
      <div class="ac-head">
        <AgentDot a={a} />
        <span class="ac-name">{channel?.name ?? a.name}</span>
        {channel && channel.name !== a.name && <span class="ac-agent">agent {a.name}</span>}
        <span class={cx("ap-kind", "ap-kind-" + a.kind)}>{KIND_LABEL[a.kind]}</span>
      </div>
      <div class="ac-status">
        <span class="ac-state">{status}</span>
        {dot && !cold && <span class="sr-only">, {CACHE_DOT_LABEL[dot]}</span>}
        {a.lead && <span class="ap-auto" data-tip="Lead agent: what it sends from a reminder or a schedule counts as yours">lead</span>}
        {a.autostart && <span class="ap-auto" data-tip="Starts with the daemon">auto</span>}
        {a.sleep_after_min != null && !st.asleep && (
          <span class="ap-auto" data-tip={`Sleeps after ${idleText(a.sleep_after_min * 60_000)} idle; the next message sent with wake on wakes it`}>
            sleeps
          </span>
        )}
        {cold && (
          <span
            class="ap-cold"
            data-tip={st.asleep
              ? "Its prompt cache has likely expired: waking it re-reads the whole conversation at full cost"
              : "Its prompt cache has likely expired: the next message re-reads the whole conversation"}
          >
            cold
          </span>
        )}
      </div>
      {move && <div class="ap-move ac-line" data-tip={moveTip(move)}>{move.text}<span class="sr-only">. {moveTip(move)}</span></div>}
      {billing.badge && (
        <div class="ac-line">
          <span
            class={cx("or-badge", billing.note && "or-badge-pending")}
            data-tip={billing.note ?? "Billed per token to your OpenRouter key, not a subscription"}
          >
            {billing.label ?? OPENROUTER_BADGE}
            {spent !== undefined && <span class="or-spent"> · ~${spent.toFixed(2)} today (est.)</span>}
          </span>
        </div>
      )}
      {persona && <div class="ac-persona">{persona}</div>}
      <div class="ac-actions">
        <button type="button" class="btn btn-sm" onClick={() => A.openAgent(a.name)}>{showLabel}</button>
        {channel && (
          <button type="button" class="btn btn-sm" onClick={() => { closeCard(); A.openChannel(channel.id, false); A.newTabHere(); }}>
            New tab
          </button>
        )}
        <button type="button" class="btn btn-sm" onClick={() => { closeCard(); A.editAgent(a.name, "memory"); }}>Memory…</button>
        <button type="button" class="btn btn-sm" onClick={() => { closeCard(); A.openAgent(a.name, true); }}>Start fresh</button>
        <button type="button" class="btn btn-sm ac-more" aria-label={`More for ${a.name}`} aria-haspopup="menu" onClick={more}>
          <IconMore size={13} />
        </button>
      </div>
      {list.length > 0 && (
        <section class="ac-section" aria-label={`Schedules of ${a.name}`}>
          <div class="ac-section-head">Schedules <span class="ap-count">{list.length}</span></div>
          <ScheduleList list={list} now={now} label={`Schedules of ${a.name}`} />
        </section>
      )}
      <div class="ac-section ac-folder">
        <div class="ac-section-head">
          {channel ? <>Channel <span class="ap-count">{tabs === 1 ? "1 tab" : `${tabs} tabs`}</span></> : "Folder"}
        </div>
        <div class="ac-path" data-tip={folder}>{folder}</div>
      </div>
    </div>
  );
}

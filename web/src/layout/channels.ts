// A window contains independently tiled channels. The split-tree reducers remain unchanged;
// this envelope owns channel selection and the shared sidebar. Zero is the Notes fallback for
// old, unassigned notes in a window with no channels (never a backend channel id).
import {
  allPanes, allSessions, emptyLayout, findPane, type LayoutState, parseLayout,
  placeBeside, reconcile,
} from "./tree.ts";
import type { AccountProfile, SessionInfo } from "../types.ts";

export type ChannelGrouping = "directory" | "account";

/** Group the current window's sessions by the account their running process uses. */
export function accountSessionGroups(sessions: SessionInfo[], profiles: AccountProfile[]) {
  const groups = new Map<string, { key: string; label: string; sessions: SessionInfo[] }>();
  for (const session of sessions) {
    const profile = session.kind === "claude" ? session.account_profile_id : null;
    const routed = session.kind !== "shell" && session.provider === "openrouter";
    const key = routed ? `${session.kind}:openrouter` : profile == null ? session.kind : `claude:${profile}`;
    const label = routed ? `${session.kind} · OpenRouter` : profile == null ? session.kind :
      profiles.find((p) => p.id === profile)?.name ?? `claude · Unavailable profile #${profile}`;
    if (!groups.has(key)) groups.set(key, { key, label, sessions: [] });
    groups.get(key)!.sessions.push(session);
  }
  return [...groups.values()].sort((a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key));
}

export interface TabHome {
  id: number;
  channel: number | null;
  beside?: number | null;
}

export interface SidebarState {
  hidden: boolean;
  channels: boolean;
  agents: boolean;
  split: number;
  collapsedProjects: number[];
  groupBy: ChannelGrouping;
  collapsedAccounts: string[];
}

export interface WindowLayoutState {
  v: 2;
  active: number | null;
  layouts: Record<string, LayoutState>;
  sidebar: SidebarState;
  /** The untouched pre-channel layout, retained for recovery. Never rendered or reconciled. */
  previous?: unknown;
}

export function defaultSidebar(): SidebarState {
  return { hidden: false, channels: true, agents: true, split: 0.45, collapsedProjects: [],
    groupBy: "directory", collapsedAccounts: [] };
}

export function sidebarRatio(value: number): number {
  return Number.isFinite(value) ? Math.max(0.2, Math.min(0.8, value)) : 0.45;
}

/** Find the saved home of a tab; useful for notes predating channel membership. */
export function savedChannel(s: WindowLayoutState, tab: number): number | null {
  for (const [id, l] of Object.entries(s.layouts)) {
    if (allSessions(l.root).includes(tab)) return Number(id);
  }
  return null;
}

function reconcileChannel(s: LayoutState, tabs: TabHome[]): LayoutState {
  const ids = tabs.map((t) => t.id);
  const existing = allSessions(s.root).filter((id) => ids.includes(id));
  // Existing panels/notes keep their places; new ones go beside their opener, when present.
  let next = reconcile(s, [...new Set([...tabs.filter((t) => t.beside == null).map((t) => t.id), ...existing])]);
  for (const t of tabs) if (t.beside != null) next = placeBeside(next, t.id, t.beside);
  return next;
}

/** Add arrivals to their own channel, prune departed tabs, and retain empty real channels. */
export function reconcileChannels(s: WindowLayoutState, channels: number[], tabs: TabHome[]): WindowLayoutState {
  const ids = new Set(channels);
  const groups = new Map<number, TabHome[]>(channels.map((id) => [id, []]));
  for (const t of tabs) {
    const saved = t.channel === null ? savedChannel(s, t.id) : null;
    const beside = t.beside == null ? null : tabs.find((x) => x.id === t.beside)?.channel;
    const channel = t.channel ?? beside ?? (saved !== null && ids.has(saved) ? saved : null) ??
      (s.active !== null && ids.has(s.active) ? s.active : channels[0]) ?? 0;
    if (!groups.has(channel)) groups.set(channel, []);
    groups.get(channel)!.push(t);
  }
  const layouts: Record<string, LayoutState> = {};
  for (const [id, items] of groups) {
    layouts[id] = reconcileChannel(s.layouts[id] ?? emptyLayout(), items);
  }
  const active = s.active !== null && Object.hasOwn(layouts, s.active) ? s.active :
    (groups.keys().next().value ?? null);
  return { ...s, active, layouts };
}

/** Read v2, or partition a legacy window tree without merging tasks with the same folder. */
export function loadChannelLayout(raw: unknown, channels: number[], tabs: TabHome[]): WindowLayoutState {
  const obj = raw && typeof raw === "object" ? raw as Record<string, unknown> : {};
  if (obj.v === 2) {
    const layouts: Record<string, LayoutState> = {};
    if (obj.layouts && typeof obj.layouts === "object") {
      for (const [key, value] of Object.entries(obj.layouts)) {
        if (!/^\d+$/.test(key) || !Number.isSafeInteger(Number(key))) continue;
        const l = parseLayout(value);
        if (l) layouts[key] = l;
      }
    }
    const side = obj.sidebar && typeof obj.sidebar === "object" ? obj.sidebar as Partial<SidebarState> : {};
    return reconcileChannels({
      v: 2,
      active: typeof obj.active === "number" && Number.isSafeInteger(obj.active) ? obj.active : null,
      layouts,
      sidebar: {
        hidden: side.hidden === true,
        channels: side.channels !== false,
        agents: side.agents !== false,
        split: sidebarRatio(Number(side.split)),
        collapsedProjects: Array.isArray(side.collapsedProjects)
          ? side.collapsedProjects.filter((id) => Number.isSafeInteger(id)) : [],
        groupBy: side.groupBy === "account" ? "account" : "directory",
        collapsedAccounts: Array.isArray(side.collapsedAccounts)
          ? side.collapsedAccounts.filter((id) => typeof id === "string") : [],
      },
      ...(obj.previous !== undefined ? { previous: obj.previous } : {}),
    }, channels, tabs);
  }
  const legacy = parseLayout(raw);
  const owners = new Map(tabs.filter((t) => t.channel !== null).map((t) => [t.id, t.channel!]));
  const focused = legacy ? findPane(legacy.root, legacy.focus)?.active : null;
  const active = focused == null ? channels[0] ?? null : owners.get(focused) ?? channels[0] ?? null;
  // Old notes have no channel. Prefer their opener, then a terminal/panel in the same tile,
  // then the focused channel. Save their chosen home in the migrated tree.
  const assigned = tabs.map((t) => {
    if (t.channel !== null) return t;
    const p = legacy ? allPanes(legacy.root).find((p) => p.tabs.includes(t.id)) : null;
    const channel = (t.beside == null ? null : owners.get(t.beside)) ??
      p?.tabs.map((id) => owners.get(id)).find((id) => id !== undefined) ?? active ?? 0;
    return { ...t, channel };
  });
  const ids = [...new Set([...channels, ...assigned.map((t) => t.channel!)])];
  const layouts: Record<string, LayoutState> = {};
  for (const id of ids) {
    layouts[id] = legacy ? reconcile(legacy, assigned.filter((t) => t.channel === id).map((t) => t.id)) : emptyLayout();
  }
  return reconcileChannels({
    v: 2, active: assigned.find((t) => t.id === focused)?.channel ?? active,
    layouts, sidebar: { ...defaultSidebar(), agents: legacy ? !!legacy.agents : true },
    ...(raw != null ? { previous: raw } : {}),
  }, channels, assigned);
}

/** Update an explicitly captured destination, even if the user switched while an RPC ran. */
export function changeChannelLayout(
  s: WindowLayoutState,
  channel: number,
  fn: (l: LayoutState) => LayoutState,
): WindowLayoutState {
  return { ...s, layouts: { ...s.layouts, [channel]: fn(s.layouts[channel] ?? emptyLayout()) } };
}

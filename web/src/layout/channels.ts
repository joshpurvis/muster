// A window contains independently tiled channels. The split-tree reducers remain unchanged;
// this envelope owns channel selection and the shared sidebar. Zero is the Notes fallback for
// old, unassigned notes in a window with no channels (never a backend channel id).
import { addonTab, PRS_TAB } from "./tree.ts";
import {
  allPanes, allSessions, emptyLayout, FIRST_PLACE, findPane, generalPlace, type LayoutState, parseLayout,
  parseTabPlace, placeBeside, placeOf, putAt, QUESTIONS_TAB, reconcile, removeSession, sameGeneralPlace,
  type TabPlace,
} from "./tree.ts";
import { type Channel, DEFAULT_PERMISSIONS, type Permissions, storedPermissions } from "../types.ts";

const trimSlash = (p: string) => p.replace(/\/+$/, "") || "/";
const samePermissions = (a: Permissions, b: Permissions) =>
  a.codex_sandbox === b.codex_sandbox && a.claude_mode === b.claude_mode && a.antigravity_mode === b.antigravity_mode;

export interface FolderJoin {
  /** The dialog's name; blank matches any channel in the folder. */
  name?: string;
  /** The permissions the new session would get; omitted means the defaults. */
  permissions?: Permissions;
  /** Skip the permission check (to explain why no channel was joined). */
  anyPermissions?: boolean;
  /** Preferred among several matches (the selected channel). */
  prefer?: number | null;
  /** Whether a channel has sessions: a failed start in a channel without any archives it. */
  live?: (id: number) => boolean;
}

/**
 * The window's channel in folder `path` that a New channel request without a new worktree joins
 * as a tab, instead of becoming a second channel for the same folder. Sessions on different
 * Claude accounts then share one channel's tabs and layout. A different explicit name, or
 * permissions other than the channel's own (they belong to the channel, so joining would
 * silently change them), still make a separate channel.
 */
export function folderChannel(list: Channel[], path: string, o: FolderJoin = {}): Channel | undefined {
  const want = trimSlash(path);
  const named = o.name?.trim();
  const perms = o.permissions ?? DEFAULT_PERMISSIONS;
  const matches = list.filter((c) => !c.archived && trimSlash(c.cwd) === want &&
    (!named || named === c.name) && (o.live?.(c.id) ?? true) &&
    (o.anyPermissions || samePermissions(perms, storedPermissions(c.permissions))));
  return matches.find((c) => c.id === o.prefer) ?? matches[0];
}

export interface TabHome {
  id: number;
  channel: number | null;
  beside?: number | null;
  /** The window's Questions tab: it arrives in a channel at that channel's place
   * (`questionsIn`), else at the window's `questionsAt`, then stays wherever it is moved. */
  questions?: boolean;
  pinned?: string;
  /** Only keep it where the saved layout has it, never place it anew (loading, before the window
   * knows whether to show it: its group survives the load). */
  kept?: boolean;
}

export interface SidebarState {
  hidden: boolean;
  channels: boolean;
  /** The dock's width in CSS pixels (drag its right edge). Wider rows show more of each agent. */
  width: number;
}

export interface TerminalDrawerState {
  session: number | null;
  open: boolean;
  height: number;
}

export function drawerHeight(value: number): number {
  return Number.isFinite(value) ? Math.max(140, Math.min(700, value)) : 260;
}

export interface PinnedPlace {
  tab: number;
  hidden?: true;
  at?: TabPlace;
  in?: Record<string, TabPlace>;
}
export interface WindowLayoutState {
  v: 2;
  active: number | null;
  layouts: Record<string, LayoutState>;
  sidebar: SidebarState;
  drawers?: Record<string, TerminalDrawerState>;
  /** The Questions tab is hidden in this window (absent: shown). */
  questions?: false;
  pinned?: Record<string, PinnedPlace>;
  /** Where the Questions tab was in each channel (by channel id), taken when it is moved there and
   * when it leaves; it comes back there in that channel. */
  questionsIn?: Record<string, TabPlace>;
  /** Where the human last put the Questions tab, in any channel: where it goes in a channel that
   * has no place of its own yet (absent: first in the first group). Saves from before
   * `questionsIn` have only this. */
  questionsAt?: TabPlace;
  /** The untouched pre-channel layout, retained for recovery. Never rendered or reconciled. */
  previous?: unknown;
}

export function defaultSidebar(): SidebarState {
  return { hidden: false, channels: true, width: SIDEBAR_WIDTH };
}

/** The sidebar's default, narrowest and widest widths. */
export const SIDEBAR_WIDTH = 240;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 420;

export function sidebarWidth(value: number): number {
  return Number.isFinite(value) ? Math.round(Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, value))) : SIDEBAR_WIDTH;
}

/** The channel a tab that follows the shown channel (Questions, an add-on tab) belongs in: the
 * shown one when it is one of the window's channels, or the unfiled note tabs (channel 0, the
 * Notes button's "Show unfiled note tabs") while there are some (`unfiled`: else that workspace
 * closes, and the tab must not keep it open); otherwise none. */
export function followingChannel(
  active: number | null | undefined,
  known: (id: number) => boolean,
  unfiled: boolean,
): number | null {
  if (active === 0) return unfiled ? 0 : null;
  return active != null && known(active) ? active : null;
}

/** Find the saved home of a tab; useful for notes predating channel membership. */
export function savedChannel(s: WindowLayoutState, tab: number): number | null {
  for (const [id, l] of Object.entries(s.layouts)) {
    if (allSessions(l.root).includes(tab)) return Number(id);
  }
  return null;
}

function reconcileChannel(s: LayoutState, tabs: TabHome[], places: Map<number, TabPlace>): LayoutState {
  const ids = tabs.map((t) => t.id);
  const existing = allSessions(s.root).filter((id) => ids.includes(id));
  // Existing panels/notes keep their places; new ones go beside their opener, when present.
  let next = reconcile(s, [...new Set([...tabs.filter((t) => t.beside == null).map((t) => t.id), ...existing])]);
  for (const t of tabs) if (t.beside != null) next = placeBeside(next, t.id, t.beside);
  // Arriving (another channel was shown, it was hidden, or its group went): at its place.
  const arrivals = tabs.filter((t) => (t.questions || t.pinned) && !existing.includes(t.id));
  const index = (t: TabHome) => {
    const i = (places.get(t.id) ?? FIRST_PLACE).index;
    return i < 0 ? Infinity : i;
  };
  // Put earlier tabs in first, so inserting another pin cannot displace a restored one.
  for (const t of arrivals.sort((a, b) => index(a) - index(b))) next = putAt(next, t.id, places.get(t.id) ?? FIRST_PLACE);
  return next;
}

/** Add arrivals to their own channel, prune departed tabs, and retain empty real channels. */
export function reconcileChannels(s: WindowLayoutState, channels: number[], tabs: TabHome[]): WindowLayoutState {
  const ids = new Set(channels);
  const drawers: Record<string, TerminalDrawerState> = {};
  const drawerIds = new Set<number>();
  for (const [key, drawer] of Object.entries(s.drawers ?? {})) {
    if (!ids.has(Number(key))) continue;
    const session = tabs.some((t) => t.id === drawer.session && t.channel === Number(key))
      ? drawer.session : null;
    drawers[key] = { ...drawer, session };
    if (session !== null) drawerIds.add(session);
  }
  const groups = new Map<number, TabHome[]>(channels.map((id) => [id, []]));
  for (const t of tabs) {
    if (drawerIds.has(t.id) || (t.pinned && s.pinned?.[t.pinned]?.hidden)) continue;
    const saved = t.channel === null ? savedChannel(s, t.id) : null;
    // Kept (its saved place while the window loads): in a channel, or in the unfiled note tabs.
    if (t.kept && (saved === null || (!ids.has(saved) && saved !== 0))) continue;
    const beside = t.beside == null ? null : tabs.find((x) => x.id === t.beside)?.channel;
    const home = saved !== null && (ids.has(saved) || (t.kept && saved === 0)) ? saved : null;
    const channel = t.channel ?? beside ?? home ??
      (s.active !== null && ids.has(s.active) ? s.active : channels[0]) ?? 0;
    if (!groups.has(channel)) groups.set(channel, []);
    groups.get(channel)!.push(t);
  }
  // Questions retains its old saved fields. All pinned tabs use the same placement rules.
  const pins: Record<string, PinnedPlace> = { ...s.pinned,
    questions: { tab: QUESTIONS_TAB, at: s.questionsAt, in: s.questionsIn } };
  for (const t of tabs) if (t.pinned && !pins[t.pinned]) pins[t.pinned] = { tab: t.id };
  for (const [key, pin] of Object.entries(pins)) {
    const fallback = pin.at ? generalPlace(pin.at) : pin.tab === QUESTIONS_TAB ? FIRST_PLACE : { group: 0, index: -1 };
    const within: Record<string, TabPlace> = {};
    for (const [id, at] of Object.entries(pin.in ?? {})) if (groups.has(Number(id))) within[id] = at;
    for (const [id, l] of Object.entries(s.layouts)) {
      if (!groups.has(Number(id)) || groups.get(Number(id))!.some((t) => t.id === pin.tab)) continue;
      const at = placeOf(l, pin.tab);
      const given = at && !within[id] ? arrivedAt(l, fallback, pin.tab) : null;
      if (at && (within[id] || !given || !sameGeneralPlace(at, given))) within[id] = at;
    }
    const { in: _, ...rest } = pin;
    pins[key] = { ...rest, ...(Object.keys(within).length ? { in: within } : {}) };
  }
  const layouts: Record<string, LayoutState> = {};
  for (const [id, items] of groups) {
    const places = new Map(Object.values(pins).map((p) => [p.tab, p.in?.[id] ?? (p.at ? generalPlace(p.at) : p.tab === QUESTIONS_TAB ? FIRST_PLACE : { group: 0, index: -1 })]));
    layouts[id] = reconcileChannel(s.layouts[id] ?? emptyLayout(), items.filter((t) => !t.pinned || !pins[t.pinned]?.hidden), places);
  }
  const { questions, ...pinned } = pins;
  const questionsIn = questions.in ?? {};
  const active = s.active !== null && Object.hasOwn(layouts, s.active) ? s.active :
    (groups.keys().next().value ?? null);
  const { questionsIn: _, pinned: _pins, ...rest } = s;
  return {
    ...rest, active, layouts, ...(s.drawers ? { drawers } : {}),
    ...(Object.keys(pinned).length ? { pinned } : {}),
    ...(Object.keys(questionsIn).length ? { questionsIn } : {}),
  };
}

/** Where Questions would be in layout `l` if it arrived there now at `place` (its own group, if it
 * has one there, closes first, as it does when it leaves). */
function arrivedAt(l: LayoutState, place: TabPlace, tab = QUESTIONS_TAB): TabPlace | null {
  const away = removeSession(l, tab);
  const back = reconcile(away, [...allSessions(away.root), tab]);
  return placeOf(putAt(back, tab, place), tab);
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
    const drawers: Record<string, TerminalDrawerState> = {};
    if (obj.drawers && typeof obj.drawers === "object") {
      for (const [key, value] of Object.entries(obj.drawers)) {
        if (!/^\d+$/.test(key) || !value || typeof value !== "object") continue;
        const d = value as Partial<TerminalDrawerState>;
        drawers[key] = {
          session: typeof d.session === "number" && Number.isSafeInteger(d.session) && d.session > 0 ? d.session : null,
          open: d.open === true, height: drawerHeight(Number(d.height)),
        };
      }
    }
    const side = obj.sidebar && typeof obj.sidebar === "object" ? obj.sidebar as Partial<SidebarState> : {};
    const questionsAt = parseTabPlace(obj.questionsAt);
    const questionsIn: Record<string, TabPlace> = {};
    if (obj.questionsIn && typeof obj.questionsIn === "object") {
      for (const [key, value] of Object.entries(obj.questionsIn)) {
        const at = /^\d+$/.test(key) && Number.isSafeInteger(Number(key)) ? parseTabPlace(value) : null;
        if (at) questionsIn[key] = at;
      }
    }
    const pinned: Record<string, PinnedPlace> = {};
    if (obj.pinned && typeof obj.pinned === "object") for (const [key, value] of Object.entries(obj.pinned)) {
      if ((key !== "prs" && !/^addon:[a-z0-9][a-z0-9-]{0,39}$/.test(key)) || !value || typeof value !== "object") continue;
      const p = value as PinnedPlace;
      const tab = key === "prs" ? PRS_TAB : addonTab(key.slice(6));
      const at = parseTabPlace(p.at);
      const within: Record<string, TabPlace> = {};
      if (p.in && typeof p.in === "object") for (const [channel, raw] of Object.entries(p.in)) {
        const place = /^\d+$/.test(channel) && Number.isSafeInteger(Number(channel)) ? parseTabPlace(raw) : null;
        if (place) within[channel] = place;
      }
      pinned[key] = { tab, ...(p.hidden === true ? { hidden: true } : {}),
        ...(at ? { at: generalPlace(at) } : {}), ...(Object.keys(within).length ? { in: within } : {}) };
    }
    return reconcileChannels({
      v: 2,
      ...(Object.keys(pinned).length ? { pinned } : {}),
      active: typeof obj.active === "number" && Number.isSafeInteger(obj.active) ? obj.active : null,
      layouts,
      ...(obj.drawers ? { drawers } : {}),
      ...(obj.questions === false ? { questions: false as const } : {}),
      ...(questionsAt ? { questionsAt: generalPlace(questionsAt) } : {}),
      ...(Object.keys(questionsIn).length ? { questionsIn } : {}),
      sidebar: {
        hidden: side.hidden === true,
        // The list now holds the agents too: a window that showed the old Agents section shows it.
        channels: side.channels !== false || (side as { agents?: unknown }).agents === true,
        width: side.width === undefined ? SIDEBAR_WIDTH : sidebarWidth(Number(side.width)),
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
  // A legacy tree predates the Questions tab: nothing of it to keep.
  const assigned = tabs.filter((t) => !t.kept).map((t) => {
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
    layouts, sidebar: defaultSidebar(),
    ...(raw != null ? { previous: raw } : {}),
  }, channels, assigned);
}

/** Remember where tab `tab` (the Questions tab) now is in the shown channel, after the human
 * moved it: that channel's own place, and the place for channels that have none yet. */
export function rememberPlace(s: WindowLayoutState, tab: number): WindowLayoutState {
  const l = s.active === null ? undefined : s.layouts[s.active];
  const at = l ? placeOf(l, tab) : null;
  if (!at || s.active === null) return s;
  if (tab !== QUESTIONS_TAB) {
    const entry = Object.entries(s.pinned ?? {}).find(([, p]) => p.tab === tab);
    if (!entry) return s;
    const [key, pin] = entry;
    return { ...s, pinned: { ...s.pinned, [key]: { ...pin, in: { ...pin.in, [s.active]: at }, at: generalPlace(at) } } };
  }
  const next = { ...s, questionsIn: { ...s.questionsIn, [s.active]: at }, questionsAt: generalPlace(at) };
  return JSON.stringify(next) === JSON.stringify(s) ? s : next;
}

/** Update an explicitly captured destination, even if the user switched while an RPC ran. */
export function changeChannelLayout(
  s: WindowLayoutState,
  channel: number,
  fn: (l: LayoutState) => LayoutState,
): WindowLayoutState {
  return { ...s, layouts: { ...s.layouts, [channel]: fn(s.layouts[channel] ?? emptyLayout()) } };
}

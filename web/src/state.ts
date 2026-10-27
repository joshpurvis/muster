// App state as signals. Server state is replaced/merged from hello + ServerEvents; this window's
// layout (its split tree of groups) is owned here and saved with window.set_layout; UI prefs
// persist to localStorage.

import { batch, computed, effect, signal } from "@preact/signals";
import type {
  AccountProfile,
  Agent,
  AgentMessage,
  AgentState,
  Channel,
  ChannelId,
  HelloResult,
  Panel,
  Schedule,
  ScheduleRun,
  ScheduleSettings,
  ServerEvent,
  SessionId,
  SessionInfo,
  TimelineEvent,
  Win,
  WindowId,
  WindowPrefs,
  Workspace,
} from "./types.ts";
import { needsHuman } from "./types.ts";
import { rpc } from "./rpc.ts";
import { sessionSizeChanged } from "./terms.ts";
import { applyNoteRemoved, applyNotesHello, applyNoteUpsert, notes, notesSupported, restoreDrafts } from "./notes.ts";
import { debounce, load, save } from "./util.ts";
import { upsertMessage } from "./agents.ts";
import { newerRun, upsertRun } from "./schedules.ts";
import { type Dismissed, releaseOfScript, type RestartWait, restartWait, type UpdateStatus } from "./updates.ts";
import {
  allSessions,
  emptyLayout,
  findPane,
  type LayoutState,
  type PaneNode,
  parseLayout,
  noteId,
  noteTab,
  panelTab,
  panelId,
  placeBeside,
  placePanel,
  reconcile,
} from "./layout/tree.ts";
import {
  confirmAfterFocus,
  confirmAfterUpsert,
  documentTitle,
  needsYouOrder,
  newerPrefs,
  pushFront,
  type RenameFields,
  renameOutlived,
  sessionsOfWindow,
  SessionVersions,
  sessionWindowId,
  upsertWindow,
} from "./model.ts";

// ---------------- server state ----------------

export const helloDone = signal(false);
export const meta = signal({ version: "", home: "", default_shell: "", login_shell: "" });
export const workspaces = signal<Workspace[]>([]);
export const accountProfiles = signal<AccountProfile[]>([]);
export const accountProfilesSupported = signal(false);
/** `hello.openrouter`: null when the backend has no OpenRouter support. */
export const openRouter = signal<{ key_present: boolean } | null>(null);
export const channels = signal<Channel[]>([]);
export const sessions = signal<Map<SessionId, SessionInfo>>(new Map());
export const panels = signal<Map<number, Panel>>(new Map());
export const panelOrigin = signal("");
/** Bumped by every hello (connect and reconnect): panels retry a resource that failed to load. */
export const helloEpoch = signal(0);

/** Every window the daemon knows (all clients). */
export const windows = signal<Win[]>([]);
/** The daemon has windows (hello carried `windows`). Without them every session is a tab here. */
export const windowsSupported = signal(false);
/** The window this client claimed (null until claimed, or without window support). */
export const windowId = signal<WindowId | null>(null);

export const channelById = computed(() => new Map(channels.value.map((c) => [c.id, c])));
export const workspaceById = computed(() => new Map(workspaces.value.map((w) => [w.id, w])));
export const windowById = computed(() => new Map(windows.value.map((w) => [w.id, w])));

/** Sessions per task (channel), oldest first. */
export const sessionsByChannel = computed(() => {
  const m = new Map<ChannelId, SessionInfo[]>();
  for (const s of sessions.value.values()) {
    let arr = m.get(s.channel_id);
    if (!arr) m.set(s.channel_id, arr = []);
    arr.push(s);
  }
  for (const arr of m.values()) arr.sort((a, b) => a.created_at - b.created_at || a.id - b.id);
  return m;
});

export const myWindow = computed(() =>
  windowId.value !== null ? windowById.value.get(windowId.value) ?? null : null
);

/** The sessions shown as tabs in this window (see `sessionsOfWindow`). */
export const windowSessions = computed((): SessionInfo[] => {
  if (!windowsSupported.value) return sessionsOfWindow(null, sessions.value.values(), channels.value);
  const w = myWindow.value;
  return w ? sessionsOfWindow(w, sessions.value.values(), channels.value) : [];
});

export const windowPanels = computed(() => [...panels.value.values()].filter((p) =>
  !p.closed && channelById.value.has(p.channel_id) && (!windowsSupported.value || p.window_id === windowId.value)
));
/** Notes whose tab is in this window. A note whose window is gone is closed. */
export const windowNotes = computed(() => [...notes.value.values()].filter((n) =>
  n.window_id !== null && (windowsSupported.value ? n.window_id === windowId.value : true)
));
export const hereIds = computed(() => new Set([
  ...windowSessions.value.map((s) => s.id), ...windowPanels.value.map((p) => panelTab(p.id)),
  ...windowNotes.value.map((n) => noteTab(n.id)),
]));

// ---------------- updates, shared window prefs, shutdown ----------------

/** Waiting updates (`hello.updates`, `updates_changed`); null from backends without them. */
export const updateStatus = signal<UpdateStatus | null>(null);
/** The UI release this page was loaded from (null: embedded or directly served assets). */
export const loadedRelease = releaseOfScript(
  document.querySelector<HTMLScriptElement>('script[type="module"][src]')?.src ?? "",
);
/** Updates this viewer dismissed (per kind, the last one), until a different one arrives. */
export const dismissedUpdates = signal<Dismissed>(load<Dismissed>("dismissedUpdates", {}));
effect(() => save("dismissedUpdates", dismissedUpdates.value));
/** Preferences every window shares (`prefs.get`); null from backends without them. */
export const windowPrefs = signal<WindowPrefs | null>(null);

// ---------------- cold prompt cache (src/cache.ts) ----------------

/** The clock the cold-cache bar and markers read: ticks every 20 s (they show minutes). */
export const cacheClock = signal(Date.now());
setInterval(() => (cacheClock.value = Date.now()), 20_000);
/** Now, for cold-cache rendering: subscribes to the tick, but never reads a stale time when
 * something else (a session update) re-renders between ticks. */
export function cacheNow(): number {
  return Math.max(cacheClock.value, Date.now());
}
/** Idle periods (`coldKey`) whose bar this viewer dismissed; the next turn's period is new. */
export const coldDismissed = signal<string[]>(
  ((d: unknown) => Array.isArray(d) ? d.filter((k) => typeof k === "string") : [])(load("coldDismissed", [])),
);
effect(() => save("coldDismissed", coldDismissed.value));
/** The backend announced a full shutdown (`daemon_stopping`): this page stops reconnecting. */
export const daemonStopped = signal(false);
/** Why the stopped screen shows: a shutdown, or a restart whose new backend never answered. */
export const stoppedReason = signal<"shutdown" | "restart_failed">("shutdown");
/** The backend announced a restart (`daemon_stopping {reason: "restart"}`): every session ends
 * and a new backend starts by itself; this page keeps reconnecting until a backend that isn't
 * `pid`, on a terminal host that isn't `host`, answers (`restartAnswered`). */
export const daemonRestarting = signal<RestartWait | null>(null);
/** The backend this page last said hello to: its pid and its terminal host's instance (null
 * when the hello didn't say). */
export const lastBackend = signal<RestartWait>({ pid: null, host: null, build: null });
/** A confirmed Restart or Shut down whose request is still out (the restart's checks may take a
 * while): its dialog stays up whatever else asks for the screen, and no shortcut acts, until it is
 * refused or Muster is restarting or stopped. */
export const lifecycleChecking = signal<"restart" | "shutdown" | null>(null);

// ---------------- named agents (docs/UI.md "Agents panel") ----------------

/** Every named agent, by name. Loaded after each hello (`agent.list`), then kept by events. */
export const agents = signal<Agent[]>([]);
/** The daemon knows agents (false: an older daemon answered "unknown method"). */
export const agentsSupported = signal(true);
/** The message log, newest first (`agent.messages`, then `agent_message` events). */
export const agentMessages = signal<AgentMessage[]>([]);
/** The panel's selected agent: the log shows only messages to it. */
export const agentFilter = signal<number | null>(null);

export const agentBySession = computed(() => {
  const m = new Map<SessionId, Agent>();
  for (const a of agents.value) if (a.session_id !== null) m.set(a.session_id, a);
  return m;
});

function sortAgents(list: Agent[]): Agent[] {
  return [...list].sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
}

async function loadAgents() {
  loadSchedules();
  try {
    const [list, log] = await Promise.all([
      rpc.call<Agent[]>("agent.list"),
      rpc.call<{ messages: AgentMessage[] }>("agent.messages", { limit: 100 }),
    ]);
    agentsSupported.value = true;
    agents.value = sortAgents(list);
    agentMessages.value = log.messages;
  } catch (e) {
    if (/^unknown method/i.test(e instanceof Error ? e.message : String(e))) agentsSupported.value = false;
  }
}

// ---------------- schedules (docs/UI.md "Agents panel", docs/PROTOCOL.md "Schedules") ----------------

/** Every schedule (`schedule.list` after each hello, then `schedule_*` events). */
export const schedules = signal<Schedule[]>([]);
/** The backend has schedules (false: an older one answered "unknown method"; all schedule UI hides). */
export const schedulesSupported = signal(false);
/** Pause all, quiet hours, the unattended cap. */
export const scheduleSettings = signal<ScheduleSettings | null>(null);
/** A schedule's history as loaded so far (`schedule.runs`, then `schedule_run` events), newest
 * first. `loading`: a page is on its way; `error`: the last load failed. */
export interface RunHistory {
  runs: ScheduleRun[];
  more: boolean;
  loading: boolean;
  error: string | null;
}
/** History lists, by schedule id. */
export const scheduleRuns = signal<Map<number, RunHistory>>(new Map());
/** Schedules whose history is shown under their row. */
export const scheduleHistoryOpen = signal<ReadonlySet<number>>(new Set());
/** Bumped when histories are thrown away (each hello): loads started before are ignored. */
let runsEpoch = 0;

function setHistory(id: number, h: RunHistory) {
  scheduleRuns.value = new Map(scheduleRuns.value).set(id, h);
}

/** Load a schedule's newest 20 runs, or (`older`) the 20 below the oldest loaded. Runs already
 * there (from events meanwhile) are kept: pages are merged in, never replace. */
export async function loadScheduleRuns(id: number, older = false) {
  const epoch = runsEpoch;
  const cur = scheduleRuns.value.get(id);
  const before = older ? cur?.runs.at(-1)?.id : undefined;
  setHistory(id, { runs: cur?.runs ?? [], more: cur?.more ?? false, loading: true, error: null });
  try {
    const r = await rpc.call<{ runs: ScheduleRun[]; more: boolean }>("schedule.runs", { id, before, limit: 20 });
    if (epoch !== runsEpoch) return;
    let runs = scheduleRuns.value.get(id)?.runs ?? [];
    for (const x of r.runs) runs = upsertRun(runs, x);
    setHistory(id, { runs, more: r.more, loading: false, error: null });
  } catch (e) {
    if (epoch !== runsEpoch) return;
    const h = scheduleRuns.value.get(id);
    setHistory(id, {
      runs: h?.runs ?? [],
      more: h?.more ?? false,
      loading: false,
      error: e instanceof Error ? e.message : String(e),
    });
  }
}

async function loadSchedules() {
  try {
    const r = await rpc.call<{ schedules: Schedule[]; settings: ScheduleSettings }>("schedule.list");
    batch(() => {
      schedulesSupported.value = true;
      schedules.value = r.schedules;
      scheduleSettings.value = r.settings;
      // Histories loaded before a reconnect may have missed runs: thrown away, and the ones on
      // screen load again now (the rest when they are next shown).
      runsEpoch++;
      scheduleRuns.value = new Map();
    });
    const known = new Set(r.schedules.map((s) => s.id));
    for (const id of scheduleHistoryOpen.value) if (known.has(id)) loadScheduleRuns(id);
  } catch (e) {
    if (/^unknown method/i.test(e instanceof Error ? e.message : String(e))) {
      batch(() => {
        schedulesSupported.value = false;
        schedules.value = [];
        scheduleSettings.value = null;
      });
    }
  }
}

/** Record a schedule from an RPC reply (its schedule_upserted event may lag). */
export function knowSchedule(s: Schedule) {
  schedules.value = upsertBy(schedules.value, s);
}

/** Record a run (an event, or a Run now reply): the schedule's last run and any loaded history. */
export function knowRun(r: ScheduleRun) {
  batch(() => {
    const s = schedules.value.find((x) => x.id === r.schedule_id);
    if (s) {
      const last = newerRun(s.last_run, r);
      if (last !== s.last_run) knowSchedule({ ...s, last_run: last });
    }
    // A history being loaded takes it too (the page merges in when it arrives).
    const h = scheduleRuns.value.get(r.schedule_id);
    if (h) setHistory(r.schedule_id, { ...h, runs: upsertRun(h.runs, r) });
  });
}

function forgetSchedule(id: number) {
  batch(() => {
    schedules.value = schedules.value.filter((s) => s.id !== id);
    if (scheduleRuns.value.has(id)) {
      const m = new Map(scheduleRuns.value);
      m.delete(id);
      scheduleRuns.value = m;
    }
    if (scheduleHistoryOpen.value.has(id)) {
      const open = new Set(scheduleHistoryOpen.value);
      open.delete(id);
      scheduleHistoryOpen.value = open;
    }
  });
}

/** The window a session is a tab in (null: none). Without window support: this one. */
export function windowOfSession(s: SessionInfo): WindowId | null {
  if (!windowsSupported.value) return windowId.value;
  return sessionWindowId(s, channelById.value.get(s.channel_id), windows.value);
}

const liveTaskIds = computed(() => new Set(channels.value.filter((c) => !c.archived).map((c) => c.id)));

/** Sessions needing the human in any window, in Alt+J order. */
export const needsYou = computed(() => needsYouOrder(sessions.value.values(), liveTaskIds.value));

/** How many of those are tabs of this window. */
export const needsYouHere = computed(() => needsYou.value.filter((s) => hereIds.value.has(s.id)).length);

// ---------------- unread ----------------

/** Latest "notable" event per task (what makes its tabs bold/unread). */
export const latestNotable = signal<Map<ChannelId, number>>(new Map());

export function isNotable(e: TimelineEvent): boolean {
  switch (e.kind) {
    case "agent_post":
    case "session_exited":
      return true;
    case "state_change": {
      const to = e.data?.to as AgentState | undefined;
      const from = e.data?.from as AgentState | undefined;
      return !!to && (needsHuman(to) || (to === "idle" && from === "working"));
    }
    case "system":
      return e.data?.level === "warn" || e.data?.level === "error";
    case "prompt":
      // Prompts from agents (MCP) are notable; the human's own are not.
      return !!e.data?.from && !["human", "user", "ui"].includes(String(e.data.from));
    default:
      return false;
  }
}

function noteNotable(e: TimelineEvent) {
  if (e.channel_id === null || !isNotable(e)) return;
  const cur = latestNotable.value.get(e.channel_id) ?? 0;
  if (e.ts > cur) {
    const m = new Map(latestNotable.value);
    m.set(e.channel_id, e.ts);
    latestNotable.value = m;
  }
}

export const unread = computed(() => {
  const out = new Set<ChannelId>();
  for (const c of channels.value) {
    const t = latestNotable.value.get(c.id) ?? 0;
    if (t > c.last_read_at) out.add(c.id);
  }
  return out;
});

// ---------------- prefs ----------------

export interface Prefs {
  fontSize: number;
  theme: "system" | "dark" | "light";
  lastAgent: "claude" | "codex" | "shell" | "none";
  /** Jump recency keys ("s:<id>", "f:<path>"), most recent first. */
  recent: string[];
  /** Folders new tabs were opened in, most recent first. */
  recentFolders: string[];
  statusLine: boolean;
  /** Pass Alt shortcuts (except Alt+Shift+…) through to a focused terminal. */
  altToTerminal: boolean;
  /** Jump / Alt+J focus the other window (window.raise) instead of pulling its tab here. */
  jumpRaises: boolean;
  /** The agents panel's message log is expanded. */
  agentFeed: boolean;
}

const defaultPrefs: Prefs = {
  fontSize: 13,
  theme: "system",
  lastAgent: "claude",
  recent: [],
  recentFolders: [],
  statusLine: true,
  altToTerminal: false,
  jumpRaises: false,
  agentFeed: true,
};

function loadPrefs(): Prefs {
  const raw = load<Partial<Prefs>>("prefs", {});
  const out: Prefs = { ...defaultPrefs };
  for (const k of Object.keys(defaultPrefs) as (keyof Prefs)[]) {
    const v = raw?.[k];
    // Only keep values of the expected type (old builds stored other shapes).
    if (
      v !== undefined && typeof v === typeof defaultPrefs[k] &&
      Array.isArray(v) === Array.isArray(defaultPrefs[k])
    ) {
      (out as unknown as Record<string, unknown>)[k] = v;
    }
  }
  return out;
}

export const prefs = signal<Prefs>(loadPrefs());
const savePrefs = debounce((p: Prefs) => save("prefs", p), 200);
effect(() => savePrefs(prefs.value));

export function setPref<K extends keyof Prefs>(k: K, v: Prefs[K]) {
  prefs.value = { ...prefs.value, [k]: v };
}

export function pushRecent(key: string) {
  setPref("recent", pushFront(prefs.value.recent, key, 30));
}

export function pushRecentFolder(path: string) {
  setPref("recentFolders", pushFront(prefs.value.recentFolders, path.replace(/\/+$/, "") || "/", 12));
}

// ---------------- overlays, toasts, inline UI ----------------

// Overlays (only one modal at a time).
/** The New / Edit schedule dialog. */
export type ScheduleOverlay = { kind: "schedule"; id?: number; agent?: string; draft?: unknown };

export type Overlay =
  | { kind: "account_profiles" }
  | { kind: "jump"; initial: string }
  | { kind: "new_tab"; worktree?: boolean; cwd?: string }
  | { kind: "shortcuts" }
  | { kind: "permissions"; channelId: number }
  | { kind: "rename_window" }
  /** "Notes…": every note, open or closed (docs/UI.md "Notes"). */
  | { kind: "notes" }
  /** "Shell for new terminals…": the shell new shell tabs run. */
  | { kind: "shell" }
  /** "Cache warning…": the cold prompt cache bar and each provider's cache lifetime. */
  | { kind: "cache_settings" }
  /** "Shut down Muster…": stop the backend and terminal host, ending every session. */
  | { kind: "shutdown" }
  /** "Restart Muster…": the same, then start them again (a terminal host update). */
  | { kind: "restart" }
  /** New agent (no `name`) or Edit agent, on its Settings or Memory tab. */
  | { kind: "agent"; name?: string; tab?: "settings" | "memory"; template?: string }
  | { kind: "agent_message"; id: number }
  /** New schedule (no `id`; `agent` preselects one) or Edit schedule. `draft`: the fields as
   * typed before Scheduling settings opened over it (the dialog's own shape). */
  | ScheduleOverlay
  /** Scheduling settings: pause all, quiet hours, the unattended cap, watched folders. `back`:
   * opened from the schedule dialog, which it returns to (with its draft) when it closes. */
  | { kind: "schedule_settings"; back?: ScheduleOverlay }
  | {
    kind: "confirm";
    title: string;
    /** One paragraph, or several. */
    body: string | string[];
    confirmLabel: string;
    danger?: boolean;
    run: () => void;
    /** A second, non-destructive action shown before the confirm button. */
    secondary?: { label: string; run: () => void };
  }
  | null;
export const overlay = signal<Overlay>(null);

export interface Toast {
  id: number;
  level: "info" | "warn" | "error" | "success";
  text: string;
  /** A toast that is updated or dismissed as its subject changes (one per key). */
  key?: string;
  /** A button after the text (clicking it also dismisses the toast). */
  action?: { label: string; run: () => void };
}
export const toasts = signal<Toast[]>([]);
let toastSeq = 0;
export function toast(level: Toast["level"], text: string, ms?: number, key?: string, action?: Toast["action"]) {
  if (key && toasts.value.some((t) => t.key === key)) {
    toasts.value = toasts.value.map((t) => t.key === key ? { ...t, level, text } : t);
    return;
  }
  const id = ++toastSeq;
  // Collapse exact duplicates that arrive in a burst.
  if (toasts.value.some((t) => t.text === text && t.level === level)) return;
  toasts.value = [...toasts.value, { id, level, text, key, action }].slice(-5);
  setTimeout(() => dismissToast(id), ms ?? (level === "error" ? 8000 : 4000));
}
export function dismissToast(id: number) {
  toasts.value = toasts.value.filter((t) => t.id !== id);
}
export function dismissToastKey(key: string) {
  toasts.value = toasts.value.filter((t) => t.key !== key);
}

/** A tab (session) whose close awaits the inline "Close? ⏎ / Esc". */
export const confirmClose = signal<SessionId | null>(null);

/** States in which closing asks first. */
export const BUSY_STATES = new Set<AgentState>(["working", "starting", "needs_approval", "asking_question"]);

/** Alt has been held on its own for a moment (the focused strip shows its Alt+1..9 numbers). */
export const altHeld = signal(false);

/** The inline rename in tab `sid`: the names it shows (`fields`, as they were when it opened), of
 * task `channel` (null when the task is unknown) and session `sid`. `from`: where it was opened
 * (its tab, or its terminal), where the keyboard goes back to when it closes. */
export const renaming = signal<
  { sid: SessionId; channel: ChannelId | null; fields: RenameFields; from: "tab" | "terminal" } | null
>(null);

// ---------------- the window's layout (groups) ----------------

/** This window's split tree of groups. Null until the window is known. */
export const layout = signal<LayoutState | null>(null);
/** The window `layout` belongs to (undefined: none built yet; null: no window support). */
let layoutOwner: WindowId | null | undefined = undefined;

/** The group shown fullscreen: its visible tab fills the whole window, with no tab strips, agents
 * panel or status line. Per window and in memory only (a reload ends it), apart from the saved
 * layout, which it never changes. Ends when focus leaves that group or the group goes. */
export const fullscreen = signal<string | null>(null);

/** The group the window shows alone: the fullscreen one, else the zoomed one. */
export const shownAlone = computed((): string | null => {
  const l = layout.value;
  if (!l) return null;
  const f = fullscreen.value;
  return f && findPane(l.root, f) ? f : l.zoom;
});

effect(() => {
  const f = fullscreen.value;
  const l = layout.value;
  if (f !== null && (!l || l.focus !== f || !findPane(l.root, f))) fullscreen.value = null;
});

/** This window shows the agents panel (kept in its layout, like zoom). */
export const agentsPanelOpen = computed(() => !!layout.value?.agents);

export const focusedGroup = computed((): PaneNode | null => {
  const l = layout.value;
  return l ? findPane(l.root, l.focus) : null;
});

/** The focused group's visible tab. */
export const focusedSession = computed((): SessionInfo | null => {
  const p = focusedGroup.value;
  if (!p || p.active === null || !hereIds.value.has(p.active)) return null;
  return sessions.value.get(p.active) ?? null;
});

/** The focused tab's task. */
export const focusedTask = computed((): Channel | null => {
  const s = focusedSession.value;
  const tab = focusedGroup.value?.active;
  const id = tab == null ? null : panelId(tab);
  const p = id === null ? null : panels.value.get(id);
  return channelById.value.get(s?.channel_id ?? p?.channel_id ?? -1) ?? null;
});

export const focusedTabId = computed(() => focusedGroup.value?.active ?? null);

/** The focused tab's session id: changes only when focus moves, not when its metadata does. */
export const focusedSessionId = computed(() => focusedSession.value?.id ?? null);

// A pending "Close?" never survives a focus change.
let confirmFocus: SessionId | null = null;
effect(() => {
  const id = focusedSessionId.value;
  confirmClose.value = confirmAfterFocus(confirmClose.peek(), confirmFocus, id);
  confirmFocus = id;
});

// Sessions we create are placed by the call that creates them (a tab of a given group, or a new
// split). While such a call is in flight, sessions that newly show up in this window are held out
// of the layout, so they don't first land in the focused group, attach there and size the PTY
// for the wrong group. Once no call is pending, anything still held joins the focused group.
let holds = 0;
const held = signal<ReadonlySet<SessionId>>(new Set());

export function holdNewSessions() {
  holds++;
}

/** Release one hold. */
export function releaseNewSessions() {
  holds = Math.max(0, holds - 1);
  if (holds === 0 && held.peek().size) held.value = new Set();
}

/** Session ids the layout shows: this window's, minus held ones, in joining order. */
const liveIds = computed(() => [...hereIds.value].filter((id) => !held.value.has(id)));

function reconcileTabs(s: LayoutState, ids: number[]): LayoutState {
  // Panels, and notes an agent showed beside itself, go beside their opener; the rest join the
  // focused group.
  const beside = (id: number) => {
    const n = noteId(id);
    return n !== null && notes.peek().get(n)?.beside != null;
  };
  const terminalIds = ids.filter((id) => panelId(id) === null && !beside(id));
  const existing = allSessions(s.root).filter((id) => ids.includes(id));
  let next = reconcile(s, [...new Set([...terminalIds, ...existing])]);
  for (const p of windowPanels.peek()) {
    if (ids.includes(panelTab(p.id))) next = placePanel(next, p.id, p.session_id);
  }
  for (const n of windowNotes.peek()) {
    if (n.beside != null && ids.includes(noteTab(n.id))) next = placeBeside(next, noteTab(n.id), n.beside);
  }
  return next;
}

/** The session ids of the layout in on-screen order (groups, then their strips). */
export const layoutOrder = computed(() => (layout.value ? allSessions(layout.value.root) : []));

// A rename ends when its tab leaves this window or its session or task is gone: reopening it when
// the tab comes back would show (and could commit) the names from before.
effect(() => {
  const r = renaming.value;
  if (r && renameOutlived(r, layoutOrder.value, sessions.value, channelById.value)) renaming.value = null;
});

/**
 * Change the layout. `fn` gets the current one; the result is reconciled with this window's
 * sessions. `place` names sessions the change puts on screen: they stop being held first.
 */
export function updateLayout(fn: (l: LayoutState) => LayoutState, place: SessionId[] = []) {
  const cur = layout.peek();
  if (!cur) return;
  batch(() => {
    if (place.some((id) => held.peek().has(id))) {
      const h = new Set(held.peek());
      for (const id of place) h.delete(id);
      held.value = h;
    }
    const next = reconcileTabs(fn(cur), liveIds.peek());
    if (JSON.stringify(next) !== JSON.stringify(cur)) layout.value = next;
  });
}

/**
 * Build this window's layout when the window becomes known (or changes): the one the daemon
 * stored (`Window.layout`), else, for a window that has none yet (first v2 run, or a daemon
 * without it), one group holding all its sessions.
 */
effect(() => {
  if (!helloDone.value) return;
  const supported = windowsSupported.value;
  const id = windowId.value;
  if (supported && id === null) return;
  const owner = supported ? id : null;
  if (layoutOwner === owner) return;
  const w = supported ? myWindow.peek() : null;
  const stored = w?.layout ? parseLayout(w.layout) : null;
  layoutOwner = owner;
  lastSaved = stored ? JSON.stringify(w!.layout) : "";
  clearTimeout(saveTimer);
  layout.value = reconcileTabs(stored ?? emptyLayout([]), liveIds.peek());
});

// Every change to this window's sessions reconciles the layout: new ones join the focused group,
// gone ones leave (a focused group that empties hands focus to its neighbour), empty groups close.
effect(() => {
  const ids = liveIds.value;
  const cur = layout.peek();
  if (!cur) return;
  const next = reconcileTabs(cur, ids);
  if (JSON.stringify(next) !== JSON.stringify(cur)) layout.value = next;
});

// Save the layout (debounced). The daemon doesn't echo it back; daemons without
// `window.set_layout` just don't keep it (a reload builds one group).
let saveTimer = 0;
let lastSaved = "";
let setLayoutMissing = false;
/** The `window.set_layout` call still on its way, if any (`lastSaved` is set when it starts). */
let saving: Promise<unknown> | null = null;

function saveLayout(id: WindowId): Promise<unknown> {
  const cur = layout.peek();
  if (!cur || windowId.peek() !== id || layoutOwner !== id) return saving ?? Promise.resolve();
  const json = JSON.stringify(cur);
  // Already sent: done once that call is.
  if (json === lastSaved) return saving ?? Promise.resolve();
  lastSaved = json;
  const p: Promise<unknown> = rpc.call("window.set_layout", { id, layout: cur }).catch((e) => {
    lastSaved = "";
    if (/^unknown method/i.test(e instanceof Error ? e.message : String(e))) setLayoutMissing = true;
  }).finally(() => {
    if (saving === p) saving = null;
  });
  saving = p;
  return p;
}

effect(() => {
  const l = layout.value;
  const id = windowId.value;
  if (!l || id === null || !windowsSupported.value || setLayoutMissing || layoutOwner !== id) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => saveLayout(id), 400);
});

/** Save a layout change still waiting for its debounce (before the page goes away). */
export function saveLayoutNow(): Promise<unknown> {
  const id = windowId.peek();
  clearTimeout(saveTimer);
  if (id === null || !windowsSupported.peek() || setLayoutMissing) return saving ?? Promise.resolve();
  return saveLayout(id);
}

// ---------------- server event handling ----------------

function upsertBy<T extends { id: number }>(arr: T[], item: T): T[] {
  const i = arr.findIndex((x) => x.id === item.id);
  if (i < 0) return [...arr, item];
  const out = [...arr];
  out[i] = item;
  return out;
}

/** Record a channel from an RPC reply (its channel_upserted event may lag). */
export function knowChannel(c: Channel) {
  channels.value = upsertBy(channels.value, c);
}

/** Record a window from an RPC reply. */
export function knowWindow(w: Win) {
  windows.value = upsertWindow(windows.value, w);
}

export const removedSessionListeners: ((id: SessionId) => void)[] = [];
/** Called when the window this client holds disappeared (it must claim again). */
export const windowLostListeners: (() => void)[] = [];

/** While a placement is pending, hold the sessions that appeared in this window since `before`. */
function holdArrivals(before: ReadonlySet<SessionId>) {
  if (holds === 0) return;
  const now = hereIds.peek();
  const h = new Set(held.peek());
  let changed = false;
  for (const id of now) {
    if (!before.has(id) && !h.has(id)) {
      h.add(id);
      changed = true;
    }
  }
  if (changed) held.value = h;
}

/** Apply a change; sessions it brings into this window are held if a placement is pending. */
function withHolds(fn: () => void) {
  batch(() => {
    const before = hereIds.peek();
    fn();
    holdArrivals(before);
  });
}

export function applyHello(h: HelloResult) {
  batch(() => {
    const before = hereIds.peek();
    meta.value = {
      version: h.version,
      home: h.home,
      default_shell: h.default_shell,
      login_shell: h.login_shell ?? h.default_shell,
    };
    updateStatus.value = h.updates ?? null;
    lastBackend.value = { pid: h.pid ?? null, host: h.terminal_host?.instance ?? null, build: h.build ?? null };
    windowPrefs.value = h.prefs ?? null;
    workspaces.value = [...h.workspaces].sort((a, b) => a.created_at - b.created_at || a.id - b.id);
    accountProfiles.value = h.account_profiles ?? [];
    accountProfilesSupported.value = Array.isArray(h.account_profiles);
    openRouter.value = h.openrouter ?? null;
    channels.value = h.channels.filter((c) => !c.archived);
    windowsSupported.value = Array.isArray(h.windows);
    windows.value = [...(h.windows ?? [])].sort((a, b) => a.id - b.id);
    const prev = sessions.value;
    versions.bumpAll();
    sessions.value = new Map(h.sessions.map((s) => [s.id, s]));
    panels.value = new Map((h.panels ?? []).filter((p) => !p.closed).map((p) => [p.id, p]));
    panelOrigin.value = h.panel_origin ?? "";
    applyNotesHello(h.notes);
    helloEpoch.value++;
    for (const id of prev.keys()) {
      if (!sessions.value.has(id)) { for (const f of removedSessionListeners) f(id); }
    }
    if (helloDone.peek()) holdArrivals(before);
    helloDone.value = true;
  });
  loadAgents();
  // Seed unread state with a little history per task.
  for (const c of h.channels) {
    if (c.archived) continue;
    rpc.call<TimelineEvent[]>("channel.timeline", { id: c.id, limit: 30 }).then((evs) => {
      for (const e of evs) noteNotable(e);
    }).catch(() => {});
  }
}

/** Record a panel change we made before its event arrives. */
export function knowPanel(p: Panel) {
  const next = new Map(panels.value);
  if (p.closed) next.delete(p.id);
  else next.set(p.id, p);
  panels.value = next;
}

export function applyEvent(ev: ServerEvent) {
  switch (ev.event) {
    case "panel_upserted":
      knowPanel(ev.data);
      break;
    case "panel_removed": {
      const next = new Map(panels.value);
      next.delete(ev.data.id);
      panels.value = next;
      break;
    }
    case "note_upserted":
      withHolds(() => applyNoteUpsert(ev.data));
      break;
    case "note_removed":
      applyNoteRemoved(ev.data.id);
      break;
    case "account_profiles_changed":
      accountProfiles.value = ev.data;
      break;
    case "workspace_upserted":
      workspaces.value = upsertBy(workspaces.value, ev.data);
      break;
    case "workspace_removed":
      workspaces.value = workspaces.value.filter((w) => w.id !== ev.data.id);
      break;
    case "channel_upserted": {
      if (ev.data.archived) {
        removeChannel(ev.data.id);
        break;
      }
      withHolds(() => (channels.value = upsertBy(channels.value, ev.data)));
      break;
    }
    case "channel_removed":
      removeChannel(ev.data.id);
      break;
    case "window_upserted": {
      // Our own layout is ours: the daemon stores what we send and never echoes it.
      withHolds(() => (windows.value = upsertWindow(windows.value, ev.data)));
      break;
    }
    case "window_removed":
      windows.value = windows.value.filter((w) => w.id !== ev.data.id);
      if (ev.data.id === windowId.value) { for (const f of windowLostListeners) f(); }
      break;
    case "session_upserted": {
      versions.bump(ev.data.id);
      upsertSession(ev.data);
      sessionSizeChanged(ev.data);
      confirmClose.value = confirmAfterUpsert(confirmClose.peek(), ev.data, BUSY_STATES);
      break;
    }
    case "session_removed": {
      versions.bump(ev.data.id);
      if (!sessions.value.has(ev.data.id)) break;
      const m = new Map(sessions.value);
      m.delete(ev.data.id);
      batch(() => {
        sessions.value = m;
        if (held.peek().has(ev.data.id)) {
          const h = new Set(held.peek());
          h.delete(ev.data.id);
          held.value = h;
        }
        if (confirmClose.value === ev.data.id) confirmClose.value = null;
      });
      for (const f of removedSessionListeners) f(ev.data.id);
      break;
    }
    case "event": {
      const e = ev.data;
      noteNotable(e);
      if (e.channel_id !== null) maybeMarkRead(e.channel_id);
      break;
    }
    case "notice":
      toast(ev.data.level === "error" ? "error" : ev.data.level === "warn" ? "warn" : "info", ev.data.text);
      break;
    case "agent_upserted":
      agents.value = sortAgents([...agents.value.filter((a) => a.id !== ev.data.id), ev.data]);
      break;
    case "agent_removed":
      agents.value = agents.value.filter((a) => a.id !== ev.data.id);
      if (agentFilter.value === ev.data.id) agentFilter.value = null;
      // Removing an agent removes its schedules (their events may come later, or not at all).
      for (const s of schedules.value) if (s.agent_id === ev.data.id) forgetSchedule(s.id);
      break;
    case "agent_message":
      agentMessages.value = upsertMessage(agentMessages.value, ev.data);
      break;
    case "schedule_upserted": {
      // An upsert carries the schedule's own last run; one recorded since (by `schedule_run`) stays.
      const cur = schedules.value.find((s) => s.id === ev.data.id);
      const last = cur?.last_run && ev.data.last_run ? newerRun(cur.last_run, ev.data.last_run) : ev.data.last_run ?? cur?.last_run ?? null;
      knowSchedule({ ...ev.data, last_run: last });
      break;
    }
    case "schedule_removed":
      forgetSchedule(ev.data.id);
      break;
    case "schedule_run":
      knowRun(ev.data);
      break;
    case "schedule_settings":
      scheduleSettings.value = ev.data;
      break;
    case "updates_changed":
      updateStatus.value = ev.data;
      break;
    case "prefs_changed":
      if (newerPrefs(windowPrefs.value, ev.data)) windowPrefs.value = ev.data;
      break;
    case "daemon_stopping":
      // A restart keeps the connection retrying; anything else (a shutdown) stops it.
      if (ev.data.reason === "restart") daemonRestarting.value = restartWait(lastBackend.peek(), updateStatus.peek());
      else daemonStopped.value = true;
      break;
  }
}

/** Record an agent from an RPC reply (its agent_upserted event may lag). */
export function knowAgent(a: Agent) {
  agents.value = sortAgents([...agents.value.filter((x) => x.id !== a.id), a]);
}

const versions = new SessionVersions();

/** Note before an RPC whose reply carries a session; pass the mark to `knowSession`. */
export function sessionMark(): number {
  return versions.mark();
}

/**
 * Record a session (from an event). One that newly shows up in this window (spawned, or
 * moved here from another window) is held while a placement is pending, else it joins the
 * focused group; one that left (moved to another window) leaves the layout.
 */
export function upsertSession(s: SessionInfo) {
  const m = new Map(sessions.value);
  m.set(s.id, s);
  withHolds(() => (sessions.value = m));
}

/**
 * Record a session from an RPC reply of a call that started at `mark` (`sessionMark()`). Ignored
 * when an event about it arrived since: that event is newer (a later move, or its removal).
 */
export function knowSession(s: SessionInfo, mark: number) {
  if (versions.fresh(s.id, mark)) upsertSession(s);
}

/**
 * The first session of task `ch` (a new task's comes as an event, maybe after the reply).
 * Null when none arrives within `ms`.
 */
export function firstSessionOf(ch: ChannelId, ms: number): Promise<SessionInfo | null> {
  const known = sessionsByChannel.peek().get(ch)?.[0];
  if (known) return Promise.resolve(known);
  return new Promise((resolve) => {
    const finish = (s: SessionInfo | null) => {
      clearTimeout(timer);
      // Not from inside the effect's own run.
      queueMicrotask(stop);
      resolve(s);
    };
    const timer = setTimeout(() => finish(null), ms);
    const stop = effect(() => {
      const s = sessionsByChannel.value.get(ch)?.[0];
      if (s) finish(s);
    });
  });
}

function removeChannel(id: ChannelId) {
  batch(() => {
    channels.value = channels.value.filter((c) => c.id !== id);
    panels.value = new Map([...panels.value].filter(([, p]) => p.channel_id !== id));
  });
}

// ---------------- read tracking ----------------

// One pending mark per task: switching tabs quickly must not drop the first one. Each mark while
// one is pending moves its `seenAt` forward (what was on screen at the latest look).
const readTimers = new Map<ChannelId, { timer: number; seenAt: number }>();
function markReadSoon(ch: ChannelId) {
  const pending = readTimers.get(ch);
  if (pending) {
    pending.seenAt = Date.now();
    return;
  }
  const entry = { timer: 0, seenAt: Date.now() };
  entry.timer = setTimeout(() => {
    readTimers.delete(ch);
    const seenAt = entry.seenAt;
    const c = channelById.value.get(ch);
    if (!c || c.last_read_at >= seenAt) return;
    channels.value = upsertBy(channels.value, { ...c, last_read_at: seenAt });
    rpc.call("channel.mark_read", { id: ch, at: seenAt }).catch(() => {});
  }, 600);
  readTimers.set(ch, entry);
}

export function markRead(ch: ChannelId) {
  if (unread.value.has(ch)) markReadSoon(ch);
}

function maybeMarkRead(ch: ChannelId) {
  if (ch === focusedTask.value?.id && document.visibilityState === "visible" && document.hasFocus()) {
    markReadSoon(ch);
  }
}

effect(() => {
  const c = focusedTask.value;
  if (c) markRead(c.id);
});

addEventListener("focus", () => {
  const c = focusedTask.value;
  if (c) markRead(c.id);
});

// Note typing this window's last page couldn't save comes back once the window is known.
effect(() => {
  if (!helloDone.value || !notesSupported.value) return;
  if (!windowsSupported.value) restoreDrafts(null);
  else if (windowId.value !== null) restoreDrafts(windowId.value);
});

effect(() => {
  document.title = documentTitle(windowsSupported.value ? myWindow.value : null, needsYouHere.value);
});

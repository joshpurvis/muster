// Named agents (docs/UI.md "Agents in the sidebar"): the sidebar's message log, the New / Edit
// agent dialog with its Memory tab, and the message overlay. Talking to an agent happens in its
// own terminal tab; its row and card in the sidebar (AgentCard.tsx) start, find and configure it.
import type { ComponentChildren } from "preact";
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import {
  workspaces,
  channels,
  accountProfiles,
  accountUsage,
  agentFilter,
  agentMessages,
  agents,
  helloEpoch,
  knowAgent,
  openRouter,
  overlay,
  prefs,
  sessions,
  setPref,
  toast,
} from "../state.ts";
import * as A from "../actions.ts";
import {
  ago,
  choiceOf,
  contextLabel,
  CUSTOM,
  effortAfterModel,
  effortsFor,
  feedFor,
  firstLine,
  carriesLateReply,
  quietStatus,
  keepForKind,
  lateStatus,
  listedModels,
  memoryPathError,
  nameError,
  needsModel,
  OPENROUTER_BADGE,
  PREFERRED_VENDOR,
  priceLabel,
  parseSleepAfter,
  readMemoryFile,
  pendingMove,
  saveNotesBlocked,
  kindSwitchMessage,
  switchMessage,
  switchPermissions,
  SLEEP_AFTER_DEFAULT,
  SLEEP_AFTER_MAX,
  SLEEP_AFTER_MIN,
  UNATTENDED_WARNING,
  unattendedWarning,
  upsertMessage,
  vendorGroups,
} from "../agents.ts";
import type {
  Agent,
  AgentWorkingLocation,
  AgentKindSwitch,
  AgentMessage,
  AgentSwitch,
  AgentTemplate,
  LateAnswer,
  ModelCatalog,
  OpenRouterModel,
  OpenRouterModels,
  Origin,
  Permissions,
  ProfileChoice,
  Provider,
} from "../types.ts";
import { DEFAULT_PERMISSIONS, KIND_LABEL, storedPermissions } from "../types.ts";
import { Kbd, Spinner } from "./common.tsx";
import { contextMenu } from "./Menu.tsx";
import { PermissionsPanel, PermissionsPill } from "./Permissions.tsx";
import { ProfilePill, ProfileUsage } from "./AccountProfiles.tsx";
import { atLimit, profileName, sameChoice, shownChoice } from "../account_profiles.ts";
import { MoreOptions, PillSelect } from "./DialogKit.tsx";
import {
  IconBranch,
  IconChevron,
  IconChevronDown,
  KindIcon,
  IconCpu,
  IconFolder,
  IconGauge,
  IconPlus,
  IconShieldAlert,
  IconX,
} from "../icons.tsx";
import { cx } from "../util.ts";
import { idleText } from "../cache.ts";

/** Re-render every `ms` (the "time in state" column). */
export function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

// ---------------- the message log ----------------

/** Older messages are fetched on demand (`agent.messages` pages, newest first). */
const olderState = { busy: false, done: new Set<string>() };

/** The sidebar's Messages section: the log, filtered to the open channel's agent. */
export function Feed({ now }: { now: number }) {
  const open = prefs.value.agentFeed;
  // A reconnect reloads the newest page of all messages: what was paged in before is gone.
  const epoch = helloEpoch.value;
  useEffect(() => olderState.done.clear(), [epoch]);
  const sel = agentFilter.value;
  const selName = sel !== null ? agents.value.find((a) => a.id === sel)?.name : undefined;
  const list = feedFor(agentMessages.value, sel);
  const [, rerender] = useState(0);
  const older = async () => {
    const key = selName ?? "*";
    if (olderState.busy || olderState.done.has(key)) return;
    olderState.busy = true;
    try {
      const last = list[list.length - 1];
      const r = await rpc.call<{ messages: AgentMessage[]; more: boolean }>("agent.messages", {
        agent: selName,
        before: last?.id,
        limit: 50,
      });
      let log = agentMessages.value;
      for (const m of r.messages) log = upsertMessage(log, m, 1000);
      agentMessages.value = log;
      if (!r.more) {
        olderState.done.add(key);
        // The button goes with nothing more to load: keyboard focus moves to the heading.
        if (document.activeElement?.closest(".ap-feed .ap-older")) {
          setTimeout(() => document.querySelector<HTMLElement>(".ap-feed-toggle")?.focus(), 0);
        }
      }
    } catch (e) {
      toast("error", `Couldn't load older messages: ${e instanceof Error ? e.message : e}`);
    } finally {
      olderState.busy = false;
      rerender((n) => n + 1);
    }
  };
  return (
    <section class={cx("ap-feed", open && "open")} aria-label="Messages">
      <div class="ap-feed-head" {...contextMenu(() => [
        { label: open ? "Hide messages" : "Show messages", run: () => setPref("agentFeed", !open) },
        { label: "Show all agents' messages", disabled: sel === null, run: () => { agentFilter.value = null; setPref("agentFeed", true); } },
        "sep",
        { label: "Hide sidebar", keys: "Alt+G", run: A.toggleSidebar },
      ])}>
        <button
          type="button"
          class="ap-feed-toggle"
          aria-expanded={open}
          onClick={() => setPref("agentFeed", !open)}
        >
          <span class="ap-chev" aria-hidden="true">
            <IconChevron size={11} />
          </span>
          Messages
          {list.length > 0 && <span class="ap-count">{list.length}</span>}
        </button>
        {selName && (
          <button
            type="button"
            class="ap-filter"
            data-tip="Show messages to every agent"
            onClick={() => (agentFilter.value = null)}
          >
            to {selName} <IconX size={10} />
          </button>
        )}
      </div>
      {open && (
        <div class="ap-feed-list">
          {list.length === 0 && (
            <div class="ap-empty">
              {selName
                ? olderState.done.has(selName) ? `No messages to ${selName} yet.` : `None of the latest messages are to ${selName}.`
                : "Messages agents send each other show here."}
            </div>
          )}
          {list.map((m) => <FeedEntry key={m.id} m={m} now={now} />)}
          {/* Filtered to one agent, its older messages may lie past the newest page of all of them. */}
          {(list.length >= 20 || selName) && !olderState.done.has(selName ?? "*") && (
            <button type="button" class="ap-older" onClick={older}>Older messages</button>
          )}
        </div>
      )}
    </section>
  );
}

function OriginChip({ origin }: { origin: Origin }) {
  return <span class={cx("ap-chip", origin !== "human" && "ap-chip-unattended")}>{origin}</span>;
}

/** Muster typed this message in after the agent's cache had likely expired. */
export function ColdChip({ ms }: { ms: number }) {
  return (
    <span
      class="ap-chip ap-chip-cold"
      data-tip={`Sent after ${idleText(ms)} idle: the agent likely re-read its whole conversation uncached`}
    >
      cold start
    </span>
  );
}

/** The agent answered this message later (`reply`): green unless it never reached the asker,
 * judged as the message's overlay judges it (a quiet carrier's own status, `lateStatus`). */
function LateChip({ a }: { a: LateAnswer }) {
  const carrier = a.msg != null ? agentMessages.value.find((x) => x.id === a.msg) : undefined;
  const { failed, text } = lateStatus(a, carrier);
  return (
    <span class={cx("ap-chip", failed ? "ap-chip-warn" : "ap-chip-ok")} data-tip={text}>
      {failed ? "late reply not delivered" : "late reply"}
    </span>
  );
}

/** A quiet message (sent with wake=false): waiting, shown at a turn's start, or expired. */
function QuietChip({ m }: { m: AgentMessage }) {
  const q = quietStatus(m);
  if (!q) return null;
  return (
    <span class={cx("ap-chip", `ap-chip-quiet-${q.state}`)} data-tip={q.text}>
      {q.chip}
    </span>
  );
}

function FeedEntry({ m, now }: { m: AgentMessage; now: number }) {
  // A quiet message's status sentence, on the focusable row itself, as its description and as a
  // tip that shows when the keyboard reaches the row (the chip's own tip is pointer-only).
  const quiet = quietStatus(m);
  return (
    <button
      type="button"
      class={cx("ap-msg", m.refused && "refused")}
      aria-description={quiet?.text}
      data-tip={quiet?.text}
      data-tip-focus={quiet ? "" : undefined}
      data-msg={m.id}
      onClick={() => (overlay.value = { kind: "agent_message", id: m.id })}
    >
      <span class="ap-msg-top">
        <span class="ap-msg-route">
          {m.from} <span class="ap-arrow">→</span> {m.to}
        </span>
        <span class="ap-msg-time">{ago(m.created_at, now)}</span>
      </span>
      <span class="ap-msg-text">{firstLine(m.text) || <i>text not kept</i>}</span>
      <span class="ap-msg-chips">
        <OriginChip origin={m.origin} />
        <span class="ap-chip">hop {m.hop}</span>
        {m.reply !== null && <span class="ap-chip ap-chip-ok">replied</span>}
        <QuietChip m={m} />
        {m.answer && <LateChip a={m.answer} />}
        {m.cold_idle_ms != null && <ColdChip ms={m.cold_idle_ms} />}
      </span>
      {m.refused && (
        <span class="ap-msg-refused">
          <IconShieldAlert size={11} /> {m.refused}
        </span>
      )}
    </button>
  );
}

// ---------------- New / Edit agent ----------------

type ModalT = (
  p: { children: ComponentChildren; class?: string; label: string; onClose?: () => void; describedBy?: string },
) => ComponentChildren;

const ORIGINS: { o: Origin; label: string; hint: string }[] = [
  { o: "human", label: "Human", hint: "you, or an agent working on something you started" },
  { o: "schedule", label: "Schedule", hint: "a timer" },
  { o: "webhook", label: "Webhook (triggered)", hint: "a trigger: a note changes, a branch moves, GitHub, a trigger URL" },
];

/** One entry of a Model or Effort select. */
interface Pick {
  value: string;
  label: string;
}

/** Model and Effort: Default, the CLI's list (`agent.models`) and "Custom…", which shows a text
 * field (`CustomPick`, under the pills) for anything else (a full model id, a level the list
 * doesn't have yet). A stored value the list doesn't know shows as custom. `custom` is the
 * dialog's latch (`choiceOf`): Custom stays chosen until another entry is picked. */
function PickField(
  { label, icon, value, picks, custom, focusTyped, describedBy, onChange }: {
    label: string;
    icon: ComponentChildren;
    value: string;
    picks: Pick[];
    custom: boolean;
    /** Set when Custom… is picked, so its field takes focus. */
    focusTyped: { current: boolean };
    describedBy: string;
    onChange: (v: string, custom: boolean) => void;
  },
) {
  const choice = choiceOf(value, picks.map((p) => p.value), custom);
  return (
    <PillSelect
      icon={icon}
      k={label}
      label={label}
      value={choice}
      tip="Default is the CLI's own choice"
      describedBy={describedBy}
      class={choice ? "set" : undefined}
      onChange={(v) => {
        focusTyped.current = v === CUSTOM;
        onChange(v === CUSTOM ? value : v, v === CUSTOM);
      }}
      options={[
        { value: "", label: "Default (the CLI's choice)", short: "Default" },
        ...picks.map((p) => ({ value: p.value, label: p.label, short: p.value })),
        { value: CUSTOM, label: "Custom…", short: value.trim() || "Custom" },
      ]}
    />
  );
}

/** The text field of a Model or Effort set to Custom…: focused, its text selected, right after
 * Custom… is picked, so typing replaces it. */
function CustomPick(
  { label, value, placeholder, focusTyped, onChange }: {
    label: string;
    value: string;
    placeholder: string;
    focusTyped: { current: boolean };
    onChange: (v: string) => void;
  },
) {
  const typed = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (focusTyped.current && typed.current) {
      focusTyped.current = false;
      typed.current.focus();
      typed.current.select();
    }
  });
  return (
    <label class="pill-panel pill-panel-row">
      <span class="field-label">{label}</span>
      <input
        ref={typed}
        value={value}
        aria-label={label}
        placeholder={placeholder}
        spellcheck={false}
        onInput={(e) => onChange((e.target as HTMLInputElement).value)}
      />
    </label>
  );
}

const modelPicks = (c: ModelCatalog | null, kind: Agent["kind"]): Pick[] =>
  (c?.[kind]?.models ?? []).map((m) => ({ value: m.id, label: m.note ? `${m.id} · ${m.note}` : m.id }));
const effortPicks = (c: ModelCatalog | null, kind: Agent["kind"], model: string): Pick[] =>
  effortsFor(c, kind, model).map((e) => ({ value: e, label: e }));

/** OpenRouter's list (`openrouter.models`), shared by every dialog for a few minutes (the daemon
 * keeps its own copy for an hour). */
let orCache: { at: number; list: OpenRouterModels } | null = null;
const OR_CACHE_MS = 5 * 60_000;

/** The Model field for an agent on OpenRouter: a search box over OpenRouter's models (tool use
 * only, grouped by vendor, the CLI's own vendor first), Default (Claude only) and Custom…. The
 * list is a listbox: ↑/↓ move, Enter or a click picks; ↓ in the search box goes to the first
 * match and Enter there picks it. */
function OpenRouterModelField(
  { kind, value, custom, list, loading, onChange }: {
    kind: Agent["kind"];
    value: string;
    custom: boolean;
    list: OpenRouterModels | null;
    loading: boolean;
    onChange: (v: string, custom: boolean) => void;
  },
) {
  const id = useId();
  const [q, setQ] = useState("");
  const box = useRef<HTMLDivElement>(null);
  const typed = useRef<HTMLInputElement>(null);
  const focusTyped = useRef(false);
  const models = list?.models ?? [];
  const choice = choiceOf(value, models.map((m) => m.id), custom);
  const groups = vendorGroups(models, q, PREFERRED_VENDOR[kind]);
  const shown = groups.reduce((n, g) => n + g.models.length, 0);
  useEffect(() => {
    if (focusTyped.current && typed.current) {
      focusTyped.current = false;
      typed.current.focus();
      typed.current.select();
    }
  });
  // The chosen model in view when the list first renders.
  useEffect(() => {
    box.current?.querySelector<HTMLElement>("[aria-selected=true]")?.scrollIntoView({ block: "nearest" });
  }, [models.length]);
  const options = () => [...(box.current?.querySelectorAll<HTMLElement>("[role=option]") ?? [])];
  const pick = (v: string) => {
    focusTyped.current = v === CUSTOM;
    onChange(v === CUSTOM ? value : v, v === CUSTOM);
  };
  const opt = (v: string, label: ComponentChildren, sub: ComponentChildren, key = v) => {
    const on = choice === v;
    return (
      <button
        key={key}
        type="button"
        role="option"
        aria-selected={on}
        tabIndex={on || (choice === CUSTOM && v === CUSTOM) ? 0 : -1}
        class={cx("or-opt", on && "on")}
        data-model={v === CUSTOM ? "custom" : v}
        onClick={() => pick(v)}
      >
        <span class="or-opt-main">{label}</span>
        {sub && <span class="or-opt-sub">{sub}</span>}
      </button>
    );
  };
  const modelRow = (m: OpenRouterModel) => {
    const price = priceLabel(m);
    const ctx = contextLabel(m.context_length);
    return opt(
      m.id,
      <>
        <span class="or-opt-name">{m.name.replace(/^[^:]+:\s*/, "")}</span>
        <code class="or-opt-id">{m.id}</code>
      </>,
      <>
        {price && <span data-tip="US dollars per million tokens, in / out">{price}</span>}
        {ctx && <span>{ctx} context</span>}
        {m.alias_of && <span>now {m.alias_of}</span>}
      </>,
    );
  };
  let status: ComponentChildren;
  if (loading && !models.length) status = <><Spinner size={11} /> Loading OpenRouter's models…</>;
  else if (!models.length) {
    status = (
      <span class="field-bad">
        Couldn't load OpenRouter's model list{list?.error ? ` (${list.error})` : ""}. Type a model id under Custom….
      </span>
    );
  } else {
    status = (
      <>
        {models.length} models with tool use, from openrouter.ai; prices per million tokens in / out.
        {list?.error && list.stale && " This copy is over an hour old: refreshing failed."}
      </>
    );
  }
  return (
    <div class="field or-field">
      <label class="field-label" for={id}>Model on OpenRouter</label>
      <input
        id={id}
        class="or-search"
        value={q}
        placeholder={models.length ? `Search ${models.length} models, e.g. sonnet or qwen coder` : "Search models"}
        spellcheck={false}
        aria-controls={`${id}-list`}
        onInput={(e) => setQ((e.target as HTMLInputElement).value)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") {
            e.preventDefault();
            options().find((o) => o.dataset.model !== "")?.focus();
          } else if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && q.trim()) {
            e.preventDefault();
            const first = groups[0]?.models[0];
            if (first) pick(first.id);
          }
        }}
      />
      <div
        ref={box}
        id={`${id}-list`}
        class="or-list"
        role="listbox"
        aria-label="OpenRouter models"
        onKeyDown={(e) => {
          const all = options();
          const i = all.indexOf(document.activeElement as HTMLElement);
          const to = e.key === "ArrowDown" ? i + 1 : e.key === "ArrowUp" ? i - 1 : e.key === "Home"
            ? 0
            : e.key === "End"
            ? all.length - 1
            : null;
          if (to === null) return;
          e.preventDefault();
          all[Math.max(0, Math.min(all.length - 1, to))]?.focus();
        }}
      >
        {kind === "claude" && !q.trim() &&
          opt("", "Default", "Claude Code's choice, through OpenRouter's latest Anthropic aliases")}
        {groups.map((g) => (
          <div key={g.vendor} role="group" aria-label={g.vendor} class="or-group">
            <div class="or-vendor" aria-hidden="true">
              {g.vendor}
              {g.vendor === PREFERRED_VENDOR[kind] && <span class="or-pref">works best with {KIND_LABEL[kind]}</span>}
            </div>
            {g.models.map(modelRow)}
          </div>
        ))}
        {q.trim() && !shown && models.length > 0 && <div class="or-none">No model matches “{q.trim()}”.</div>}
        {opt(CUSTOM, "Custom…", "any OpenRouter model id")}
      </div>
      {choice === CUSTOM && (
        <input
          ref={typed}
          value={value}
          aria-label="Custom OpenRouter model id"
          placeholder={kind === "codex" ? "e.g. openai/gpt-6.1-sol" : "e.g. anthropic/claude-sonnet-5.5"}
          spellcheck={false}
          onInput={(e) => onChange((e.target as HTMLInputElement).value, true)}
        />
      )}
      <span class="field-hint">{status}</span>
    </div>
  );
}

interface Draft {
  working_location: AgentWorkingLocation;
  account_profile: ProfileChoice;
  provider: Provider;
  name: string;
  kind: Agent["kind"];
  model: string;
  effort: string;
  persona: string;
  permissions: Permissions;
  accepts: Origin[];
  autostart: boolean;
  /** Minutes as typed; "" with `sleep` off. */
  sleepAfter: string;
  sleep: boolean;
  /** May start itself fresh (Edit, Claude only; the human's to grant). */
  selfHandoff: boolean;
  /** Muster's message tools in the prompt from the start (Edit, Claude only). */
  toolsUpFront: boolean;
  /** A lead agent: what it sends from scheduled work is taken like the human's (Edit, Claude and Antigravity; the human's to set). */
  lead: boolean;
  /** Other workspaces it may start sessions in (Edit; the human's to grant, in this window only). */
  spawnWorkspaces: number[];
}

function fromTemplate(t: AgentTemplate): Draft {
  return {
    working_location: { mode: "home" },
    account_profile: "default",
    provider: "subscription",
    name: t.name,
    kind: t.kind,
    model: t.model ?? "",
    effort: t.effort ?? "",
    persona: t.persona,
    permissions: storedPermissions(t.permissions),
    accepts: [...t.accepts],
    autostart: t.autostart,
    sleep: false,
    sleepAfter: String(SLEEP_AFTER_DEFAULT),
    selfHandoff: false,
    toolsUpFront: false,
    lead: false,
    spawnWorkspaces: [],
  };
}

function fromAgent(a: Agent): Draft {
  return {
    working_location: a.working_location ?? { mode: "home" },
    account_profile: a.account_profile_id ?? "default",
    provider: a.provider ?? "subscription",
    name: a.name,
    kind: a.kind,
    model: a.model ?? "",
    effort: a.effort ?? "",
    persona: a.persona,
    permissions: storedPermissions(a.permissions),
    accepts: [...a.accepts_origins],
    autostart: a.autostart,
    sleep: a.sleep_after_min != null,
    sleepAfter: String(a.sleep_after_min ?? SLEEP_AFTER_DEFAULT),
    selfHandoff: !!a.self_handoff,
    toolsUpFront: !!a.tools_up_front,
    lead: !!a.lead,
    spawnWorkspaces: [...(a.spawn_workspaces ?? [])].sort((x, y) => x - y),
  };
}

const BLANK: Draft = {
  working_location: { mode: "home" },
  account_profile: "default",
  provider: "subscription",
  name: "",
  kind: "claude",
  model: "",
  effort: "",
  persona: "",
  permissions: { ...DEFAULT_PERMISSIONS },
  accepts: ["human"],
  autostart: false,
  sleep: false,
  sleepAfter: String(SLEEP_AFTER_DEFAULT),
  selfHandoff: false,
  toolsUpFront: false,
  lead: false,
  spawnWorkspaces: [],
};

let templateCache: AgentTemplate[] | null = null;
let catalogCache: ModelCatalog | null = null;

export function AgentDialog(
  { name, tab: tabInit, template, focus, Modal, close }: {
    name?: string;
    tab?: "settings" | "memory";
    /** Edit only: "profile" opens on the account picker (Switch account…). */
    focus?: "profile";
    /** New only: start from this template (e.g. "worker" from the New schedule dialog). */
    template?: string;
    Modal: ModalT;
    close: () => void;
  },
) {
  const editing = name !== undefined;
  const agent = editing ? agents.value.find((a) => a.name === name) : undefined;
  const startTpl = !editing && template ? templateCache?.find((t) => t.id === template) : undefined;
  const [tab, setTab] = useState<"settings" | "memory">(editing ? tabInit ?? "settings" : "settings");
  const [templates, setTemplates] = useState<AgentTemplate[]>(templateCache ?? []);
  const [catalog, setCatalog] = useState<ModelCatalog | null>(catalogCache);
  const [orList, setOrList] = useState<OpenRouterModels | null>(orCache?.list ?? null);
  const [orLoading, setOrLoading] = useState(false);
  const [tpl, setTpl] = useState(startTpl?.id ?? "blank");
  const [d, setD] = useState<Draft>(agent ? fromAgent(agent) : startTpl ? fromTemplate(startTpl) : BLANK);
  /** "May start sessions in" as the dialog opened: Save sends only the ticks changed here, applied to
   * the list as it is then, so a workspace taken away meanwhile (another window) doesn't come back. */
  const openedStartIn = useRef(d.spawnWorkspaces);
  /** Model / Effort show Custom… until another entry is picked (`choiceOf`). */
  const [custom, setCustom] = useState({ model: false, effort: false });
  const [nameTouched, setNameTouched] = useState(false);
  const [conversation, setConversation] = useState<"" | "keep" | "reset">("");
  /** A move to another account: when it applies, Save notes first, and why. */
  const [moveNow, setMoveNow] = useState(false);
  /** A switch between Claude and Antigravity: restart it now (if idle) or at its next start. */
  const [kindNow, setKindNow] = useState(false);
  // Off by default: asking starts a turn, and HANDOFF.md and memory/ carry it over anyway.
  const [saveNotes, setSaveNotes] = useState(false);
  const [restarting, setRestarting] = useState(false);
  /** The account picker was changed here: until then it follows the agent's account, so a move
   * made elsewhere (the CLI, an agent) while this dialog is open is never undone by Save. */
  const [profileTouched, setProfileTouched] = useState(false);
  const [moveReason, setMoveReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const memDirty = useRef(false);
  /** The Memory tab was opened: it stays mounted (hidden) from then on, so its draft survives
   * switching tabs. */
  const [memSeen, setMemSeen] = useState(tab === "memory");
  /** Closing with unsaved memory edits asks first (a bar in the footer). */
  const [askDiscard, setAskDiscard] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const profileRef = useRef<HTMLSelectElement>(null);
  const focusModel = useRef(false);
  const focusEffort = useRef(false);
  const [permsOpen, setPermsOpen] = useState(false);
  const [usage, setUsage] = useState(false);
  const [more, setMore] = useState(false);
  const id = useId();

  useEffect(() => {
    if (editing || templateCache) return;
    rpc.call<AgentTemplate[]>("agent.templates").then((t) => {
      templateCache = t;
      setTemplates(t);
      const want = template ? t.find((x) => x.id === template) : undefined;
      if (want) {
        setTpl(want.id);
        setD((x) => ({ ...fromTemplate(want), name: x.name.trim() ? x.name : want.name }));
      }
    }).catch(() => {});
  }, []);
  // Daemons without `agent.models` leave only Default and Custom… in the selects.
  useEffect(() => {
    if (catalogCache?.antigravity) return;
    rpc.call<ModelCatalog>("agent.models").then((c) => {
      catalogCache = c;
      setCatalog(c);
    }).catch(() => {});
  }, []);
  useLayoutEffect(() => {
    if (!editing) nameRef.current?.focus();
    else if (focus === "profile") profileRef.current?.focus();
  }, []);
  // OpenRouter's models, once OpenRouter is chosen (the daemon fetches and caches them).
  useEffect(() => {
    if (d.provider !== "openrouter" || (orCache && Date.now() - orCache.at < OR_CACHE_MS)) return;
    setOrLoading(true);
    rpc.call<OpenRouterModels>("openrouter.models").then((list) => {
      // An empty answer (the fetch failed) is asked again by the next dialog, not kept.
      if (list.models.length) orCache = { at: Date.now(), list };
      setOrList(list);
    }).catch((e) => {
      setOrList({ key_present: false, models: [], fetched_at: null, stale: false, error: String(e?.message ?? e) });
    }).finally(() => setOrLoading(false));
  }, [d.provider]);

  if (editing && !agent) return null;
  const set = (p: Partial<Draft>) => setD((x) => ({ ...x, ...p }));
  const orModels = orList?.models ?? [];
  /** The backend can run agents on OpenRouter (null: an older backend, no Provider choice). */
  const or = openRouter.value;
  /** A model or effort the new kind doesn't list goes back to Default (the kind already chosen:
   * nothing changes). */
  const setKind = (k: Agent["kind"]) => {
    if (k === d.kind) return;
    const provider = k === "antigravity" ? "subscription" : d.provider;
    const model = keepForKind(d.model, listedModels(provider, k, catalog, orModels));
    // Editing: the modes the switch gives it (`switchPermissions`), from the agent's own, so the
    // Permissions pill shows what it will run with (switching back restores them).
    const permissions = agent
      ? k === agent.kind ? storedPermissions(agent.permissions) : switchPermissions(storedPermissions(agent.permissions), agent.kind, k)
      : d.permissions;
    set({ kind: k, provider, model, effort: keepForKind(d.effort, effortsFor(catalog, k, model)), permissions });
    setCustom({ model: false, effort: false });
  };
  /** Switching provider keeps the model only when the other list has it (an OpenRouter id means
   * nothing to the subscription, and the other way round). */
  const setProvider = (p: Provider) => {
    if (p === d.provider) return;
    set({ provider: p, model: keepForKind(d.model, listedModels(p, d.kind, catalog, orModels)) });
    setCustom((c) => ({ ...c, model: false }));
  };
  /** A level the new model doesn't offer goes back to Default; a custom effort stays, except on a
   * model that takes no level at all (an Antigravity Orchestrator, or Antigravity's Default):
   * Effort hides there, and a hidden level would still be sent and refused by agy. */
  const setModel = (model: string, on: boolean) => {
    const after = effortsFor(catalog, d.kind, model);
    const none = !!catalog && after.length === 0;
    const effort = none ? "" : effortAfterModel(d.effort, effortsFor(catalog, d.kind, d.model), after, custom.effort);
    set({ model, effort });
    setCustom((c) => ({ ...c, model: on, effort: none ? false : c.effort }));
  };
  const pick = (t: AgentTemplate) => {
    setTpl(t.id);
    setCustom({ model: false, effort: false });
    const next = fromTemplate(t);
    setD((x) => ({ ...next, name: nameTouched && x.name.trim() ? x.name : next.name }));
  };
  const taken = agents.value.filter((a) => a.id !== agent?.id).map((a) => a.name);
  const nErr = editing ? null : nameError(d.name, taken);
  const warn = unattendedWarning(d.kind, d.accepts);
  const showEffort = catalog ? effortsFor(catalog, d.kind, d.model).length > 0 : d.kind !== "antigravity";
  const orBlocked = needsModel(d.provider, d.kind, d.model);
  const sleepMin = d.sleep ? parseSleepAfter(d.sleepAfter) : 0;
  const locationChanged = !!agent && JSON.stringify(d.working_location) !== JSON.stringify(agent.working_location ?? { mode: "home" });
  /** Moving a Claude agent to another account (docs/ACCOUNT_PROFILES.md). */
  const profileChoice = agent && !profileTouched ? agent.account_profile_id ?? "default" : d.account_profile;
  // Default and the profile it means (one at ~/.claude) are one account: no move between them.
  const moving = !!agent && agent.kind === "claude"
    && !sameChoice(shownChoice(accountProfiles.value, profileChoice), shownChoice(accountProfiles.value, agent.account_profile_id));
  const agentSession = agent?.session_id != null ? sessions.value.get(agent.session_id) : undefined;
  // Where its conversation is: with an earlier move still pending, the account it moves from.
  const fromName = profileName(
    accountProfiles.value,
    agent?.handoff ? agent.handoff.from : agent?.account_profile_id,
  );
  const toName = profileName(accountProfiles.value, profileChoice === "default" ? null : profileChoice);
  // Save notes asks the running process, on the account it started with.
  const runProfile = agentSession?.account_profile_id ?? null;
  const notesBlocked = moving
    ? saveNotesBlocked(
      agentSession,
      atLimit(accountUsage.value, shownChoice(accountProfiles.value, runProfile), Date.now()),
      profileName(accountProfiles.value, runProfile),
    )
    : null;
  // A move already waiting for its next start (none while another account is picked here).
  const waiting = agent && !moving ? pendingMove(agent, agentSession, accountProfiles.value) : null;
  const locationValid = d.working_location.mode === "home" || workspaces.value.some(w => w.is_git && w.id === (d.working_location as { workspace_id: number }).workspace_id);
  const canSave = locationValid && (!locationChanged || moving || conversation !== "") && !busy && !nErr && d.accepts.length > 0 && !orBlocked && sleepMin !== null;
  const checkoutHint = d.working_location.mode === "worktree"
    ? "Keeps a separate branch and working files for this agent. The worktree is reused on subsequent starts."
    : "Shares files, branch and uncommitted changes with anyone using this checkout.";
  /** Switching kind: an existing Claude or Antigravity agent on a backend that can. */
  const canSwitchKind = !!agent && !!agent.switch_kinds?.includes(agent.kind);
  /** The Kind choice differs from what it runs now. */
  const switching = !!agent && d.kind !== agent.kind;
  /** Self handoff: an existing Claude agent (staying Claude) on a backend that knows it. */
  const canSelfHandoff = !!agent && agent.kind === "claude" && d.kind === "claude" && agent.self_handoff !== undefined;
  /** Message tools up front: an existing Claude agent (staying Claude) on a backend that knows it. */
  const canToolsUpFront = !!agent && agent.kind === "claude" && d.kind === "claude" && agent.tools_up_front !== undefined;
  /** Lead agent: an existing Claude or Antigravity agent on a backend that knows it. */
  const canLead = !!agent && (agent.kind === "claude" || agent.kind === "antigravity") && agent.lead !== undefined;
  /** May start sessions in: an existing agent on a backend that knows it, offered every workspace
   * but the one its own tab is in (there it needs no leave). */
  const ownWorkspace = agent?.channel_id != null ? channels.value.find((c) => c.id === agent.channel_id)?.workspace_id : undefined;
  const startIn = agent && agent.spawn_workspaces !== undefined
    ? workspaces.value.filter((w) => w.id !== ownWorkspace).sort((x, y) => x.name.localeCompare(y.name))
    : [];
  /** The ticked ones that still exist (a removed workspace's id is dropped on save). */
  const startInPicked = d.spawnWorkspaces.filter((w) => startIn.some((x) => x.id === w));
  const moreSummary = [
    or && (d.provider === "openrouter" ? "OpenRouter" : "Subscription"),
    d.autostart ? "Autostart on" : "No autostart",
    d.sleep ? (sleepMin ? `Sleeps after ${sleepMin} min idle` : "Sleep time needs fixing") : "Never sleeps",
    canSelfHandoff && d.selfHandoff && "Can start itself fresh",
    canToolsUpFront && d.toolsUpFront && "Message tools up front",
    canLead && d.lead && "Lead agent",
    startInPicked.length > 0 && `May start in ${startInPicked.length === 1 ? startIn.find((w) => w.id === startInPicked[0])?.name : `${startInPicked.length} workspaces`}`,
  ].filter(Boolean).join(" · ");
  const keyHint = or && !or.key_present && (
    <span class="field-hint or-key-hint" id="agent-provider-hint">
      {d.provider === "openrouter"
        ? "This agent won't start: the backend has no OpenRouter key. "
        : "OpenRouter is off. "}
      It needs <code>OPENROUTER_API_KEY</code>{" "}
      in the environment of Muster's backend, read only when the backend starts. From a shell
      that has the key, run <code>env OPENROUTER_API_KEY=… muster reload</code>{" "}
      (terminals keep running).
    </span>
  );
  const toggleOrigin = (o: Origin, on: boolean) =>
    set({ accepts: ORIGINS.map((x) => x.o).filter((x) => (x === o ? on : d.accepts.includes(x))) });

  const save = async () => {
    if (!canSave) return;
    setBusy(true);
    setErr(null);
    try {
      if (!agent) {
        const a = await rpc.call<Agent>("agent.create", {
          working_location: d.working_location,
          name: d.name.trim(),
          kind: d.kind,
          account_profile: d.kind === "claude" ? d.account_profile : undefined,
          // Older backends don't know the field: sent only to one that has the choice.
          provider: or ? d.provider : undefined,
          model: d.model.trim() || undefined,
          effort: d.effort.trim() || undefined,
          persona: d.persona,
          accepts: d.accepts,
          permissions: d.permissions,
          autostart: d.autostart,
          // Older backends don't know the field: sent only when sleep is on.
          sleep_after_min: sleepMin || undefined,
        });
        knowAgent(a);
        overlay.value = null;
        A.revealSidebar();
        toast("success", `Created ${a.name}. Click its row in the sidebar to start it.`);
        A.focusActiveTerminalSoon();
        return;
      }
      const before = fromAgent(agent);
      const patch: Record<string, unknown> = {};
      if (locationChanged) {
        patch.working_location = d.working_location;
        patch.reset_conversation = conversation === "reset";
      }
      if (d.provider !== before.provider) patch.provider = d.provider;
      if (d.model.trim() !== before.model) patch.model = d.model.trim();
      if (d.effort.trim() !== before.effort) patch.effort = d.effort.trim();
      if (d.persona !== before.persona) patch.persona = d.persona;
      if (d.autostart !== before.autostart) patch.autostart = d.autostart;
      if ((agent.sleep_after_min ?? 0) !== sleepMin) patch.sleep_after_min = sleepMin;
      if (canSelfHandoff && d.selfHandoff !== before.selfHandoff) patch.self_handoff = d.selfHandoff;
      if (canToolsUpFront && d.toolsUpFront !== before.toolsUpFront) patch.tools_up_front = d.toolsUpFront;
      if (canLead && d.lead !== before.lead) patch.lead = d.lead;
      if (agent.spawn_workspaces !== undefined) {
        const opened = openedStartIn.current;
        const added = startInPicked.filter((w) => !opened.includes(w));
        const removed = opened.filter((w) => !d.spawnWorkspaces.includes(w));
        // Ids of removed workspaces drop off too.
        const next = [...new Set([...before.spawnWorkspaces.filter((w) => !removed.includes(w)), ...added])]
          .filter((w) => workspaces.value.some((x) => x.id === w))
          .sort((x, y) => x - y);
        if (next.join() !== before.spawnWorkspaces.join()) patch.spawn_workspaces = next;
      }
      if (switching) {
        patch.kind = d.kind;
        patch.now = kindNow;
      }
      if (d.accepts.join() !== before.accepts.join()) patch.accepts = d.accepts;
      // A switch maps the modes itself, on the record and on its tab (`agents::switch_permissions`):
      // sent only when changed by hand from what the switch gives, so a tab narrowed on its own
      // isn't widened back to the record's modes.
      const unedited = switching ? switchPermissions(before.permissions, agent.kind, d.kind) : before.permissions;
      if (JSON.stringify(d.permissions) !== JSON.stringify(unedited)) {
        patch.permissions = d.permissions;
      }
      if (moving) {
        patch.account_profile = profileChoice;
        patch.now = moveNow;
        patch.save_notes = saveNotes && !notesBlocked;
        if (moveReason.trim()) patch.handoff_reason = moveReason.trim();
        // A new conversation anyway: nothing to keep.
        if (locationChanged) patch.reset_conversation = true;
      }
      if (Object.keys(patch).length) {
        const a = await rpc.call<Agent & { switch?: AgentSwitch; kind_switch?: AgentKindSwitch }>(
          "agent.update",
          { name: agent.name, ...patch },
        );
        knowAgent(a);
        if (a.kind_switch) {
          const m = kindSwitchMessage(a.name, a.kind_switch);
          toast(m.level, m.text, 8000);
        } else if (a.switch) {
          const m = switchMessage(a.name, a.switch);
          toast(m.level, m.text, 8000);
        } else {
          const later = ["provider", "model", "effort", "permissions", "working_location"].some((k) => k in patch) && A.agentLive(agent);
          toast(
            "success",
            later ? "Saved. Run settings and working location apply from its next start." : "Saved.",
          );
        }
      }
      overlay.value = null;
      A.focusActiveTerminalSoon();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  // Unsaved memory edits are never dropped silently: closing asks first (Esc on the question
  // keeps editing).
  const onClose = () => {
    if (askDiscard) return setAskDiscard(false);
    if (memDirty.current) {
      setTab("memory");
      setAskDiscard(true);
      return;
    }
    close();
  };

  const title = agent ? agent.name : "New agent";
  return (
    <Modal
      class="dialog dialog-scroll dlg-calm agent-dlg"
      label={agent ? `Edit ${agent.name}` : "New agent"}
      describedBy={`${id}-about`}
      onClose={onClose}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (tab === "settings") save();
        }}
        onKeyDown={(e) => {
          if (tab === "settings" && e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            save();
          }
        }}
      >
        <div class="dlg-head">
          <h2>
            {agent && <span class={cx("ap-kind", "ap-kind-" + agent.kind)}>{KIND_LABEL[agent.kind]}</span>}
            {" "}
            {title}
            {agent?.provider === "openrouter" && <span class="or-badge or-badge-head">{OPENROUTER_BADGE}</span>}
          </h2>
          {agent
            ? <code class="agent-home" id={`${id}-about`}>{agent.home_dir}</code>
            : (
              <span class="sr-only" id={`${id}-about`}>
                A persistent agent with its own folder, memory/ and tab. You talk to it in its tab.
              </span>
            )}
          {agent && (
            <div class="dlg-tabs" role="tablist">
              {(["settings", "memory"] as const).map((t) => (
                <button
                  key={t}
                  type="button"
                  role="tab"
                  aria-selected={tab === t}
                  class={cx("dlg-tab", tab === t && "on")}
                  onClick={() => {
                    setTab(t);
                    if (t === "memory") setMemSeen(true);
                  }}
                >
                  {t === "settings" ? "Settings" : "Memory"}
                </button>
              ))}
            </div>
          )}
        </div>
        {agent && memSeen && <MemoryTab agent={agent} dirty={memDirty} hidden={tab !== "memory"} />}
        <div class="dlg-body" hidden={tab !== "settings"}>
          {!agent && templates.length > 0 && (
            <div class="tpl-block">
              <div class="tpl-list" role="radiogroup" aria-label="Template" aria-describedby={`${id}-tpl`}>
                {templates.map((t) => (
                  <button
                    key={t.id}
                    type="button"
                    role="radio"
                    aria-checked={tpl === t.id}
                    class={cx("tpl", tpl === t.id && "on")}
                    data-tpl={t.id}
                    data-tip={`${KIND_LABEL[t.kind]}: ${t.about}`}
                    onClick={() => pick(t)}
                  >
                    {t.label}
                  </button>
                ))}
              </div>
              <span class="field-hint tpl-about" id={`${id}-tpl`}>{templates.find((t) => t.id === tpl)?.about}</span>
            </div>
          )}
          {!agent && (
            <div class="hero-row">
              <input
                ref={nameRef}
                class="hero-input"
                value={d.name}
                placeholder="Agent name, e.g. reviewer"
                aria-label="Name"
                spellcheck={false}
                aria-invalid={!!d.name && !!nErr}
                aria-describedby={d.name && nErr ? `${id}-name-err` : undefined}
                onInput={(e) => {
                  setNameTouched(true);
                  set({ name: (e.target as HTMLInputElement).value });
                }}
              />
              <div class="seg seg-kind" role="radiogroup" aria-label="Agent CLI">
                {(["claude", "codex", "antigravity"] as const).map((k) => (
                  <button
                    key={k}
                    type="button"
                    role="radio"
                    aria-checked={d.kind === k}
                    class={cx("seg-btn", d.kind === k && "on")}
                    onClick={() => setKind(k)}
                  >
                    <KindIcon kind={k} size={13} /> {KIND_LABEL[k]}
                  </button>
                ))}
              </div>
            </div>
          )}
          {!agent && d.name && nErr && <span class="field-hint field-bad" id={`${id}-name-err`}>{nErr}</span>}
          {canSwitchKind && agent && (
            <div class="kind-row">
              <span class="field-label" id={`${id}-kind`}>Runs on</span>
              <div class="seg seg-kind" role="radiogroup" aria-labelledby={`${id}-kind`}>
                {(agent.switch_kinds ?? []).map((k) => (
                  <button
                    key={k}
                    type="button"
                    role="radio"
                    data-kind={k}
                    aria-checked={d.kind === k}
                    class={cx("seg-btn", d.kind === k && "on")}
                    onClick={() => setKind(k)}
                  >
                    <KindIcon kind={k} size={13} /> {KIND_LABEL[k]}
                  </button>
                ))}
              </div>
            </div>
          )}
          {switching && agent && (
            <div class="field pill-panel move-panel kind-panel" role="group" aria-labelledby={`${id}-switch`}>
              <span class="field-label" id={`${id}-switch`}>Switch to {KIND_LABEL[d.kind]}</span>
              <span class="field-hint move-explain">
                Its next start is a new conversation in {KIND_LABEL[d.kind]}. Its memory/ notes, tab, schedules,
                reminders and waiting messages stay{agent.lead ? ", and it stays a lead agent" : ""}. Its persona is
                written to {d.kind === "claude" ? "CLAUDE.md" : "AGENTS.md"}; the old{" "}
                {agent.kind === "claude" ? "CLAUDE.md" : "AGENTS.md"} is kept beside it.
                {d.kind === "antigravity" && " Claude-only settings (account, OpenRouter, tools up front, starting itself fresh) are cleared."}
              </span>
              <div class="seg" role="radiogroup" aria-label="When it switches">
                {([false, true] as const).map((now) => (
                  <button
                    key={String(now)}
                    type="button"
                    role="radio"
                    aria-checked={kindNow === now}
                    class={cx("seg-btn", kindNow === now && "on")}
                    onClick={() => setKindNow(now)}
                  >
                    {now ? "Now" : "At its next start"}
                  </button>
                ))}
              </div>
              {kindNow && (
                <span class="field-hint">
                  Restarts it only if it is idle. If it is busy, it switches at its next start.
                </span>
              )}
            </div>
          )}
          <div class="persona-field">
            <textarea
              class="quiet-input persona-input"
              rows={6}
              value={d.persona}
              aria-label="Persona"
              aria-describedby={`${id}-persona`}
              placeholder="Persona: who it is and how it works. Muster adds how memory/ and messages work."
              onInput={(e) => set({ persona: (e.target as HTMLTextAreaElement).value })}
            />
            <span class="field-hint persona-hint" id={`${id}-persona`}>
              Persona, written to {d.kind === "claude" ? "CLAUDE.md" : "AGENTS.md"}{" "}
              in its folder; its CLI reads it when a conversation starts.
            </span>
          </div>
          <div class="pills" role="group" aria-label="Agent settings">
            {d.provider !== "openrouter" && (
              <PickField
                label="Model"
                icon={<IconCpu size={13} />}
                value={d.model}
                picks={modelPicks(catalog, d.kind)}
                custom={custom.model}
                focusTyped={focusModel}
                describedBy={`${id}-default`}
                onChange={setModel}
              />
            )}
            {showEffort && <PickField
              label="Effort"
              icon={<IconGauge size={13} />}
              value={d.effort}
              picks={effortPicks(catalog, d.kind, d.model)}
              custom={custom.effort}
              focusTyped={focusEffort}
              describedBy={`${id}-default`}
              onChange={(effort, on) => {
                set({ effort });
                setCustom((c) => ({ ...c, effort: on }));
              }}
            />}
            {d.kind === "claude" && (
              <ProfilePill
                value={profileChoice}
                onChange={(choice) => {
                  setProfileTouched(true);
                  set({ account_profile: choice ?? "default" });
                }}
                tip={editing
                  ? "Another profile moves this agent to that account: a new conversation there, with its memory/ notes."
                  : undefined}
                selectRef={profileRef}
                usage={usage}
                onUsage={setUsage}
              />
            )}
            <PillSelect
              id="agent-repository"
              icon={<IconFolder size={13} />}
              label="Working repository"
              value={d.working_location.mode === "home" ? "" : String(d.working_location.workspace_id)}
              tip="Where it works. Add a repository as a Muster workspace to select it here. Identity and memory stay in the agent home."
              describedBy={`${id}-repo`}
              class={d.working_location.mode !== "home" ? "set" : undefined}
              onChange={(v) => {
                set({ working_location: v ? { mode: "worktree", workspace_id: Number(v) } : { mode: "home" } });
                setConversation("");
              }}
              options={[
                { value: "", label: "Agent home (no repository)", short: "Agent home" },
                ...workspaces.value.filter((w) => w.is_git).map((w) => ({
                  value: String(w.id),
                  label: `${w.name} — ${w.path}`,
                  short: w.name,
                })),
              ]}
            />
            {d.working_location.mode !== "home" && (
              <PillSelect
                id="agent-checkout"
                icon={<IconBranch size={13} />}
                label="Working checkout"
                value={d.working_location.mode}
                tip={checkoutHint}
                describedBy={`${id}-checkout`}
                onChange={(v) => {
                  if (d.working_location.mode !== "home") {
                    set({ working_location: { ...d.working_location, mode: v as "worktree" | "main" } });
                    setConversation("");
                  }
                }}
                options={[
                  { value: "worktree", label: "Dedicated worktree (recommended)", short: "Own worktree" },
                  { value: "main", label: "Repository's main checkout", short: "Main checkout" },
                ]}
              />
            )}
            <PermissionsPill
              value={d.permissions}
              only={d.kind}
              open={permsOpen}
              onToggle={setPermsOpen}
              controls={`${id}-perms`}
            />
          </div>
          <span class="sr-only" id={`${id}-default`}>Default is the CLI's own choice.</span>
          <span class="sr-only" id={`${id}-repo`}>
            Add a repository as a Muster workspace to select it here. Identity and memory stay in the agent home.
          </span>
          {d.working_location.mode !== "home" && <span class="sr-only" id={`${id}-checkout`}>{checkoutHint}</span>}
          {d.provider !== "openrouter" && choiceOf(d.model, modelPicks(catalog, d.kind).map((p) => p.value), custom.model) === CUSTOM && (
            <CustomPick
              label="Custom model"
              value={d.model}
              placeholder={d.kind === "antigravity" ? "e.g. gemini-4-neon-high" : d.kind === "claude" ? "a model id, e.g. claude-opus-5-5" : "a model id"}
              focusTyped={focusModel}
              onChange={(v) => setModel(v, true)}
            />
          )}
          {showEffort && choiceOf(d.effort, effortPicks(catalog, d.kind, d.model).map((p) => p.value), custom.effort) === CUSTOM && (
            <CustomPick
              label="Custom effort"
              value={d.effort}
              placeholder="a level the CLI accepts"
              focusTyped={focusEffort}
              onChange={(effort) => {
                set({ effort });
                setCustom((c) => ({ ...c, effort: true }));
              }}
            />
          )}
          {d.provider === "openrouter" && (
            <OpenRouterModelField
              kind={d.kind}
              value={d.model}
              custom={custom.model}
              list={orList}
              loading={orLoading}
              onChange={setModel}
            />
          )}
          {orBlocked && (
            <span class="field-hint field-bad">
              Codex on OpenRouter needs a model with its vendor, e.g. openai/gpt-6.1-sol.
            </span>
          )}
          {d.kind === "claude" && (!editing || usage) && (
            <ProfileUsage
              value={profileChoice}
              table={usage}
              onUse={(choice) => {
                setProfileTouched(true);
                set({ account_profile: choice });
                profileRef.current?.focus();
              }}
            />
          )}
          {permsOpen && (
            <PermissionsPanel
              id={`${id}-perms`}
              value={d.permissions}
              onChange={(p) => set({ permissions: p })}
              only={d.kind}
              note="The profile of the agent's own tab. Only you can change it; it applies from the agent's next start."
            />
          )}
          {moving && agent && (
            <div class="field pill-panel move-panel" role="group" aria-labelledby={`${id}-move`}>
              <span class="field-label" id={`${id}-move`}>Move to {toName}</span>
              <span class="field-hint move-explain">
                Moving to {toName} starts a new conversation. Its memory/ notes come along. The current conversation
                stays on {fromName}.
              </span>
              <div class="seg" role="radiogroup" aria-label="When it moves">
                {([false, true] as const).map((now) => (
                  <button
                    key={String(now)}
                    type="button"
                    role="radio"
                    aria-checked={moveNow === now}
                    class={cx("seg-btn", moveNow === now && "on")}
                    onClick={() => setMoveNow(now)}
                  >
                    {now ? "Now" : "At its next start"}
                  </button>
                ))}
              </div>
              {moveNow && (
                <span class="field-hint">
                  Restarts it only if it is idle. If it is busy, it moves at its next start.
                </span>
              )}
              <label class={cx("check", notesBlocked && "muted")}>
                <input
                  type="checkbox"
                  checked={saveNotes && !notesBlocked}
                  disabled={!!notesBlocked}
                  aria-describedby={`${id}-notes`}
                  onChange={(e) => setSaveNotes((e.target as HTMLInputElement).checked)}
                />
                Wake it first to save notes
              </label>
              <span class="field-hint" id={`${id}-notes`}>
                {notesBlocked ?? (!saveNotes
                  ? "Off: nothing is sent to it. Its memory/ notes come along, and HANDOFF.md gives it its recent Muster messages."
                  : moveNow
                  ? `Muster first asks it to write what it needs into memory/ (a turn on ${fromName}). Once it replies, or after 10 minutes, it restarts on ${toName} if it is idle.`
                  : `Muster first asks it to write what it needs into memory/ (a turn on ${fromName}). It still moves at its next start.`)}
              </span>
              <input
                class="quiet-input move-reason"
                value={moveReason}
                maxLength={2000}
                placeholder="Reason (optional), for its handoff note"
                aria-label="Reason for the move"
                onInput={(e) => setMoveReason((e.target as HTMLInputElement).value)}
              />
            </div>
          )}
          {waiting && agent && (
            <div class="field pill-panel move-panel" role="group" aria-labelledby={`${id}-waiting`}>
              <span class="field-label" id={`${id}-waiting`}>{waiting.text}</span>
              <span class="field-hint" id={`${id}-waiting-hint`}>
                {waiting.running && !waiting.off
                  ? `Until then it keeps running on ${waiting.from}. Restarting starts a new conversation on ${waiting.to}; its memory/ notes come along.`
                  : waiting.running
                  ? `Until then it keeps running on ${waiting.from}.`
                  : waiting.why}
              </span>
              {waiting.running && (
                <div class="move-restart">
                  <button
                    type="button"
                    class="btn"
                    disabled={!waiting.restart}
                    // Kept focusable while the request runs (a disabled button drops focus).
                    aria-disabled={restarting || undefined}
                    aria-describedby={waiting.why ? `${id}-waiting-hint ${id}-waiting-why` : `${id}-waiting-hint`}
                    onClick={async (e) => {
                      if (restarting) return;
                      const button = e.currentTarget;
                      setRestarting(true);
                      await A.restartToMove(agent);
                      setRestarting(false);
                      // Unless the owner moved on: back to the button if it is still there, or to
                      // the account picker once the panel went (the move happened or changed).
                      const lost = document.activeElement === button || document.activeElement === document.body || !document.activeElement;
                      if (!lost) return;
                      if (button.isConnected && !button.disabled) button.focus();
                      else profileRef.current?.focus();
                    }}
                  >
                    Restart now
                  </button>
                  {waiting.why && <span class="field-hint" id={`${id}-waiting-why`}>{waiting.why}</span>}
                </div>
              )}
            </div>
          )}
          {locationChanged && !moving && (
            <div class="field pill-panel">
              <label class="field-label" for="agent-location-conversation">Conversation at next start</label>
              <span class="select-wrap">
                <select
                  id="agent-location-conversation"
                  required
                  value={conversation}
                  aria-describedby={`${id}-conv`}
                  onChange={(e) => setConversation(e.currentTarget.value as "" | "keep" | "reset")}
                >
                  <option value="">Choose how to continue</option>
                  <option value="keep">Keep the existing conversation</option>
                  <option value="reset">Start a new conversation</option>
                </select>
                <IconChevronDown size={14} />
              </span>
              <span class="field-hint" id={`${id}-conv`}>
                Saving does not interrupt the running agent. Its next start uses this location; persistent memory is kept either way.
              </span>
            </div>
          )}
          {agent?.working_directory && (
            <p class="field-hint">
              Current working directory: <code>{agent.working_directory}</code>
              {/* A waiting move has its own panel above. */}
              {!agent.handoff && agent.reset_on_next_start ? " · New conversation at next start" : ""}
            </p>
          )}
          <div class="inline-field">
            <span class="field-label" id={`${id}-origins`}>Takes messages started by</span>
            <div class="check-row chips" role="group" aria-labelledby={`${id}-origins`}>
              {ORIGINS.map((o) => (
                <label key={o.o} class={cx("check chip", d.accepts.includes(o.o) && "on")} data-tip={o.hint}>
                  <input
                    type="checkbox"
                    checked={d.accepts.includes(o.o)}
                    onChange={(e) => toggleOrigin(o.o, (e.target as HTMLInputElement).checked)}
                  />
                  {o.label}
                </label>
              ))}
            </div>
          </div>
          {warn && (
            <span class="perm-warn agent-warn" role="note">
              <IconShieldAlert size={13} /> {UNATTENDED_WARNING}
            </span>
          )}
          {d.accepts.length === 0 && <span class="field-hint field-bad">Pick at least one.</span>}
          {d.provider === "openrouter" && or && (
            <span class="perm-warn or-warn" role="note">
              <IconShieldAlert size={13} />{" "}
              {OPENROUTER_BADGE}: billed per token to your OpenRouter key, not your {KIND_LABEL[d.kind]}{" "}
              subscription.{" "}
              {d.kind === "claude"
                ? "Claude Code is built for Anthropic's models; others may not work."
                : "OpenAI's models work best with Codex."}
            </span>
          )}
          {d.provider === "openrouter" && or && !or.key_present && keyHint}
          <MoreOptions open={more} onToggle={setMore} summary={moreSummary}>
            {or && (
              <div class="inline-field">
                <span class="field-label" id="agent-provider-label">Provider</span>
                <div class="seg" role="radiogroup" aria-labelledby="agent-provider-label">
                  {(["subscription", "openrouter"] as const).map((p) => {
                    const off = p === "openrouter" && (d.kind === "antigravity" || (!or.key_present && d.provider !== "openrouter"));
                    return (
                      <button
                        key={p}
                        type="button"
                        role="radio"
                        aria-checked={d.provider === p}
                        aria-describedby={off && d.kind !== "antigravity" ? "agent-provider-hint" : undefined}
                        disabled={off}
                        data-provider={p}
                        class={cx("seg-btn", d.provider === p && "on")}
                        onClick={() => setProvider(p)}
                      >
                        {p === "subscription" ? "Subscription" : "OpenRouter"}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}
            {or && d.kind !== "antigravity" && d.provider !== "openrouter" && !or.key_present && keyHint}
            <label class="toggle">
              <input
                type="checkbox"
                checked={d.autostart}
                onChange={(e) => set({ autostart: (e.target as HTMLInputElement).checked })}
              />
              <span class="toggle-track">
                <span class="toggle-thumb" />
              </span>
              <span>
                Autostart <span class="muted">(start it whenever the daemon starts)</span>
              </span>
            </label>
            <div class="sleep-field">
              <label class="toggle">
                <input
                  type="checkbox"
                  checked={d.sleep}
                  onChange={(e) => set({ sleep: (e.target as HTMLInputElement).checked })}
                />
                <span class="toggle-track">
                  <span class="toggle-thumb" />
                </span>
                <span>
                  Sleep when idle <span class="muted">(stop its process; a message sent with wake on wakes it)</span>
                </span>
              </label>
              {d.sleep && (
                <div class="sleep-after">
                  <label class="sleep-after-input">
                    <span>After</span>
                    <input
                      class="sleep-min"
                      inputMode="numeric"
                      aria-label="Minutes idle before it sleeps"
                      aria-invalid={sleepMin === null}
                      aria-describedby="sleep-after-hint"
                      value={d.sleepAfter}
                      onInput={(e) => set({ sleepAfter: (e.target as HTMLInputElement).value })}
                    />
                    <span class="muted">minutes idle</span>
                  </label>
                  <span id="sleep-after-hint" class={cx("field-hint", sleepMin === null && "field-bad")}>
                    {sleepMin === null
                      ? `${SLEEP_AFTER_MIN} to ${SLEEP_AFTER_MAX} minutes.`
                      : "Waking re-reads its whole conversation, at full cost once the prompt cache has expired (about an hour on a Claude subscription), so a wake after that costs no more than any late message."}
                  </span>
                </div>
              )}
            </div>
            {canSelfHandoff && (
              <div class="sleep-field">
                <label class="toggle">
                  <input
                    type="checkbox"
                    data-field="self-handoff"
                    aria-describedby="self-handoff-hint"
                    checked={d.selfHandoff}
                    onChange={(e) => set({ selfHandoff: (e.target as HTMLInputElement).checked })}
                  />
                  <span class="toggle-track">
                    <span class="toggle-thumb" />
                  </span>
                  <span>
                    Can start itself fresh <span class="muted">(clear its own conversation from a handoff file)</span>
                  </span>
                </label>
                <span id="self-handoff-hint" class="field-hint sleep-after">
                  When its conversation has grown long, it writes a handoff file and asks. Once it is idle, Muster
                  clears the conversation (its process and background tasks keep running) and points it at the
                  file. At most once every 30 minutes. Only you can turn this on.
                </span>
              </div>
            )}
            {canToolsUpFront && (
              <div class="sleep-field">
                <label class="toggle">
                  <input
                    type="checkbox"
                    data-field="tools-up-front"
                    aria-describedby="tools-up-front-hint"
                    checked={d.toolsUpFront}
                    onChange={(e) => set({ toolsUpFront: (e.target as HTMLInputElement).checked })}
                  />
                  <span class="toggle-track">
                    <span class="toggle-thumb" />
                  </span>
                  <span>
                    Load message tools up front <span class="muted">(skip the tool search for them)</span>
                  </span>
                </label>
                <span id="tools-up-front-hint" class="field-hint sleep-after">
                  Puts Muster's message tools (ask_agent, tell_agent, reply and four more) in its prompt from the
                  start, so it doesn't look each one up first. About 2,200 more tokens on every request, mostly
                  cached. Applies from its next start.
                </span>
              </div>
            )}
            {canLead && (
              <div class="sleep-field">
                <label class="toggle">
                  <input
                    type="checkbox"
                    data-field="lead"
                    aria-describedby="lead-hint"
                    checked={d.lead}
                    onChange={(e) => set({ lead: (e.target as HTMLInputElement).checked })}
                  />
                  <span class="toggle-track">
                    <span class="toggle-thumb" />
                  </span>
                  <span>
                    Lead agent <span class="muted">(its messages from scheduled work count as yours)</span>
                  </span>
                </label>
                <span id="lead-hint" class="field-hint sleep-after">
                  In a turn a reminder or a schedule started, it can still start builders and message other
                  agents, which take its messages as if you sent them. Each message still counts one step,
                  and a chain stops after 8. Work a trigger started keeps the usual rules, and nothing scheduled
                  work reaches can set reminders or schedules. Only you can turn this on.
                </span>
              </div>
            )}
            {startIn.length > 0 && (
              <div class="sleep-field start-in-field">
                <span class="field-label" id={`${id}-start-in`}>May start sessions in</span>
                <div
                  class="check-row chips"
                  role="group"
                  aria-labelledby={`${id}-start-in`}
                  aria-describedby={`${id}-start-in-hint`}
                >
                  {startIn.map((w) => {
                    const on = d.spawnWorkspaces.includes(w.id);
                    return (
                      <label key={w.id} class={cx("check chip", on && "on")} data-tip={w.path}>
                        <input
                          type="checkbox"
                          data-field="spawn-workspace"
                          checked={on}
                          onChange={(e) => {
                            const checked = (e.target as HTMLInputElement).checked;
                            set({
                              spawnWorkspaces: checked
                                ? [...d.spawnWorkspaces, w.id].sort((x, y) => x - y)
                                : d.spawnWorkspaces.filter((x) => x !== w.id),
                            });
                          }}
                        />
                        {w.name}
                      </label>
                    );
                  })}
                </div>
                <span id={`${id}-start-in-hint`} class="field-hint">
                  Lets it start sessions in the ticked workspaces, so their CLAUDE.md, AGENTS.md and hooks apply. A
                  new tab there gets its own branch in a Git workspace. They get this agent's permissions. It can
                  follow and message the sessions it starts there, not your own. Only you can turn this on, here.
                </span>
              </div>
            )}
          </MoreOptions>
          {!more && sleepMin === null && (
            <span class="field-hint field-bad">
              Sleep when idle: {SLEEP_AFTER_MIN} to {SLEEP_AFTER_MAX} minutes (under More options).
            </span>
          )}
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          {askDiscard
            ? (
              <>
                <span class="dlg-foot-hint mem-discard">Discard your unsaved memory/ edits?</span>
                <button type="button" class="btn" onClick={() => setAskDiscard(false)}>Keep editing</button>
                <button type="button" class="btn btn-danger" onClick={close}>Discard</button>
              </>
            )
            : tab === "settings"
            ? (
              <>
                <span class="muted dlg-foot-hint">
                  <Kbd k="Ctrl+Enter" /> {agent ? "save" : "create"}
                </span>
                <button type="button" class="btn" onClick={onClose}>Cancel</button>
                <button type="submit" class="btn btn-primary" disabled={!canSave}>
                  {busy ? <Spinner size={12} /> : null} {agent ? "Save" : "Create agent"}
                </button>
              </>
            )
            : <button type="button" class="btn" onClick={onClose}>Close</button>}
        </div>
      </form>
    </Modal>
  );
}

interface MemFile {
  path: string;
  size: number;
  modified: number | null;
}

/** What the last Add files… (or drop) did: each file added (and under which name) or skipped. */
interface AddResult {
  added: { from: string; path: string; renamed: boolean }[];
  skipped: { name: string; why: string }[];
}

const errText = (e: unknown) => e instanceof Error ? e.message : String(e);

/** "a", "a and b", "a, b and c". */
function listing(items: string[]): string {
  return items.length < 2 ? items.join("") : `${items.slice(0, -1).join(", ")} and ${items.at(-1)}`;
}

function MemoryTab(
  { agent, dirty, hidden }: { agent: Agent; dirty: { current: boolean }; hidden: boolean },
) {
  const [files, setFiles] = useState<MemFile[] | null>(null);
  const [sel, setSel] = useState<string | null>(null);
  const [text, setText] = useState("");
  /** What's on disk for `sel` (null: a new file not saved yet). */
  const [orig, setOrig] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [added, setAdded] = useState<AddResult | null>(null);
  /** Asking before deleting the open file (a bar under its name, like the unsaved-changes one). */
  const [delAsk, setDelAsk] = useState(false);
  /** Files are dragged over the tab (enter/leave counted: children fire their own). */
  const [drag, setDrag] = useState(false);
  const dragDepth = useRef(0);
  const picker = useRef<HTMLInputElement>(null);
  const newRef = useRef<HTMLInputElement>(null);
  const delCancel = useRef<HTMLButtonElement>(null);
  const delBtn = useRef<HTMLButtonElement>(null);
  const addBtn = useRef<HTMLButtonElement>(null);
  /** Bumped by every edit and every change of the open file. A read, save or delete that was
   * started before a bump must not overwrite what the human did since (the RPCs are async). */
  const gen = useRef(0);
  const bump = () => ++gen.current;
  /** The open file right now (for async results; the closure's `sel` is the one they started with). */
  const selNow = useRef<string | null>(null);
  selNow.current = sel;
  /** Files picked or dropped while a batch is still being added: added after it, never lost. */
  const queue = useRef<File[]>([]);
  const isDirty = sel !== null && text !== (orig ?? "");
  dirty.current = isDirty || (sel !== null && orig === null && text !== "");
  const personaFile = agent.kind === "claude" ? "CLAUDE.md" : "AGENTS.md";
  useEffect(() => {
    if (delAsk) delCancel.current?.focus();
  }, [delAsk]);
  /** Close the delete question; Cancel and Esc put the keyboard back on Delete. */
  const cancelDel = () => {
    setDelAsk(false);
    setTimeout(() => delBtn.current?.focus());
  };

  const refresh = () =>
    rpc.call<{ files: MemFile[] }>("agent.memory.list", { name: agent.name })
      .then((r) => setFiles(r.files))
      .catch((e) => setErr(e instanceof Error ? e.message : String(e)));
  useEffect(() => {
    refresh();
  }, [agent.name]);

  /** Open `path`. The text is applied only if nothing was edited or opened meanwhile. */
  const load = async (path: string) => {
    setPending(null);
    setDelAsk(false);
    setErr(null);
    const g = bump();
    try {
      const r = await rpc.call<{ text: string }>("agent.memory.read", { name: agent.name, path });
      if (gen.current !== g) return;
      setSel(path);
      setText(r.text);
      setOrig(r.text);
    } catch (e) {
      if (gen.current === g) setErr(errText(e));
    }
  };
  const open = (path: string) => {
    if (path === sel) return;
    if (isDirty) setPending(path);
    else load(path);
  };
  const save = async () => {
    if (sel === null || busy) return;
    const [path, saved] = [sel, text];
    setBusy(true);
    setErr(null);
    try {
      await rpc.call("agent.memory.write", { name: agent.name, path, text: saved });
      // What's on disk is what was sent, whatever was typed since; another file opened
      // meanwhile has its own.
      if (selNow.current === path) setOrig(saved);
      await refresh();
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };
  const nErr = newName.trim() ? memoryPathError(newName) : null;
  const create = () => {
    const p = newName.trim();
    if (!p || nErr) return;
    setNewName("");
    if (files?.some((f) => f.path === p)) return open(p);
    if (isDirty) {
      setErr("Save or discard the open file first.");
      return;
    }
    bump();
    setSel(p);
    setText("");
    setOrig(null);
    setDelAsk(false);
  };

  /** Copy picked or dropped files into memory/ (`agent.memory.add`: a taken name gets `-2`,
   * nothing is replaced). Binary, huge and hidden files are skipped and listed with the reason.
   * Files that arrive while a batch runs join it (the result note lists them all). */
  const addFiles = async (list: File[]) => {
    if (!list.length) return;
    queue.current.push(...list);
    if (queue.current.length > list.length) return; // a batch is running and will take them
    setAdding(true);
    setErr(null);
    setAdded(null);
    const res: AddResult = { added: [], skipped: [] };
    for (let f = queue.current[0]; f; queue.current.shift(), f = queue.current[0]) {
      const name = f.name || "(no name)";
      const r = await readMemoryFile(f);
      if ("skip" in r) {
        res.skipped.push({ name, why: r.skip });
        continue;
      }
      try {
        const out = await rpc.call<{ path: string; renamed: boolean }>("agent.memory.add", {
          name: agent.name,
          file_name: f.name,
          text: r.text,
        });
        res.added.push({ from: name, path: out.path, renamed: out.renamed });
      } catch (e) {
        res.skipped.push({ name, why: errText(e) });
      }
    }
    await refresh();
    setAdded(res);
    setAdding(false);
    // Show what was added, unless that would take the place of unsaved edits (`load` itself
    // gives way to anything edited or opened while it reads).
    if (res.added.length && !dirty.current) load(res.added[0].path);
  };
  const pick = () => picker.current?.click();
  const del = async () => {
    if (sel === null) return;
    const path = sel;
    const onDisk = orig !== null;
    setDelAsk(false);
    const g = gen.current;
    if (onDisk) {
      setBusy(true);
      setErr(null);
      try {
        await rpc.call("agent.memory.delete", { name: agent.name, path });
      } catch (e) {
        setErr(errText(e));
        setBusy(false);
        return;
      }
      setBusy(false);
    }
    if (gen.current === g) {
      // Nothing edited or opened while it was deleted: close the file.
      bump();
      setSel(null);
      setText("");
      setOrig(null);
      setPending(null);
      setTimeout(() => addBtn.current?.focus());
    } else if (selNow.current === path) {
      // Edited meanwhile: keep the edits, as a file that isn't saved any more.
      setOrig(null);
    }
    await refresh();
  };
  const hasFiles = (e: DragEvent) => !!e.dataTransfer?.types.includes("Files");

  const shown = useMemo(() => {
    const list = [...(files ?? [])];
    if (sel !== null && orig === null && !list.some((f) => f.path === sel)) {
      list.push({ path: sel, size: 0, modified: null });
    }
    return list.sort((a, b) => a.path.localeCompare(b.path));
  }, [files, sel, orig]);

  const empty = files !== null && shown.length === 0;
  return (
    <div
      class={cx("dlg-body mem", drag && "mem-dragging")}
      hidden={hidden}
      onDragEnter={(e) => {
        if (!hasFiles(e)) return;
        e.preventDefault();
        dragDepth.current++;
        setDrag(true);
      }}
      onDragOver={(e) => {
        if (!hasFiles(e)) return;
        e.preventDefault();
        e.dataTransfer!.dropEffect = "copy";
      }}
      onDragLeave={(e) => {
        if (!hasFiles(e)) return;
        if (--dragDepth.current <= 0) {
          dragDepth.current = 0;
          setDrag(false);
        }
      }}
      onDrop={(e) => {
        if (!hasFiles(e)) return;
        e.preventDefault();
        dragDepth.current = 0;
        setDrag(false);
        addFiles([...(e.dataTransfer?.files ?? [])]);
      }}
    >
      <input
        ref={picker}
        type="file"
        multiple
        hidden
        aria-hidden="true"
        tabIndex={-1}
        onChange={(e) => {
          const input = e.target as HTMLInputElement;
          const picked = [...(input.files ?? [])];
          input.value = ""; // the same file can be picked again
          addFiles(picked);
        }}
      />
      {drag && (
        <div class="mem-drop" aria-hidden="true">
          {adding ? "Drop to add these after the files being added" : "Drop to add to memory/"}
        </div>
      )}
      <div class="mem-side">
        <button ref={addBtn} type="button" class="btn btn-sm mem-add" onClick={pick}>
          {adding ? <Spinner size={11} /> : <IconPlus size={12} />} Add files…
        </button>
        <div class="mem-files" role="listbox" aria-label="memory/ files">
          {files === null && <span class="muted mem-note">Loading…</span>}
          {empty && <span class="muted mem-note">No files yet.</span>}
          {shown.map((f) => (
            <button
              key={f.path}
              type="button"
              role="option"
              aria-selected={f.path === sel}
              class={cx("mem-file", f.path === sel && "on")}
              data-tip={f.modified ? `${f.size} bytes` : "not saved yet"}
              onClick={() => open(f.path)}
            >
              {f.path}
            </button>
          ))}
        </div>
        <div class="mem-new">
          <input
            ref={newRef}
            value={newName}
            placeholder="new-note.md"
            spellcheck={false}
            aria-label="New note: file name"
            aria-invalid={!!nErr}
            onInput={(e) => setNewName((e.target as HTMLInputElement).value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                e.stopPropagation();
                create();
              }
            }}
          />
          <button
            type="button"
            class="btn btn-sm"
            aria-label="New note"
            data-tip="New note"
            disabled={!newName.trim() || !!nErr}
            onClick={create}
          >
            <IconPlus size={12} />
          </button>
        </div>
        {nErr && <span class="field-hint field-bad">{nErr}</span>}
      </div>
      <div class="mem-edit">
        {added && (
          <div class="mem-added" role="status">
            <div class="mem-added-text">
              {added.added.length > 0 && (
                <p>
                  Added {listing(added.added.map((a) => a.path))}
                  {added.added.some((a) => a.renamed) &&
                    ` (${
                      listing(added.added.filter((a) => a.renamed).map((a) => a.from))
                    } already existed, so ${
                      added.added.filter((a) => a.renamed).length === 1 ? "it got" : "they got"
                    } a number)`}. When a conversation starts, {agent.name} reads the memory/ files its persona
                  names (all of them if it names none) and searches the rest when a task needs it; to use{" "}
                  {added.added.length === 1 ? "it" : "them"}{" "}
                  now, ask it in its tab to read {added.added.length === 1 ? "it" : "them"}.
                </p>
              )}
              {added.skipped.map((s, i) => <p key={i} class="mem-skipped">Skipped {s.name}: {s.why}.</p>)}
            </div>
            <button
              type="button"
              class="btn btn-ghost btn-sm mem-added-x"
              aria-label="Dismiss"
              onClick={() => setAdded(null)}
            >
              <IconX size={11} />
            </button>
          </div>
        )}
        {sel === null && empty && (
          <div class="mem-intro">
            <h3>Nothing in memory yet</h3>
            <p>
              Memory is {agent.name}'s notebook: the files in <code>memory/</code> in its folder. Its{" "}
              <code>{personaFile}</code>{" "}
              tells it to keep its own notes there and, when a conversation starts, to read the files its persona
              names (all of them if it names none), searching the rest when a task needs it.
            </p>
            <p>Seed it with what it should know, like project facts, a style guide or past decisions.</p>
            <div class="mem-intro-actions">
              <button type="button" class="btn btn-primary" onClick={pick}>
                {adding ? <Spinner size={12} /> : null} Add files…
              </button>
              <button type="button" class="btn" onClick={() => newRef.current?.focus()}>New note</button>
            </div>
            <p class="muted mem-intro-hint">
              Or drop files here. Text files up to 256 KB each; a name that's taken gets a number.
            </p>
          </div>
        )}
        {sel === null && !empty && (
          <div class="mem-empty muted">
            {files === null ? "" : "Pick a file to read or edit it, or drop files here to add them."}
          </div>
        )}
        {sel !== null && (
          <>
            <div class="mem-head">
              <code>memory/{sel}</code>
              {isDirty && <span class="mem-dirty">unsaved</span>}
              <button
                ref={delBtn}
                type="button"
                class="btn btn-sm btn-ghost mem-del"
                disabled={busy}
                aria-expanded={delAsk}
                onClick={() => (orig === null && text === "" ? del() : setDelAsk(true))}
              >
                Delete
              </button>
              <button
                type="button"
                class="btn btn-sm btn-primary mem-save"
                disabled={busy || (!isDirty && orig !== null)}
                onClick={save}
              >
                {busy ? <Spinner size={11} /> : null} Save
              </button>
            </div>
            {delAsk && (
              <div
                class="mem-pending"
                role="alertdialog"
                aria-modal="false"
                aria-labelledby="mem-del-q"
                onKeyDown={(e) => {
                  // Esc answers this question (no), not the dialog.
                  if (e.key === "Escape") {
                    e.preventDefault();
                    e.stopPropagation();
                    cancelDel();
                  }
                }}
              >
                <span id="mem-del-q">
                  {orig === null
                    ? `Discard ${sel}? It isn't saved yet.`
                    : `Delete memory/${sel}${
                      isDirty ? " and your unsaved changes" : ""
                    }? This can't be undone.`}
                </span>
                <button
                  ref={delCancel}
                  type="button"
                  class="btn btn-sm"
                  aria-describedby="mem-del-q"
                  onClick={cancelDel}
                >
                  Cancel
                </button>
                <button
                  type="button"
                  class="btn btn-sm btn-danger mem-del-yes"
                  aria-describedby="mem-del-q"
                  onClick={del}
                >
                  {orig === null ? "Discard" : "Delete"}
                </button>
              </div>
            )}
            {pending !== null && (
              <div class="mem-pending">
                Unsaved changes to {sel}.
                <button type="button" class="btn btn-sm" onClick={() => load(pending)}>Discard</button>
                <button type="button" class="btn btn-sm" onClick={() => setPending(null)}>
                  Keep editing
                </button>
              </div>
            )}
            <textarea
              class="mem-text"
              value={text}
              spellcheck={false}
              aria-label={`memory/${sel}`}
              onInput={(e) => {
                bump();
                setText((e.target as HTMLTextAreaElement).value);
              }}
              onKeyDown={(e) => {
                if (e.key === "s" && (e.ctrlKey || e.metaKey)) {
                  e.preventDefault();
                  save();
                }
              }}
            />
          </>
        )}
        {err && <div class="form-error">{err}</div>}
      </div>
    </div>
  );
}

// ---------------- a message and its reply ----------------

export function MessageDialog({ id, Modal, close }: { id: number; Modal: ModalT; close: () => void }) {
  const m = agentMessages.value.find((x) => x.id === id);
  if (!m) return null;
  const when = new Date(m.created_at).toLocaleString();
  // This message carried another one's late reply.
  const carried = carriesLateReply(m);
  const quiet = quietStatus(m);
  // The message that carried its later reply, when that went quietly: where it stands now.
  const carrier = m.answer?.msg != null ? agentMessages.value.find((x) => x.id === m.answer!.msg) : undefined;
  const late = m.answer ? lateStatus(m.answer, carrier) : null;
  return (
    <Modal class="dialog dialog-scroll dlg-calm msg-dlg" label={`Message ${m.id}`}>
      <div class="dlg-head">
        <h2>
          {m.from} <span class="ap-arrow">→</span> {m.to}
        </h2>
        <span class="muted msg-meta">
          Message {m.id} · {when} · <OriginChip origin={m.origin} /> <span class="ap-chip">hop {m.hop}</span>
          {m.reply_to !== null && (carried
            ? ` · late reply to message ${m.reply_to}`
            : ` · while handling message ${m.reply_to}`)}
          {m.cold_idle_ms != null && <>{" · "}<ColdChip ms={m.cold_idle_ms} /></>}
          {quiet && <>{" · "}<QuietChip m={m} /></>}
        </span>
      </div>
      <div class="dlg-body">
        {quiet && <div class="msg-quiet muted">{quiet.text}</div>}
        {m.refused && (
          <div class="form-error msg-refused">
            <IconShieldAlert size={13} /> Refused, never delivered: {m.refused}
          </div>
        )}
        <div class="field">
          <span class="field-label">Message</span>
          {m.text !== null
            ? <pre class="msg-pre" tabIndex={0}>{m.text}</pre>
            : <span class="muted">Not kept (sent before muster stored message text).</span>}
        </div>
        {!m.refused && (
          <div class="field">
            <span class="field-label">
              Reply{" "}
              {m.replied_at && <span class="muted">({new Date(m.replied_at).toLocaleTimeString()})</span>}
            </span>
            {m.reply !== null
              ? <pre class="msg-pre" tabIndex={0}>{m.reply}</pre>
              : m.answer
              ? <span class="muted">None in that turn; the agent answered later (below).</span>
              : quiet
              ? <span class="muted">None: a quiet message gets no reply of its own. The agent can answer it with its reply tool.</span>
              : <span class="muted">No reply yet. Replies arrive when the agent's turn ends.</span>}
          </div>
        )}
        {m.answer && (
          <div class="field">
            <span class="field-label">
              Later reply <span class="muted">({new Date(m.answer.at).toLocaleTimeString()})</span>
            </span>
            <pre class="msg-pre" tabIndex={0}>{m.answer.text}</pre>
            <span
              role="status"
              aria-live="polite"
              class={cx("msg-late-status", late?.failed ? "form-error" : "muted")}
            >
              {late?.text}
            </span>
          </div>
        )}
      </div>
      <div class="dlg-foot">
        {m.text !== null && (
          <button
            type="button"
            class="btn btn-ghost dlg-foot-left"
            onClick={() => A.copy(m.text!, "Message copied")}
          >
            Copy message
          </button>
        )}
        {agents.value.some((a) => a.id === m.to_agent) && (
          <button
            type="button"
            class="btn"
            onClick={() => {
              const a = agents.value.find((x) => x.id === m.to_agent)!;
              overlay.value = null;
              A.openAgent(a.name);
            }}
          >
            Show {m.to}
          </button>
        )}
        <button type="button" class="btn btn-primary" onClick={close}>Close</button>
      </div>
    </Modal>
  );
}

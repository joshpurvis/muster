// Named agents (docs/UI.md "Agents panel"): the left dock (rows, message log), the New / Edit
// agent dialog with its Memory tab, and the message overlay. Talking to an agent happens in its
// own terminal tab; the panel only starts, finds and configures agents.
import { type ComponentChildren, Fragment } from "preact";
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { AgentSchedules, agentsHeaderMenu, moveRow, SchedulesPausedNotice } from "./Schedules.tsx";
import {
  schedulesSupported,
  agentFilter,
  cacheNow,
  agentMessages,
  agents,
  agentsPanelOpen,
  agentsSupported,
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
  agentBilling,
  agentStatus,
  ago,
  choiceOf,
  contextLabel,
  CUSTOM,
  effortAfterModel,
  effortsFor,
  feedFor,
  firstLine,
  keepForKind,
  listedModels,
  memoryPathError,
  nameError,
  needsModel,
  OPENROUTER_BADGE,
  PREFERRED_VENDOR,
  priceLabel,
  readMemoryFile,
  UNATTENDED_WARNING,
  unattendedWarning,
  upsertMessage,
  vendorGroups,
} from "../agents.ts";
import type {
  Agent,
  AgentMessage,
  AgentTemplate,
  ModelCatalog,
  OpenRouterModel,
  OpenRouterModels,
  Origin,
  Permissions,
  ProfileChoice,
  Provider,
} from "../types.ts";
import { DEFAULT_PERMISSIONS, KIND_LABEL } from "../types.ts";
import { Kbd, Spinner, StateDot } from "./common.tsx";
import { openMenu } from "./Menu.tsx";
import { PermissionsSection } from "./Permissions.tsx";
import { ProfileSelect } from "./AccountProfiles.tsx";
import {
  IconAgents,
  IconChevron,
  IconChevronDown,
  IconCodex,
  IconMore,
  IconPlus,
  IconShieldAlert,
  IconSpark,
  IconX,
} from "../icons.tsx";
import { cx } from "../util.ts";
import { idleText, isCold } from "../cache.ts";

/** Re-render every `ms` (the "time in state" column). */
function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

// ---------------- the panel ----------------

export function AgentsPanel() {
  const now = useNow();
  const list = agents.value;
  const sel = agentFilter.value;
  return (
    <aside class="agents-panel" aria-label="Agents">
      <div class="ap-head">
        <IconAgents size={14} />
        <span class="ap-title">Agents</span>
        {list.length > 0 && <span class="ap-count">{list.length}</span>}
        {agentsSupported.value && (
          <button
            type="button"
            class="ap-icon-btn ap-head-more"
            aria-label="Agents and schedules menu"
            aria-haspopup="menu"
            data-tip={schedulesSupported.value ? "New agent or schedule, pause all, settings" : "New agent"}
            onClick={(e) => agentsHeaderMenu(e.currentTarget)}
          >
            <IconMore size={13} />
          </button>
        )}
        <button
          type="button"
          class="ap-icon-btn ap-head-x"
          aria-label="Hide the agents panel"
          data-tip="Hide"
          data-keys="Alt+G"
          onClick={() => A.toggleAgentsPanel()}
        >
          <IconX size={13} />
        </button>
      </div>
      {agentsSupported.value && <SchedulesPausedNotice />}
      {!agentsSupported.value
        ? <div class="ap-empty">This daemon has no agents. Update muster to use them.</div>
        : (
          <div class="ap-list" role="list">
            {list.length === 0 && (
              <div class="ap-empty">
                Persistent agents with their own folder and memory, which you and other agents can ask things.
              </div>
            )}
            {list.map((a) => (
              <Fragment key={a.id}>
                <AgentRow a={a} now={now} selected={sel === a.id} />
                <AgentSchedules agent={a} now={now} />
              </Fragment>
            ))}
            <button type="button" class="ap-new" onClick={() => A.newAgentDialog()}>
              <IconPlus size={12} /> New agent
            </button>
          </div>
        )}
      {agentsSupported.value && <Feed now={now} />}
    </aside>
  );
}

function agentMenu(a: Agent, x: number, y: number) {
  const live = A.agentLive(a);
  openMenu({
    x,
    y,
    minWidth: 190,
    items: [
      { header: a.name },
      { label: "Edit…", run: () => A.editAgent(a.name) },
      { label: "Memory…", run: () => A.editAgent(a.name, "memory") },
      ...(schedulesSupported.value ? [{ label: "New schedule…", run: () => A.newScheduleDialog(a.name) }] : []),
      "sep",
      { label: live ? "Show" : "Start", run: () => A.openAgent(a.name) },
      {
        label: "Start fresh",
        hint: a.kind === "claude" ? "new conversation" : undefined,
        run: () => A.openAgent(a.name, true),
      },
      { label: "Stop", disabled: !live, run: () => A.stopAgent(a) },
      { label: "Autostart", checked: a.autostart, run: () => A.setAgentAutostart(a, !a.autostart) },
      "sep",
      { label: "Remove…", danger: true, run: () => A.removeAgent(a) },
    ],
  });
}

function AgentRow({ a, now, selected }: { a: Agent; now: number; selected: boolean }) {
  const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
  const st = agentStatus(a, s);
  const billing = agentBilling(a, s);
  const cold = !!s && isCold(s, cacheNow());
  const persona = firstLine(a.persona);
  const open = () => {
    agentFilter.value = a.id;
    A.openAgent(a.name);
  };
  return (
    <div
      role="listitem"
      class={cx("ap-row", selected && "sel", !st.live && "stopped")}
      data-agent={a.name}
      data-tip={persona || undefined}
      tabIndex={0}
      aria-label={`${a.name}, ${KIND_LABEL[a.kind]}${billing.badge ? `, ${billing.note ?? OPENROUTER_BADGE}` : ""}, ${st.label}${
        cold ? ", prompt cache likely expired" : ""
      }`}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          open();
        } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          moveRow(e);
        } else if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
          e.preventDefault();
          const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
          agentMenu(a, r.left + 24, r.bottom - 4);
        }
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        agentMenu(a, e.clientX, e.clientY);
      }}
    >
      <StateDot state={st.state} />
      <span class="ap-row-text">
        <span class="ap-row-top">
          <span class="ap-name">{a.name}</span>
          <span class={cx("ap-kind", "ap-kind-" + a.kind)}>{KIND_LABEL[a.kind]}</span>
        </span>
        <span class="ap-row-sub">
          {st.label}
          {st.since !== null && ` · ${ago(st.since, now)}`}
          {a.autostart && <span class="ap-auto" data-tip="Starts with the daemon">auto</span>}
          {cold && (
            <span class="ap-cold" data-tip="Its prompt cache has likely expired: the next message re-reads the whole conversation">
              cold
            </span>
          )}
        </span>
        {billing.badge && (
          <span
            class={cx("or-badge", billing.note && "or-badge-pending")}
            data-tip={billing.note ?? "Billed per token to your OpenRouter key, not a subscription"}
          >
            {billing.label}
          </span>
        )}
      </span>
      <button
        type="button"
        class="ap-more"
        aria-label={`${a.name} menu`}
        onClick={(e) => {
          e.stopPropagation();
          const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
          agentMenu(a, r.left, r.bottom + 4);
        }}
      >
        <IconMore size={13} />
      </button>
    </div>
  );
}

/** Older messages are fetched on demand (`agent.messages` pages, newest first). */
const olderState = { busy: false, done: new Set<string>() };

function Feed({ now }: { now: number }) {
  const open = prefs.value.agentFeed;
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
      if (!r.more) olderState.done.add(key);
    } catch (e) {
      toast("error", `Couldn't load older messages: ${e instanceof Error ? e.message : e}`);
    } finally {
      olderState.busy = false;
      rerender((n) => n + 1);
    }
  };
  return (
    <section class={cx("ap-feed", open && "open")} aria-label="Messages">
      <div class="ap-feed-head">
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
              {selName ? `No messages to ${selName} yet.` : "Messages agents send each other show here."}
            </div>
          )}
          {list.map((m) => <FeedEntry key={m.id} m={m} now={now} />)}
          {list.length >= 20 && !olderState.done.has(selName ?? "*") && (
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

function FeedEntry({ m, now }: { m: AgentMessage; now: number }) {
  return (
    <button
      type="button"
      class={cx("ap-msg", m.refused && "refused")}
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
  p: { children: ComponentChildren; class?: string; label: string; onClose?: () => void },
) => ComponentChildren;

const ORIGINS: { o: Origin; label: string; hint: string }[] = [
  { o: "human", label: "Human", hint: "you, or an agent working on something you started" },
  { o: "schedule", label: "Schedule", hint: "a timer" },
  { o: "webhook", label: "Webhook", hint: "an outside trigger" },
];

/** One entry of a Model or Effort select. */
interface Pick {
  value: string;
  label: string;
}

/** Model and Effort: Default, the CLI's list (`agent.models`) and "Custom…", which shows a text
 * field for anything else (a full model id, a level the list doesn't have yet). A stored value
 * the list doesn't know shows as custom. `custom` is the dialog's latch (`choiceOf`): Custom
 * stays chosen until another entry is picked. */
function PickField(
  { label, value, picks, custom, customLabel, placeholder, onChange }: {
    label: string;
    value: string;
    picks: Pick[];
    custom: boolean;
    customLabel: string;
    placeholder: string;
    onChange: (v: string, custom: boolean) => void;
  },
) {
  const id = useId();
  const typed = useRef<HTMLInputElement>(null);
  /** Custom… was just picked: focus its field, the current value selected so typing replaces it. */
  const focusTyped = useRef(false);
  const choice = choiceOf(value, picks.map((p) => p.value), custom);
  useEffect(() => {
    if (focusTyped.current && typed.current) {
      focusTyped.current = false;
      typed.current.focus();
      typed.current.select();
    }
  });
  return (
    <div class="field">
      <label class="field-label" for={id}>{label}</label>
      <span class="select-wrap">
        <select
          id={id}
          value={choice}
          onChange={(e) => {
            const v = e.currentTarget.value;
            focusTyped.current = v === CUSTOM;
            onChange(v === CUSTOM ? value : v, v === CUSTOM);
          }}
        >
          <option value="">Default (the CLI's choice)</option>
          {picks.map((p) => <option key={p.value} value={p.value}>{p.label}</option>)}
          <option value={CUSTOM}>Custom…</option>
        </select>
        <IconChevronDown size={14} />
      </span>
      {choice === CUSTOM && (
        <input
          ref={typed}
          value={value}
          aria-label={customLabel}
          placeholder={placeholder}
          spellcheck={false}
          onInput={(e) => onChange((e.target as HTMLInputElement).value, true)}
        />
      )}
    </div>
  );
}

const modelPicks = (c: ModelCatalog | null, kind: Agent["kind"]): Pick[] =>
  (c?.[kind].models ?? []).map((m) => ({ value: m.id, label: m.note ? `${m.id} · ${m.note}` : m.id }));
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
}

function fromTemplate(t: AgentTemplate): Draft {
  return {
    account_profile: "default",
    provider: "subscription",
    name: t.name,
    kind: t.kind,
    model: t.model ?? "",
    effort: t.effort ?? "",
    persona: t.persona,
    permissions: { ...DEFAULT_PERMISSIONS, ...t.permissions },
    accepts: [...t.accepts],
    autostart: t.autostart,
  };
}

function fromAgent(a: Agent): Draft {
  return {
    account_profile: a.account_profile_id ?? "default",
    provider: a.provider ?? "subscription",
    name: a.name,
    kind: a.kind,
    model: a.model ?? "",
    effort: a.effort ?? "",
    persona: a.persona,
    permissions: { ...DEFAULT_PERMISSIONS, ...a.permissions },
    accepts: [...a.accepts_origins],
    autostart: a.autostart,
  };
}

const BLANK: Draft = {
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
};

let templateCache: AgentTemplate[] | null = null;
let catalogCache: ModelCatalog | null = null;

export function AgentDialog(
  { name, tab: tabInit, template, Modal, close }: {
    name?: string;
    tab?: "settings" | "memory";
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
  /** Model / Effort show Custom… until another entry is picked (`choiceOf`). */
  const [custom, setCustom] = useState({ model: false, effort: false });
  const [nameTouched, setNameTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const memDirty = useRef(false);
  /** The Memory tab was opened: it stays mounted (hidden) from then on, so its draft survives
   * switching tabs. */
  const [memSeen, setMemSeen] = useState(tab === "memory");
  /** Closing with unsaved memory edits asks first (a bar in the footer). */
  const [askDiscard, setAskDiscard] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

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
    if (catalogCache) return;
    rpc.call<ModelCatalog>("agent.models").then((c) => {
      catalogCache = c;
      setCatalog(c);
    }).catch(() => {});
  }, []);
  useLayoutEffect(() => {
    if (!editing) nameRef.current?.focus();
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
    const model = keepForKind(d.model, listedModels(d.provider, k, catalog, orModels));
    set({ kind: k, model, effort: keepForKind(d.effort, effortsFor(catalog, k, model)) });
    setCustom({ model: false, effort: false });
  };
  /** Switching provider keeps the model only when the other list has it (an OpenRouter id means
   * nothing to the subscription, and the other way round). */
  const setProvider = (p: Provider) => {
    if (p === d.provider) return;
    set({ provider: p, model: keepForKind(d.model, listedModels(p, d.kind, catalog, orModels)) });
    setCustom((c) => ({ ...c, model: false }));
  };
  /** A level the new model doesn't offer goes back to Default; a custom effort stays. */
  const setModel = (model: string, on: boolean) => {
    const effort = effortAfterModel(
      d.effort,
      effortsFor(catalog, d.kind, d.model),
      effortsFor(catalog, d.kind, model),
      custom.effort,
    );
    set({ model, effort });
    setCustom((c) => ({ ...c, model: on }));
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
  const orBlocked = needsModel(d.provider, d.kind, d.model);
  const canSave = !busy && !nErr && d.accepts.length > 0 && !orBlocked;
  const toggleOrigin = (o: Origin, on: boolean) =>
    set({ accepts: ORIGINS.map((x) => x.o).filter((x) => (x === o ? on : d.accepts.includes(x))) });

  const save = async () => {
    if (!canSave) return;
    setBusy(true);
    setErr(null);
    try {
      if (!agent) {
        const a = await rpc.call<Agent>("agent.create", {
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
        });
        knowAgent(a);
        overlay.value = null;
        if (!agentsPanelOpen.value) A.toggleAgentsPanel();
        toast("success", `Created ${a.name}. Click it in the agents panel to start it.`);
        A.focusActiveTerminalSoon();
        return;
      }
      const before = fromAgent(agent);
      const patch: Record<string, unknown> = {};
      if (d.provider !== before.provider) patch.provider = d.provider;
      if (d.model.trim() !== before.model) patch.model = d.model.trim();
      if (d.effort.trim() !== before.effort) patch.effort = d.effort.trim();
      if (d.persona !== before.persona) patch.persona = d.persona;
      if (d.autostart !== before.autostart) patch.autostart = d.autostart;
      if (d.accepts.join() !== before.accepts.join()) patch.accepts = d.accepts;
      if (JSON.stringify(d.permissions) !== JSON.stringify(before.permissions)) {
        patch.permissions = d.permissions;
      }
      if (Object.keys(patch).length) {
        const a = await rpc.call<Agent>("agent.update", { name: agent.name, ...patch });
        knowAgent(a);
        const later = ["provider", "model", "effort", "permissions"].some((k) => k in patch) && A.agentLive(agent);
        toast(
          "success",
          later ? "Saved. Provider, model, effort and permissions apply from its next start." : "Saved.",
        );
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
      class="dialog dialog-scroll agent-dlg"
      label={agent ? `Edit ${agent.name}` : "New agent"}
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
          <span class="muted">
            {agent
              ? <code class="agent-home">{agent.home_dir}</code>
              : "A persistent agent with its own folder, memory/ and tab. You talk to it in its tab."}
          </span>
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
        {
          <div class="dlg-body" hidden={tab !== "settings"}>
            {!agent && templates.length > 0 && (
              <div class="field">
                <span class="field-label">Start from</span>
                <div class="tpl-list" role="radiogroup" aria-label="Template">
                  {templates.map((t) => (
                    <button
                      key={t.id}
                      type="button"
                      role="radio"
                      aria-checked={tpl === t.id}
                      class={cx("tpl", tpl === t.id && "on")}
                      data-tpl={t.id}
                      onClick={() => pick(t)}
                    >
                      <span class="tpl-label">
                        {t.kind === "claude"
                          ? <IconSpark size={12} class="kind-claude" />
                          : <IconCodex size={12} class="kind-codex" />}
                        {t.label}
                      </span>
                    </button>
                  ))}
                </div>
                <span class="field-hint">{templates.find((t) => t.id === tpl)?.about}</span>
              </div>
            )}
            {!agent && (
              <div class="field-row">
                <label class="field">
                  <span class="field-label">Name</span>
                  <input
                    ref={nameRef}
                    value={d.name}
                    placeholder="e.g. reviewer"
                    spellcheck={false}
                    aria-invalid={!!d.name && !!nErr}
                    onInput={(e) => {
                      setNameTouched(true);
                      set({ name: (e.target as HTMLInputElement).value });
                    }}
                  />
                  {d.name && nErr && <span class="field-hint field-bad">{nErr}</span>}
                </label>
                <div class="field">
                  <span class="field-label">Runs</span>
                  <div class="seg seg-lg" role="radiogroup" aria-label="Agent CLI">
                    {(["claude", "codex"] as const).map((k) => (
                      <button
                        key={k}
                        type="button"
                        role="radio"
                        aria-checked={d.kind === k}
                        class={cx("seg-btn", d.kind === k && "on")}
                        onClick={() => setKind(k)}
                      >
                        {k === "claude" ? <IconSpark size={13} /> : <IconCodex size={13} />} {KIND_LABEL[k]}
                      </button>
                    ))}
                  </div>
                </div>
              </div>
            )}
            {d.kind === "claude" && <>
              <ProfileSelect value={d.account_profile} onChange={(choice) => set({ account_profile: choice ?? "default" })} disabled={editing} />
              {editing && <span class="field-hint">This agent keeps its profile and conversation together. Create a new agent to use another profile.</span>}
            </>}
            {or && (
              <div class="field">
                <span class="field-label" id="agent-provider-label">Provider</span>
                <div class="seg seg-lg" role="radiogroup" aria-labelledby="agent-provider-label">
                  {(["subscription", "openrouter"] as const).map((p) => {
                    const off = p === "openrouter" && !or.key_present && d.provider !== "openrouter";
                    return (
                      <button
                        key={p}
                        type="button"
                        role="radio"
                        aria-checked={d.provider === p}
                        aria-describedby={off ? "agent-provider-hint" : undefined}
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
                {!or.key_present && (
                  <span class="field-hint or-key-hint" id="agent-provider-hint">
                    {d.provider === "openrouter"
                      ? "This agent won't start: the backend has no OpenRouter key. "
                      : "OpenRouter is off. "}
                    It needs <code>OPENROUTER_API_KEY</code>{" "}
                    in the environment of Muster's backend, read only when the backend starts. From a shell
                    that has the key, run <code>env OPENROUTER_API_KEY=… muster reload</code>{" "}
                    (terminals keep running).
                  </span>
                )}
                {d.provider === "openrouter" && (
                  <span class="perm-warn or-warn" role="note">
                    <IconShieldAlert size={13} />{" "}
                    {OPENROUTER_BADGE}: billed per token to your OpenRouter key, not your {KIND_LABEL[d.kind]}{" "}
                    subscription.{" "}
                    {d.kind === "claude"
                      ? "Claude Code is built for Anthropic's models; others may not work."
                      : "OpenAI's models work best with Codex."}
                  </span>
                )}
              </div>
            )}
            <div class="field-row">
              {d.provider === "openrouter"
                ? (
                  <OpenRouterModelField
                    kind={d.kind}
                    value={d.model}
                    custom={custom.model}
                    list={orList}
                    loading={orLoading}
                    onChange={setModel}
                  />
                )
                : (
                  <PickField
                    label="Model"
                    value={d.model}
                    picks={modelPicks(catalog, d.kind)}
                    custom={custom.model}
                    customLabel="Custom model"
                    placeholder={d.kind === "claude" ? "a model id, e.g. claude-opus-5-5" : "a model id"}
                    onChange={setModel}
                  />
                )}
              <PickField
                label="Effort"
                value={d.effort}
                picks={effortPicks(catalog, d.kind, d.model)}
                custom={custom.effort}
                customLabel="Custom effort"
                placeholder="a level the CLI accepts"
                onChange={(effort, on) => {
                  set({ effort });
                  setCustom((c) => ({ ...c, effort: on }));
                }}
              />
            </div>
            {orBlocked && (
              <span class="field-hint field-bad">
                Codex on OpenRouter needs a model with its vendor, e.g. openai/gpt-6.1-sol.
              </span>
            )}
            <label class="field">
              <span class="field-label">Persona</span>
              <textarea
                class="persona-input"
                rows={7}
                value={d.persona}
                placeholder="Who it is and how it works. Muster adds how memory/ and messages work."
                onInput={(e) => set({ persona: (e.target as HTMLTextAreaElement).value })}
              />
              <span class="field-hint">
                Written to {d.kind === "claude" ? "CLAUDE.md" : "AGENTS.md"}{" "}
                in its folder; its CLI reads it when a conversation starts.
              </span>
            </label>
            <div class="field">
              <span class="field-label">Takes messages started by</span>
              <div class="check-row">
                {ORIGINS.map((o) => (
                  <label key={o.o} class="check" data-tip={o.hint}>
                    <input
                      type="checkbox"
                      checked={d.accepts.includes(o.o)}
                      onChange={(e) => toggleOrigin(o.o, (e.target as HTMLInputElement).checked)}
                    />
                    {o.label}
                  </label>
                ))}
              </div>
              {warn && (
                <span class="perm-warn agent-warn" role="note">
                  <IconShieldAlert size={13} /> {UNATTENDED_WARNING}
                </span>
              )}
              {d.accepts.length === 0 && <span class="field-hint field-bad">Pick at least one.</span>}
            </div>
            <PermissionsSection
              value={d.permissions}
              onChange={(p) => set({ permissions: p })}
              only={d.kind}
              note="The profile of the agent's own tab. Only you can change it; it applies from the agent's next start."
            />
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
            {err && <div class="form-error">{err}</div>}
          </div>
        }
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
                    } a number)`}. {agent.name} reads memory/ when a new conversation starts; to use{" "}
                  {added.added.length === 1 ? "it" : "them"}{" "}
                  in the current one, ask it in its tab to read memory/.
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
              tells it to read them when a conversation starts and to keep its own notes there.
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
  return (
    <Modal class="dialog msg-dlg" label={`Message ${m.id}`}>
      <div class="dlg-head">
        <h2>
          {m.from} <span class="ap-arrow">→</span> {m.to}
        </h2>
        <span class="muted msg-meta">
          Message {m.id} · {when} · <OriginChip origin={m.origin} /> <span class="ap-chip">hop {m.hop}</span>
          {m.reply_to !== null && ` · while handling message ${m.reply_to}`}
          {m.cold_idle_ms != null && <>{" · "}<ColdChip ms={m.cold_idle_ms} /></>}
        </span>
      </div>
      <div class="dlg-body">
        {m.refused && (
          <div class="form-error msg-refused">
            <IconShieldAlert size={13} /> Refused, never delivered: {m.refused}
          </div>
        )}
        <div class="field">
          <span class="field-label">Message</span>
          {m.text !== null
            ? <pre class="msg-pre">{m.text}</pre>
            : <span class="muted">Not kept (sent before muster stored message text).</span>}
        </div>
        {!m.refused && (
          <div class="field">
            <span class="field-label">
              Reply{" "}
              {m.replied_at && <span class="muted">({new Date(m.replied_at).toLocaleTimeString()})</span>}
            </span>
            {m.reply !== null
              ? <pre class="msg-pre">{m.reply}</pre>
              : <span class="muted">No reply yet. Replies arrive when the agent's turn ends.</span>}
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

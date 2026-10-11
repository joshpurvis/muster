// Named agents (docs/UI.md "Agents panel"): the left dock (rows, message log), the New / Edit
// agent dialog with its Memory tab, and the message overlay. Talking to an agent happens in its
// own terminal tab; the panel only starts, finds and configures agents.
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import {
  agentFilter,
  agentMessages,
  agents,
  agentsPanelOpen,
  agentsSupported,
  knowAgent,
  overlay,
  prefs,
  sessions,
  setPref,
  toast,
} from "../state.ts";
import * as A from "../actions.ts";
import {
  agentStatus,
  ago,
  feedFor,
  firstLine,
  memoryPathError,
  nameError,
  UNATTENDED_WARNING,
  unattendedWarning,
  upsertMessage,
} from "../agents.ts";
import type { Agent, AgentMessage, AgentTemplate, Origin, Permissions } from "../types.ts";
import { DEFAULT_PERMISSIONS, KIND_LABEL } from "../types.ts";
import { Kbd, Spinner, StateDot } from "./common.tsx";
import { openMenu } from "./Menu.tsx";
import { PermissionsSection } from "./Permissions.tsx";
import {
  IconAgents,
  IconChevron,
  IconCodex,
  IconMore,
  IconPlus,
  IconShieldAlert,
  IconSpark,
  IconX,
} from "../icons.tsx";
import { cx } from "../util.ts";

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
        <button
          type="button"
          class="ap-icon-btn"
          aria-label="Hide the agents panel"
          data-tip="Hide"
          data-keys="Alt+G"
          onClick={() => A.toggleAgentsPanel()}
        >
          <IconX size={13} />
        </button>
      </div>
      {!agentsSupported.value
        ? <div class="ap-empty">This daemon has no agents. Update muster to use them.</div>
        : (
          <div class="ap-list" role="list">
            {list.length === 0 && (
              <div class="ap-empty">
                Persistent agents with their own folder and memory, which you and other agents can ask things.
              </div>
            )}
            {list.map((a) => <AgentRow key={a.id} a={a} now={now} selected={sel === a.id} />)}
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
      aria-label={`${a.name}, ${KIND_LABEL[a.kind]}, ${st.label}`}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          open();
        } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          const rows = [...document.querySelectorAll<HTMLElement>(".ap-row")];
          const i = rows.indexOf(e.currentTarget as HTMLElement);
          rows[i + (e.key === "ArrowDown" ? 1 : -1)]?.focus();
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
        </span>
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

const EFFORTS: Record<Agent["kind"], string[]> = {
  claude: ["low", "medium", "high", "max"],
  codex: ["minimal", "low", "medium", "high", "xhigh"],
};

interface Draft {
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

export function AgentDialog(
  { name, tab: tabInit, Modal, close }: {
    name?: string;
    tab?: "settings" | "memory";
    Modal: ModalT;
    close: () => void;
  },
) {
  const editing = name !== undefined;
  const agent = editing ? agents.value.find((a) => a.name === name) : undefined;
  const [tab, setTab] = useState<"settings" | "memory">(editing ? tabInit ?? "settings" : "settings");
  const [templates, setTemplates] = useState<AgentTemplate[]>(templateCache ?? []);
  const [tpl, setTpl] = useState("blank");
  const [d, setD] = useState<Draft>(agent ? fromAgent(agent) : BLANK);
  const [nameTouched, setNameTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const memDirty = useRef(false);
  const escArmed = useRef(false);
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing || templateCache) return;
    rpc.call<AgentTemplate[]>("agent.templates").then((t) => {
      templateCache = t;
      setTemplates(t);
    }).catch(() => {});
  }, []);
  useLayoutEffect(() => {
    if (!editing) nameRef.current?.focus();
  }, []);

  if (editing && !agent) return null;
  const set = (p: Partial<Draft>) => setD((x) => ({ ...x, ...p }));
  const pick = (t: AgentTemplate) => {
    setTpl(t.id);
    const next = fromTemplate(t);
    setD((x) => ({ ...next, name: nameTouched && x.name.trim() ? x.name : next.name }));
  };
  const taken = agents.value.filter((a) => a.id !== agent?.id).map((a) => a.name);
  const nErr = editing ? null : nameError(d.name, taken);
  const warn = unattendedWarning(d.kind, d.accepts);
  const canSave = !busy && !nErr && d.accepts.length > 0;
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
        const later = ["model", "effort", "permissions"].some((k) => k in patch) && A.agentLive(agent);
        toast(
          "success",
          later ? "Saved. Model, effort and permissions apply from its next start." : "Saved.",
        );
      }
      overlay.value = null;
      A.focusActiveTerminalSoon();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  // Unsaved memory edits survive one Esc (a toast says why); a second one discards them.
  const onClose = () => {
    if (memDirty.current && !escArmed.current) {
      escArmed.current = true;
      toast("warn", "Unsaved changes in memory/. Save, or close again to discard them.");
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
                  onClick={() => setTab(t)}
                >
                  {t === "settings" ? "Settings" : "Memory"}
                </button>
              ))}
            </div>
          )}
        </div>
        {tab === "memory" && agent ? <MemoryTab agent={agent} dirty={memDirty} /> : (
          <div class="dlg-body">
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
                        onClick={() => set({ kind: k })}
                      >
                        {k === "claude" ? <IconSpark size={13} /> : <IconCodex size={13} />} {KIND_LABEL[k]}
                      </button>
                    ))}
                  </div>
                </div>
              </div>
            )}
            <div class="field-row">
              <label class="field">
                <span class="field-label">
                  Model <span class="muted">(optional)</span>
                </span>
                <input
                  value={d.model}
                  placeholder={d.kind === "claude" ? "the CLI's default, e.g. opus" : "the CLI's default"}
                  spellcheck={false}
                  onInput={(e) => set({ model: (e.target as HTMLInputElement).value })}
                />
              </label>
              <label class="field">
                <span class="field-label">
                  Effort <span class="muted">(optional)</span>
                </span>
                <input
                  value={d.effort}
                  list={`efforts-${d.kind}`}
                  placeholder="the CLI's default"
                  spellcheck={false}
                  onInput={(e) => set({ effort: (e.target as HTMLInputElement).value })}
                />
                <datalist id={`efforts-${d.kind}`}>
                  {EFFORTS[d.kind].map((e) => <option key={e} value={e} />)}
                </datalist>
              </label>
            </div>
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
        )}
        <div class="dlg-foot">
          {tab === "settings"
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

function MemoryTab({ agent, dirty }: { agent: Agent; dirty: { current: boolean } }) {
  const [files, setFiles] = useState<MemFile[] | null>(null);
  const [sel, setSel] = useState<string | null>(null);
  const [text, setText] = useState("");
  /** What's on disk for `sel` (null: a new file not saved yet). */
  const [orig, setOrig] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const isDirty = sel !== null && text !== (orig ?? "");
  dirty.current = isDirty || (sel !== null && orig === null && text !== "");

  const refresh = () =>
    rpc.call<{ files: MemFile[] }>("agent.memory.list", { name: agent.name })
      .then((r) => setFiles(r.files))
      .catch((e) => setErr(e instanceof Error ? e.message : String(e)));
  useEffect(() => {
    refresh();
  }, [agent.name]);

  const load = async (path: string) => {
    setPending(null);
    setErr(null);
    try {
      const r = await rpc.call<{ text: string }>("agent.memory.read", { name: agent.name, path });
      setSel(path);
      setText(r.text);
      setOrig(r.text);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };
  const open = (path: string) => {
    if (path === sel) return;
    if (isDirty) setPending(path);
    else load(path);
  };
  const save = async () => {
    if (sel === null || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await rpc.call("agent.memory.write", { name: agent.name, path: sel, text });
      setOrig(text);
      await refresh();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
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
    setSel(p);
    setText("");
    setOrig(null);
  };

  const shown = useMemo(() => {
    const list = [...(files ?? [])];
    if (sel !== null && orig === null && !list.some((f) => f.path === sel)) {
      list.push({ path: sel, size: 0, modified: null });
    }
    return list.sort((a, b) => a.path.localeCompare(b.path));
  }, [files, sel, orig]);

  return (
    <div class="dlg-body mem">
      <div class="mem-side">
        <div class="mem-files" role="listbox" aria-label="memory/ files">
          {files === null && <span class="muted mem-note">Loading…</span>}
          {files !== null && shown.length === 0 && (
            <span class="muted mem-note">Empty. The agent keeps notes here; you can seed a file.</span>
          )}
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
            value={newName}
            placeholder="new-file.md"
            spellcheck={false}
            aria-label="New file name"
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
          <button type="button" class="btn btn-sm" disabled={!newName.trim() || !!nErr} onClick={create}>
            <IconPlus size={12} />
          </button>
        </div>
        {nErr && <span class="field-hint field-bad">{nErr}</span>}
      </div>
      <div class="mem-edit">
        {sel === null ? <div class="mem-empty muted">Pick a file to read or edit it.</div> : (
          <>
            <div class="mem-head">
              <code>memory/{sel}</code>
              {isDirty && <span class="mem-dirty">unsaved</span>}
              <button
                type="button"
                class="btn btn-sm btn-primary mem-save"
                disabled={busy || (!isDirty && orig !== null)}
                onClick={save}
              >
                {busy ? <Spinner size={11} /> : null} Save
              </button>
            </div>
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
              onInput={(e) => setText((e.target as HTMLTextAreaElement).value)}
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

// Schedules in the agents panel (docs/UI.md "Agents panel"): each agent's schedules under its
// row with their run history, the "All schedules paused" notice, the panel header menu, and the
// New / Edit schedule and Scheduling settings dialogs. Runs are ordinary agent messages, so a
// run's message opens in the panel's message overlay.
import type { ComponentChildren } from "preact";
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import {
  agentMessages,
  agents,
  agentsPanelOpen,
  knowSchedule,
  overlay,
  scheduleHistoryOpen,
  loadScheduleRuns,
  scheduleRuns,
  schedules,
  scheduleSettings,
  schedulesSupported,
  toast,
} from "../state.ts";
import * as A from "../actions.ts";
import { notes } from "../notes.ts";
import { UNATTENDED_WARNING } from "../agents.ts";
import {
  type AgentChoice,
  blankWhen,
  DAY_NAMES,
  defaultScheduleAgent,
  draftOfWhen,
  expiresLabel,
  isExpired,
  isSubscriptionClaude,
  longWhen,
  missedAutoText,
  nextLabel,
  nextWhen,
  noteChoice,
  OUTCOME_LABEL,
  outcomeTone,
  parseCap,
  parseTime,
  PRESETS,
  runKindLabel,
  runLine,
  runReason,
  scheduleAgents,
  type ScheduleFields,
  schedulePatch,
  sortSchedules,
  type WhenDraft,
  whenOfDraft,
  whenToSend,
  segStep,
  blankThen,
  blankTrigger,
  draftOfThen,
  draftOfTrigger,
  isTrigger,
  slug,
  type ThenDraft,
  thenOfDraft,
  TRIGGER_KINDS,
  type TriggerDraft,
  type TriggerKind,
  watchLabel,
  whenOfTrigger,
  withSteps,
  nestedSteps,
  noteOptions,
  isProposal,
  madeByLabel,
  proposalFacts,
  waitingProposals,
} from "../schedules.ts";
import type { Agent, Schedule, SchedulePreview, ScheduleRun, ScheduleSettings, WhenInput } from "../types.ts";
import { KIND_LABEL } from "../types.ts";
import { Kbd, Spinner } from "./common.tsx";
import { openMenu, openMenuAt } from "./Menu.tsx";
import { IconChevron, IconChevronDown, IconClock, IconMore, IconPause, IconShieldAlert, IconSpark, IconZap } from "../icons.tsx";
import { cx } from "../util.ts";
import { idleText } from "../cache.ts";

/** "$1.23". */
export function money(v: number): string {
  return `$${v.toFixed(2)}`;
}

type ModalT = (
  p: { children: ComponentChildren; class?: string; label: string; onClose?: () => void },
) => ComponentChildren;

// ---------------- the panel ----------------

/** The panel header's ⋯: new agent or schedule, pause all, scheduling settings. */
export function agentsHeaderMenu(el: Element) {
  const sup = schedulesSupported.value;
  const paused = !!scheduleSettings.value?.paused;
  openMenuAt(el, [
    { label: "New agent…", run: () => A.newAgentDialog() },
    ...(sup
      ? [
        { label: "New schedule…", run: () => A.newScheduleDialog() },
        "sep" as const,
        { label: "Pause all schedules", checked: paused, run: () => A.setAllSchedulesPaused(!paused) },
        { label: "Scheduling settings…", run: () => A.scheduleSettingsDialog() },
      ]
      : []),
  ], { minWidth: 200, alignRight: false });
}

/** "All schedules paused · Resume", under the panel header while Pause all is on. */
export function SchedulesPausedNotice() {
  if (!schedulesSupported.value || !scheduleSettings.value?.paused) return null;
  return (
    <div class="sch-paused-note" role="status">
      <IconPause size={12} />
      <span>All schedules paused</span>
      <button type="button" class="sch-link" onClick={() => A.setAllSchedulesPaused(false)}>Resume</button>
    </div>
  );
}

/** "2 schedules proposed by agents wait for you · Show", under the panel header while any wait. */
export function ProposalsNotice() {
  if (!schedulesSupported.value) return null;
  const waiting = waitingProposals(schedules.value);
  if (!waiting.length) return null;
  const show = () => {
    const el = document.querySelector<HTMLElement>(`.sch-prop[data-schedule="${waiting[0].id}"]`);
    el?.scrollIntoView({ block: "nearest" });
    el?.focus();
  };
  const by = [...new Set(waiting.map((s) => s.made_by?.agent ?? "an agent"))].join(", ");
  return (
    <div class="sch-paused-note sch-prop-note" role="status">
      <IconSpark size={12} />
      <span>
        {waiting.length === 1 ? "1 schedule" : `${waiting.length} schedules`} proposed by {by} {waiting.length === 1 ? "waits" : "wait"}{" "}
        for your approval
      </span>
      <button type="button" class="sch-link" onClick={show}>Show</button>
    </div>
  );
}

/** A proposal's ⋯ menu: approve, edit first, or dismiss. Nothing else: it hasn't run. */
function proposalMenu(s: Schedule, x: number, y: number) {
  openMenu({
    x,
    y,
    minWidth: 180,
    items: [
      { header: `${s.name} (proposed by ${s.made_by?.agent ?? "an agent"})` },
      { label: "Approve", run: () => A.approveSchedule(s) },
      { label: "Edit…", hint: "then approve", run: () => A.editSchedule(s.id) },
      "sep",
      { label: "Dismiss", danger: true, run: () => A.dismissProposal(s) },
    ],
  });
}

function scheduleMenu(s: Schedule, x: number, y: number) {
  if (isProposal(s)) return proposalMenu(s, x, y);
  const open = scheduleHistoryOpen.value.has(s.id);
  const over = s.status === "done" || isExpired(s, Date.now()) || s.next_run === null;
  openMenu({
    x,
    y,
    minWidth: 190,
    items: [
      { header: s.name },
      { label: "Edit…", run: () => A.editSchedule(s.id) },
      { label: "Run now", hint: s.trigger ? "without an event" : "outside its timing", run: () => A.runScheduleNow(s) },
      ...(s.url
        ? [
          { label: "Copy trigger command", hint: "curl, with its secret", run: () => A.copyTriggerCommand(s) },
          { label: "New secret…", run: () => A.rotateTriggerSecret(s) },
        ]
        : []),
      ...(s.note && !s.note.triggers
        ? [{ label: `Allow triggers on ${s.note.title}`, run: () => A.setNoteTriggers(s.note!.id, true) }]
        : []),
      "sep",
      { label: s.paused ? "Resume" : "Pause", run: () => A.pauseSchedule(s, !s.paused) },
      ...(s.trigger ? [] : [{
        label: s.skip_next ? "Don't skip next run" : "Skip next run",
        hint: !s.skip_next && s.next_run !== null && !over ? nextWhen(s.next_run, Date.now()) : undefined,
        disabled: !s.skip_next && (over || s.paused),
        run: () => A.skipNextRun(s, !s.skip_next),
      }]),
      ...(s.expires_at !== null ? [{ label: "Renew", hint: "7 more days", run: () => A.renewSchedule(s) }] : []),
      { label: "History", checked: open, run: () => A.toggleScheduleHistory(s.id) },
      "sep",
      { label: "Remove…", danger: true, run: () => A.removeSchedule(s) },
    ],
  });
}

/** An agent's schedules, under its row: name, next run, last outcome, and the history when open. */
export function AgentSchedules({ agent, now }: { agent: Agent; now: number }) {
  if (!schedulesSupported.value) return null;
  const list = sortSchedules(schedules.value.filter((s) => s.agent_id === agent.id));
  if (!list.length) return null;
  return (
    <div role="listitem" class="sch-list" aria-label={`Schedules of ${agent.name}`}>
      <div role="list">
        {list.map((s) => <ScheduleRow key={s.id} s={s} now={now} />)}
      </div>
    </div>
  );
}

/** Up / Down move through agent and schedule rows alike. */
export function moveRow(e: KeyboardEvent) {
  e.preventDefault();
  const rows = [...document.querySelectorAll<HTMLElement>(".ap-row, .sch-row, .sch-prop")];
  const i = rows.indexOf(e.currentTarget as HTMLElement);
  rows[i + (e.key === "ArrowDown" ? 1 : -1)]?.focus();
}

/** An agent's proposal, waiting for you: what will run, when, on which agent, why, and what it
 * would use, with Approve, Edit and Dismiss. It never runs before you approve it. */
function ProposalCard({ s, now }: { s: Schedule; now: number }) {
  const m = s.made_by!;
  const sub = s.estimate?.billing === "claude_subscription";
  const facts = proposalFacts(s);
  const label = `Proposed by ${m.agent}: ${s.name}, ${s.reading}, on ${s.agent ?? "a removed agent"}. Waiting for your approval.`;
  return (
    <div
      role="listitem"
      class="sch-prop"
      data-schedule={s.id}
      tabIndex={0}
      aria-label={label}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === "ArrowDown" || e.key === "ArrowUp") moveRow(e);
        else if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
          e.preventDefault();
          const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
          proposalMenu(s, r.left + 24, r.top + 24);
        }
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
        proposalMenu(s, e.clientX, e.clientY);
      }}
    >
      <div class="sch-prop-head">
        <IconSpark size={12} class="sch-prop-icon" />
        <span class="sch-prop-by">
          Proposed by <b>{m.agent}</b>
        </span>
        <span class="sch-prop-at" data-tip={new Date(m.at).toLocaleString()}>{nextWhenPast(m.at, now)}</span>
      </div>
      <div class="sch-prop-name">{s.name}</div>
      <dl class="sch-prop-facts">
        {facts.map((f) => (
          <div key={f.label} class="sch-prop-fact">
            <dt>{f.label}</dt>
            <dd>{f.value}</dd>
          </div>
        ))}
        <div class="sch-prop-fact sch-prop-fact-block">
          <dt>Prompt</dt>
          <dd>
            <pre class="sch-prop-prompt">{s.prompt}</pre>
          </dd>
        </div>
        {m.reason && (
          <div class="sch-prop-fact sch-prop-fact-block">
            <dt>Why</dt>
            <dd class="sch-prop-reason" data-tip={`${m.agent}'s words`}>{m.reason}</dd>
          </div>
        )}
        {s.estimate && (
          <div class="sch-prop-fact sch-prop-fact-block">
            <dt>Use</dt>
            <dd class={cx("sch-prop-use", sub && "sub")}>
              {sub && <IconShieldAlert size={12} />}
              <span>{s.estimate.text}</span>
            </dd>
          </div>
        )}
      </dl>
      <div class="sch-prop-actions">
        <button type="button" class="btn btn-sm btn-primary sch-approve" onClick={() => A.approveSchedule(s)}>
          Approve
        </button>
        <button type="button" class="btn btn-sm" onClick={() => A.editSchedule(s.id)}>Edit…</button>
        <button type="button" class="btn btn-sm sch-dismiss" onClick={() => A.dismissProposal(s)}>Dismiss</button>
      </div>
    </div>
  );
}

/** "today 10:02", "yesterday 18:40", "Mon 09:00": when a proposal was made. */
function nextWhenPast(ms: number, now: number): string {
  const d = new Date(ms), n = new Date(now);
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  if (d.toDateString() === n.toDateString()) return `today ${hm}`;
  const y = new Date(n.getFullYear(), n.getMonth(), n.getDate() - 1, 12);
  if (d.toDateString() === y.toDateString()) return `yesterday ${hm}`;
  return nextWhen(ms, now);
}

function ScheduleRow({ s, now }: { s: Schedule; now: number }) {
  if (isProposal(s)) return <ProposalCard s={s} now={now} />;
  return <TimedRow s={s} now={now} />;
}

function TimedRow({ s, now }: { s: Schedule; now: number }) {
  const open = scheduleHistoryOpen.value.has(s.id);
  const next = nextLabel(s, now);
  const exp = expiresLabel(s, now);
  const watch = watchLabel(s);
  const made = madeByLabel(s);
  const last = s.last_run;
  const allPaused = !!scheduleSettings.value?.paused;
  const toggle = () => A.toggleScheduleHistory(s.id);
  const renew = (e: Event) => {
    e.stopPropagation();
    A.renewSchedule(s);
  };
  const label = [
    s.name,
    s.reading,
    made?.tip,
    next.text,
    watch && `${watch.text}: ${watch.tip}`,
    exp,
    last ? `last run ${OUTCOME_LABEL[last.outcome]}` : "no runs yet",
  ].filter(Boolean).join(", ");
  return (
    <>
      <div
        role="listitem"
        class={cx("sch-row", open && "open", s.paused && "paused", next.tone !== "next" && "idle")}
        data-schedule={s.id}
        tabIndex={0}
        aria-expanded={open}
        aria-label={label}
        data-tip={`${s.reading}${allPaused && next.tone === "next" ? " · all schedules are paused" : ""}`}
        onClick={toggle}
        onKeyDown={(e) => {
          if (e.target !== e.currentTarget) return;
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            toggle();
          } else if (e.key === "ArrowDown" || e.key === "ArrowUp") moveRow(e);
          else if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
            e.preventDefault();
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            scheduleMenu(s, r.left + 24, r.bottom - 4);
          }
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          scheduleMenu(s, e.clientX, e.clientY);
        }}
      >
        {s.trigger ? <IconZap size={12} class="sch-icon sch-icon-trig" /> : <IconClock size={12} class="sch-icon" />}
        <span class="sch-text">
          <span class="sch-name">{s.name}</span>
          <span class="sch-sub">
            {next.tone === "paused" && s.paused
              ? <span class="sch-badge">paused</span>
              : <span class={cx("sch-next", next.tone, allPaused && next.tone === "next" && "held")}>{next.text}</span>}
            {made && <span class="sch-made" data-tip={made.tip}>{made.text}</span>}
            {watch && <span class="sch-chip sch-bad sch-watch" data-tip={watch.tip}>{watch.text}</span>}
            {next.tone === "expired" && s.expires_at !== null && (
              <button type="button" class="sch-link" onClick={renew}>Renew</button>
            )}
            {last && (
              <span class={cx("sch-chip", "sch-" + outcomeTone(last.outcome))} data-outcome={last.outcome}>
                {OUTCOME_LABEL[last.outcome]}
              </span>
            )}
          </span>
          {watch && watch.tip && <span class="sch-watch-detail">{watch.tip}</span>}
          {exp && (
            <span class="sch-expires">
              {exp} ·{" "}
              <button type="button" class="sch-link" onClick={renew}>Renew</button>
            </span>
          )}
        </span>
        <button
          type="button"
          class="ap-more sch-more"
          aria-label={`${s.name} menu`}
          onClick={(e) => {
            e.stopPropagation();
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            scheduleMenu(s, r.left, r.bottom + 4);
          }}
        >
          <IconMore size={13} />
        </button>
      </div>
      {open && <RunHistory s={s} now={now} />}
    </>
  );
}

function RunHistory({ s, now }: { s: Schedule; now: number }) {
  const h = scheduleRuns.value.get(s.id);
  const loading = !h || h.loading;
  const runs = h?.runs ?? [];
  return (
    <div role="listitem" class="sch-runs" aria-label={`Runs of ${s.name}`}>
      {loading && runs.length === 0 && <span class="sch-runs-note"><Spinner size={10} /> Loading runs…</span>}
      {h && !h.loading && !h.error && runs.length === 0 && <span class="sch-runs-note">No runs yet.</span>}
      {withSteps(runs).map(({ run, depth }) => <RunEntry key={run.id} r={run} now={now} depth={depth} />)}
      {h?.error && !h.loading && (
        <span class="sch-runs-note sch-runs-err">
          Couldn't load runs.{" "}
          <button type="button" class="sch-link" data-tip={h.error} onClick={() => loadScheduleRuns(s.id)}>
            Try again
          </button>
        </span>
      )}
      {h?.more && !h.error && (
        <button
          type="button"
          class="ap-older sch-older"
          disabled={h.loading}
          onClick={() => loadScheduleRuns(s.id, true)}
        >
          {h.loading ? <Spinner size={10} /> : null} Older runs
        </button>
      )}
    </div>
  );
}

function RunEntry({ r, now, depth = 0 }: { r: ScheduleRun; now: number; depth?: number }) {
  const tone = outcomeTone(r.outcome);
  const kind = runKindLabel(r);
  // A triggered run says what happened (its first line), unless it has a reason of its own.
  const happened = r.kind === "triggered" && r.event
    ? r.event.text.split("\n").find((l) => l.trim() && !l.startsWith("(earlier")) ?? null
    : null;
  const reason = runReason(r) ?? (happened && r.event && r.event.count > 1 ? `${happened} (+${r.event.count - 1} more)` : happened);
  // The message's live copy (a cold start is marked once the prompt is typed in).
  const msg = (r.msg_id !== null ? agentMessages.value.find((m) => m.id === r.msg_id) : undefined) ?? r.message;
  const cold = msg?.cold_idle_ms ?? null;
  const body = (
    <>
      <span class="sch-run-top">
        <span class={cx("sch-dot", "sch-" + tone)} aria-hidden="true" />
        <span class="sch-run-line">{runLine(r, now)}</span>
      </span>
      {(kind || reason || r.late_reply || cold !== null) && (
        <span class="sch-run-reason">
          {kind && <span class="ap-chip">{kind}</span>}
          {cold !== null && (
            <span
              class="ap-chip ap-chip-cold"
              data-tip={`Sent after ${idleText(cold)} idle: the agent likely re-read its whole conversation uncached`}
            >
              cold start
            </span>
          )}
          {r.late_reply && <span class="ap-chip" data-tip="The reply came after the run stopped waiting">late reply</span>}
          {reason}
        </span>
      )}
    </>
  );
  const cls = cx("sch-run", "sch-run-" + tone, (depth > 0 || r.kind === "chain") && "sch-run-step");
  if (!r.message) return <div class={cls} data-run={r.id}>{body}</div>;
  return (
    <button
      type="button"
      class={cx(cls, "has-msg")}
      data-run={r.id}
      data-tip="Show the message and its reply"
      onClick={() => A.openRunMessage(r)}
    >
      {body}
    </button>
  );
}

// ---------------- New / Edit schedule ----------------

/** A label and a row of segmented buttons (a radio group), inside a `.field`. */
function Seg<T extends string>(
  { label, value, options, onChange }: {
    label: string;
    value: T;
    options: { v: T; label: string }[];
    onChange: (v: T) => void;
  },
) {
  const id = useId();
  // One Tab stop (the chosen option); arrows, Home and End move and pick, as Permissions' do.
  const move = (e: KeyboardEvent) => {
    const i = Math.max(0, options.findIndex((o) => o.v === value));
    const to = segStep(i, options.length, e.key);
    if (to === null) return;
    e.preventDefault();
    onChange(options[to].v);
    const group = e.currentTarget as HTMLElement;
    requestAnimationFrame(() => group.querySelector<HTMLElement>(".seg-btn.on")?.focus());
  };
  return (
    <>
      <span class="field-label" id={id}>{label}</span>
      <div class="seg sch-seg" role="radiogroup" aria-labelledby={id} onKeyDown={move}>
        {options.map((o) => (
          <button
            key={o.v}
            type="button"
            role="radio"
            aria-checked={value === o.v}
            tabIndex={value === o.v ? 0 : -1}
            data-value={o.v}
            class={cx("seg-btn", value === o.v && "on")}
            onClick={() => onChange(o.v)}
          >
            {o.label}
          </button>
        ))}
      </div>
    </>
  );
}

interface Draft {
  agent: string;
  name: string;
  prompt: string;
  /** "At a time" or "When something happens" (a trigger). */
  mode: "time" | "event";
  when: WhenDraft;
  trig: TriggerDraft;
  then: ThenDraft;
  tz: string;
  missed: Schedule["missed"];
  conversation: Schedule["conversation"];
  notify: Schedule["notify"];
  noteOn: boolean;
  noteTitle: string;
  /** null: the agent's default (on for Claude subscription agents), until changed. */
  quiet: boolean | null;
  cap: string;
}

const browserZone = () => {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "";
  } catch {
    return "";
  }
};

function draftOf(s: Schedule | undefined, agent: string, now: number): Draft {
  if (!s) {
    const tz = browserZone() || scheduleSettings.value?.tz || "";
    return {
      agent,
      name: "",
      prompt: "",
      mode: "time",
      when: blankWhen(now, tz),
      trig: blankTrigger(),
      then: blankThen(),
      tz,
      missed: "auto",
      conversation: "continue",
      notify: "on_failure",
      noteOn: false,
      noteTitle: "",
      quiet: null,
      cap: "",
    };
  }
  return {
    agent: s.agent ?? "",
    name: s.name,
    prompt: s.prompt,
    mode: isTrigger(s.when) ? "event" : "time",
    // A one-off shows at its wall-clock time in the schedule's own zone.
    when: draftOfWhen(s.when, blankWhen(now, s.tz), s.tz),
    trig: draftOfTrigger(s.when, blankTrigger()),
    then: draftOfThen(s.then),
    tz: s.tz,
    missed: s.missed,
    conversation: s.conversation,
    notify: s.notify,
    noteOn: s.note_id !== null,
    noteTitle: s.note_title ?? "",
    quiet: s.quiet_hours,
    cap: s.daily_cap ? String(s.daily_cap) : "",
  };
}

const groupLabel = (g: AgentChoice["group"], work: string) =>
  g === "unattended"
    ? `Best for ${work} work: Codex and OpenRouter`
    : g === "claude"
    ? "Claude subscription (you opted in)"
    : `Don't take ${work} work`;

const MINUTES = Array.from({ length: 60 }, (_, i) => i);

export function ScheduleDialog(
  { id, agent: wanted, Modal, close }: { id?: number; agent?: string; Modal: ModalT; close: () => void },
) {
  const editing = id !== undefined;
  const sched = editing ? schedules.value.find((s) => s.id === id) : undefined;
  const timeChoices = useMemo(() => scheduleAgents(agents.value), [agents.value]);
  const [d, setD] = useState<Draft>(() => draftOf(sched, defaultScheduleAgent(timeChoices, wanted), Date.now()));
  /** The fields as the dialog opened: Save sends `when` only when the timing controls changed. */
  const [initial] = useState(d);
  const [preview, setPreview] = useState<SchedulePreview | null>(null);
  const [pErr, setPErr] = useState<string | null>(null);
  const [pBusy, setPBusy] = useState(false);
  /** Asks the backend again (after "Allow triggers" on the watched note). */
  const [pAgain, setPAgain] = useState(0);
  const [more, setMore] = useState(() => !!sched && (sched.daily_cap !== null || sched.tz !== browserZone()));
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const agentRef = useRef<HTMLSelectElement>(null);
  const seq = useRef(0);
  const set = (p: Partial<Draft>) => setD((x) => ({ ...x, ...p }));
  const setWhen = (p: Partial<WhenDraft>) => setD((x) => ({ ...x, when: { ...x.when, ...p } }));
  const setTrig = (p: Partial<TriggerDraft>) => setD((x) => ({ ...x, trig: { ...x.trig, ...p } }));
  const setThen = (p: Partial<ThenDraft>) => setD((x) => ({ ...x, then: { ...x.then, ...p } }));

  /** Triggers and "Then…" (Phase 2 backends: their settings carry the OpenRouter spend). */
  const phase2 = !!scheduleSettings.value?.spend || sched?.trigger !== undefined;
  /** File triggers (Phase 3 backends: their settings carry the watched folders). */
  const watched = scheduleSettings.value?.watch_folders ?? [];
  const phase3 = scheduleSettings.value?.watch_folders !== undefined;
  const proposal = !!sched && isProposal(sched);
  const event = d.mode === "event";
  /** A file trigger, but no folder may be watched yet (the default): it can't be saved. */
  const noWatched = event && d.trig.kind === "files" && phase3 && watched.length === 0;
  const origin = event ? "webhook" : "schedule";
  const choices = useMemo(() => scheduleAgents(agents.value, origin), [agents.value, origin]);
  const chosen = agents.value.find((a) => a.name === d.agent);
  const choice = choices.find((c) => c.agent.name === d.agent);
  const subClaude = !!chosen && isSubscriptionClaude(chosen);
  const eligible = choices.filter((c) => c.group !== "off");
  const wantedOff = !editing && wanted && choices.find((c) => c.agent.name === wanted)?.group === "off";
  const noteList = [...notes.value.values()];
  const w = event ? whenOfTrigger(d.trig, noteList) : whenOfDraft(d.when);
  const whenKey = "when" in w ? JSON.stringify(w.when) : "";
  const steps = thenOfDraft(d.then, sched?.then);
  const stepAgentNames = [d.then.okAgent, d.then.failMode === "ask" ? d.then.failAgent : ""].filter(Boolean);
  const quietOn = d.quiet ?? preview?.quiet_hours_default ?? subClaude;
  const cap = parseCap(d.cap);
  const settings = scheduleSettings.value;
  /** The zone the times are read in (a one-off's date and time too). */
  const zone = d.tz.trim() || browserZone() || "this computer's time zone";
  const quietHours = settings ? `${settings.quiet_start}–${settings.quiet_end}` : "";

  useLayoutEffect(() => {
    if (editing || !d.agent) agentRef.current?.focus();
    else promptRef.current?.focus();
  }, []);

  // What the timing means, as the backend reads it (debounced; a newer answer always wins).
  useEffect(() => {
    const mine = ++seq.current;
    if (!("when" in w)) {
      setPreview(null);
      setPErr(null);
      return;
    }
    setPBusy(true);
    const t = setTimeout(() => {
      rpc.call<SchedulePreview>("schedule.preview", {
        when: w.when,
        tz: d.tz.trim() || undefined,
        quiet_hours: d.quiet ?? undefined,
        agent: d.agent || undefined,
        id: sched && isTrigger(w.when) ? sched.id : undefined,
        then_agents: stepAgentNames,
      }).then((p) => {
        if (seq.current !== mine) return;
        setPreview(p);
        setPErr(null);
      }).catch((e) => {
        if (seq.current !== mine) return;
        setPreview(null);
        setPErr(e instanceof Error ? e.message : String(e));
      }).finally(() => {
        if (seq.current === mine) setPBusy(false);
      });
    }, 250);
    return () => clearTimeout(t);
  }, [whenKey, d.tz, d.quiet, d.agent, pAgain, stepAgentNames.join("\n")]);

  if (editing && !sched) return null;

  const note = noteChoice(d.noteOn, d.noteTitle, noteList);
  const noteBad = d.noteOn && !d.noteTitle.trim();
  const canSave = !busy && !!d.prompt.trim() && "when" in w && !("error" in cap) && !noteBad && "then" in steps &&
    !noWatched && (editing || (!!choice && choice.group !== "off"));
  /** What Save sends as `when`: a changed trigger, a switch between time and event, or the
   * timing controls' own rule. */
  const whenForSave = (): WhenInput | null => {
    if (!("when" in w)) return null;
    if (d.mode !== initial.mode) return w.when;
    if (event) {
      const was = whenOfTrigger(initial.trig, noteList);
      return "when" in was && JSON.stringify(was.when) === JSON.stringify(w.when) ? null : w.when;
    }
    return whenToSend(initial.when, d.when, d.tz.trim() !== initial.tz.trim());
  };

  const save = async () => {
    if (!canSave || !("when" in w) || "error" in cap || !("then" in steps)) return;
    setBusy(true);
    setErr(null);
    const fields: ScheduleFields = {
      name: d.name,
      prompt: d.prompt,
      // Unchanged timing controls send no `when`: a stored `@daily` stays `@daily`.
      when: whenForSave(),
      then: phase2 ? steps.then : undefined,
      tz: d.tz,
      missed: d.missed,
      conversation: d.conversation,
      notify: d.notify,
      note,
      quiet_hours: quietOn,
      daily_cap: cap.cap,
    };
    try {
      if (!sched) {
        const s = await rpc.call<Schedule>("schedule.create", {
          agent: d.agent,
          prompt: d.prompt,
          when: w.when,
          name: d.name.trim() || undefined,
          tz: d.tz.trim() || undefined,
          missed: d.missed,
          conversation: d.conversation,
          notify: d.notify,
          note: note ?? undefined,
          // Left alone, the backend's default for the agent (on for Claude subscription agents).
          quiet_hours: d.quiet ?? undefined,
          // Left empty for a trigger: the backend's default cap (10 a day).
          daily_cap: cap.cap ?? undefined,
          then: steps.then ?? undefined,
        });
        knowSchedule(s);
        overlay.value = null;
        if (!agentsPanelOpen.value) A.toggleAgentsPanel();
        toast(
          "success",
          s.trigger
            ? `${s.name} for ${s.agent ?? d.agent}: ${s.reading.replace(/^When /, "runs when ")}.`
            : `Scheduled ${s.name} for ${s.agent ?? d.agent}${s.next_run !== null ? `: next ${nextWhen(s.next_run, Date.now())}` : ""}.`,
        );
        A.focusActiveTerminalSoon();
        return;
      }
      const patch = schedulePatch(sched, fields);
      if (Object.keys(patch).length) {
        const s = await rpc.call<Schedule>("schedule.update", { id: sched.id, ...patch });
        knowSchedule(s);
        toast("success", "Saved.", 2500);
      }
      overlay.value = null;
      A.focusActiveTerminalSoon();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  const work = event ? "triggered" : "scheduled";
  const tick = event ? "Webhook (triggered)" : "Schedule";

  const agentField = editing
    ? (
      <div class="field">
        <span class="field-label">Agent</span>
        <span class="sch-agent-fixed">
          {chosen && <span class={cx("ap-kind", "ap-kind-" + chosen.kind)}>{KIND_LABEL[chosen.kind]}</span>}
          {sched!.agent ?? "(removed)"}
        </span>
      </div>
    )
    : eligible.length === 0
    ? (
      <div class="field">
        <span class="field-label">Agent</span>
        <div class="sch-none" role="note">
          <p>
            None of your agents takes {work} work yet. A Codex agent made from the Background worker template is
            the usual fit.
          </p>
          <button type="button" class="btn btn-sm" onClick={() => A.newAgentDialog("worker")}>New agent…</button>
        </div>
        {choices.length > 0 && (
          <span class="field-hint">Or tick {tick} under "Takes messages started by" in an agent's Edit agent.</span>
        )}
      </div>
    )
    : (
      <label class="field">
        <span class="field-label">Agent</span>
        <span class="select-wrap">
          <select
            ref={agentRef}
            class="sch-agent"
            value={d.agent}
            onChange={(e) => set({ agent: e.currentTarget.value, quiet: null })}
          >
            {!d.agent && <option value="">Pick an agent</option>}
            {(["unattended", "claude", "off"] as const).map((g) => {
              const items = choices.filter((c) => c.group === g);
              if (!items.length) return null;
              return (
                <optgroup key={g} label={groupLabel(g, work)}>
                  {items.map((c) => (
                    <option key={c.agent.id} value={c.agent.name} disabled={g === "off"}>
                      {c.agent.name} ({KIND_LABEL[c.agent.kind]}
                      {c.agent.provider === "openrouter" ? " on OpenRouter, pay per use" : ""})
                    </option>
                  ))}
                </optgroup>
              );
            })}
          </select>
          <IconChevronDown size={14} />
        </span>
        {wantedOff && (
          <span class="field-hint field-bad">
            {wanted} doesn't take {work} work: tick {tick} in its Edit agent first.
          </span>
        )}
        {!wantedOff && choices.some((c) => c.group === "off") && (
          <span class="field-hint">
            Greyed-out agents don't take {work} work; tick {tick} in their Edit agent to use them.
          </span>
        )}
      </label>
    );

  const whenInputs = (() => {
    const time = (
      <input
        class="sch-time"
        value={d.when.time}
        aria-label="Time of day"
        placeholder="09:00"
        inputMode="numeric"
        aria-invalid={!parseTime(d.when.time)}
        onInput={(e) => setWhen({ time: (e.target as HTMLInputElement).value })}
      />
    );
    switch (d.when.preset) {
      case "hourly":
        return (
          <>
            <span class="sch-at">at minute</span>
            <span class="select-wrap sch-minute">
              <select
                aria-label="Minute past the hour"
                value={String(d.when.minute)}
                onChange={(e) => setWhen({ minute: Number(e.currentTarget.value) })}
              >
                {MINUTES.map((m) => <option key={m} value={String(m)}>:{String(m).padStart(2, "0")}</option>)}
              </select>
              <IconChevronDown size={14} />
            </span>
          </>
        );
      case "daily":
      case "weekdays":
        return <><span class="sch-at">at</span>{time}</>;
      case "weekly":
        return (
          <>
            <span class="sch-at">on</span>
            <span class="select-wrap sch-day">
              <select
                aria-label="Day of the week"
                value={String(d.when.day)}
                onChange={(e) => setWhen({ day: Number(e.currentTarget.value) })}
              >
                {[1, 2, 3, 4, 5, 6, 0].map((i) => <option key={i} value={String(i)}>{DAY_NAMES[i]}</option>)}
              </select>
              <IconChevronDown size={14} />
            </span>
            <span class="sch-at">at</span>
            {time}
          </>
        );
      case "once":
        return (
          <>
            <input
              type="datetime-local"
              class="sch-once"
              aria-label={`Date and time, in ${zone}`}
              value={d.when.once}
              onInput={(e) => setWhen({ once: (e.target as HTMLInputElement).value })}
            />
            <span class="sch-at sch-zone" data-tip="The time zone it's read in: change it under More options">
              in <span class="sch-zone-name">{zone}</span>
            </span>
          </>
        );
      case "custom":
        return (
          <input
            class="sch-cron"
            value={d.when.cron}
            aria-label="Cron line"
            placeholder="0 9 * * 1-5"
            spellcheck={false}
            onInput={(e) => setWhen({ cron: (e.target as HTMLInputElement).value })}
          />
        );
      default:
        return null;
    }
  })();

  /** "When something happens": the event picker and its fields. */
  const num = (cls: string, value: string, label: string, onInput: (v: string) => void) => (
    <input
      class={cx("sch-num", cls)}
      value={value}
      inputMode="numeric"
      aria-label={label}
      onInput={(e) => onInput((e.target as HTMLInputElement).value)}
    />
  );
  const triggerInputs = (() => {
    const t = d.trig;
    switch (t.kind) {
      case "note":
        return (
          <>
            <span class="select-wrap sch-trig-note-wrap">
              <select
                class="sch-trig-note"
                aria-label="The note to watch"
                value={t.note === null ? "" : String(t.note)}
                onChange={(e) => setTrig({ note: e.currentTarget.value ? Number(e.currentTarget.value) : null })}
              >
                {t.note === null && <option value="">Pick a note</option>}
                {noteOptions(noteList).map((o) => <option key={o.id} value={String(o.id)}>{o.label}</option>)}
              </select>
              <IconChevronDown size={14} />
            </span>
            <span class="sch-at">after</span>
            {num("sch-trig-quiet", t.quiet, "Minutes of quiet after the last edit", (quiet) => setTrig({ quiet }))}
            <span class="sch-at">minutes of quiet</span>
          </>
        );
      case "git":
        return (
          <>
            <input
              class="sch-trig-repo"
              value={t.repo}
              aria-label="The repository's folder"
              placeholder="~/projects/muster"
              spellcheck={false}
              onInput={(e) => setTrig({ repo: (e.target as HTMLInputElement).value })}
            />
            <span class="sch-at">branch</span>
            <input
              class="sch-trig-branch"
              value={t.branch}
              aria-label="Branch"
              placeholder="master"
              spellcheck={false}
              onInput={(e) => setTrig({ branch: (e.target as HTMLInputElement).value })}
            />
          </>
        );
      case "pr_opened":
      case "ci_failed":
        return (
          <>
            <input
              class="sch-trig-gh"
              value={t.ghRepo}
              aria-label="GitHub repository, OWNER/NAME"
              placeholder="owner/repo"
              spellcheck={false}
              onInput={(e) => setTrig({ ghRepo: (e.target as HTMLInputElement).value })}
            />
            {t.kind === "ci_failed" && (
              <>
                <span class="sch-at">on</span>
                <input
                  class="sch-trig-branch"
                  value={t.ghBranch}
                  aria-label="Only CI on this branch (empty: any branch)"
                  placeholder="any branch"
                  spellcheck={false}
                  onInput={(e) => setTrig({ ghBranch: (e.target as HTMLInputElement).value })}
                />
              </>
            )}
            <span class="sch-at">checked every</span>
            {num("sch-trig-every", t.every, "Minutes between checks", (every) => setTrig({ every }))}
            <span class="sch-at">min</span>
          </>
        );
      case "url":
        return (
          <>
            <span class="sch-at">named</span>
            <input
              class="sch-trig-name"
              value={t.name}
              aria-label="Trigger name (goes in the URL)"
              placeholder={slug(d.name || d.prompt.split("\n")[0] || "") || "deploy-done"}
              spellcheck={false}
              onInput={(e) => setTrig({ name: (e.target as HTMLInputElement).value })}
            />
          </>
        );
      case "files":
        return (
          <>
            <input
              class="sch-trig-folder"
              value={t.folder}
              aria-label="The folder to watch"
              placeholder={watched[0] ?? "~/inbox"}
              list="sch-watch-folders"
              spellcheck={false}
              onInput={(e) => setTrig({ folder: (e.target as HTMLInputElement).value })}
            />
            <datalist id="sch-watch-folders">
              {watched.map((f) => <option key={f} value={f} />)}
            </datalist>
            <span class="sch-at">files named</span>
            <input
              class="sch-trig-pattern"
              value={t.pattern}
              aria-label="File names to watch, like *.pdf (empty: any file)"
              placeholder="any (or *.pdf)"
              spellcheck={false}
              onInput={(e) => setTrig({ pattern: (e.target as HTMLInputElement).value })}
            />
            <span class="sch-at">after</span>
            {num("sch-trig-quiet", t.quiet, "Minutes of quiet after the last change", (quiet) => setTrig({ quiet }))}
            <span class="sch-at">min of quiet</span>
            <label class="sch-trig-sub">
              <input type="checkbox" checked={t.subfolders} onChange={(e) => setTrig({ subfolders: (e.target as HTMLInputElement).checked })} />
              <span>and subfolders</span>
            </label>
          </>
        );
    }
  })();
  const triggerHint: Record<TriggerKind, string> = {
    note: "Fires only on notes where you switched on Allow triggers. Edits by its own agent, or by any scheduled or " +
      "triggered work, never fire it, so it can't loop.",
    git: "Muster reads the branch every 15 seconds (no hooks are installed). The new commits' subjects are passed on as data.",
    pr_opened: "Polled with your own gh login; nothing from GitHub reaches Muster. The first check only notes what is open.",
    ci_failed: "Polled with your own gh login; nothing from GitHub reaches Muster. The first check only notes past failures.",
    url: "This computer only, with a secret: after saving, copy the ready-made command from the schedule's menu. " +
      "Data sent with it is handed to the agent as untrusted text.",
    files: "Only inside folders you chose under Scheduling settings → Watched folders. Muster lists the folder every " +
      "15 seconds (at most 5000 entries; hidden and partial files are skipped). Changes made while its agent, or any " +
      "scheduled or triggered work, is working count as an agent's own and don't fire it.",
  };
  const watchedNote = event && d.trig.kind === "note" && "when" in w && w.when.kind === "note" ? w.when.note_id : null;
  const stepAgents = choices.filter((c) => c.group !== "off");
  const stepAgentSelect = (value: string, label: string, onChange: (v: string) => void, none?: string) => (
    <span class="select-wrap">
      <select aria-label={label} value={value} onChange={(e) => onChange(e.currentTarget.value)}>
        {none !== undefined ? <option value="">{none}</option> : !value && <option value="">Pick an agent</option>}
        {stepAgents.map((c) => <option key={c.agent.id} value={c.agent.name}>{c.agent.name} ({KIND_LABEL[c.agent.kind]})</option>)}
      </select>
      <IconChevronDown size={14} />
    </span>
  );

  const missedOptions = [
    { v: "auto" as const, label: "Automatic" },
    { v: "run_once" as const, label: "Run once when Muster is back" },
    { v: "skip" as const, label: "Skip" },
  ];

  return (
    <Modal class="dialog dialog-scroll sch-dlg" label={sched ? `Edit schedule ${sched.name}` : "New schedule"}>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            save();
          }
        }}
      >
        <div class="dlg-head">
          <h2>{sched ? sched.name : "New schedule"}</h2>
          <span class="muted">
            {proposal
              ? `Proposed by ${sched!.made_by?.agent ?? "an agent"}: saving keeps it waiting; it runs only once you approve it.`
              : sched
              ? sched.reading
              : event
              ? "A message Muster sends an agent when something happens, marked as triggered."
              : "A message Muster sends an agent on a timer, as if from you but marked as scheduled."}
          </span>
        </div>
        <div class="dlg-body">
          {agentField}
          {subClaude && (
            <div class="sch-claude-warn" role="note">
              <span class="perm-warn agent-warn">
                <IconShieldAlert size={13} /> {UNATTENDED_WARNING}
              </span>
              <span class="field-hint">
                Its recurring schedules stop after 7 days unless you renew them, and quiet hours are on by default.
              </span>
            </div>
          )}
          <label class="field">
            <span class="field-label">Prompt</span>
            <textarea
              ref={promptRef}
              class="sch-prompt"
              rows={4}
              value={d.prompt}
              placeholder="e.g. Summarise the pull requests opened since {last_run} and post it to your channel."
              onInput={(e) => set({ prompt: (e.target as HTMLTextAreaElement).value })}
            />
            <span class="field-hint sch-hint-wrap">
              <span>
                Filled in each run: <code>{"{date}"}</code> the run's date, <code>{"{last_run}"}</code>{" "}
                when it last ran, <code>{"{last_reply}"}</code> the newest reply.
                {event && (
                  <>
                    {" "}
                    <code>{"{event}"}</code> what happened{d.trig.kind === "url" && (
                      <>
                        , <code>{"{payload}"}</code> the data sent with it
                      </>
                    )}, quoted as data (added at the end when the prompt doesn't use it).
                  </>
                )}
              </span>
            </span>
          </label>
          <label class="field">
            <span class="field-label">
              Name <span class="muted">(optional)</span>
            </span>
            <input
              class="sch-name-input"
              value={d.name}
              placeholder="From the prompt's first line"
              spellcheck={false}
              onInput={(e) => set({ name: (e.target as HTMLInputElement).value })}
            />
          </label>
          <div class="field">
            {phase2
              ? (
                <Seg
                  label="When"
                  value={d.mode}
                  options={[{ v: "time", label: "At a time" }, { v: "event", label: "When something happens" }]}
                  onChange={(mode) => set({ mode, quiet: null })}
                />
              )
              : <span class="field-label" id="sch-when-label">When</span>}
            {event
              ? (
                <div class="sch-when sch-trig" role="group" aria-label="What happens">
                  <span class="select-wrap sch-preset">
                    <select
                      aria-label="Event"
                      class="sch-trig-kind"
                      value={d.trig.kind}
                      onChange={(e) => setTrig({ kind: e.currentTarget.value as TriggerKind })}
                    >
                      {TRIGGER_KINDS.filter((k) => k.id !== "files" || phase3 || d.trig.kind === "files").map((k) => (
                        <option key={k.id} value={k.id}>{k.label}</option>
                      ))}
                    </select>
                    <IconChevronDown size={14} />
                  </span>
                  {triggerInputs}
                </div>
              )
              : (
                <div class="sch-when" role="group" aria-label="Timing" aria-labelledby={phase2 ? undefined : "sch-when-label"}>
                  <span class="select-wrap sch-preset">
                    <select
                      aria-label="Timing"
                      value={d.when.preset}
                      onChange={(e) => setWhen({ preset: e.currentTarget.value as WhenDraft["preset"] })}
                    >
                      {PRESETS.map((p) => <option key={p.id} value={p.id}>{p.label}</option>)}
                    </select>
                    <IconChevronDown size={14} />
                  </span>
                  {whenInputs}
                </div>
              )}
            <div class="sch-preview" aria-live="polite">
              {"error" in w
                ? <span class="field-bad">{w.error}</span>
                : pErr
                ? <span class="field-bad sch-preview-err">{pErr}</span>
                : preview && event
                ? (
                  <>
                    <span class="sch-reading">{preview.reading}</span>
                    {preview.note_triggers_off && watchedNote !== null && (
                      <span class="sch-trig-off">
                        Triggers are off for this note, so it won't fire yet.{" "}
                        <button
                          type="button"
                          class="sch-link"
                          onClick={() => {
                            A.setNoteTriggers(watchedNote, true);
                            setTimeout(() => setPAgain((n) => n + 1), 300);
                          }}
                        >
                          Allow triggers on it
                        </button>
                      </span>
                    )}
                    {preview.expires && <span class="sch-expiry-note">Stops after 7 days unless you renew it.</span>}
                  </>
                )
                : preview
                ? (
                  <>
                    <span class="sch-reading">{preview.reading}</span>
                    {/* A one-off's reading already says when; only quiet hours are worth a line. */}
                    <span class="sch-next-runs" hidden={w.when.kind === "once" && !preview.quiet[0]}>
                      {preview.next.length === 0 ? "No run coming up." : "Next: "}
                      {preview.next.map((t, i) => (
                        <span key={t} class={cx("sch-next-run", preview.quiet[i] && "quiet")}>
                          {longWhen(t)}
                          {preview.quiet[i] && <span class="sch-quiet"> (quiet hours: skipped)</span>}
                          {i < preview.next.length - 1 && <span class="sch-sep"> · </span>}
                        </span>
                      ))}
                    </span>
                    {preview.expires && (
                      <span class="sch-expiry-note">Stops after 7 days unless you renew it.</span>
                    )}
                  </>
                )
                : <span class="muted">{pBusy ? <Spinner size={10} /> : null} Reading it…</span>}
            </div>
          </div>
          {noWatched
            ? (
              <div class="sch-none sch-trig-none" role="note">
                <p>
                  File triggers are off until you choose the folders they may watch, so none can use your agents'
                  time without you deciding where.
                </p>
                <button type="button" class="btn btn-sm" onClick={() => A.scheduleSettingsDialog()}>
                  Scheduling settings…
                </button>
              </div>
            )
            : event && <span class="field-hint sch-trig-hint">{triggerHint[d.trig.kind]}</span>}
          {!event && (
            <div class="field">
              <Seg
                label="If missed (Muster was off or asleep)"
                value={d.missed}
                options={missedOptions}
                onChange={(missed) => set({ missed })}
              />
              <span class="field-hint">
                {d.missed === "auto"
                  ? `Automatic: ${missedAutoText(preview?.missed_default)}`
                  : d.missed === "run_once"
                  ? "The newest missed run happens once, shortly after Muster is back."
                  : "Missed runs are recorded as skipped; it waits for its next time."}
              </span>
            </div>
          )}
          <div class="field-row">
            <div class="field">
              <Seg
                label="Conversation"
                value={d.conversation}
                options={[{ v: "continue", label: "Continue" }, { v: "fresh", label: "Fresh each run" }]}
                onChange={(conversation) => set({ conversation })}
              />
              <span class="field-hint sch-hint-wrap">
                <span>
                  {d.conversation === "fresh"
                    ? "Starts a new conversation each run, never while you're in a turn with it."
                    : "Each run joins the agent's current conversation."}
                </span>
              </span>
            </div>
            <div class="field">
              <Seg
                label="Notify me"
                value={d.notify}
                options={[{ v: "on_failure", label: "On failure" }, { v: "always", label: "Always" }, {
                  v: "never",
                  label: "Never",
                }]}
                onChange={(notify) => set({ notify })}
              />
              <span class="field-hint sch-hint-wrap">
                <span>
                  {d.notify === "on_failure"
                    ? "A notice and a desktop notification when a run is blocked, refused or gets no reply."
                    : d.notify === "always"
                    ? "A notice and a desktop notification after every run."
                    : "No notices; the history still shows every run."}
                </span>
              </span>
            </div>
          </div>
          <div class="field">
            <Seg
              label="Also put the reply in"
              value={d.noteOn ? "note" : "none"}
              options={[{ v: "none", label: "Nothing" }, { v: "note", label: "A note" }]}
              onChange={(v) => set({ noteOn: v === "note" })}
            />
            {d.noteOn && (
              <>
                <input
                  class="sch-note"
                  list="sch-note-titles"
                  value={d.noteTitle}
                  aria-label="Note title"
                  placeholder="A note's title: pick one, or type a new one"
                  spellcheck={false}
                  onInput={(e) => set({ noteTitle: (e.target as HTMLInputElement).value })}
                />
                <datalist id="sch-note-titles">
                  {[...notes.value.values()].map((n) => <option key={n.id} value={n.title} />)}
                </datalist>
                <span class={cx("field-hint", noteBad && "field-bad")}>
                  {noteBad
                    ? "Name the note."
                    : note && "title" in note
                    ? `A new note named ${note.title} is made. Each reply is added under a heading with its date.`
                    : "Each reply is added to the end of the note under a heading with its date."}
                </span>
              </>
            )}
          </div>
          {phase2 && (
            <fieldset class="field sch-then">
              <legend class="field-label">Then…</legend>
              <div class="sch-then-row">
                <span class="sch-at">When it replies, ask</span>
                {stepAgentSelect(d.then.okAgent, "Agent to ask when it replies", (okAgent) => setThen({ okAgent }), "nobody")}
              </div>
              {d.then.okAgent && (
                <textarea
                  class="sch-then-ok"
                  rows={2}
                  aria-label={`What to ask ${d.then.okAgent} when it replies`}
                  value={d.then.okPrompt}
                  placeholder="e.g. Here are the results: {reply} File a note with the likely causes."
                  onInput={(e) => setThen({ okPrompt: (e.target as HTMLTextAreaElement).value })}
                />
              )}
              <Seg
                label="When it fails (blocked, refused, no reply, an error)"
                value={d.then.failMode}
                options={[{ v: "none", label: "Nothing more" }, { v: "notify", label: "Notify me" }, {
                  v: "ask",
                  label: "Ask an agent",
                }]}
                onChange={(failMode) => setThen({ failMode })}
              />
              {d.then.failMode === "ask" && (
                <>
                  <div class="sch-then-row">
                    <span class="sch-at">Ask</span>
                    {stepAgentSelect(d.then.failAgent, "Agent to ask when it fails", (failAgent) => setThen({ failAgent }))}
                  </div>
                  <textarea
                    class="sch-then-fail"
                    rows={2}
                    aria-label="What to ask when it fails"
                    value={d.then.failPrompt}
                    placeholder="e.g. The nightly check ended {outcome}. Look into why."
                    onInput={(e) => setThen({ failPrompt: (e.target as HTMLTextAreaElement).value })}
                  />
                </>
              )}
              <span class={cx("field-hint", "sch-hint-wrap", "error" in steps && "field-bad")}>
                <span>
                  {"error" in steps
                    ? steps.error
                    : d.then.failMode === "notify"
                    ? "A failure also sends you a notice and a desktop notification, whatever Notify me says."
                    : <><code>{"{reply}"}</code> quotes the reply as data, <code>{"{outcome}"}</code> says how it ended. Each step is one hop further down the same chain (messages stop after 4 hops).</>}
                </span>
              </span>
              {nestedSteps(sched?.then) > 0 && (
                <span class="field-hint sch-hint-wrap">
                  <span>
                    Its steps go {nestedSteps(sched?.then) + 1} levels deep (made with the API); the deeper ones stay as
                    they are while the step above them still asks an agent.
                  </span>
                </span>
              )}
            </fieldset>
          )}
          <label class="toggle">
            <input
              type="checkbox"
              class="sch-quiet-toggle"
              checked={quietOn}
              onChange={(e) => set({ quiet: (e.target as HTMLInputElement).checked })}
            />
            <span class="toggle-track">
              <span class="toggle-thumb" />
            </span>
            <span>
              Skip runs in quiet hours{quietHours && ` (${quietHours})`}{" "}
              <span class="muted">{subClaude ? "(on by default for Claude subscription agents)" : ""}</span>
            </span>
          </label>
          <button
            type="button"
            class="sch-more-toggle"
            aria-expanded={more}
            onClick={() => setMore(!more)}
          >
            <span class="ap-chev" aria-hidden="true">
              <IconChevron size={11} />
            </span>
            More options
          </button>
          {more && (
            <div class="field-row sch-more">
              <label class="field">
                <span class="field-label">At most this many runs a day</span>
                <input
                  class="sch-cap"
                  value={d.cap}
                  inputMode="numeric"
                  placeholder={event && !editing ? "10 (a trigger's default)" : "No cap"}
                  aria-invalid={"error" in cap}
                  onInput={(e) => set({ cap: (e.target as HTMLInputElement).value })}
                />
                <span class={cx("field-hint", "error" in cap && "field-bad")}>
                  {"error" in cap ? cap.error : event ? "Run now doesn't count. A note's triggers also stop at 10 runs a day together." : "Run now doesn't count."}
                </span>
              </label>
              <label class="field">
                <span class="field-label">Time zone</span>
                <input
                  class="sch-tz"
                  value={d.tz}
                  placeholder={browserZone() || "e.g. Europe/London"}
                  spellcheck={false}
                  onInput={(e) => set({ tz: (e.target as HTMLInputElement).value })}
                />
                <span class="field-hint">Times are read here, so 09:00 stays 09:00 when the clocks change.</span>
              </label>
            </div>
          )}
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          <span class="muted dlg-foot-hint">
            <Kbd k="Ctrl+Enter" /> {sched ? "save" : "create"}
          </span>
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!canSave}>
            {busy ? <Spinner size={12} /> : null} {sched ? "Save" : "Create schedule"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- Scheduling settings ----------------

export function ScheduleSettingsDialog({ Modal, close }: { Modal: ModalT; close: () => void }) {
  const [cur, setCur] = useState<ScheduleSettings | null>(scheduleSettings.value);
  const [paused, setPaused] = useState(cur?.paused ?? false);
  const [from, setFrom] = useState(cur?.quiet_start ?? "23:00");
  const [to, setTo] = useState(cur?.quiet_end ?? "07:00");
  const [capText, setCapText] = useState(String(cur?.unattended_cap ?? 24));
  const [spendText, setSpendText] = useState(cur?.spend ? String(cur.spend.cap) : "5");
  /** Per-agent caps as typed, by agent name (empty: none). */
  const [agentCaps, setAgentCaps] = useState<Record<string, string>>(() =>
    Object.fromEntries((cur?.spend?.agents ?? []).map((a) => [a.name, a.cap === null ? "" : String(a.cap)]))
  );
  /** Watched folders, one per line (Phase 3 backends). */
  const [foldersText, setFoldersText] = useState((cur?.watch_folders ?? []).join("\n"));
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const first = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => first.current?.focus(), []);
  useEffect(() => {
    // Always asked again: today's spend and the OpenRouter agents may have changed (the backend
    // also reads the key again when its reading is over a minute old, and says so with an event).
    rpc.call<ScheduleSettings>("schedule.settings.get").then((s) => {
      scheduleSettings.value = s;
      if (cur) {
        // Fields already showing stay as typed; agents new to the list get theirs.
        setCur(s);
        setAgentCaps((m) => ({
          ...Object.fromEntries((s.spend?.agents ?? []).map((a) => [a.name, a.cap === null ? "" : String(a.cap)])),
          ...m,
        }));
        return;
      }
      setCur(s);
      setPaused(s.paused);
      setFrom(s.quiet_start);
      setTo(s.quiet_end);
      setCapText(String(s.unattended_cap));
      setFoldersText((s.watch_folders ?? []).join("\n"));
      if (s.spend) {
        setSpendText(String(s.spend.cap));
        setAgentCaps(Object.fromEntries(s.spend.agents.map((a) => [a.name, a.cap === null ? "" : String(a.cap)])));
      }
    }).catch((e) => setErr(e instanceof Error ? e.message : String(e)));
  }, []);
  // The latest reading of today's spend (the settings event keeps it current).
  const spend = scheduleSettings.value?.spend ?? cur?.spend;
  const hhmm = (t: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(t.trim());
  const capN = /^\d+$/.test(capText.trim()) ? Number(capText.trim()) : NaN;
  const capOk = capN >= 1 && capN <= 200;
  const dollars = (t: string) => /^\$?\d+(\.\d{1,2})?$/.test(t.trim()) ? Number(t.trim().replace("$", "")) : NaN;
  const spendN = dollars(spendText);
  const spendOk = !spend || (spendN >= 0.01 && spendN <= 1000);
  const agentCapBad = Object.values(agentCaps).some((t) => t.trim() !== "" && !(dollars(t) >= 0.01 && dollars(t) <= 1000));
  const ok = !busy && hhmm(from) && hhmm(to) && capOk && spendOk && !agentCapBad;
  const save = async () => {
    if (!ok) return;
    const change: Record<string, unknown> = {};
    if (!cur || paused !== cur.paused) change.paused = paused;
    if (!cur || from.trim() !== cur.quiet_start) change.quiet_start = from.trim();
    if (!cur || to.trim() !== cur.quiet_end) change.quiet_end = to.trim();
    if (!cur || capN !== cur.unattended_cap) change.unattended_cap = capN;
    const folders = foldersText.split("\n").map((l) => l.trim()).filter(Boolean);
    if (cur?.watch_folders !== undefined && folders.join("\n") !== cur.watch_folders.join("\n")) change.watch_folders = folders;
    if (cur?.spend) {
      if (spendN !== cur.spend.cap) change.spend_cap = spendN;
      const caps: Record<string, number | null> = {};
      for (const a of cur.spend.agents) {
        const t = (agentCaps[a.name] ?? "").trim();
        const v = t === "" ? null : dollars(t);
        if (v !== a.cap) caps[a.name] = v;
      }
      if (Object.keys(caps).length) change.agent_spend_caps = caps;
    }
    if (!Object.keys(change).length) return close();
    setBusy(true);
    setErr(null);
    try {
      scheduleSettings.value = await rpc.call<ScheduleSettings>("schedule.settings.set", change);
      toast("success", "Scheduling settings saved.", 2500);
      close();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <Modal class="dialog dialog-scroll sch-settings-dlg" label="Scheduling settings">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            save();
          }
        }}
      >
        <div class="dlg-head">
          <h2>Scheduling settings</h2>
          <span class="muted">For every schedule and every agent.</span>
        </div>
        <div class="dlg-body">
          <label class="toggle">
            <input
              ref={first}
              type="checkbox"
              class="sch-pause-all"
              checked={paused}
              onChange={(e) => setPaused((e.target as HTMLInputElement).checked)}
            />
            <span class="toggle-track">
              <span class="toggle-thumb" />
            </span>
            <span>
              Pause all schedules <span class="muted">(each one skips its runs until you resume)</span>
            </span>
          </label>
          <div class="field">
            <span class="field-label" id="sch-quiet-label">Quiet hours</span>
            <div class="sch-quiet-row" role="group" aria-labelledby="sch-quiet-label">
              <span class="sch-at">from</span>
              <input
                class="sch-time sch-quiet-from"
                value={from}
                aria-label="Quiet hours start"
                placeholder="23:00"
                inputMode="numeric"
                aria-invalid={!hhmm(from)}
                onInput={(e) => setFrom((e.target as HTMLInputElement).value)}
              />
              <span class="sch-at">to</span>
              <input
                class="sch-time sch-quiet-to"
                value={to}
                aria-label="Quiet hours end"
                placeholder="07:00"
                inputMode="numeric"
                aria-invalid={!hhmm(to)}
                onInput={(e) => setTo((e.target as HTMLInputElement).value)}
              />
            </div>
            <span class={cx("field-hint", "sch-hint-wrap", (!hhmm(from) || !hhmm(to)) && "field-bad")}>
              <span>
                {!hhmm(from) || !hhmm(to)
                  ? "Times as HH:MM, like 23:00."
                  : `Runs that fall in these hours are skipped, for schedules with quiet hours on: the default for Claude subscription agents. In this computer's time zone${
                    cur?.tz ? ` (${cur.tz})` : ""
                  }.`}
              </span>
            </span>
          </div>
          <label class="field">
            <span class="field-label">Unattended messages per agent per day</span>
            <input
              class="sch-unattended"
              value={capText}
              inputMode="numeric"
              aria-invalid={!capOk}
              onInput={(e) => setCapText((e.target as HTMLInputElement).value)}
            />
            <span class={cx("field-hint", "sch-hint-wrap", !capOk && "field-bad")}>
              <span>
                {capOk
                  ? "Counts scheduled and triggered messages, not yours, over any 24 hours. Past it, Muster refuses the message and says why. Default 24."
                  : "A number from 1 to 200."}
              </span>
            </span>
          </label>
          {cur?.watch_folders !== undefined && (
            <label class="field">
              <span class="field-label">Watched folders</span>
              <textarea
                class="sch-watch-folders"
                rows={3}
                value={foldersText}
                placeholder="None yet, so file triggers are off. For example: ~/inbox"
                spellcheck={false}
                onInput={(e) => setFoldersText((e.target as HTMLTextAreaElement).value)}
              />
              <span class="field-hint sch-hint-wrap">
                <span>
                  One folder per line. A file trigger may watch only these folders (or folders inside them); with none,
                  file triggers are off. Removing a folder stops the triggers that watch it.
                </span>
              </span>
            </label>
          )}
          {spend && (
            <div class="field sch-spend">
              <span class="field-label">OpenRouter spend</span>
              <div class="sch-spend-today" role="status">
                {!spend.key_present
                  ? (
                    <span class="muted">
                      No OpenRouter key on this backend, so spend can't be read: scheduled and triggered work on an
                      OpenRouter agent still running from before is refused.
                    </span>
                  )
                  : spend.today === null
                  ? <span class="muted">{spend.error ? "" : "Not read yet today: it is read while an OpenRouter agent works."}</span>
                  : (
                    <>
                      <span class={cx("sch-spend-num", spend.today >= spend.cap && "over")}>{money(spend.today)}</span>
                      <span class="muted"> today of {money(spend.cap)} · resets at 00:00 UTC (OpenRouter's day)</span>
                      {spend.checked_at && <span class="muted"> · read {idleText(Date.now() - spend.checked_at)} ago</span>}
                    </>
                  )}
                {spend.key_present && spend.error && (
                  <span class="sch-spend-err">The last reading failed: {spend.error}</span>
                )}
                {spend.key_present && (spend.checked_at === null || Date.now() - spend.checked_at > 15 * 60_000) && (
                  <span class="sch-spend-err">
                    {spend.checked_at === null ? "No reading yet" : "No reading in the last 15 minutes"}: scheduled and triggered
                    work on OpenRouter is refused until one succeeds (what you type still goes).
                  </span>
                )}
              </div>
              <label class="sch-spend-cap">
                <span>Daily cap for all OpenRouter agents, in dollars</span>
                <input
                  class="sch-spend-input"
                  value={spendText}
                  inputMode="decimal"
                  aria-invalid={!spendOk}
                  onInput={(e) => setSpendText((e.target as HTMLInputElement).value)}
                />
              </label>
              {spend.agents.length > 0 && (
                <div class="sch-spend-agents" role="group" aria-label="Per-agent caps">
                  {spend.agents.map((a) => (
                    <label key={a.id} class="sch-spend-agent">
                      <span class="sch-spend-name">{a.name}</span>
                      <span class="muted sch-spend-spent" data-tip="Estimated: what the key spent while this agent was working (shared evenly when several were)">~{money(a.spent)} today (estimated)</span>
                      <input
                        class="sch-spend-input"
                        value={agentCaps[a.name] ?? ""}
                        placeholder="no own cap"
                        inputMode="decimal"
                        aria-label={`Daily cap for ${a.name}, in dollars`}
                        onInput={(e) => setAgentCaps({ ...agentCaps, [a.name]: (e.target as HTMLInputElement).value })}
                      />
                    </label>
                  ))}
                </div>
              )}
              <span class={cx("field-hint", "sch-hint-wrap", (!spendOk || agentCapBad) && "field-bad")}>
                <span>
                  {!spendOk || agentCapBad
                    ? "Dollars from 0.01 to 1000 (the overall cap is always on)."
                    : "At a cap, scheduled and triggered messages to OpenRouter agents are refused (\"budget\") with one alert; a run already going finishes, and what you type always goes. Read from OpenRouter's own count for the key."}
                </span>
              </span>
            </div>
          )}
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!ok}>
            {busy ? <Spinner size={12} /> : null} Save
          </button>
        </div>
      </form>
    </Modal>
  );
}

// Schedules in the agents panel (docs/UI.md "Agents panel"): each agent's schedules under its
// row with their run history, the "All schedules paused" notice, the panel header menu, and the
// New / Edit schedule and Scheduling settings dialogs. Runs are ordinary agent messages, so a
// run's message opens in the panel's message overlay.
import type { ComponentChildren } from "preact";
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import {
  agents,
  agentsPanelOpen,
  knowSchedule,
  overlay,
  scheduleHistoryOpen,
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
} from "../schedules.ts";
import type { Agent, Schedule, SchedulePreview, ScheduleRun, ScheduleSettings } from "../types.ts";
import { KIND_LABEL } from "../types.ts";
import { Kbd, Spinner } from "./common.tsx";
import { openMenu, openMenuAt } from "./Menu.tsx";
import { IconChevron, IconChevronDown, IconClock, IconMore, IconPause, IconShieldAlert } from "../icons.tsx";
import { cx } from "../util.ts";

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

function scheduleMenu(s: Schedule, x: number, y: number) {
  const open = scheduleHistoryOpen.value.has(s.id);
  const over = s.status === "done" || s.status === "expired" || s.next_run === null;
  openMenu({
    x,
    y,
    minWidth: 190,
    items: [
      { header: s.name },
      { label: "Edit…", run: () => A.editSchedule(s.id) },
      { label: "Run now", hint: "outside its timing", run: () => A.runScheduleNow(s) },
      "sep",
      { label: s.paused ? "Resume" : "Pause", run: () => A.pauseSchedule(s, !s.paused) },
      {
        label: s.skip_next ? "Don't skip next run" : "Skip next run",
        hint: !s.skip_next && s.next_run !== null && !over ? nextWhen(s.next_run, Date.now()) : undefined,
        disabled: !s.skip_next && (over || s.paused),
        run: () => A.skipNextRun(s, !s.skip_next),
      },
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
  const rows = [...document.querySelectorAll<HTMLElement>(".ap-row, .sch-row")];
  const i = rows.indexOf(e.currentTarget as HTMLElement);
  rows[i + (e.key === "ArrowDown" ? 1 : -1)]?.focus();
}

function ScheduleRow({ s, now }: { s: Schedule; now: number }) {
  const open = scheduleHistoryOpen.value.has(s.id);
  const next = nextLabel(s, now);
  const exp = expiresLabel(s, now);
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
    next.text,
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
        <IconClock size={12} class="sch-icon" />
        <span class="sch-text">
          <span class="sch-name">{s.name}</span>
          <span class="sch-sub">
            {next.tone === "paused"
              ? <span class="sch-badge">paused</span>
              : <span class={cx("sch-next", next.tone, allPaused && next.tone === "next" && "held")}>{next.text}</span>}
            {next.tone === "expired" && s.expires_at !== null && (
              <button type="button" class="sch-link" onClick={renew}>Renew</button>
            )}
            {last && (
              <span class={cx("sch-chip", "sch-" + outcomeTone(last.outcome))} data-outcome={last.outcome}>
                {OUTCOME_LABEL[last.outcome]}
              </span>
            )}
          </span>
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
  const [busy, setBusy] = useState(false);
  const older = async () => {
    if (busy) return;
    setBusy(true);
    await A.loadScheduleRuns(s.id, true);
    setBusy(false);
  };
  return (
    <div role="listitem" class="sch-runs" aria-label={`Runs of ${s.name}`}>
      {!h && <span class="sch-runs-note"><Spinner size={10} /> Loading runs…</span>}
      {h && h.runs.length === 0 && <span class="sch-runs-note">No runs yet.</span>}
      {h?.runs.map((r) => <RunEntry key={r.id} r={r} now={now} />)}
      {h?.more && (
        <button type="button" class="ap-older sch-older" disabled={busy} onClick={older}>
          {busy ? <Spinner size={10} /> : null} Older runs
        </button>
      )}
    </div>
  );
}

function RunEntry({ r, now }: { r: ScheduleRun; now: number }) {
  const tone = outcomeTone(r.outcome);
  const kind = runKindLabel(r);
  const reason = runReason(r);
  const body = (
    <>
      <span class="sch-run-top">
        <span class={cx("sch-dot", "sch-" + tone)} aria-hidden="true" />
        <span class="sch-run-line">{runLine(r, now)}</span>
      </span>
      {(kind || reason || r.late_reply) && (
        <span class="sch-run-reason">
          {kind && <span class="ap-chip">{kind}</span>}
          {r.late_reply && <span class="ap-chip" data-tip="The reply came after the run stopped waiting">late reply</span>}
          {reason}
        </span>
      )}
    </>
  );
  const cls = cx("sch-run", "sch-run-" + tone);
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
  return (
    <>
      <span class="field-label" id={id}>{label}</span>
      <div class="seg sch-seg" role="radiogroup" aria-labelledby={id}>
        {options.map((o) => (
          <button
            key={o.v}
            type="button"
            role="radio"
            aria-checked={value === o.v}
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
  when: WhenDraft;
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
  const base = blankWhen(now);
  if (!s) {
    return {
      agent,
      name: "",
      prompt: "",
      when: base,
      tz: browserZone() || scheduleSettings.value?.tz || "",
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
    when: draftOfWhen(s.when, base),
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

const GROUP_LABEL: Record<AgentChoice["group"], string> = {
  unattended: "Best for scheduled work: Codex and OpenRouter",
  claude: "Claude subscription (you opted in)",
  off: "Don't take scheduled work",
};

const MINUTES = Array.from({ length: 60 }, (_, i) => i);

export function ScheduleDialog(
  { id, agent: wanted, Modal, close }: { id?: number; agent?: string; Modal: ModalT; close: () => void },
) {
  const editing = id !== undefined;
  const sched = editing ? schedules.value.find((s) => s.id === id) : undefined;
  const choices = useMemo(() => scheduleAgents(agents.value), [agents.value]);
  const [d, setD] = useState<Draft>(() => draftOf(sched, defaultScheduleAgent(choices, wanted), Date.now()));
  const [preview, setPreview] = useState<SchedulePreview | null>(null);
  const [pErr, setPErr] = useState<string | null>(null);
  const [pBusy, setPBusy] = useState(false);
  const [more, setMore] = useState(() => !!sched && (sched.daily_cap !== null || sched.tz !== browserZone()));
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const agentRef = useRef<HTMLSelectElement>(null);
  const seq = useRef(0);
  const set = (p: Partial<Draft>) => setD((x) => ({ ...x, ...p }));
  const setWhen = (p: Partial<WhenDraft>) => setD((x) => ({ ...x, when: { ...x.when, ...p } }));

  const chosen = agents.value.find((a) => a.name === d.agent);
  const choice = choices.find((c) => c.agent.name === d.agent);
  const subClaude = !!chosen && isSubscriptionClaude(chosen);
  const eligible = choices.filter((c) => c.group !== "off");
  const wantedOff = !editing && wanted && choices.find((c) => c.agent.name === wanted)?.group === "off";
  const w = whenOfDraft(d.when);
  const whenKey = "when" in w ? JSON.stringify(w.when) : "";
  const quietOn = d.quiet ?? preview?.quiet_hours_default ?? subClaude;
  const cap = parseCap(d.cap);
  const settings = scheduleSettings.value;
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
  }, [whenKey, d.tz, d.quiet, d.agent]);

  if (editing && !sched) return null;

  const note = noteChoice(d.noteOn, d.noteTitle, [...notes.value.values()]);
  const noteBad = d.noteOn && !d.noteTitle.trim();
  const canSave = !busy && !!d.prompt.trim() && "when" in w && !("error" in cap) && !noteBad &&
    (editing || (!!choice && choice.group !== "off"));

  const save = async () => {
    if (!canSave || !("when" in w) || "error" in cap) return;
    setBusy(true);
    setErr(null);
    const fields: ScheduleFields = {
      name: d.name,
      prompt: d.prompt,
      when: w.when,
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
          daily_cap: cap.cap ?? undefined,
        });
        knowSchedule(s);
        overlay.value = null;
        if (!agentsPanelOpen.value) A.toggleAgentsPanel();
        toast(
          "success",
          `Scheduled ${s.name} for ${s.agent ?? d.agent}${s.next_run !== null ? `: next ${nextWhen(s.next_run, Date.now())}` : ""}.`,
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
            None of your agents takes scheduled work yet. A Codex agent made from the Background worker template is
            the usual fit.
          </p>
          <button type="button" class="btn btn-sm" onClick={() => A.newAgentDialog("worker")}>New agent…</button>
        </div>
        {choices.length > 0 && (
          <span class="field-hint">Or tick Schedule under "Takes messages started by" in an agent's Edit agent.</span>
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
                <optgroup key={g} label={GROUP_LABEL[g]}>
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
            {wanted} doesn't take scheduled work: tick Schedule in its Edit agent first.
          </span>
        )}
        {!wantedOff && choices.some((c) => c.group === "off") && (
          <span class="field-hint">
            Greyed-out agents don't take scheduled work; tick Schedule in their Edit agent to use them.
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
          <input
            type="datetime-local"
            class="sch-once"
            aria-label="Date and time"
            value={d.when.once}
            onInput={(e) => setWhen({ once: (e.target as HTMLInputElement).value })}
          />
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
            {sched
              ? sched.reading
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
            <span class="field-label" id="sch-when-label">When</span>
            <div class="sch-when" role="group" aria-labelledby="sch-when-label">
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
            <div class="sch-preview" aria-live="polite">
              {"error" in w
                ? <span class="field-bad">{w.error}</span>
                : pErr
                ? <span class="field-bad sch-preview-err">{pErr}</span>
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
                  placeholder="No cap"
                  aria-invalid={"error" in cap}
                  onInput={(e) => set({ cap: (e.target as HTMLInputElement).value })}
                />
                <span class={cx("field-hint", "error" in cap && "field-bad")}>
                  {"error" in cap ? cap.error : "Run now doesn't count."}
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
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const first = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => first.current?.focus(), []);
  useEffect(() => {
    if (cur) return;
    rpc.call<ScheduleSettings>("schedule.settings.get").then((s) => {
      setCur(s);
      setPaused(s.paused);
      setFrom(s.quiet_start);
      setTo(s.quiet_end);
      setCapText(String(s.unattended_cap));
    }).catch((e) => setErr(e instanceof Error ? e.message : String(e)));
  }, []);
  const hhmm = (t: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(t.trim());
  const capN = /^\d+$/.test(capText.trim()) ? Number(capText.trim()) : NaN;
  const capOk = capN >= 1 && capN <= 200;
  const ok = !busy && hhmm(from) && hhmm(to) && capOk;
  const save = async () => {
    if (!ok) return;
    const change: Record<string, unknown> = {};
    if (!cur || paused !== cur.paused) change.paused = paused;
    if (!cur || from.trim() !== cur.quiet_start) change.quiet_start = from.trim();
    if (!cur || to.trim() !== cur.quiet_end) change.quiet_end = to.trim();
    if (!cur || capN !== cur.unattended_cap) change.unattended_cap = capN;
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
    <Modal class="dialog sch-settings-dlg" label="Scheduling settings">
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

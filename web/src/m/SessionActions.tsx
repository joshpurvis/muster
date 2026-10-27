// A session's title in its header; on a manage-scoped phone it opens a sheet to restart, stop,
// rename or close the session, or rename or archive its task.
import { useEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { type SessionInfo, STATE_LABEL } from "../types.ts";
import { canManage, loadInfo, useHistoryLayer, useModal } from "./manage.ts";
import {
  agentName,
  type Agents,
  archiveConfirm,
  closeConfirm,
  type Confirm,
  restartConfirm,
  sameConfirm,
  stopConfirm,
} from "./manage_logic.ts";
import { channels, errorText, notify, ready, sessions } from "./store.ts";
import { Icon } from "./ui.tsx";
import "./manage.css";

export function SessionTitle({ session: s }: { session: SessionInfo }) {
  const [open, setOpen] = useState(false);
  const task = channels.value.get(s.channel_id)?.name ?? "Session";
  if (!canManage.value) {
    return (
      <div class="session-title">
        <h1>{s.name}</h1>
        <span>{task}</span>
      </div>
    );
  }
  return (
    <>
      <button
        type="button"
        class="session-title session-title-button"
        aria-haspopup="dialog"
        aria-label={`${s.name}, ${task}. Tab and task actions`}
        onClick={() => setOpen(true)}
      >
        <h1>
          <span class="title-text">{s.name}</span>
          <Icon name="down" />
        </h1>
        <span>{task}</span>
      </button>
      {open && <ActionSheet session={s} onClose={() => setOpen(false)} />}
    </>
  );
}

type Which = "restart" | "stop" | "close" | "archive";
type View =
  | { kind: "menu" }
  /** `target`: the session's or task's id when the form opened (it renames that one). */
  | { kind: "rename"; what: "session" | "task"; target: number; initial: string }
  /** `shown`: what the user has been told; `changed`: it was replaced since. */
  | { kind: "confirm"; which: Which; shown: Confirm; changed: boolean };
/** Which sessions are named agents', read when the sheet opens and after each reconnect:
 * Stop, Close and Archive say different things for them, so they wait for it. */
type Known =
  | { state: "loading" }
  | { state: "ok"; agents: Agents; seen: Set<number> }
  | { state: "error"; message: string }
  | { state: "old" };

function ActionSheet({ session: s, onClose }: { session: SessionInfo; onClose: () => void }) {
  const [view, setView] = useState<View>({ kind: "menu" });
  const [busy, setBusy] = useState(false);
  const [known, setKnown] = useState<Known>({ state: "loading" });
  const [attempt, setAttempt] = useState(0);
  const sheet = useRef<HTMLDivElement>(null);
  const layer = useHistoryLayer(onClose);
  useModal(sheet);
  const online = ready.value;
  useEffect(() => {
    setKnown({ state: "loading" });
    if (!online) return;
    let current = true;
    loadInfo().then(
      (i) =>
        current && setKnown(i.agents ? { state: "ok", agents: i.agents, seen: i.seen } : { state: "old" }),
      (e) => current && setKnown({ state: "error", message: errorText(e) }),
    );
    return () => current = false;
  }, [online, attempt]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => e.key === "Escape" && layer.close();
    document.addEventListener("keydown", key);
    return () => document.removeEventListener("keydown", key);
  }, []);
  useEffect(() => {
    const root = sheet.current;
    (root?.querySelector<HTMLElement>("[data-autofocus]") ?? root?.querySelector<HTMLElement>("h2"))?.focus();
  }, [view.kind]);

  // Everything below is read from live state on every render, so what a confirmation says (and
  // what its button does) follows tabs that start, move, end or turn out to be agents meanwhile.
  const agents = known.state === "ok" ? known.agents : null;
  const task = channels.value.get(s.channel_id);
  const inTask = [...sessions.value.values()].filter((x) => x.channel_id === s.channel_id);
  const agent = agentName(s, agents);
  const exited = s.state === "exited";
  const canAct = online && !busy;
  // A tab that appeared after the agent list was read (one that started or moved in since) may be
  // an agent: read the list again, and wait for it.
  const unseen = known.state === "ok"
    ? [s, ...inTask].filter((x) => !known.seen.has(x.id)).map((x) => x.id).sort().join(",")
    : "";
  useEffect(() => {
    if (unseen) setAttempt((n) => n + 1);
  }, [unseen]);
  const canDestroy = canAct && known.state === "ok" && !unseen;
  const targets = JSON.stringify({ session: s.id, task: s.channel_id, tabs: inTask.map((x) => x.id).sort() });
  const build = (which: Which): Confirm | null => {
    const words = which === "restart"
      ? restartConfirm(s)
      : which === "stop"
      ? stopConfirm(s, agent)
      : which === "close"
      ? closeConfirm(s, agent, task, inTask.length === 1)
      : task
      ? archiveConfirm(task, inTask, agents)
      : null;
    return words && { ...words, targets };
  };
  const works: Record<Which, () => Promise<void>> = {
    restart: async () => {
      await rpc.call("session.restart", { id: s.id });
      notify(exited ? `${s.name} started again` : `${s.name} restarted`);
      layer.close();
    },
    stop: async () => {
      await rpc.call("session.kill", { id: s.id });
      notify(`${s.name} stopped`);
      layer.close();
    },
    // Back to the list first: the tab is gone before the reply arrives.
    close: async () => {
      layer.leaveSession();
      const r = await rpc.call<{ hidden?: boolean }>("session.remove", { id: s.id });
      notify(r?.hidden ? `${agent ?? s.name} keeps running in the background` : "Tab closed");
    },
    archive: async () => {
      const id = s.channel_id;
      layer.leaveSession();
      await rpc.call("channel.archive", { id, remove_worktree: false });
      notify("Task archived");
    },
  };
  const run = async (work: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    try {
      await work();
    } catch (e) {
      notify(errorText(e));
      setBusy(false);
    }
  };
  const ask = (which: Which) => {
    if (which !== "restart" && !canDestroy) return;
    const confirm = build(which);
    if (confirm) setView({ kind: "confirm", which, shown: confirm, changed: false });
    else if (which === "restart") run(works.restart);
  };
  // Restart asks only while mid-turn: once it no longer is, the warning already read still holds.
  const liveConfirm = view.kind === "confirm"
    ? build(view.which) ?? (view.which === "restart" ? view.shown : null)
    : null;
  const stale = view.kind === "confirm" && !!liveConfirm && !sameConfirm(liveConfirm, view.shown);
  useEffect(() => {
    if (view.kind === "confirm" && liveConfirm && stale) {
      setView({ ...view, shown: liveConfirm, changed: true });
    }
  });
  const confirmAction = () => {
    if (view.kind !== "confirm" || !liveConfirm) return;
    // Something changed since this was drawn: show it and ask again rather than act on it.
    if (!sameConfirm(liveConfirm, view.shown)) return setView({ ...view, shown: liveConfirm, changed: true });
    run(works[view.which]);
  };

  const agentNote = !online
    ? null
    : known.state === "loading"
    ? "Checking which tabs are named agents. Stop, Close and Archive wait for it."
    : known.state === "error"
    ? "Couldn't check which tabs are named agents, so Stop, Close and Archive are off."
    : known.state === "old"
    ? "Muster on your computer can't say which tabs are named agents, so Stop, Close and Archive are off here. Update Muster, then try again."
    : null;

  return (
    <div
      class="sheet-backdrop"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget && !busy) layer.close();
      }}
    >
      <div
        class="action-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="sheet-title"
        aria-describedby={view.kind === "confirm" ? "sheet-desc" : undefined}
        ref={sheet}
      >
        <span class="sheet-grabber" aria-hidden="true" />
        {view.kind === "menu" && (
          <>
            <div class="sheet-head">
              <h2 id="sheet-title" tabIndex={-1}>{s.name}</h2>
              <p>{STATE_LABEL[s.state]}{agent ? ` · named agent ${agent}` : ""}</p>
              {agentNote && (
                <p class="sheet-notice" role="status">
                  {agentNote}
                  {known.state === "error" && (
                    <button
                      type="button"
                      class="notice-retry"
                      onClick={() => setAttempt(attempt + 1)}
                    >
                      Try again
                    </button>
                  )}
                </p>
              )}
            </div>
            <div class="sheet-group">
              <button
                type="button"
                class="sheet-item"
                onClick={() => setView({ kind: "rename", what: "session", target: s.id, initial: s.name })}
              >
                Rename tab
              </button>
              <button type="button" class="sheet-item" disabled={!canAct} onClick={() => ask("restart")}>
                {exited ? "Start again" : "Restart"}
              </button>
              {!exited && (
                <button
                  type="button"
                  class="sheet-item danger"
                  disabled={!canDestroy}
                  onClick={() => ask("stop")}
                >
                  Stop
                </button>
              )}
              {!s.hidden && (
                <button
                  type="button"
                  class={`sheet-item ${agent ? "" : "danger"}`}
                  disabled={!canDestroy}
                  onClick={() => ask("close")}
                >
                  Close tab
                </button>
              )}
            </div>
            {task && (
              <>
                <p class="sheet-label">Task · {task.name}</p>
                <div class="sheet-group">
                  <button
                    type="button"
                    class="sheet-item"
                    onClick={() =>
                      setView({ kind: "rename", what: "task", target: task.id, initial: task.name })}
                  >
                    Rename task
                  </button>
                  <button
                    type="button"
                    class="sheet-item danger"
                    disabled={!canDestroy}
                    onClick={() => ask("archive")}
                  >
                    Archive task
                  </button>
                </div>
              </>
            )}
            <button type="button" class="sheet-cancel" onClick={layer.close}>Cancel</button>
          </>
        )}
        {view.kind === "rename" && (
          <RenameForm
            label={view.what === "session" ? "Rename tab" : `Rename task “${view.initial}”`}
            initial={view.initial}
            busy={busy}
            onCancel={() => setView({ kind: "menu" })}
            onSave={(name) =>
              run(async () => {
                if (view.what === "session") await rpc.call("session.rename", { id: view.target, name });
                else await rpc.call("channel.rename", { id: view.target, name });
                notify("Renamed");
                layer.close();
              })}
          />
        )}
        {view.kind === "confirm" && (
          <div class="sheet-confirm">
            <h2 id="sheet-title" tabIndex={-1}>{(liveConfirm ?? view.shown).title}</h2>
            <div id="sheet-desc">
              {liveConfirm
                ? liveConfirm.body.map((line) => <p key={line}>{line}</p>)
                : <p>This task has already closed.</p>}
            </div>
            {view.changed && liveConfirm && (
              <p class="sheet-notice" role="alert">
                What happens changed while this was open. Read it again before you confirm.
              </p>
            )}
            <div class="sheet-actions">
              <button type="button" disabled={busy} onClick={() => setView({ kind: "menu" })}>Cancel</button>
              {liveConfirm && (
                <button
                  type="button"
                  class={liveConfirm.danger ? "danger-fill" : "primary"}
                  aria-describedby="sheet-desc"
                  disabled={view.which === "restart" ? !canAct : !canDestroy}
                  onClick={confirmAction}
                >
                  {busy ? "Working…" : liveConfirm.action}
                </button>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function RenameForm(
  { label, initial, busy, onCancel, onSave }: {
    label: string;
    initial: string;
    busy: boolean;
    onCancel: () => void;
    onSave: (name: string) => void;
  },
) {
  const [name, setName] = useState(initial);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.select(), []);
  const clean = name.trim();
  const canSave = !!clean && clean !== initial && !busy && ready.value;
  return (
    <form
      class="sheet-confirm"
      onSubmit={(e) => {
        e.preventDefault();
        if (canSave) onSave(clean);
      }}
    >
      <h2 id="sheet-title">{label}</h2>
      <input
        ref={input}
        class="sheet-input"
        data-autofocus
        value={name}
        maxLength={60}
        aria-label="Name"
        autoCapitalize="sentences"
        autoComplete="off"
        enterKeyHint="done"
        onInput={(e) => setName(e.currentTarget.value)}
      />
      <div class="sheet-actions">
        <button type="button" disabled={busy} onClick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={!canSave}>{busy ? "Saving…" : "Save"}</button>
      </div>
    </form>
  );
}

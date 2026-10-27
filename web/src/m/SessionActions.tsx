// A session's title in its header; on a manage-scoped phone it opens a sheet to restart, stop,
// rename or close the session, or rename or archive its task.
import { useEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { type SessionInfo, STATE_LABEL } from "../types.ts";
import { canManage, info, loadInfo } from "./manage.ts";
import {
  archiveConfirm,
  closeConfirm,
  type Confirm,
  isAgent,
  restartConfirm,
  stopConfirm,
} from "./manage_logic.ts";
import { channels, closeSession, errorText, notify, ready, sessions } from "./store.ts";
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

type View =
  | { kind: "menu" }
  | { kind: "rename"; what: "session" | "task" }
  | { kind: "confirm"; confirm: Confirm; run: () => Promise<void> };

function ActionSheet({ session: s, onClose }: { session: SessionInfo; onClose: () => void }) {
  const [view, setView] = useState<View>({ kind: "menu" });
  const [busy, setBusy] = useState(false);
  const sheet = useRef<HTMLDivElement>(null);
  const task = channels.value.get(s.channel_id);
  const inTask = [...sessions.value.values()].filter((x) => x.channel_id === s.channel_id);
  const agents = info.value?.agents ?? null;
  const agent = isAgent(s, agents);
  const exited = s.state === "exited";
  useEffect(() => {
    // Which sessions are named agents' decides what Close and Archive say: read it fresh.
    loadInfo().catch(() => {});
    const key = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    document.addEventListener("keydown", key);
    return () => document.removeEventListener("keydown", key);
  }, []);
  useEffect(() => {
    sheet.current?.querySelector<HTMLElement>("[data-autofocus], button")?.focus();
  }, [view.kind]);
  const run = async (work: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    try {
      await work();
      onClose();
    } catch (e) {
      notify(errorText(e));
      setBusy(false);
    }
  };
  const ask = (confirm: Confirm | null, work: () => Promise<void>) =>
    confirm ? setView({ kind: "confirm", confirm, run: work }) : run(work);

  const restart = async () => {
    await rpc.call("session.restart", { id: s.id });
    notify(exited ? `${s.name} started again` : `${s.name} restarted`);
  };
  const stop = async () => {
    await rpc.call("session.kill", { id: s.id });
    notify(`${s.name} stopped`);
  };
  // Back to the list first: the tab is gone before the reply arrives.
  const close = async () => {
    closeSession();
    const r = await rpc.call<{ hidden?: boolean }>("session.remove", { id: s.id });
    notify(r?.hidden ? `${s.name} keeps running in the background` : "Tab closed");
  };
  const archive = async () => {
    closeSession();
    await rpc.call("channel.archive", { id: s.channel_id, remove_worktree: false });
    notify("Task archived");
  };

  return (
    <div
      class="sheet-backdrop"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div class="action-sheet" role="dialog" aria-modal="true" aria-labelledby="sheet-title" ref={sheet}>
        <span class="sheet-grabber" aria-hidden="true" />
        {view.kind === "menu" && (
          <>
            <div class="sheet-head">
              <h2 id="sheet-title">{s.name}</h2>
              <p>{STATE_LABEL[s.state]}{agent ? " · named agent" : ""}</p>
            </div>
            <div class="sheet-group">
              <button
                type="button"
                class="sheet-item"
                onClick={() => setView({ kind: "rename", what: "session" })}
              >
                Rename tab
              </button>
              <button
                type="button"
                class="sheet-item"
                disabled={busy || !ready.value}
                onClick={() => ask(restartConfirm(s), restart)}
              >
                {exited ? "Start again" : "Restart"}
              </button>
              {!exited && (
                <button
                  type="button"
                  class="sheet-item danger"
                  disabled={busy || !ready.value}
                  onClick={() => ask(stopConfirm(s, agent), stop)}
                >
                  Stop
                </button>
              )}
              {!s.hidden && (
                <button
                  type="button"
                  class={`sheet-item ${agent ? "" : "danger"}`}
                  disabled={busy || !ready.value}
                  onClick={() => ask(closeConfirm(s, agent, task, inTask.length === 1), close)}
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
                    onClick={() => setView({ kind: "rename", what: "task" })}
                  >
                    Rename task
                  </button>
                  <button
                    type="button"
                    class="sheet-item danger"
                    disabled={busy || !ready.value}
                    onClick={() => ask(archiveConfirm(task, inTask, agents), archive)}
                  >
                    Archive task
                  </button>
                </div>
              </>
            )}
            <button type="button" class="sheet-cancel" onClick={onClose}>Cancel</button>
          </>
        )}
        {view.kind === "rename" && (
          <RenameForm
            label={view.what === "session" ? "Rename tab" : "Rename task"}
            initial={view.what === "session" ? s.name : task?.name ?? ""}
            busy={busy}
            onCancel={() => setView({ kind: "menu" })}
            onSave={(name) =>
              run(async () => {
                if (view.what === "session") await rpc.call("session.rename", { id: s.id, name });
                else await rpc.call("channel.rename", { id: s.channel_id, name });
                notify("Renamed");
              })}
          />
        )}
        {view.kind === "confirm" && (
          <div class="sheet-confirm">
            <h2 id="sheet-title">{view.confirm.title}</h2>
            {view.confirm.body.map((line) => <p key={line}>{line}</p>)}
            <div class="sheet-actions">
              <button type="button" disabled={busy} onClick={() => setView({ kind: "menu" })}>Cancel</button>
              <button
                type="button"
                class={view.confirm.danger ? "danger-fill" : "primary"}
                disabled={busy || !ready.value}
                onClick={() => run(view.run)}
              >
                {busy ? "Working…" : view.confirm.action}
              </button>
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

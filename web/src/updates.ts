// Waiting updates (docs/PROTOCOL.md "Updates"): which one the update banner offers, if any.
// Pure logic; the banner is components/UpdateBanner.tsx.

/** `hello.updates` / the `updates_changed` event. */
export interface UpdateStatus {
  /** The installed UI release new pages load (null: embedded assets or MUSTER_WEB_DIR). */
  ui_release: string | null;
  /** Content id of a different backend build waiting at the backend's launch path. */
  backend: string | null;
  /** Content id of a different terminal host waiting beside it. */
  host: string | null;
}

export type UpdateKind = "backend" | "ui" | "host";

/** The update to offer, and the id that dismissing it remembers. */
export interface PendingUpdate {
  kind: UpdateKind;
  key: string;
}

/** Dismissed updates: per kind, the id of the one dismissed last. */
export type Dismissed = Partial<Record<UpdateKind, string>>;

/** The UI release a page was loaded from, from its script path (`/_ui/<id>/app.js`, as
 * `muster ui-install` pins it); null for embedded or directly served assets. */
export function releaseOfScript(src: string): string | null {
  let path = src;
  try {
    path = new URL(src, "http://x/").pathname;
  } catch { /* keep the raw string */ }
  const m = /^\/_ui\/([0-9a-fA-F-]{1,64})\//.exec(path);
  return m ? m[1] : null;
}

/** Every waiting update, in the order the banner offers them: restarting the backend first (it
 * may come with a matching UI), then reloading the UI, then the terminal host (a full restart). */
export function waitingUpdates(s: UpdateStatus | null, loaded: string | null): PendingUpdate[] {
  if (!s) return [];
  const out: PendingUpdate[] = [];
  if (s.backend) out.push({ kind: "backend", key: s.backend });
  if (s.ui_release && s.ui_release !== loaded) out.push({ kind: "ui", key: s.ui_release });
  if (s.host) out.push({ kind: "host", key: s.host });
  return out;
}

/** The one update the banner shows: the first waiting one that wasn't dismissed. Dismissing
 * hides that update only; a later, different one of the same kind shows again. A dismissed
 * update doesn't hide the ones after it. */
export function pickUpdate(s: UpdateStatus | null, loaded: string | null, dismissed: Dismissed): PendingUpdate | null {
  return waitingUpdates(s, loaded).find((u) => dismissed[u.kind] !== u.key) ?? null;
}

/** `daemon.restart`'s params: the backend and terminal host builds this window was shown (null:
 * none of that kind waiting). The backend refuses if what it would start differs, so a build
 * still being written, or replaced since, is never started. */
export function restartExpect(s: UpdateStatus | null): { backend: string | null; host: string | null } {
  return { backend: s?.backend ?? null, host: s?.host ?? null };
}

/** How long a window waits for the new backend after a restart before it says to run `muster`.
 * The restart command waits up to 120 s for the old backend to exit (saving state, ending every
 * session), 30 s for the terminal host, and about 15 s for the new backend to answer. */
export const RESTART_WAIT_MS = 180_000;

/** The backend a window last talked to: its pid, its terminal host's instance and its build
 * (`hello.build`) (null: the hello didn't say). While restarting, `build` is the build the
 * restart starts instead (see `restartWait`). */
export interface RestartWait {
  pid: number | null;
  host: string | null;
  build: string | null;
}

/** What a window waits for once a restart begins: another backend than `last`, on another
 * terminal host, running the backend build the window was shown (`status.backend`), or else the
 * same build as before. */
export function restartWait(last: RestartWait, status: UpdateStatus | null): RestartWait {
  return { ...last, build: status?.backend ?? last.build };
}

/** Did a hello on reconnect come from the restarted Muster? It must run on a terminal host, a
 * new one (the restart replaces it), and be another backend than the old one (which may answer
 * while it is still stopping, and a shell's `muster reload` meanwhile would start a backend on
 * the old host). What the old backend didn't say isn't compared. */
export function restartAnswered(
  restarting: RestartWait | null,
  hello: { pid?: number; terminal_host?: { instance: string } | null },
): boolean {
  if (!restarting) return false;
  const host = hello.terminal_host?.instance;
  if (!host || (restarting.host !== null && host === restarting.host)) return false;
  return restarting.pid === null || hello.pid === undefined || hello.pid !== restarting.pid;
}

/** What a hello on reconnect says of a restart: not done yet ("waiting"), done ("restarted"),
 * or done but by a backend of another build than the restart started ("other_build":
 * something else started Muster meanwhile). */
export function restartOutcome(
  restarting: RestartWait | null,
  hello: { pid?: number; build?: string | null; terminal_host?: { instance: string } | null },
): "waiting" | "restarted" | "other_build" {
  if (!restartAnswered(restarting, hello)) return "waiting";
  const build = hello.build ?? null;
  return restarting?.build && build && build !== restarting.build ? "other_build" : "restarted";
}

/** "3 sessions are running (2 agents)." for the shutdown and restart dialogs: `running` are the
 * sessions that haven't exited; shells aren't agents. */
export function runningText(running: { kind: string }[]): string {
  const n = running.length;
  if (n === 0) return "No sessions are running.";
  const agents = running.filter((s) => s.kind !== "shell").length;
  const head = n === 1 ? "1 session is running" : `${n} sessions are running`;
  if (agents === 0) return `${head}.`;
  if (agents === n) return `${head} ${n === 1 ? "(an agent)" : "(all agents)"}.`;
  return `${head} (${agents} ${agents === 1 ? "agent" : "agents"}).`;
}

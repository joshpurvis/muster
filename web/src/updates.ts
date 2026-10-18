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

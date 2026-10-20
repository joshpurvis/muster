// The web_preview panel's logic (WebPreview.tsx draws it): which URLs it frames, the frame's
// address on the panel server, and the notices it shows from the backend's check
// (`panel.preview_check`, src/web_preview.rs). Kept free of the DOM for preview_test.ts.

/** What the backend learned from the dev server (src/web_preview.rs `check`). */
export interface PreviewCheck {
  reachable: boolean;
  /** false: connected but no answer in 3 s; null: https (not read); true: answered. */
  answered?: boolean | null;
  status?: number;
  /** Why a browser would refuse to frame it, e.g. "X-Frame-Options: DENY". */
  refused?: string | null;
  /** A redirect to another origin, which the frame may not follow. */
  leaves_to?: string | null;
}

export type NoticeKind = "down" | "refused" | "leaves" | "silent" | "slow";

export interface Notice {
  kind: NoticeKind;
  title: string;
  detail: string;
  /** The raw evidence, shown in mono for the curious (a header, a redirect). */
  evidence?: string;
  /** Covers the frame (it shows only a browser error page), or a bar above it. */
  blocking: boolean;
}

/** While nothing answers, check again this often, for this long, then reload when it does. */
export const RETRY_MS = 2000;
export const RETRY_FOR_MS = 120_000;
/** A frame that has not finished loading by then gets a hint. */
export const SLOW_MS = 10_000;

// Not [::1]: browsers cannot hold a frame to an IPv6 address (src/web_preview.rs).
const HOSTS = new Set(["localhost", "127.0.0.1"]);

/** The UI's own check, after the backend's: only loopback http(s) without credentials. */
export function isPreviewUrl(value: unknown): value is string {
  if (typeof value !== "string") return false;
  try {
    const u = new URL(value);
    return (u.protocol === "http:" || u.protocol === "https:") && HOSTS.has(u.hostname) && !u.username && !u.password;
  } catch {
    return false;
  }
}

/** The panel server's page that frames `url` (src/server.rs `/preview`). */
export function previewFrameSrc(origin: string, url: string): string {
  return `${origin}/preview?url=${encodeURIComponent(url)}`;
}

/** The notice for a check, if it found a problem. `retrying`: still checking while it is down.
 * `host` names the dev server in the title ("localhost:5173"). */
export function previewNotice(check: PreviewCheck | null, retrying: boolean, host = "this address"): Notice | null {
  if (!check) return null;
  if (!check.reachable) {
    return {
      kind: "down",
      title: `Nothing is running at ${host} yet`,
      detail: retrying
        ? "The preview reloads by itself as soon as the dev server starts."
        : "Start the dev server, or check its port, then press Reload.",
      blocking: true,
    };
  }
  if (check.refused) {
    return {
      kind: "refused",
      title: "This page won't show in a panel",
      detail: "The server tells browsers not to put it in a frame. It works in your normal browser.",
      evidence: check.refused,
      blocking: true,
    };
  }
  if (check.leaves_to) {
    return {
      kind: "leaves",
      title: "This page sends you to another site",
      detail: `It redirects away from ${host}, and a preview stays on the dev server. Open it in your browser instead.`,
      evidence: `→ ${check.leaves_to}`,
      blocking: true,
    };
  }
  if (check.answered === false) {
    return {
      kind: "silent",
      title: "The server took the connection but has not answered yet.",
      detail: "It may still be starting, or be stuck.",
      blocking: false,
    };
  }
  return null;
}

/** Which input a frame shows: it loads again when the agent sends data (even the same URL), not
 * on a title-only update. Backends before `data_revision` fall back to the revision. */
export function previewRevision(panel: { revision: number; data_revision?: number }): number {
  return panel.data_revision || panel.revision;
}

export interface CheckLoop {
  check: () => Promise<PreviewCheck>;
  /** The latest result (null before any arrived); `retrying`: it is down and another check is due. */
  report: (check: PreviewCheck | null, retrying: boolean) => void;
  /** It answered after being down: load the page again. */
  up: () => void;
  now?: () => number;
  later?: (run: () => void, ms: number) => number;
  cancel?: (timer: number) => void;
}

/** Check a frame's server now and, while it is down or a check fails (a dropped connection),
 * every RETRY_MS until RETRY_FOR_MS after the start; then stop and report `retrying: false`.
 * Returns the stop function. */
export function startChecks(o: CheckLoop): () => void {
  const now = o.now ?? Date.now;
  const later = o.later ?? ((run, ms) => setTimeout(run, ms));
  const cancel = o.cancel ?? clearTimeout;
  const until = now() + RETRY_FOR_MS;
  let live = true, timer = 0, down = false;
  let last: PreviewCheck | null = null;
  const again = () => {
    if (now() >= until) return false;
    timer = later(run, RETRY_MS);
    return true;
  };
  function run() {
    o.check().then((c) => {
      if (!live) return;
      if (c.reachable && down) return o.up();
      down = !c.reachable;
      last = c;
      o.report(c, down && again());
    }, () => {
      if (!live) return;
      // Retried within the same deadline; the notice keeps what the last check found.
      const retrying = again();
      o.report(last, down && retrying);
    });
  }
  run();
  return () => {
    live = false;
    cancel(timer);
  };
}

export const SLOW_NOTICE: Notice = {
  kind: "slow",
  title: "Still loading after 10 s.",
  detail: "The dev server may still be compiling, or the page may be waiting on something.",
  blocking: false,
};

// ----- the address bar: the user types a local address; `panel.navigate` completes and checks
// it in the backend (src/web_preview.rs `parse_typed`), the UI never decides -----

export interface AddressState {
  /** The user's text, shown instead of the panel's address while `dirty`. */
  draft: string;
  /** The user has typed (focus alone is not an edit: the field keeps following the address). */
  dirty: boolean;
  /** The backend's reason the last address was refused, shown under the bar. */
  error: string;
  /** The navigation in flight (its request id), or 0. Escape does not end it. */
  pending: number;
  /** Counts typing and Escape, so a completion can tell whether the user moved on since. */
  edits: number;
  /** `edits` and the text when the pending navigation was sent. */
  sentEdits: number;
  sent: string;
}

export type AddressAction =
  | { type: "input"; text: string }
  | { type: "submit"; id: number; text: string }
  | { type: "done"; id: number }
  | { type: "refused"; id: number; reason: string }
  | { type: "cancel" }
  | { type: "blur"; url: string };

export const ADDRESS_IDLE: AddressState = { draft: "", dirty: false, error: "", pending: 0, edits: 0, sentEdits: 0, sent: "" };

/** Did the user leave the field alone since the pending navigation was sent? */
export const untouchedSinceSubmit = (s: AddressState) => s.edits === s.sentEdits;

export function addressStep(s: AddressState, a: AddressAction): AddressState {
  switch (a.type) {
    case "input": return { ...s, draft: a.text, dirty: true, error: "", edits: s.edits + 1 };
    // One navigation at a time; `text` is what the field showed.
    case "submit": return s.pending ? s : { ...s, pending: a.id, sent: a.text, sentEdits: s.edits, error: "" };
    // A completion that is not the pending one, or that the user moved on from, changes only `pending`.
    case "done":
      if (a.id !== s.pending) return s;
      return untouchedSinceSubmit(s) ? ADDRESS_IDLE : { ...s, pending: 0 };
    // A refused address stays, so the user can fix it.
    case "refused":
      if (a.id !== s.pending) return s;
      return untouchedSinceSubmit(s) ? { ...s, pending: 0, draft: s.sent, dirty: true, error: a.reason } : { ...s, pending: 0 };
    // Escape: show the panel's address again; a navigation in flight still completes.
    case "cancel": return { ...s, draft: "", dirty: false, error: "", edits: s.edits + 1 };
    // Leaving the field drops an edit that matches the address; other text and a refusal stay.
    case "blur": return !s.dirty || s.pending || s.error || s.draft.trim() !== a.url ? s : { ...s, draft: "", dirty: false };
  }
}

/** What the field shows: the user's text, or the panel's current address. */
export function addressText(s: AddressState, url: string | null): string {
  return s.dirty ? s.draft : url ?? "";
}

/** The address bar's status dot: the page shows (ok), the server answered but the page can't be
 * shown here (warn), or waiting (wait: nothing yet, still loading, or nothing answering). */
export function previewStatus(url: string | null, loaded: boolean, notice: Notice | null): "ok" | "warn" | "wait" {
  if (!url) return "wait";
  if (notice?.kind === "refused" || notice?.kind === "leaves") return "warn";
  if (notice?.kind === "down" || notice?.kind === "silent") return "wait";
  return loaded ? "ok" : "wait";
}

/** "1:20": what is left of the checks, for "stops in 1:20". */
export function clock(ms: number): string {
  const s = Math.max(0, Math.ceil(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

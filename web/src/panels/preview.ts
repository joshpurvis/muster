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

/** The notice for a check, if it found a problem. `retrying`: still checking while it is down. */
export function previewNotice(check: PreviewCheck | null, retrying: boolean): Notice | null {
  if (!check) return null;
  if (!check.reachable) {
    return {
      kind: "down",
      title: "Nothing is answering at this address yet.",
      detail: retrying
        ? "Start the dev server, or check its port. The preview reloads by itself once the server answers."
        : "Start the dev server, or check its port, then press Reload.",
      blocking: true,
    };
  }
  if (check.refused) {
    return {
      kind: "refused",
      title: "This page refuses to be shown inside another page.",
      detail: `It sends ${check.refused}. Open it in your browser instead, or allow framing in the dev server's settings.`,
      blocking: true,
    };
  }
  if (check.leaves_to) {
    return {
      kind: "leaves",
      title: "This page sends you on to another address.",
      detail: `It redirects to ${check.leaves_to}, which a preview cannot show. Open it in your browser instead.`,
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
  title: "Still loading after 10 seconds.",
  detail: "The dev server may be busy, or the page may be waiting on something.",
  blocking: false,
};

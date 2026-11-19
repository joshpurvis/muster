// Merges the human started in this window, each on its own countdown (docs/UI.md "PRs to merge").
// A click on Merge queues the card here; its countdown runs whatever the human does next (another
// card, another Merge, another tab), and at 0 the merge is sent. Only Cancel on that card stops
// it, or the card changing under it (the caller's `stop`). Each card has its own timer, so one
// card's countdown or send never waits on or ends another's. Pure: the clock, timers and the send
// come in `deps`, so tests drive it without a window (prs_merge_queue_test.ts).

/** What the human saw when they clicked Merge: the merge goes ahead only for that. */
export interface Seen {
  id: number;
  sha: string;
  base: string;
  post: number;
}

export interface Queued {
  seen: Seen;
  /** When the countdown runs out (ms, `deps.now`). */
  endsAt: number;
  /** "count": Cancel still stops it. "send": sent, waiting for Muster's answer. */
  phase: "count" | "send";
}

export interface QueueDeps {
  now: () => number;
  setTimer: (fn: () => void, ms: number) => unknown;
  clearTimer: (t: unknown) => void;
  /** Sends the merge (`pr.merge`); resolves once Muster has taken it. */
  send: (seen: Seen) => Promise<unknown>;
  /** The send failed (not connected, refused at once): `id` is back to its plain Merge. */
  failed: (seen: Seen, e: unknown) => void;
  /** Called after every change, with the whole queue. */
  changed: (all: ReadonlyMap<number, Queued>) => void;
}

export function mergeQueue(countdownMs: number, deps: QueueDeps) {
  const all = new Map<number, Queued>();
  const timers = new Map<number, unknown>();
  const changed = () => deps.changed(new Map(all));

  const fire = (id: number) => {
    timers.delete(id);
    const q = all.get(id);
    if (!q || q.phase !== "count") return;
    all.set(id, { ...q, phase: "send" });
    changed();
    deps.send(q.seen).then(
      () => { all.delete(id); changed(); },
      (e) => { all.delete(id); changed(); deps.failed(q.seen, e); },
    );
  };

  return {
    /** Start card `seen.id`'s countdown. False when that card has one already (a repeat click). */
    start(seen: Seen): boolean {
      if (all.has(seen.id)) return false;
      all.set(seen.id, { seen, endsAt: deps.now() + countdownMs, phase: "count" });
      timers.set(seen.id, deps.setTimer(() => fire(seen.id), countdownMs));
      changed();
      return true;
    },
    /** Stop card `id`'s countdown. False when there is none to stop (none, or sent already). */
    stop(id: number): boolean {
      const q = all.get(id);
      if (!q || q.phase !== "count") return false;
      deps.clearTimer(timers.get(id));
      timers.delete(id);
      all.delete(id);
      changed();
      return true;
    },
    get: (id: number) => all.get(id),
  };
}

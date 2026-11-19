import { mergeQueue, type Queued, type Seen } from "./prs_merge_queue.ts";

function assertEquals(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}

/** A queue on a fake clock: `tick(ms)` runs the timers due; `sent` and `failed` record calls. */
function setup(answer: (s: Seen) => Promise<unknown> = () => Promise.resolve()) {
  let now = 0;
  let next = 1;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const sent: number[] = [];
  const failed: number[] = [];
  let last: ReadonlyMap<number, Queued> = new Map();
  const q = mergeQueue(5000, {
    now: () => now,
    setTimer: (fn, ms) => { const id = next++; timers.set(id, { at: now + ms, fn }); return id; },
    clearTimer: (t) => { timers.delete(t as number); },
    send: (s) => { sent.push(s.id); return answer(s); },
    failed: (s) => { failed.push(s.id); },
    changed: (all) => { last = all; },
  });
  const tick = async (ms: number) => {
    now += ms;
    for (const [id, t] of [...timers].sort((a, b) => a[1].at - b[1].at)) {
      if (t.at <= now) { timers.delete(id); t.fn(); }
    }
    await new Promise((r) => setTimeout(r, 0));
  };
  return { q, tick, sent, failed, shown: () => last };
}
const seen = (id: number): Seen => ({ id, sha: `sha${id}`, base: "master", post: 1 });

Deno.test("merging a second card never stops the first: both are sent, each after its own countdown", async () => {
  const { q, tick, sent, shown } = setup();
  q.start(seen(1));
  await tick(1500);
  q.start(seen(2));
  assertEquals([...shown().keys()], [1, 2]);
  await tick(3500);
  assertEquals(sent, [1]);
  await tick(1500);
  assertEquals(sent, [1, 2]);
  assertEquals(shown().size, 0);
});

Deno.test("Cancel stops only its own card, and only before the merge is sent", async () => {
  const { q, tick, sent } = setup();
  q.start(seen(1));
  q.start(seen(2));
  assertEquals(q.stop(1), true);
  await tick(5000);
  assertEquals(sent, [2]);
  assertEquals(q.stop(2), false);
});

Deno.test("a repeat click on the same card starts nothing new; nothing is sent twice", async () => {
  const { q, tick, sent } = setup();
  assertEquals(q.start(seen(1)), true);
  await tick(2000);
  assertEquals(q.start(seen(1)), false);
  await tick(10_000);
  assertEquals(sent, [1]);
});

Deno.test("a send that fails is reported and frees the card; the other card still merges", async () => {
  const { q, tick, sent, failed, shown } = setup((s) => s.id === 1 ? Promise.reject(new Error("not connected")) : Promise.resolve());
  q.start(seen(1));
  q.start(seen(2));
  await tick(5000);
  assertEquals(sent, [1, 2]);
  assertEquals(failed, [1]);
  assertEquals(shown().size, 0);
  assertEquals(q.start(seen(1)), true);
});

Deno.test("while sent and waiting for Muster's answer, the card shows as sending and can't be cancelled", async () => {
  let answer = () => {};
  const { q, tick, shown } = setup(() => new Promise<void>((r) => { answer = r; }));
  q.start(seen(1));
  await tick(5000);
  assertEquals(shown().get(1)?.phase, "send");
  assertEquals(q.stop(1), false);
  answer();
  await tick(0);
  assertEquals(shown().size, 0);
});

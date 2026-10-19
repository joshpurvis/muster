import {
  deltas,
  fmtMs,
  formatReport,
  formatTable,
  type KeySample,
  KeyTracker,
  median,
  summarize,
  uaFamily,
} from "./latency.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

function key(idle: number, total: number): KeySample {
  return {
    sid: 1,
    idle,
    queue: 0.5,
    t0: 100,
    t1: 101,
    t2: 103,
    t3: 103.5,
    t4: 100 + total - 1,
    t5: 100 + total,
    frameGap: 16.7,
    webgl: true,
  };
}

Deno.test("deltas splits keydown-to-paint into its stages", () => {
  const d = deltas(key(50, 20));
  assertEquals(d, { ui: 1, rtt: 2, parse: 0.5, render: 15.5, paint: 1, total: 20 });
});

Deno.test("median handles odd, even and empty lists", () => {
  assertEquals(median([5, 1, 3]), 3);
  assertEquals(median([4, 1, 3, 2]), 2.5);
  assertEquals(Number.isNaN(median([])), true);
});

Deno.test("summarize separates first keys after a pause from bursts", () => {
  const s = summarize([key(Infinity, 90), key(2000, 50), key(1000, 70), key(80, 10), key(120, 30)]);
  assertEquals(s.idle, { n: 3, median: 70, max: 90 });
  assertEquals(s.burst, { n: 2, median: 20, max: 30 });
  const empty = summarize([]);
  assertEquals(empty.idle.n, 0);
});

Deno.test("fmtMs keeps one decimal below 100 ms and marks unknowns", () => {
  assertEquals(fmtMs(3.14159), "3.1");
  assertEquals(fmtMs(123.6), "124");
  assertEquals(fmtMs(NaN), "-");
  assertEquals(fmtMs(null), "-");
});

Deno.test("uaFamily tells the native window from Chromium", () => {
  const gtk =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
  const chromium =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
  assertEquals(uaFamily(gtk), "WebKitGTK");
  assertEquals(uaFamily(chromium), "Chromium");
  assertEquals(uaFamily("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"), "Firefox");
});

Deno.test("formatTable aligns columns and shows a first key", () => {
  const lines = formatTable([key(Infinity, 20)]).split("\n");
  assertEquals(lines[0], "  idle   evt    ui   rtt parse  rend paint total");
  assertEquals(lines[1], " first   0.5   1.0   2.0   0.5  15.5   1.0  20.0");
});

Deno.test("formatReport carries the environment, summary and the last keys", () => {
  const keys = Array.from({ length: 60 }, (_, i) => key(i === 0 ? Infinity : 40, 20 + i));
  const r = formatReport("WebKitGTK · devicePixelRatio 1", keys);
  const lines = r.split("\n");
  assertEquals(lines[1], "WebKitGTK · devicePixelRatio 1");
  assertEquals(r.includes("after ≥1s idle: n=1 median 20.0 ms, max 20.0 ms"), true);
  assertEquals(r.includes("in a burst:     n=59"), true);
  // Header plus the last 50 keys, each with the frame gap and WebGL columns.
  const rows = lines.slice(lines.findIndex((l) => l.startsWith("  idle")));
  assertEquals(rows.length, 51);
  assertEquals(rows[50].endsWith("  79.0  16.7     y"), true);
});

const K = { queue: 0.1, frameGap: 16, webgl: true };

/** One key through every stage, each `step` ms after the last, starting at t. */
function typeKey(tr: KeyTracker, sid: number, t: number, step = 1): boolean {
  tr.keydown(sid, t, K);
  tr.sent(sid, t + step);
  const waiting = tr.output(sid, t + 2 * step);
  tr.parsed(tr.gen, waiting, t + 3 * step);
  const rendered = tr.render(sid, t + 4 * step);
  return tr.painted(tr.gen, sid, rendered, t + 5 * step);
}

Deno.test("KeyTracker times a key through every stage", () => {
  const tr = new KeyTracker();
  assertEquals(typeKey(tr, 1, 100), true);
  assertEquals(tr.done.length, 1);
  assertEquals(deltas(tr.done[0]), { ui: 1, rtt: 1, parse: 1, render: 1, paint: 1, total: 5 });
  assertEquals(tr.pending.size, 0);
  typeKey(tr, 1, 400);
  assertEquals(tr.done[1].idle, 300);
});

Deno.test("KeyTracker drops a key whose output comes after the expiry, at any stage", () => {
  const tr = new KeyTracker();
  // A password prompt: no echo, then unrelated output 5 s later with no other keydown.
  tr.keydown(1, 200, K);
  tr.sent(1, 201);
  assertEquals(tr.output(1, 5200).length, 0);
  assertEquals(tr.pending.size, 0);
  // Expires between output and parse, and between render and the next frame.
  tr.keydown(1, 10000, K);
  tr.sent(1, 10001);
  const w = tr.output(1, 10002);
  tr.parsed(tr.gen, w, 13500);
  assertEquals(Number.isNaN(w[0].t3), true);
  tr.keydown(2, 20000, K);
  tr.sent(2, 20001);
  tr.parsed(tr.gen, tr.output(2, 20002), 20003);
  const r = tr.render(2, 20004);
  assertEquals(tr.painted(tr.gen, 2, r, 23500), false);
  assertEquals(tr.done.length, 0);
});

Deno.test("KeyTracker drops keys that sent nothing and ignores bytes with no key", () => {
  const tr = new KeyTracker();
  tr.keydown(1, 100, K); // Ctrl+Shift+C: no bytes
  tr.keydown(1, 150, K);
  assertEquals(tr.pending.get(1)!.length, 1);
  tr.sent(1, 151);
  tr.sent(1, 160); // a second chunk does not move t1
  assertEquals(tr.pending.get(1)![0].t1, 151);
  tr.sent(2, 170); // no key in this session
  assertEquals(tr.pending.has(2), false);
});

Deno.test("KeyTracker gives one output frame to every key waiting for it", () => {
  const tr = new KeyTracker();
  tr.keydown(1, 100, K);
  tr.sent(1, 101);
  tr.keydown(1, 110, K);
  tr.sent(1, 111);
  assertEquals(tr.output(1, 120).length, 2);
  assertEquals(tr.output(1, 125).length, 0);
});

Deno.test("KeyTracker ignores callbacks from before reset or clear", () => {
  const tr = new KeyTracker();
  tr.keydown(1, 100, K);
  tr.sent(1, 101);
  const gen = tr.gen;
  const w = tr.output(1, 102);
  tr.reset(); // the monitor was turned off (and maybe on again)
  tr.parsed(gen, w, 103);
  assertEquals(Number.isNaN(w[0].t3), true);
  assertEquals(tr.painted(gen, 1, w, 105), false);
  assertEquals(tr.done.length, 0);
  // After reset the idle baseline is gone: the next key is a first key.
  typeKey(tr, 1, 200);
  assertEquals(tr.done[0].idle, Infinity);

  // Clear also forgets finished keys, and a key rendered before it never lands.
  tr.keydown(1, 300, K);
  tr.sent(1, 301);
  tr.parsed(tr.gen, tr.output(1, 302), 303);
  const g2 = tr.gen;
  const r = tr.render(1, 304);
  tr.clear();
  assertEquals(tr.done.length, 0);
  assertEquals(tr.pending.size, 0);
  assertEquals(tr.painted(g2, 1, r, 305), false);
  assertEquals(tr.done.length, 0);
});

Deno.test("KeyTracker forgets a disposed terminal and keeps a bounded history", () => {
  const tr = new KeyTracker(3);
  tr.keydown(7, 100, K);
  tr.dispose(7);
  assertEquals(tr.pending.size, 0);
  typeKey(tr, 7, 200);
  assertEquals(tr.done[0].idle, Infinity);
  for (let i = 0; i < 5; i++) typeKey(tr, 1, 1000 + i * 50);
  assertEquals(tr.done.length, 3);
});

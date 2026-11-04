import { SampleBuffer } from "./metrics.ts";
import { KeyTracker } from "./latency.ts";

Deno.test("metrics buffer bounds disconnected history and reports lost samples", () => {
  const b = new SampleBuffer();
  for (let i = 0; i < 300; i++) b.add("ui.typing_ms", i);
  b.add("ui.typing_ms", NaN);
  b.add("ui.typing_ms", -1);
  const batch = b.take();
  if (batch.length !== 256 || batch[255].value !== 45) throw new Error("unbounded or silent loss");
  if (b.take().length) throw new Error("batch replayed");
});

Deno.test("a key without output expires visibly, a shortcut does not count as a failed echo", () => {
  const t = new KeyTracker(10);
  t.keydown(1, 0, { queue: null, frameGap: null, webgl: false });
  t.sent(1, 1);
  t.keydown(2, 0, { queue: null, frameGap: null, webgl: false });
  t.expire(4000);
  if (t.expired !== 1 || t.pending.size) throw new Error("timeout accounting incorrect");
  t.expire(8000);
  if (t.expired !== 1) throw new Error("counted twice");
});

Deno.test("typing tracking stays bounded when output stops", () => {
  const t = new KeyTracker(10);
  for (let i = 0; i < 1000; i++) {
    t.keydown(1, i, { queue: null, frameGap: null, webgl: false });
    t.sent(1, i);
  }
  if (t.pending.get(1)?.length !== 256 || t.overflow !== 744) throw new Error("unbounded pending keys");
});

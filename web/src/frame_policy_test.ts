import { framePolicy, refreshFramePolicy } from "./frame_policy.ts";
import { leaveWithDrafts } from "./leave.ts";
function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) {
    throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
  }
}
Deno.test("page frame policy stays fixed while registry and panel origins change", () => {
  const policy = framePolicy(
    "frame-ancestors 'none'; frame-src about: http://127.0.0.1:7434/proxy http://127.0.0.1:7434/preview http://localhost:5173 https://localhost:443",
  );
  equal(policy.panel, "http://127.0.0.1:7434");
  equal(policy.addons.has("http://localhost:5173"), true);
  equal(policy.addons.has("https://localhost"), true);
  equal(policy.addons.has("http://localhost:5174"), false);
  equal(framePolicy("").addons.size, 0);
  equal(framePolicy("frame-src * https: file://localhost:5173 http://elsewhere.test:5173").addons.size, 0);
});
Deno.test("changed panel origin reloads safely or prompts for unsaved notes", () => {
  const loaded = "http://127.0.0.1:7434", changed = "http://127.0.0.1:7435";
  let reloads = 0, prompts = 0;
  const reload = (unsaved: number[], stored: boolean) => () =>
    leaveWithDrafts(
      unsaved,
      () => stored,
      () => reloads++,
      () => prompts++,
    );
  equal(refreshFramePolicy(loaded, loaded, reload([], true)), false);
  equal(refreshFramePolicy(loaded, undefined, reload([], true)), false);
  equal(reloads, 0);
  equal(refreshFramePolicy(loaded, changed, reload([], true)), true);
  equal(reloads, 1);
  refreshFramePolicy(loaded, changed, reload([1], true));
  equal(reloads, 2);
  refreshFramePolicy(loaded, changed, reload([1], false));
  equal(reloads, 2);
  equal(prompts, 1);
});

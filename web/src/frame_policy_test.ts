import { framePolicy, needsFramePolicyReload } from "./frame_policy.ts";
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
Deno.test("matching or unknown panel origins do not show the reload banner", () => {
  const loaded = "http://127.0.0.1:7434";
  equal(needsFramePolicyReload(loaded, loaded), false);
  equal(needsFramePolicyReload(loaded, undefined), false);
  equal(needsFramePolicyReload(loaded, ""), false);
  equal(needsFramePolicyReload("", loaded), false);
});
Deno.test("panel reload banner tracks origin changes without automatically reloading", () => {
  const loaded = "http://127.0.0.1:7434", changed = "http://127.0.0.1:7435";
  let reloads = 0;
  const location = Object.getOwnPropertyDescriptor(globalThis, "location");
  Object.defineProperty(globalThis, "location", {
    configurable: true,
    value: { reload: () => reloads++ },
  });
  try {
    equal(needsFramePolicyReload(loaded, loaded), false);
    equal(needsFramePolicyReload(loaded, changed), true);
    equal(needsFramePolicyReload(loaded, changed), true); // Reconnecting to the same new backend.
    equal(needsFramePolicyReload(loaded, "http://127.0.0.1:7436"), true);
    equal(needsFramePolicyReload(loaded, loaded), false); // Another restart restored the loaded origin.
    equal(reloads, 0);
  } finally {
    if (location) Object.defineProperty(globalThis, "location", location);
    else Reflect.deleteProperty(globalThis, "location");
  }
});

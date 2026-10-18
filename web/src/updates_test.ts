import { pickUpdate, releaseOfScript, type UpdateStatus, waitingUpdates } from "./updates.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

const none: UpdateStatus = { ui_release: null, backend: null, host: null };

Deno.test("releaseOfScript reads the pinned release id", () => {
  assertEquals(releaseOfScript("http://127.0.0.1:7433/_ui/1760000000000-00ab12cd34ef5678/app.js"), "1760000000000-00ab12cd34ef5678");
  assertEquals(releaseOfScript("/_ui/1-a/app.js"), "1-a");
  assertEquals(releaseOfScript("http://127.0.0.1:7433/app.js"), null);
  assertEquals(releaseOfScript("/m/_ui/1-a/app.js"), null);
  assertEquals(releaseOfScript("/_ui/../app.js"), null);
});

Deno.test("no status or nothing new: no banner", () => {
  assertEquals(pickUpdate(null, null, {}), null);
  assertEquals(pickUpdate(none, null, {}), null);
  assertEquals(pickUpdate({ ...none, ui_release: "1-a" }, "1-a", {}), null);
});

Deno.test("a page from embedded assets sees a newly installed release", () => {
  assertEquals(pickUpdate({ ...none, ui_release: "1-a" }, null, {}), { kind: "ui", key: "1-a" });
});

Deno.test("priority: backend, then UI, then terminal host", () => {
  const all: UpdateStatus = { ui_release: "2-b", backend: "abc", host: "def" };
  assertEquals(waitingUpdates(all, "1-a").map((u) => u.kind), ["backend", "ui", "host"]);
  assertEquals(pickUpdate(all, "1-a", {})?.kind, "backend");
  // After the backend restart (no backend update left), the UI banner shows.
  assertEquals(pickUpdate({ ...all, backend: null }, "1-a", {})?.kind, "ui");
  assertEquals(pickUpdate({ ...all, backend: null }, "2-b", {})?.kind, "host");
});

Deno.test("dismissing hides that update until a different one arrives", () => {
  const s: UpdateStatus = { ...none, backend: "abc" };
  assertEquals(pickUpdate(s, null, { backend: "abc" }), null);
  assertEquals(pickUpdate({ ...s, backend: "abd" }, null, { backend: "abc" })?.key, "abd");
  // A dismissed update lets the next one through.
  assertEquals(pickUpdate({ ...s, host: "h1" }, null, { backend: "abc" }), { kind: "host", key: "h1" });
  assertEquals(pickUpdate({ ...s, ui_release: "2-b" }, "1-a", { backend: "abc", ui: "2-b" }), null);
});

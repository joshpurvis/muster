import {
  pickUpdate,
  releaseOfScript,
  restartAnswered,
  restartExpect,
  restartOutcome,
  restartWait,
  runningText,
  type UpdateStatus,
  waitingUpdates,
} from "./updates.ts";

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

Deno.test("a restart expects the builds the window was shown", () => {
  assertEquals(restartExpect(null), { backend: null, host: null });
  assertEquals(restartExpect(none), { backend: null, host: null });
  assertEquals(restartExpect({ ui_release: "1-a", backend: "abc", host: "def" }), { backend: "abc", host: "def" });
});

Deno.test("only another backend on a new terminal host ends the restart wait", () => {
  const old = { pid: 5, host: "h1", build: null };
  const on = (pid: number | undefined, host: string | null) => ({ pid, terminal_host: host ? { instance: host } : null });
  assertEquals(restartAnswered(null, on(6, "h2")), false, "not restarting");
  assertEquals(restartAnswered(old, on(5, "h1")), false, "the old backend, still stopping");
  assertEquals(restartAnswered(old, on(6, "h2")), true);
  // A shell's `muster reload` meanwhile: a new backend on the old host is not the restart.
  assertEquals(restartAnswered(old, on(6, "h1")), false, "a new backend on the old host");
  assertEquals(restartAnswered(old, on(6, null)), false, "a backend without a host");
  assertEquals(restartAnswered(old, { pid: 6 }), false, "a hello that names no host");
  assertEquals(restartAnswered({ pid: null, host: null, build: null }, on(6, "h2")), true, "the old backend said neither");
  assertEquals(restartAnswered(old, on(undefined, "h2")), true, "the new backend doesn't say its pid");
});

Deno.test("a restart expects the build it starts, and tells another one apart", () => {
  const last = { pid: 5, host: "h1", build: "aaaaaaaaaaaa" };
  // No backend update shown: the same build again; one shown: that one.
  assertEquals(restartWait(last, none), last);
  assertEquals(restartWait(last, { ui_release: null, backend: "bbbbbbbbbbbb", host: null }).build, "bbbbbbbbbbbb");
  const wait = restartWait(last, { ui_release: null, backend: "bbbbbbbbbbbb", host: "cccccccccccc" });
  const hello = (build?: string | null) => ({ pid: 6, build, terminal_host: { instance: "h2" } });
  assertEquals(restartOutcome(wait, { pid: 5, build: "aaaaaaaaaaaa", terminal_host: { instance: "h1" } }), "waiting");
  assertEquals(restartOutcome(wait, hello("bbbbbbbbbbbb")), "restarted");
  // Something else started Muster meanwhile (an older build, say): the restart is over, but not as asked.
  assertEquals(restartOutcome(wait, hello("aaaaaaaaaaaa")), "other_build");
  // A backend that doesn't say, or a wait that didn't know the build: not compared.
  assertEquals(restartOutcome(wait, hello(undefined)), "restarted");
  assertEquals(restartOutcome({ ...wait, build: null }, hello("dddddddddddd")), "restarted");
  assertEquals(restartOutcome(null, hello("bbbbbbbbbbbb")), "waiting");
});

Deno.test("the dialogs count running sessions and agents", () => {
  const sh = { kind: "shell" };
  const cl = { kind: "claude" };
  assertEquals(runningText([]), "No sessions are running.");
  assertEquals(runningText([sh]), "1 session is running.");
  assertEquals(runningText([cl]), "1 session is running (an agent).");
  assertEquals(runningText([cl, cl]), "2 sessions are running (all agents).");
  assertEquals(runningText([cl, sh, sh]), "3 sessions are running (1 agent).");
  assertEquals(runningText([cl, cl, sh]), "3 sessions are running (2 agents).");
});

import { addDismissal, coldKey, coldText, idleText, isCold, parseTtl, ttlFor, ttlText, wakeCold } from "./cache.ts";
import { DEFAULT_CACHE_TTL } from "./cache.ts";
import type { SessionInfo } from "./types.ts";

const MIN = 60_000;

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

function sess(over: Partial<SessionInfo> = {}): SessionInfo {
  return {
    id: 1, channel_id: 1, kind: "claude", name: "a", cwd: "/", state: "idle", state_detail: null,
    state_since: 0, title: null, parent_id: null, pid: null, exit_code: null, cols: 80, rows: 24,
    queued: 0, created_at: 0, last_turn_at: 0, cache_cold_at: 60 * MIN, ...over,
  };
}

Deno.test("waking an asleep (exited) agent is cold past cache_cold_at too", () => {
  assertEquals(wakeCold(sess({ state: "exited" }), 60 * MIN), true);
  assertEquals(wakeCold(sess({ state: "exited" }), 60 * MIN - 1), false);
  assertEquals(wakeCold(sess({ state: "exited", cache_cold_at: null }), 999 * MIN), false);
  assertEquals(wakeCold(sess({ state: "exited", kind: "shell" }), 999 * MIN), false);
  assertEquals(wakeCold(undefined, 999 * MIN), false);
});

Deno.test("cold only past cache_cold_at, while waiting, never for shells", () => {
  assertEquals(isCold(sess(), 60 * MIN - 1), false);
  assertEquals(isCold(sess(), 60 * MIN), true);
  for (const state of ["working", "starting", "exited"] as const) {
    assertEquals(isCold(sess({ state }), 999 * MIN), false, state);
  }
  for (const state of ["needs_approval", "asking_question", "error", "stuck"] as const) {
    assertEquals(isCold(sess({ state }), 999 * MIN), true, state);
  }
  assertEquals(isCold(sess({ kind: "shell" }), 999 * MIN), false);
  // No turn yet, the warning off, an older daemon: never cold.
  assertEquals(isCold(sess({ cache_cold_at: null }), 999 * MIN), false);
  assertEquals(isCold(sess({ cache_cold_at: undefined }), 999 * MIN), false);
});

Deno.test("copy and lifetimes", () => {
  assertEquals(idleText(80 * MIN), "1 h 20 m");
  assertEquals(idleText(120 * MIN), "2 h");
  assertEquals(idleText(45 * MIN), "45 m");
  assertEquals(ttlText(5), "5 min");
  assertEquals(ttlText(60), "1 h");
  assertEquals(ttlText(90), "1 h 30 min");
  assertEquals(
    coldText(sess(), 80 * MIN),
    "Idle 1 h 20 m. The prompt cache has likely expired, so your next message re-reads the whole conversation at full cost.",
  );
  assertEquals(ttlFor(sess(), DEFAULT_CACHE_TTL), 60);
  assertEquals(ttlFor(sess({ provider: "openrouter" }), DEFAULT_CACHE_TTL), 5);
  assertEquals(ttlFor(sess({ kind: "codex" }), DEFAULT_CACHE_TTL), 30);
  assertEquals(ttlFor(sess({ kind: "codex", provider: "openrouter" }), DEFAULT_CACHE_TTL), 10);
  assertEquals(ttlFor(sess({ kind: "shell" }), DEFAULT_CACHE_TTL), null);
  assertEquals([parseTtl(" 30 "), parseTtl("0"), parseTtl("1441"), parseTtl("1.5"), parseTtl("")], [30, null, null, null, null]);
});

Deno.test("a dismissal lasts one idle period", () => {
  const before = coldKey(sess({ last_turn_at: 5 }));
  assertEquals(before === coldKey(sess({ last_turn_at: 9 })), false, "the next turn's idle period is new");
  let list = addDismissal([], before, 2);
  list = addDismissal(list, "x", 2);
  list = addDismissal(list, before, 2);
  assertEquals(list, ["x", before]);
  assertEquals(addDismissal(list, "y", 2), [before, "y"]);
});

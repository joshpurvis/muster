import { byDay, channelColour, conversationKeeper, dayLabel, duration, endedMatches, endText } from "./ended.ts";
import type { EndedSession } from "./types.ts";

function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}

function e(id: number, at: number, over: Partial<EndedSession> = {}): EndedSession {
  return {
    id, channel_id: 1, channel_name: "alpha", channel_archived: false, kind: "claude", name: `tab ${id}`, title: null,
    cwd: "/home/u/demo-app", account_profile_id: null, agent: null, conversation: null, exit_code: null,
    created_at: at - 60_000, ended_at: at, ...over,
  };
}

const NOW = new Date(2026, 10, 23, 15, 0).getTime();
const at = (d: number, h: number) => new Date(2026, 10, d, h, 0).getTime();

Deno.test("days: today, yesterday, weekday, then dates (with the year when it differs)", () => {
  equal(dayLabel(at(23, 1), NOW), "Today");
  equal(dayLabel(at(22, 23), NOW), "Yesterday");
  equal(dayLabel(at(19, 12), NOW), "Thursday");
  equal(dayLabel(at(10, 12), NOW), "10 Nov");
  equal(dayLabel(new Date(2025, 11, 31, 12).getTime(), NOW), "31 Dec 2025");
});

Deno.test("rows keep their order and group by day", () => {
  const groups = byDay([e(5, at(23, 14)), e(4, at(23, 9)), e(3, at(22, 20), { channel_id: 2 }), e(1, at(10, 8))], NOW);
  equal(groups.map((g) => [g.day, g.rows.map((r) => r.id)]), [["Today", [5, 4]], ["Yesterday", [3]], ["10 Nov", [1]]]);
});

Deno.test("a channel keeps its colour, different channels mostly differ", () => {
  equal(channelColour(3), channelColour(3));
  // Neighbouring channels (consecutive ids) never share a colour.
  for (let id = 1; id < 20; id++) equal(channelColour(id) === channelColour(id + 1), false);
  equal(channelColour(-1).startsWith("var(--ansi-"), true);
});

Deno.test("filter: name, title, channel, agent and folder", () => {
  const x = e(1, NOW, { name: "build", title: "fixing login", channel_name: "beta", agent: "helper", cwd: "/srv/demo-shop" });
  for (const q of ["", "build", "login", "beta", "helper", "shop"]) equal(endedMatches(q, x), true);
  equal(endedMatches("zzz", x), false);
});

Deno.test("words: duration, how it ended, who keeps the conversation", () => {
  equal(duration(20_000), "under a minute");
  equal(duration(25 * 60_000), "25 min");
  equal(duration(125 * 60_000), "2 h 5 min");
  equal(duration(28 * 3600_000), "1 day 4 h");
  equal(endText(e(1, NOW)), "Still running when the tab closed (closing stopped it)");
  equal(endText(e(1, NOW, { exit_code: 0 })), "Had already finished (exit code 0)");
  equal(endText(e(1, NOW, { exit_code: 2 })), "Had already stopped (exit code 2)");
  equal(conversationKeeper(e(1, NOW)), null);
  equal(conversationKeeper(e(1, NOW, { conversation: "c1" })), "Claude Code");
  equal(conversationKeeper(e(1, NOW, { kind: "codex", conversation: "t1" })), "Codex");
});

import {
  pendingMove,
  restartShown,
  restartShownChanged,
  saveNotesBlocked,
  sessionBusy,
  switchMessage,
  agentStatus,
  carriesLateReply,
  ago,
  asleepText,
  isAsleep,
  wakeFailedText,
  parseSleepAfter,
  choiceOf,
  contextLabel,
  CUSTOM,
  effortAfterModel,
  effortsFor,
  feedFor,
  firstLine,
  lateStatus,
  lateText,
  keepForKind,
  listedModels,
  MEMORY_FILE_MAX,
  memoryFileNameError,
  memoryPathError,
  nameError,
  needsModel,
  agentBilling,
  providerPendingNote,
  sessionOnOpenRouter,
  priceLabel,
  quietAnswerText,
  quietStatus,
  quietWaitingText,
  readMemoryFile,
  unattendedWarning,
  upsertMessage,
  vendorGroups,
} from "./agents.ts";
import type { Agent, AgentMessage, LateAnswer, ModelCatalog, OpenRouterModel, SessionInfo } from "./types.ts";

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(msg);
}

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

const agent = (over: Partial<Agent> = {}): Agent => ({
  id: 1,
  name: "ada",
  kind: "claude",
  model: null,
  effort: null,
  persona: "",
  permissions: { codex_sandbox: "workspace_write", claude_mode: "default" },
  accepts_origins: ["human"],
  autostart: false,
  home_dir: "/d/agents/ada",
  channel_id: null,
  session_id: null,
  resumable: false,
  created_at: 0,
  ...over,
});

const session = (state: SessionInfo["state"], since = 100): SessionInfo => ({
  id: 7,
  channel_id: 3,
  kind: "claude",
  name: "ada",
  cwd: "/d/agents/ada",
  state,
  state_detail: null,
  state_since: since,
  title: null,
  parent_id: null,
  pid: null,
  exit_code: null,
  cols: 80,
  rows: 24,
  queued: 0,
  created_at: 0,
});

const msg = (id: number, to_agent = 1, over: Partial<AgentMessage> = {}): AgentMessage => ({
  id,
  from: "human",
  from_session: null,
  to: "ada",
  to_agent,
  to_session: 7,
  origin: "human",
  hop: 0,
  reply_to: null,
  created_at: id * 1000,
  text: `message ${id}`,
  reply: null,
  replied_at: null,
  refused: null,
  ...over,
});

Deno.test("agent status: never started, stopped, live", () => {
  assertEquals(agentStatus(agent(), undefined), { state: null, live: false, asleep: false, label: "stopped", since: null });
  const a = agent({ session_id: 7 });
  assertEquals(agentStatus(a, session("exited", 50)), {
    state: "exited",
    live: false,
    asleep: false,
    label: "stopped",
    since: 50,
  });
  assertEquals(agentStatus(a, session("needs_approval")), {
    state: "needs_approval",
    live: true,
    asleep: false,
    label: "needs approval",
    since: 100,
  });
});

Deno.test("agent status: asleep is its own state, never stopped or exited", () => {
  const a = agent({ session_id: 7, asleep: true, asleep_at: 40 });
  assertEquals(agentStatus(a, session("exited", 50)), {
    state: "exited",
    live: false,
    asleep: true,
    label: "asleep",
    since: 40,
  });
  // Its tab was closed for good: still asleep.
  check(isAsleep(a, undefined), "asleep without a session");
  // Waking: the record may lag behind the run that already started.
  check(!isAsleep(a, session("starting")), "a starting run is not asleep");
  check(!isAsleep(agent({ session_id: 7 }), session("exited")), "a stopped agent is not asleep");
  check(!isAsleep(undefined, session("exited")), "a plain tab is not asleep");
});

Deno.test("sleep: the idle time field and the asleep bar's words", () => {
  assertEquals([parseSleepAfter("60"), parseSleepAfter(" 5 "), parseSleepAfter("10080")], [60, 5, 10080]);
  assertEquals([parseSleepAfter("4"), parseSleepAfter("10081"), parseSleepAfter("1.5"), parseSleepAfter("")], [
    null,
    null,
    null,
    null,
  ]);
  const MIN = 60_000;
  assertEquals(
    asleepText("ada", 0, false, 90 * MIN),
    "ada has been asleep for 1 h 30 m. The next message sent with wake on, or typing here, wakes it into the same conversation.",
  );
  // An idle agent reset forgot its conversation: a wake starts a new one, whatever its old cache.
  assertEquals(
    asleepText("ada", null, true, 0, true),
    "ada is asleep. The next message sent with wake on, or typing here, wakes it into a new conversation, which reads its memory/ notes first.",
  );
  const cold = asleepText("ada", null, true, 0);
  check(cold.startsWith("ada is asleep. The next message") && cold.includes("full cost"), cold);
  // A wake that failed: still asleep, says why and how to try again.
  assertEquals(
    wakeFailedText("ada", "account profile Work is disabled.", false),
    "Waking ada failed: account profile Work is disabled. It is still asleep: the next message sent with wake on, or typing here, tries again.",
  );
  check(wakeFailedText("ada", "x", true).endsWith("at full cost."), "the cost still said");
});

Deno.test("ago and firstLine", () => {
  assertEquals(ago(0, 5_000), "now");
  assertEquals(ago(0, 45_000), "45s");
  assertEquals(ago(0, 12 * 60_000), "12m");
  assertEquals(ago(0, 3 * 3600_000), "3h");
  assertEquals(ago(0, 72 * 3600_000), "3d");
  assertEquals(firstLine("\n  \n  Reviews diffs.\nMore."), "Reviews diffs.");
  assertEquals(firstLine(null), "");
});

Deno.test("the message log: upsert keeps newest first, replies replace, filter by agent", () => {
  let log: AgentMessage[] = [];
  log = upsertMessage(log, msg(2));
  log = upsertMessage(log, msg(5, 2));
  log = upsertMessage(log, msg(3));
  assertEquals(log.map((m) => m.id), [5, 3, 2]);
  log = upsertMessage(log, msg(3, 1, { reply: "done" }));
  assertEquals(log.map((m) => [m.id, m.reply]), [[5, null], [3, "done"], [2, null]]);
  assertEquals(feedFor(log, 1).map((m) => m.id), [3, 2]);
  assertEquals(feedFor(log, null).length, 3);
  assertEquals(upsertMessage(log, msg(9), 2).map((m) => m.id), [9, 5]);
});

Deno.test("dialog rules: names, memory paths, the unattended warning", () => {
  assertEquals(nameError("scout-2", []), null);
  assertEquals(nameError("", []), "Give it a name");
  assertEquals(nameError("-x", []) !== null, true);
  assertEquals(nameError("Ada", ["ada"]), "An agent named Ada already exists");
  assertEquals(memoryPathError("people/ada.md"), null);
  for (const bad of ["", "/etc/x", "../x", "a/./b", "a//b", "dir/"]) {
    assertEquals(memoryPathError(bad) !== null, true);
  }
  assertEquals(unattendedWarning("claude", ["human"]), false);
  assertEquals(unattendedWarning("claude", ["human", "schedule"]), true);
  assertEquals(unattendedWarning("codex", ["webhook"]), false);
});

Deno.test("adding files to memory/: names, size, text only", async () => {
  assertEquals(memoryFileNameError("Style guide (v2).md"), null);
  const u = (...cs: number[]) => String.fromCodePoint(...cs);
  const bads = [
    "",
    ".env",
    "..",
    "a/b.md",
    "a\\b",
    `x${u(0x202e)}txt.exe`,
    `zero${u(0x200b)}width`,
    "nl\n.md",
  ];
  for (
    const bad of [
      ...bads,
      " notes.md",
      "notes.md ",
      `no${u(0x34f)}tes.md`,
      `e${u(0xfe0f)}.md`,
      `t${u(0xe0041)}.md`,
    ]
  ) {
    assertEquals(memoryFileNameError(bad) !== null, true, bad);
  }
  assertEquals(memoryFileNameError("a".repeat(201)) !== null, true);
  // The new-note field: the same characters refused in every part.
  assertEquals(memoryPathError(" people/ada.md "), null, "what was typed is trimmed");
  for (const bad of [`no${u(0x34f)}tes.md`, "people /ada.md", `dir${u(0x2066)}/x.md`, "tab\t.md"]) {
    assertEquals(memoryPathError(bad) !== null, true, bad);
  }
  const file = (name: string, body: BlobPart) => new File([body], name);
  assertEquals(await readMemoryFile(file("notes.md", "# Notes\nhi")), { text: "# Notes\nhi" });
  assertEquals(await readMemoryFile(file("bom.md", `${u(0xfeff)}hi`)), { text: "hi" });
  assertEquals(await readMemoryFile(file("p.png", new Uint8Array([0x89, 0x50, 0xff, 0xfe]))), {
    skip: "not a text file",
  });
  assertEquals(await readMemoryFile(file("nul.txt", "a\0b")), { skip: "not a text file" });
  assertEquals(await readMemoryFile(file("big.log", "x".repeat(MEMORY_FILE_MAX + 1))), {
    skip: "larger than 256 KB",
  });
  assertEquals("text" in await readMemoryFile(file("max.log", "x".repeat(MEMORY_FILE_MAX))), true);
  assertEquals("skip" in await readMemoryFile(file(".hidden", "x")), true);
});

Deno.test("model and effort selects: Default, listed, custom; a kind switch drops what it doesn't list", () => {
  const listed = ["opus", "sonnet"];
  assertEquals(choiceOf("", listed, false), "");
  assertEquals(choiceOf("  ", listed, false), "");
  assertEquals(choiceOf("opus", listed, false), "opus");
  assertEquals(choiceOf(" opus ", listed, false), "opus");
  // A stored value the list doesn't know stays, shown as custom.
  assertEquals(choiceOf("claude-opus-5-5", listed, false), CUSTOM);
  assertEquals(choiceOf("anything", [], false), CUSTOM);
  // "Custom…" picked: custom even while empty or matching a listed id.
  assertEquals(choiceOf("", listed, true), CUSTOM);
  assertEquals(choiceOf("opus", listed, true), CUSTOM);
  assertEquals(CUSTOM !== CUSTOM.trim(), true, "never a real (trimmed) value");
  assertEquals(keepForKind("high", ["low", "high"]), "high");
  assertEquals(keepForKind("max", ["low", "high"]), "");
  assertEquals(keepForKind("opus", ["gpt-6.1-sol"]), "");
  assertEquals(keepForKind("", ["low"]), "");
});

Deno.test("efforts per model: a model's own list, else the CLI's; a model change drops what it lacks", () => {
  const all = ["low", "high", "ultra"];
  const c: ModelCatalog = {
    claude: { models: [{ id: "opus", note: null, efforts: null }], efforts: ["low", "max"] },
    codex: {
      models: [{ id: "sol", note: null, efforts: null }, {
        id: "luna",
        note: null,
        efforts: ["low", "high"],
      }],
      efforts: all,
    },
  };
  assertEquals(effortsFor(c, "codex", "sol"), all);
  assertEquals(effortsFor(c, "codex", " luna "), ["low", "high"]);
  assertEquals(effortsFor(c, "codex", "my-own-model"), all);
  assertEquals(effortsFor(c, "claude", ""), ["low", "max"]);
  assertEquals(effortsFor(null, "codex", "sol"), [], "an older daemon: no list");
  const [sol, luna] = [effortsFor(c, "codex", "sol"), effortsFor(c, "codex", "luna")];
  assertEquals(effortAfterModel("ultra", sol, luna, false), "", "a picked level luna lacks");
  assertEquals(effortAfterModel("high", sol, luna, false), "high");
  assertEquals(effortAfterModel("", sol, luna, false), "");
  assertEquals(effortAfterModel("ultra", sol, luna, true), "ultra", "typed in Custom: kept");
  assertEquals(effortAfterModel("minimal", sol, luna, false), "minimal", "an unlisted stored level: kept");
  assertEquals(effortAfterModel("ultra", [], [], false), "ultra", "no catalog: shown as custom, kept");
});

const orModel = (id: string, over: Partial<OpenRouterModel> = {}): OpenRouterModel => ({
  id,
  name: id,
  vendor: id.replace(/^~/, "").split("/")[0],
  context_length: 200000,
  prompt_per_m: 3,
  completion_per_m: 15,
  created: 1,
  ...over,
});

Deno.test("OpenRouter: models grouped by vendor, the CLI's own vendor first, searched by every word", () => {
  const models = [
    orModel("google/gemini-3.8-flash", { name: "Google: Gemini 3.8 Flash" }),
    orModel("anthropic/claude-sonnet-5.5", { name: "Anthropic: Claude Sonnet 5.5" }),
    orModel("openai/gpt-6.1-sol", { name: "OpenAI: GPT-6.1 Sol" }),
    orModel("qwen/qwen3-coder", { name: "Qwen: Qwen3 Coder" }),
  ];
  assertEquals(vendorGroups(models, "", "anthropic").map((g) => g.vendor), ["anthropic", "google", "openai", "qwen"]);
  assertEquals(vendorGroups(models, "", "openai").map((g) => g.vendor), ["openai", "anthropic", "google", "qwen"]);
  assertEquals(vendorGroups(models, "qwen CODER", "anthropic").map((g) => g.models[0].id), ["qwen/qwen3-coder"]);
  assertEquals(vendorGroups(models, "sonnet 5.5", "openai").map((g) => g.vendor), ["anthropic"]);
  assertEquals(vendorGroups(models, "nothing-like-it", "openai"), []);
});

Deno.test("OpenRouter: prices, context, listed models and the Codex model rule", () => {
  assertEquals(priceLabel(orModel("a/b")), "$3 / $15");
  assertEquals(priceLabel(orModel("a/b", { prompt_per_m: 0.25, completion_per_m: 1.5 })), "$0.25 / $1.50");
  assertEquals(priceLabel(orModel("a/b", { prompt_per_m: 0.004, completion_per_m: 0 })), "<$0.01 / $0");
  assertEquals(priceLabel(orModel("a/b:free", { prompt_per_m: 0, completion_per_m: 0 })), "free");
  assertEquals(priceLabel(orModel("a/b", { prompt_per_m: null })), "");
  assertEquals([contextLabel(1_000_000), contextLabel(1_050_000), contextLabel(200_000), contextLabel(null)], [
    "1M",
    "1.1M",
    "200k",
    "",
  ]);
  const catalog: ModelCatalog = {
    claude: { models: [{ id: "opus", note: null, efforts: null }], efforts: [] },
    codex: { models: [{ id: "gpt-6-sol", note: null, efforts: null }], efforts: [] },
  };
  const or = [orModel("anthropic/claude-sonnet-5.5")];
  assertEquals(listedModels("subscription", "claude", catalog, or), ["opus"]);
  assertEquals(listedModels("openrouter", "codex", catalog, or), ["anthropic/claude-sonnet-5.5"]);
  assertEquals(listedModels("subscription", "codex", null, or), []);
  // Switching provider keeps a model only when the other list has it.
  assertEquals(keepForKind("opus", listedModels("openrouter", "claude", catalog, or)), "");
  assertEquals(needsModel("openrouter", "codex", " "), true);
  assertEquals(needsModel("openrouter", "claude", ""), false, "Claude Code's aliases map to OpenRouter's");
  assertEquals(needsModel("subscription", "codex", ""), false);
});

Deno.test("OpenRouter: the badge follows the run, not only the saved setting", () => {
  const run = (provider: "openrouter" | "subscription" | undefined, state: SessionInfo["state"] = "idle") => ({
    ...session(state),
    provider,
  });
  // Tabs: the session's own run.
  assertEquals(sessionOnOpenRouter(run("openrouter")), true);
  assertEquals(sessionOnOpenRouter(run("openrouter", "exited")), false, "nothing bills once it exited");
  assertEquals(sessionOnOpenRouter(run("subscription")), false);
  assertEquals(sessionOnOpenRouter(undefined), false);
  // Rows: switched to Subscription while running on OpenRouter: still badged, until restart.
  const onOr = agent({ provider: "subscription", session_id: 7 });
  const b = agentBilling(onOr, run("openrouter"));
  assertEquals([b.badge, b.label], [true, "OpenRouter · until restart"]);
  check(!!b.note?.includes("applies when it restarts"), "the row says the change waits");
  check(!!providerPendingNote(onOr, run("openrouter"))?.includes("still runs on OpenRouter"), "clicking it says so too");
  // Switched to OpenRouter while running on the subscription.
  const toOr = agentBilling(agent({ provider: "openrouter", session_id: 7 }), run("subscription"));
  assertEquals([toOr.badge, toOr.label], [true, "OpenRouter · from restart"]);
  // Settled, stopped, or an older backend without the session field.
  assertEquals(agentBilling(agent({ provider: "openrouter", session_id: 7 }), run("openrouter")).note, null);
  assertEquals(agentBilling(agent({ provider: "openrouter" }), undefined), {
    badge: true,
    label: "OpenRouter · pay per use",
    note: null,
  });
  assertEquals(agentBilling(agent({ provider: "subscription", session_id: 7 }), run(undefined)).badge, false);
  assertEquals(providerPendingNote(agent({ provider: "openrouter", session_id: 7 }), run("openrouter")), null);
  assertEquals(providerPendingNote(onOr, run("openrouter", "exited")), null);
});

Deno.test("a late reply says how it reached the asker", () => {
  const a = { text: "42", at: 1, from_session: 3, origin: "human" as const, hop: 2 };
  check(lateText({ ...a, status: "returned" }).includes("still waiting"), "returned");
  assertEquals(lateText({ ...a, status: "delivered", note: "delivered to ada as message 9" }), "Delivered to ada as message 9.");
  check(lateText({ ...a, status: "shown" }).startsWith("Shown here"), "shown");
  assertEquals(
    lateText({ ...a, status: "refused", note: "not delivered to ada: agent ada accepts only human messages" }),
    "Kept here, not delivered: to ada: agent ada accepts only human messages.",
  );
  assertEquals(lateText({ ...a, status: "undelivered", note: "not delivered: session 4, which asked, has ended" }),
    "Kept here, not delivered: session 4, which asked, has ended.");
});

Deno.test("a message that carried a late reply is told by its own text", () => {
  check(carriesLateReply(msg(9, 1, { reply_to: 4, text: 'Late reply to your message 4 ("x"):\nok' })), "carrier");
  check(!carriesLateReply(msg(9, 1, { reply_to: 4, text: "while handling 4" })), "ordinary chained message");
  check(!carriesLateReply(msg(9, 1, { reply_to: 5, text: "Late reply to your message 4:\nok" })), "another message's");
});

Deno.test("a quiet message says whether it waits, was shown or expired", () => {
  const t = (ms: number) => `t${ms}`;
  assertEquals(quietStatus(msg(9)), null);
  assertEquals(quietStatus(msg(9, 1, { quiet: true, refused: "agent ada has 20 messages waiting" })), null);
  const waiting = quietStatus(msg(9, 1, { quiet: true, shown_at: null, expired_at: null }), t)!;
  assertEquals(waiting.state, "waiting");
  assertEquals(waiting.chip, "waits for a turn");
  // It waits for a turn that can take it: a schedule's note waits for a turn of its own kind.
  assertEquals(
    waiting.text,
    "Waits for ada's next turn that can take it and arrives as context when that turn starts. Sending it didn't wake ada.",
  );
  const shown = quietStatus(msg(9, 1, { quiet: true, shown_at: 5000 }), t)!;
  assertEquals([shown.state, shown.chip, shown.text], ["shown", "shown", "Shown to ada at the start of a turn, t5000."]);
  const expired = quietStatus(msg(9, 1, { quiet: true, expired_at: 7000 }), t)!;
  assertEquals(expired.state, "expired");
  // It may have waited through turns of another kind: no claim that none came.
  assertEquals(expired.text, "Expired after 7 days without being shown to ada.");
  // An older daemon (no fields) reads as an ordinary message.
  assertEquals(quietStatus(msg(9, 1, { quiet: undefined })), null);
});

Deno.test("a late reply that went quietly says where its carrying message stands", () => {
  const t = (ms: number) => `t${ms}`;
  const answer = (over: Partial<LateAnswer> = {}): LateAnswer => ({
    text: "done", at: 0, from_session: 3, origin: "human", hop: 2, status: "delivered",
    note: "waits for ada's next turn that can take it, as message 12", msg: 12, session: 5, quiet: true, ...over,
  });
  const carrier = (over: Partial<AgentMessage> = {}) => msg(12, 1, { quiet: true, ...over });
  assertEquals(
    quietAnswerText(answer(), carrier(), t),
    "Waits for ada's next turn that can take it (message 12); it arrives as context when that turn starts.",
  );
  assertEquals(
    quietAnswerText(answer(), carrier({ shown_at: 9000 }), t),
    "Shown to ada at the start of a turn, t9000 (message 12).",
  );
  assertEquals(
    quietAnswerText(answer(), carrier({ expired_at: 9000 }), t),
    "Expired after 7 days without being shown to ada (message 12).",
  );
  // Not quiet, not delivered, or (an older daemon) the carrier not loaded: the stored note says it.
  assertEquals(quietAnswerText(answer({ quiet: false }), carrier(), t), null);
  assertEquals(quietAnswerText(answer({ status: "refused" }), carrier(), t), null);
  assertEquals(quietAnswerText(answer(), undefined, t), null);
  assertEquals(quietAnswerText(answer(), msg(13, 1, { quiet: true }), t), null);
});

Deno.test("a quiet late reply's status comes from the daemon whatever page holds its carrier", () => {
  const t = (ms: number) => `t${ms}`;
  const answer = (quiet_state: LateAnswer["quiet_state"]): LateAnswer => ({
    text: "done", at: 0, from_session: 3, origin: "human", hop: 2, status: "delivered",
    note: "waits for ada's next turn that can take it, as message 12", msg: 12, quiet: true, quiet_state,
  });
  // The carrier isn't loaded (log paging): the daemon's state still says where it stands.
  assertEquals(
    quietAnswerText(answer({ state: "shown", at: 9000, msg: 12, to: "ada" }), undefined, t),
    "Shown to ada at the start of a turn, t9000 (message 12).",
  );
  assertEquals(
    quietAnswerText(answer({ state: "expired", at: 9000, msg: 12, to: "ada" }), undefined, t),
    "Expired after 7 days without being shown to ada (message 12).",
  );
  // A plain session's carrier has no agent message at all.
  assertEquals(
    quietAnswerText(answer({ state: "waiting", session: 5 }), undefined, t),
    "Waits for session 5's next turn that can take it; it arrives as context when that turn starts.",
  );
  assertEquals(
    quietAnswerText(answer({ state: "dropped", reason: "the session ended before its next turn", session: 5 }), undefined, t),
    "Not delivered: the session ended before its next turn.",
  );
  assertEquals(quietAnswerText(answer({ state: "shown", at: 4000, session: 5 }), undefined, t), "Shown to session 5 at the start of a turn, t4000.");
});

Deno.test("the log's late reply chip and the overlay show one status for a quiet late reply", () => {
  const t = (ms: number) => `t${ms}`;
  const answer = (over: Partial<LateAnswer> = {}): LateAnswer => ({
    text: "done", at: 0, from_session: 3, origin: "human", hop: 2, status: "delivered",
    note: "waits for session 5's next turn that can take it", session: 5, quiet: true, ...over,
  });
  // The carrier was dropped (the asking session ended): not delivered, whatever the stored note said.
  assertEquals(
    lateStatus(answer({ quiet_state: { state: "dropped", reason: "the session ended before its next turn", session: 5 } }), undefined, t),
    { failed: true, text: "Not delivered: the session ended before its next turn." },
  );
  assertEquals(
    lateStatus(answer({ quiet_state: { state: "expired", session: 5 } }), undefined, t).failed,
    true,
  );
  assertEquals(
    lateStatus(answer({ quiet_state: { state: "shown", at: 4000, session: 5 } }), undefined, t),
    { failed: false, text: "Shown to session 5 at the start of a turn, t4000." },
  );
  // Not quiet: the stored status and note.
  assertEquals(lateStatus(answer({ quiet: false, note: "delivered to ada as message 9" }), undefined, t),
    { failed: false, text: "Delivered to ada as message 9." });
  assertEquals(lateStatus(answer({ quiet: false, status: "undelivered", note: null }), undefined, t).failed, true);
});

Deno.test("the tab badge counts quiet messages waiting", () => {
  assertEquals(quietWaitingText(1), "1 message waiting for a later turn (sent without waking it)");
  assertEquals(quietWaitingText(3), "3 messages waiting for a later turn (sent without waking it)");
});

Deno.test("Save notes first is offered only to a running, idle agent with room", () => {
  assertEquals(saveNotesBlocked(undefined, false, "Personal"), "It isn't running, so there is nothing to save from.");
  assertEquals(saveNotesBlocked(session("exited"), false, "Personal")?.startsWith("It isn't running"), true);
  assertEquals(saveNotesBlocked(session("working"), false, "Personal"), "It isn't idle. Muster asks only an idle agent.");
  assertEquals(saveNotesBlocked(session("error"), false, "Personal")?.startsWith("It isn't idle"), true);
  assertEquals(sessionBusy({ ...session("idle"), queued: 1 }), true);
  assertEquals(saveNotesBlocked(session("idle"), true, "Personal"), "Personal is at its limit, so it couldn't answer.");
  assertEquals(saveNotesBlocked(session("idle"), false, "Personal"), null);
});

Deno.test("the message after a move says when it takes effect", () => {
  const sw = { to: "Work", now: false, notes: "skipped" as const, notes_skipped: "not_running" as const };
  assertEquals(switchMessage("ada", { ...sw, outcome: "next_start" }).text, "Saved. ada moves to Work at its next start.");
  assertEquals(switchMessage("ada", { ...sw, now: true, outcome: "restarted" }).text,
    "ada moved to Work: a new conversation started there.");
  assertEquals(switchMessage("ada", { ...sw, now: true, outcome: "busy", notes_skipped: "busy" }).level, "warn");
  assertEquals(switchMessage("ada", { ...sw, now: true, outcome: "busy", why: "keys were typed into it just now" }).text,
    "ada can't restart now (keys were typed into it just now), so it moves to Work at its next start.");
  assertEquals(switchMessage("ada", { ...sw, outcome: "next_start", notes_skipped: "at_limit" }).text.includes("at its limit"), true);
  assertEquals(switchMessage("ada", { ...sw, now: true, notes: "asked", outcome: "after_notes" }).text.includes("waits up to 10 minutes for its reply, then restarts it on Work if it is idle; otherwise it moves at its next start"), true);
});

Deno.test("a move waiting for a restart says so, and offers Restart now only when it is idle", () => {
  const profiles = [{ id: 1, name: "Personal" }, { id: 2, name: "Work" }] as Parameters<typeof pendingMove>[2];
  const moving = agent({ account_profile_id: 2, handoff: { from: 1, at: 5 }, session_id: 7 });
  assertEquals(pendingMove(agent(), session("idle"), profiles), null, "no move waiting");
  assertEquals(pendingMove(agent({ kind: "codex", handoff: { from: 1, at: 5 } }), undefined, profiles), null);
  assertEquals(pendingMove(moving, session("idle"), profiles), {
    to: "Work",
    from: "Personal",
    text: "Moves to Work at its next start",
    running: true,
    restart: true,
    off: false,
    why: null,
    wait: undefined,
  });
  const busy = pendingMove(moving, session("working"), profiles)!;
  assertEquals([busy.restart, busy.why, busy.wait], [false, "It is busy. Restart now works once it is idle.", "when idle"]);
  const queued = pendingMove(moving, { ...session("idle"), queued: 2 }, profiles)!;
  assertEquals([queued.restart, queued.wait], [false, "messages waiting"]);
  check(queued.why!.startsWith("It has messages waiting to be typed"), queued.why!);
  const off = pendingMove(moving, session("exited"), profiles)!;
  assertEquals([off.restart, off.why], [false, "It isn't running: it starts on Work."]);
  assertEquals(pendingMove({ ...moving, account_profile_id: null }, undefined, profiles)!.to, "Default");
  // Its destination turned off since: no Restart now, and the reason says why.
  const disabled = [{ id: 1, name: "Personal", enabled: true }, { id: 2, name: "Work", enabled: false }] as Parameters<typeof pendingMove>[2];
  const turnedOff = pendingMove(moving, session("idle"), disabled)!;
  assertEquals([turnedOff.restart, turnedOff.off, turnedOff.wait, turnedOff.text], [false, true, "account disabled", "Can't move to Work yet: it is disabled"]);
  check(turnedOff.why!.startsWith("Work is disabled in Accounts"), turnedOff.why!);
  assertEquals(pendingMove(moving, session("exited"), disabled)!.off, true);
});

Deno.test("a busy tab's Restart confirmation asks again when its run or move changed", () => {
  const moving = agent({ account_profile_id: 2, handoff: { from: 1, at: 5 }, session_id: 7 });
  const run = { ...session("working"), pid: 100 };
  const shown = restartShown(moving, run);
  assertEquals(shown, { pid: 100, move: 5, to: 2 });
  assertEquals(restartShownChanged(shown, restartShown(moving, run)), false, "nothing changed");
  assertEquals(restartShownChanged(shown, restartShown({ ...moving, account_profile_id: 1, handoff: { from: 1, at: 9 } }, run)), true, "move replaced");
  assertEquals(restartShownChanged(shown, restartShown({ ...moving, handoff: null }, run)), true, "move applied or undone");
  assertEquals(restartShownChanged(shown, restartShown(moving, { ...run, pid: 101 })), true, "another run");
  assertEquals(restartShownChanged(shown, null), true, "the tab is gone");
  // No move shown: a move saved since is a change too.
  const plain = restartShown(agent({ session_id: 7 }), run);
  assertEquals(plain, { pid: 100, move: null, to: null });
  assertEquals(restartShownChanged(plain, restartShown(moving, run)), true);
});

import {
  agentStatus,
  ago,
  choiceOf,
  contextLabel,
  CUSTOM,
  effortAfterModel,
  effortsFor,
  feedFor,
  firstLine,
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
  readMemoryFile,
  unattendedWarning,
  upsertMessage,
  vendorGroups,
} from "./agents.ts";
import type { Agent, AgentMessage, ModelCatalog, OpenRouterModel, SessionInfo } from "./types.ts";

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
  assertEquals(agentStatus(agent(), undefined), { state: null, live: false, label: "stopped", since: null });
  const a = agent({ session_id: 7 });
  assertEquals(agentStatus(a, session("exited", 50)), {
    state: "exited",
    live: false,
    label: "stopped",
    since: 50,
  });
  assertEquals(agentStatus(a, session("needs_approval")), {
    state: "needs_approval",
    live: true,
    label: "needs approval",
    since: 100,
  });
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

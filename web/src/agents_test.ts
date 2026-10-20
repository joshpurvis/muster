import {
  agentStatus,
  ago,
  choiceOf,
  CUSTOM,
  effortAfterModel,
  effortsFor,
  feedFor,
  firstLine,
  keepForKind,
  MEMORY_FILE_MAX,
  memoryFileNameError,
  memoryPathError,
  nameError,
  readMemoryFile,
  unattendedWarning,
  upsertMessage,
} from "./agents.ts";
import type { Agent, AgentMessage, ModelCatalog, SessionInfo } from "./types.ts";

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
  for (const bad of ["", ".env", "..", "a/b.md", "a\\b", "x‮txt.exe", "zero​width", "nl\n.md"]) {
    assertEquals(memoryFileNameError(bad) !== null, true, bad);
  }
  assertEquals(memoryFileNameError("a".repeat(201)) !== null, true);
  const file = (name: string, body: BlobPart) => new File([body], name);
  assertEquals(await readMemoryFile(file("notes.md", "# Notes\nhi")), { text: "# Notes\nhi" });
  assertEquals(await readMemoryFile(file("bom.md", "﻿hi")), { text: "hi" });
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

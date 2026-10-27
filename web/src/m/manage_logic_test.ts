import {
  archiveConfirm,
  closeConfirm,
  isAgent,
  restartConfirm,
  stopConfirm,
  tildify,
  workspaceChoices,
} from "./manage_logic.ts";
import type { Channel, SessionInfo, Workspace } from "../types.ts";

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}
function has(text: string, part: string) {
  if (!text.includes(part)) {
    throw new Error(`Expected ${JSON.stringify(text)} to mention ${JSON.stringify(part)}`);
  }
}
function lacks(text: string, part: string) {
  if (text.includes(part)) {
    throw new Error(`Expected ${JSON.stringify(text)} not to mention ${JSON.stringify(part)}`);
  }
}

const session = (over: Partial<SessionInfo>) =>
  ({
    id: 1,
    channel_id: 10,
    kind: "claude",
    name: "ada",
    state: "idle",
    created_at: 0,
    ...over,
  }) as SessionInfo;
const channel = (over: Partial<Channel>) =>
  ({
    id: 10,
    workspace_id: 100,
    name: "fix login",
    owns_worktree: false,
    branch: null,
    created_at: 0,
    ...over,
  }) as Channel;
const workspace = (id: number, name: string, created_at = 0) =>
  ({ id, name, path: `/home/me/${name}`, is_git: true, created_at }) as Workspace;

Deno.test("a named agent is known from the daemon's list, or from its hidden tab", () => {
  equal(isAgent(session({}), null), false);
  equal(isAgent(session({}), new Set([1])), true);
  equal(isAgent(session({ hidden: true }), null), true);
});

Deno.test("closing a named agent's tab says it keeps running; any other tab says it stops", () => {
  const agent = closeConfirm(session({}), true, channel({}), true);
  has(agent.body.join(" "), "keeps running");
  lacks(agent.body.join(" "), "can't be undone");
  equal(agent.danger, false);
  const plain = closeConfirm(session({ state: "working" }), false, channel({}), false);
  has(plain.body.join(" "), "program stops");
  lacks(plain.body.join(" "), "task closes");
  equal(plain.danger, true);
  const last = closeConfirm(session({}), false, channel({ owns_worktree: true, branch: "muster/7" }), true);
  has(last.body.join(" "), "task closes too");
  has(last.body.join(" "), "muster/7");
  has(closeConfirm(session({ state: "exited" }), false, channel({}), false).body[0], "saved output");
});

Deno.test("stop and restart say what is lost", () => {
  has(stopConfirm(session({ state: "working" }), false).body[0], "current turn is lost");
  has(stopConfirm(session({ kind: "shell" }), false).body[0], "dev server");
  has(stopConfirm(session({}), true).body.join(" "), "named agent");
  equal(restartConfirm(session({ state: "idle" })), null);
  equal(restartConfirm(session({ kind: "shell", state: "working" })), null);
  has(restartConfirm(session({ state: "working" }))!.body.join(" "), "same conversation");
  has(restartConfirm(session({ kind: "codex", state: "needs_approval" }))!.body.join(" "), "starts fresh");
});

Deno.test("archiving names the running terminals, any named agent, and what stays", () => {
  const tabs = [
    session({ id: 1, state: "working" }),
    session({ id: 2, kind: "shell", state: "exited", name: "sh" }),
  ];
  const c = archiveConfirm(channel({}), tabs, null);
  has(c.body[0], "its running terminal and removes its 2 tabs");
  has(c.body.join(" "), "folder and files stay");
  has(archiveConfirm(channel({}), tabs, new Set([1])).body.join(" "), "named agent ada");
  has(archiveConfirm(channel({}), [tabs[1]], null).body[0], "Removes its tab.");
});

Deno.test("folders: most recently used first, the agents' own workspace left out", () => {
  const ws = [workspace(1, "old", 5), workspace(2, "recent", 1), workspace(3, "agents", 9)];
  const channels = [
    channel({ id: 20, workspace_id: 2, created_at: 50 }),
    channel({ id: 30, workspace_id: 3, created_at: 60 }),
  ];
  const all = [session({ id: 7, channel_id: 30, created_at: 70 })];
  equal(workspaceChoices(ws, channels, all, new Set([7])).map((w) => w.name), ["recent", "old"]);
  // A folder that merely happens to be called "agents" stays.
  equal(workspaceChoices(ws, channels, all, null).map((w) => w.name), ["agents", "recent", "old"]);
});

Deno.test("home becomes ~", () => {
  equal(tildify("/home/me/src/x", "/home/me"), "~/src/x");
  equal(tildify("/home/me", "/home/me/"), "~");
  equal(tildify("/home/meow", "/home/me"), "/home/meow");
  equal(tildify("/srv", ""), "/srv");
});

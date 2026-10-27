import {
  agentName,
  archiveConfirm,
  closeConfirm,
  restartConfirm,
  sameConfirm,
  splitFolder,
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

Deno.test("a named agent is known from the daemon's list (by its own name), or from its hidden tab", () => {
  equal(agentName(session({}), null), null);
  equal(agentName(session({}), new Map([[1, "scout"]])), "scout");
  equal(agentName(session({ hidden: true }), null), "ada");
  // A backend without agent_names: still an agent, under its tab's name.
  equal(agentName(session({}), new Map([[1, null]])), "ada");
});

Deno.test("closing a named agent's tab says it keeps running; any other tab says it stops", () => {
  const agent = closeConfirm(session({}), "scout", channel({}), true);
  has(agent.title, "scout's tab");
  has(agent.body.join(" "), "scout is a named agent");
  has(agent.body.join(" "), "keeps running");
  lacks(agent.body.join(" "), "can't be undone");
  equal(agent.danger, false);
  const plain = closeConfirm(session({ state: "working" }), null, channel({}), false);
  has(plain.body.join(" "), "program stops");
  lacks(plain.body.join(" "), "task closes");
  equal(plain.danger, true);
  const last = closeConfirm(session({}), null, channel({ owns_worktree: true, branch: "muster/7" }), true);
  has(last.body.join(" "), "task closes too");
  has(last.body.join(" "), "muster/7");
  has(closeConfirm(session({ state: "exited" }), null, channel({}), false).body[0], "saved output");
});

Deno.test("stop and restart say what is lost", () => {
  has(stopConfirm(session({ state: "working" }), null).body[0], "current turn is lost");
  has(stopConfirm(session({ kind: "shell" }), null).body[0], "dev server");
  // A named agent doesn't stay stopped: the next message or schedule starts it.
  has(
    stopConfirm(session({}), "scout").body.join(" "),
    "scout is a named agent: the next message or schedule",
  );
  equal(restartConfirm(session({ state: "idle" })), null);
  equal(restartConfirm(session({ kind: "shell", state: "working" })), null);
  // Claude and Codex both reopen their last conversation (Codex resume).
  has(restartConfirm(session({ state: "working" }))!.body.join(" "), "reopens its last conversation");
  has(
    restartConfirm(session({ kind: "codex", state: "needs_approval" }))!.body.join(" "),
    "reopens its last conversation",
  );
});

Deno.test("archiving names the running terminals, any named agent, and what stays", () => {
  const tabs = [
    session({ id: 1, state: "working" }),
    session({ id: 2, kind: "shell", state: "exited", name: "sh" }),
  ];
  const c = archiveConfirm(channel({}), tabs, null);
  has(c.body[0], "its running terminal and removes its 2 tabs");
  has(c.body.join(" "), "folder and files stay");
  const withAgent = archiveConfirm(channel({}), tabs, new Map([[1, "scout"]]));
  has(withAgent.body.join(" "), "named agent scout");
  // A tab that starts, ends or turns out to be an agent changes the confirmation.
  equal(sameConfirm(c, archiveConfirm(channel({}), tabs, null)), true);
  equal(sameConfirm(c, withAgent), false);
  // The same words about another task (same name, same tabs) are not the same confirmation.
  equal(sameConfirm({ ...c, targets: '{"task":10}' }, { ...c, targets: '{"task":11}' }), false);
  equal(
    sameConfirm(c, archiveConfirm(channel({}), [...tabs, session({ id: 3, state: "idle" })], null)),
    false,
  );
  has(archiveConfirm(channel({}), [tabs[1]], null).body[0], "Removes its tab.");
});

Deno.test("folders: most recently used first, the agents' own workspace left out", () => {
  const ws = [workspace(1, "old", 5), workspace(2, "recent", 1), workspace(3, "agents", 9)];
  const channels = [
    channel({ id: 20, workspace_id: 2, created_at: 50 }),
    channel({ id: 30, workspace_id: 3, created_at: 60 }),
  ];
  const all = [session({ id: 7, channel_id: 30, created_at: 70 })];
  equal(workspaceChoices(ws, channels, all, new Map([[7, "scout"]])).map((w) => w.name), ["recent", "old"]);
  // A folder that merely happens to be called "agents" stays.
  equal(workspaceChoices(ws, channels, all, null).map((w) => w.name), ["agents", "recent", "old"]);
});

Deno.test("home becomes ~", () => {
  equal(tildify("/home/me/src/x", "/home/me"), "~/src/x");
  equal(tildify("/home/me", "/home/me/"), "~");
  equal(tildify("/home/meow", "/home/me"), "/home/meow");
  equal(tildify("/srv", ""), "/srv");
});

Deno.test("folder suggestions keep their own name and where they are", () => {
  equal(splitFolder("~/src/muster/.claude/worktrees/agent-a1/"), {
    name: "agent-a1",
    parent: "~/src/muster/.claude/worktrees/",
  });
  equal(splitFolder("~/"), { name: "~", parent: "" });
  equal(splitFolder("/"), { name: "/", parent: "" });
});

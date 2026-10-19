// The merge command and merge event, shared by pr_decision (its Merge button) and pr_overview
// (Merge on a quick-win card), so both show and send exactly the same thing.
// docs/AGENT_PANELS.md "PR decision, end to end" is the contract.
import type { Draft } from "./questions.ts";

export const METHODS = ["squash", "merge", "rebase"] as const;
export type Method = typeof METHODS[number];

export const SHA = /^[0-9a-f]{40}$/;
export const REPO = /^[A-Za-z0-9_.-]{1,100}\/[A-Za-z0-9_.-]{1,100}$/;
/** Git ref rules that matter here: no spaces, control characters or ~^:?*[\\. */
export const branchOk = (b: string) =>
  b.length > 0 && b.length <= 255 && ![...b].some((c) => c.charCodeAt(0) < 33 || c.charCodeAt(0) === 127 || "~^:?*[\\".includes(c));

/** The exact command the agent runs on Merge (docs/AGENT_PANELS.md). `--repo` pins the target
 * even when the agent's environment sets GH_REPO; `--match-head-commit` pins the commit. */
export function mergeCommand(repo: string, number: number, method: Method, deleteBranch: boolean, headSha: string): string {
  if (!REPO.test(repo)) throw new Error("invalid repo");
  if (!Number.isSafeInteger(number) || number < 1) throw new Error("invalid PR number");
  if (!METHODS.includes(method)) throw new Error("invalid merge method");
  if (!SHA.test(headSha)) throw new Error("invalid head SHA");
  return `gh pr merge ${number} --repo ${repo} --${method}${deleteBranch ? " --delete-branch" : ""} --match-head-commit ${headSha}`;
}

export interface MergeEvent {
  action: "merge"; repo: string; number: number; base: string; method: Method; delete_branch: boolean; head_sha: string;
  answers: Record<string, string>; options: string[];
}

/** What a merge is about: the PR as the panel showed it. */
export interface MergeTarget { repo: string; number: number; base: string; head_sha: string }

/** The merge event; the text ends with the exact command. `draft` is what rides along (answers
 * and the user's switched-on options), described by `extra` for people. */
export function mergeEventFor(t: MergeTarget, method: Method, deleteBranch: boolean, draft: Draft, extra = ""):
  { kind: "merge"; text: string; data: MergeEvent } {
  const command = mergeCommand(t.repo, t.number, method, deleteBranch, t.head_sha);
  return {
    kind: "merge",
    text: `Merge PR #${t.number} (${t.repo}) into ${t.base}.${extra} Run exactly: ${command}`,
    data: { action: "merge", repo: t.repo, number: t.number, base: t.base, method, delete_branch: deleteBranch, head_sha: t.head_sha,
      answers: { ...draft.answers }, options: [...draft.options] },
  };
}

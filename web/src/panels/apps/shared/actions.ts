// The user's own actions on a PR (`actions` in pr_overview and pr_decision): buttons the agent
// adds for options the user asked for ("a Needs QA button"). Each sends one `custom` event; an
// `outward` one (it changes GitHub or anything off this machine) needs a second, armed confirm
// that shows what the agent will do. Custom actions never merge: the agent merges only on a
// pr_decision `merge` event. src/panel_apps.rs checks the same rules at panel_open/update.
import type { Problems } from "./validate.ts";

export interface CustomAction { id: string; label: string; key?: string; description: string; outward: boolean }

/** Single keys the built-in PR apps use (pr_decision: M R A O S H; pr_overview: J K O H N).
 * Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain letter or digit
 * never clashes with them. */
export const RESERVED_KEYS = ["m", "r", "a", "o", "s", "h", "j", "k", "n"] as const;
/** Event names of the built-in actions; a custom id may not reuse one. */
export const BUILTIN_IDS = ["merge", "request_changes", "ask", "open", "skip", "hold", "next", "custom", "closed", "overflow"] as const;
export const MAX_ACTIONS = 8;
const ID = /^[a-z0-9][a-z0-9_-]{0,39}$/;
const KEY = /^[a-z0-9]$/;

export function validateActions(p: Problems, o: Record<string, unknown>): CustomAction[] {
  const out: CustomAction[] = [];
  const list = p.array(o, "actions", "", { max: MAX_ACTIONS });
  const ids = new Set<string>(), keys = new Set<string>();
  list?.forEach((a, i) => {
    const at = `actions[${i}]`;
    const ao = p.object(a, at);
    if (!ao) return;
    p.known(ao, at, ["id", "label", "key", "description", "outward"]);
    const id = p.string(ao, "id", at, { required: true, max: 40 });
    if (id !== undefined) {
      if (!ID.test(id)) { p.add(`${at}.id`, "must be lowercase letters, digits, - or _ (at most 40)"); return; }
      if ((BUILTIN_IDS as readonly string[]).includes(id)) { p.add(`${at}.id`, `"${id}" is a built-in action`); return; }
      if (ids.has(id)) { p.add(`${at}.id`, `"${id}" is used twice`); return; }
      ids.add(id);
    }
    const label = p.string(ao, "label", at, { required: true, max: 40 });
    const description = p.string(ao, "description", at, { required: true, max: 300 });
    const rawKey = p.string(ao, "key", at, { max: 1, nonEmpty: true });
    let key: string | undefined;
    if (rawKey !== undefined) {
      const k = rawKey.toLowerCase();
      if (!KEY.test(k)) p.add(`${at}.key`, "must be one letter or digit");
      else if ((RESERVED_KEYS as readonly string[]).includes(k)) p.add(`${at}.key`, `"${k}" is a built-in key (${RESERVED_KEYS.join(" ")} are taken)`);
      else if (keys.has(k)) p.add(`${at}.key`, `"${k}" is used twice`);
      else { keys.add(k); key = k; }
    }
    const outward = p.boolean(ao, "outward", at) ?? false;
    if (id !== undefined && label !== undefined && description !== undefined) out.push({ id, label, key, description, outward });
  });
  return out;
}

export interface Target { repo: string; number: number; head_sha?: string }

export function customEvent(a: CustomAction, t: Target) {
  const data: { action: "custom"; id: string; repo: string; number: number; head_sha?: string } =
    { action: "custom", id: a.id, repo: t.repo, number: t.number };
  if (t.head_sha) data.head_sha = t.head_sha;
  return {
    kind: "custom" as const,
    text: `The user chose "${a.label}" (${a.id}) on PR #${t.number} (${t.repo}). Do exactly this and nothing else: ${a.description}`,
    data,
  };
}

/** The action bound to a plain key press, if any. */
export function actionForKey(actions: CustomAction[], key: string): CustomAction | undefined {
  const k = key.length === 1 ? key.toLowerCase() : "";
  return k ? actions.find((a) => a.key === k) : undefined;
}

/** What an outward confirm is bound to: the action as described and the PR it acts on. */
export function outwardTarget(a: CustomAction, t: Target): string {
  return JSON.stringify([a.id, a.label, a.description, a.outward, t.repo, t.number, t.head_sha ?? null]);
}

// The user's own actions on a PR (`actions` in pr_overview and pr_decision): buttons the agent
// adds for options the user asked for ("a Needs QA button"). In pr_decision a `toggle` action
// (the default `mode`) is an option switched on or off that rides along with the next event
// sent (shared/questions.ts); a `now` action, and every action in pr_overview, sends one
// `custom` event at once. An `outward` one (it changes GitHub or anything off this machine)
// needs a second, armed confirm that shows what the agent will do, before it is sent or
// switched on. Custom actions never merge: the agent merges only on a `merge` event.
// src/panel_apps.rs checks the same rules at panel_open/update.
import type { Problems } from "./validate.ts";

export const ACTION_MODES = ["toggle", "now"] as const;
export type ActionMode = typeof ACTION_MODES[number];
export interface CustomAction { id: string; label: string; key?: string; description: string; outward: boolean; mode: ActionMode }

/** Single keys the built-in PR apps use (pr_decision: M R A O S H; pr_overview: J K O H N).
 * Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain letter or digit
 * never clashes with them. */
export const RESERVED_KEYS = ["m", "r", "a", "o", "s", "h", "j", "k", "n"] as const;
/** Event names of the built-in actions; a custom id may not reuse one. */
export const BUILTIN_IDS = ["merge", "request_changes", "ask", "answers", "open", "skip", "hold", "next", "custom", "closed", "overflow"] as const;
export const MAX_ACTIONS = 8;
/** Visible names of the apps' own buttons: a custom label may not look like one of them. */
export const BUILTIN_LABELS = ["merge", "merge anyway", "confirm merge", "request changes", "ask", "open", "open on github",
  "github", "skip", "hold", "next", "cancel", "confirm", "send answers"] as const;

/** A label as the user reads it (same as src/panel_apps.rs `label_key`). */
export function labelKey(label: string): string {
  return label.split(/\s+/).filter(Boolean).join(" ").toLowerCase().replace(/[….!:]+$/u, "").replace(/^#+/, "").trim();
}
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
    p.known(ao, at, ["id", "label", "key", "description", "outward", "mode"]);
    const id = p.string(ao, "id", at, { required: true, max: 40 });
    if (id !== undefined) {
      if (!ID.test(id)) { p.add(`${at}.id`, "must be lowercase letters, digits, - or _ (at most 40)"); return; }
      if ((BUILTIN_IDS as readonly string[]).includes(id)) { p.add(`${at}.id`, `"${id}" is a built-in action`); return; }
      if (ids.has(id)) { p.add(`${at}.id`, `"${id}" is used twice`); return; }
      ids.add(id);
    }
    const label = p.string(ao, "label", at, { required: true, max: 40 });
    if (label !== undefined && (BUILTIN_LABELS as readonly string[]).includes(labelKey(label))) {
      p.add(`${at}.label`, `"${label}" looks like a built-in button; name what it does instead`);
    }
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
    // Required (Muster refuses an action without it); missing here means "ask first", the safe side.
    const outward = p.boolean(ao, "outward", at) ?? true;
    const mode = p.oneOf(ao, "mode", at, ACTION_MODES) ?? "toggle";
    if (id !== undefined && label !== undefined && description !== undefined) out.push({ id, label, key, description, outward, mode });
  });
  return out;
}

export interface Target {
  repo: string;
  number: number;
  head_sha?: string;
  /** Not sent; an outward confirm is bound to them too (the branch it targets, its status). */
  base?: string;
  status?: string;
}

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

/** What an outward confirm is bound to: the action as described and the PR it acts on, with
 * its head, base and status: a change to any of them cancels the confirm. */
export function outwardTarget(a: CustomAction, t: Target): string {
  return JSON.stringify([a.id, a.label, a.description, a.outward, t.repo, t.number, t.head_sha ?? null, t.base ?? null, t.status ?? null]);
}

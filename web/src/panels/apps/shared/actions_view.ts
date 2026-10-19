// Buttons and the outward confirm for the user's own actions (shared/actions.ts), shared by
// pr_overview and pr_decision so both look and behave the same.
import { h } from "./dom.ts";
import type { CustomAction } from "./actions.ts";

export function actionButton(a: CustomAction, onclick: () => void, disabled = false): HTMLButtonElement {
  // The title is the button's accessible description (its name comes from the label).
  return h("button", {
    type: "button", id: `act-${a.id}`, class: a.outward ? "custom outward" : "custom", disabled,
    title: `${a.description}${a.outward ? " (asks you to confirm first)" : ""}`, "aria-keyshortcuts": a.key?.toUpperCase(), onclick,
  }, a.label, a.outward ? h("span", { class: "out", "aria-hidden": "true" }, " ↗") : null,
  a.key ? [" ", h("kbd", null, a.key.toUpperCase())] : null) as HTMLButtonElement;
}

/** The second step of an outward action: says exactly what the agent will do. */
export function outwardDrawer(a: CustomAction, what: string, onConfirm: () => void, onCancel: () => void): HTMLElement {
  return h("div", { class: "drawer", role: "group", "aria-labelledby": "outward-title" },
    h("div", { id: "outward-title", class: "drawer-title" }, `${a.label} on ${what}? The agent will:`),
    h("p", { class: "prose outward-desc" }, a.description),
    h("p", { class: "muted small" }, "This changes things outside this machine (GitHub or elsewhere)."),
    h("div", { class: "row" },
      h("button", { type: "button", id: "confirm-outward", class: "primary", onclick: onConfirm }, `Confirm ${a.label} `, h("kbd", null, "Enter")),
      h("button", { type: "button", onclick: onCancel }, "Cancel ", h("kbd", null, "Esc"))));
}

/** Whether the panel has room to show the user's actions without folding them away. */
export const roomy = () => innerHeight >= 560 && innerWidth > 520;

/**
 * The user's own actions as their own, visibly separate group ("Your actions"), never mixed
 * with the built-in buttons. In a small panel it folds behind one toggle so the sticky bar stays
 * short; their keys work either way.
 */
export function actionsGroup(actions: CustomAction[], run: (a: CustomAction) => void, off: (a: CustomAction) => boolean,
  open: boolean, toggle: () => void): HTMLElement | null {
  if (!actions.length) return null;
  const folds = !roomy();
  const shown = !folds || open;
  return h("div", { class: "custom-group", role: "group", "aria-labelledby": "your-actions-label" },
    folds
      ? h("button", { type: "button", id: "your-actions", class: "fold", "aria-expanded": String(shown), onclick: toggle },
        h("span", { id: "your-actions-label" }, `Your actions (${actions.length})`), shown ? " ▴" : " ▾")
      : h("span", { id: "your-actions-label", class: "group-label" }, "Your actions"),
    shown ? actions.map((a) => actionButton(a, () => run(a), off(a))) : null);
}

/** The keyboard shortcuts, for the `?` key (key hints are hidden in small panels). */
export function shortcutsDrawer(rows: [string, string][], actions: CustomAction[], onClose: () => void): HTMLElement {
  const all = [...rows, ...actions.filter((a) => a.key).map((a) => [a.key!.toUpperCase(), `${a.label} (your action${a.outward ? ", asks first" : ""})`] as [string, string])];
  return h("div", { class: "drawer shortcuts", role: "group", "aria-labelledby": "shortcuts-title" },
    h("div", { id: "shortcuts-title", class: "drawer-title" }, "Keyboard shortcuts"),
    h("dl", null, all.map(([k, what]) => [h("dt", null, h("kbd", null, k)), h("dd", null, what)])),
    h("div", { class: "row" }, h("button", { type: "button", id: "shortcuts-close", onclick: onClose }, "Close ", h("kbd", null, "Esc"))));
}

export function helpButton(onclick: () => void, open: boolean): HTMLElement {
  return h("button", { type: "button", id: "keys-help", class: "help", "aria-label": "Keyboard shortcuts", "aria-expanded": String(open),
    "aria-keyshortcuts": "?", title: "Keyboard shortcuts (?)", onclick }, "?");
}

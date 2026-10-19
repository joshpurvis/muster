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

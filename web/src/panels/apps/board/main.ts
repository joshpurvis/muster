// The board app: a dashboard from plain JSON (title, scope, takeaway, then blocks of stats,
// charts, lists, meters, callouts and actions), drawn by the kit. No HTML and no script come
// from the agent. Actions send panel events with the kit's button rules (two steps when they
// carry `confirm`, busy then sent, off until the agent's next update).
import { sendEvent, startApp, typing } from "../shared/client.ts";
import { problems } from "../shared/dom.ts";
import { board } from "../../kit/board.ts";
import { press, resetAll, wireButtons } from "../../kit/buttons.ts";
import { validateBoard } from "./logic.ts";

const root = document.getElementById("app")!;

// Events are posted straight from the click or key handler: the host takes them only during a
// real user action in this panel.
const send = (kind: string, data: Record<string, string>, text: string) => sendEvent(kind, text, data);
wireButtons(send);

addEventListener("keydown", (e) => {
  if (typing(e) || e.ctrlKey || e.metaKey || e.altKey || e.repeat || e.key.length !== 1) return;
  const b = document.querySelector<HTMLButtonElement>(`[data-send][data-key="${CSS.escape(e.key.toLowerCase())}"]`);
  if (!b) return;
  e.preventDefault();
  b.focus();
  press(b, send);
});

startApp({
  name: "Muster Board",
  input: (data) => {
    const r = validateBoard(data);
    if (!r.ok) { problems(root, "This board panel's input is invalid", r.errors); return; }
    const focused = (document.activeElement as HTMLElement | null)?.dataset?.send;
    board(root, r.value, r.warnings);
    // Any update is the agent's answer: buttons waiting on it come back.
    resetAll();
    if (focused) document.querySelector<HTMLElement>(`[data-send="${CSS.escape(focused)}"]`)?.focus();
  },
});

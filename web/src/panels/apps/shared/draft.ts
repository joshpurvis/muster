// The user's draft in a PR decision panel: their answers and the toggle options they switched
// on, plus the bookkeeping that keeps a refused send from bringing back something stale.
// Pure (no DOM), so deno tests cover the races. shared/questions.ts has the input rules.
import type { CustomAction } from "./actions.ts";
import { type Choices, type Draft, draftAnswers, draftSig, pruneChoices, type Question, toggleSig } from "./questions.ts";

/** What a send was about: a refused send may undo its effects only while this still holds. */
export interface SendToken { gen: number; identity: string }

export class DraftState {
  /** The user's own picks and typed text, by question id. */
  choices: Choices = {};
  /** Toggle actions switched on, each with the action as it was when switched on (toggleSig):
   * an option counts only while the action still matches it. */
  private on = new Map<string, string>();
  /** The answers last sent (their draftSig): Send answers shows "sent" until they change. */
  sentAnswers: string | null = null;
  /** Which PR the draft belongs to (`repo#number`). */
  identity = "";
  /** Advances on every user edit, every send and every input from the agent. A refused send
   * may undo its own effects only if nothing happened since (same generation). */
  gen = 0;

  private bump() { this.gen++; }

  /** Something else happened (invalid input from the agent): earlier sends may undo nothing. */
  touch() { this.bump(); }

  token(): SendToken { return { gen: this.gen, identity: this.identity }; }

  /** Nothing changed since `t` was taken: no edit, no other send, no update from the agent. */
  fresh(t: SendToken): boolean { return t.gen === this.gen && t.identity === this.identity; }

  /** New input from the agent. Another PR starts a new draft; answers to questions that are
   * gone, and options whose action is gone or changed, drop. Always a new generation. */
  input(identity: string, questions: Question[], actions: CustomAction[]) {
    if (identity !== this.identity) {
      this.identity = identity;
      this.choices = {};
      this.on.clear();
      this.sentAnswers = null;
    }
    this.choices = pruneChoices(questions, this.choices);
    for (const [id, sig] of this.on) {
      const a = actions.find((x) => x.id === id);
      if (!a || a.mode !== "toggle" || toggleSig(a) !== sig) this.on.delete(id);
    }
    this.bump();
  }

  setChoice(id: string, value: string) {
    this.choices = { ...this.choices, [id]: value };
    this.bump();
  }

  switchOn(a: CustomAction) {
    this.on.set(a.id, toggleSig(a));
    this.bump();
  }

  switchOff(id: string) {
    this.on.delete(id);
    this.bump();
  }

  /** Whether the toggle is on for the action as it is now. */
  isOn(a: CustomAction): boolean { return a.mode === "toggle" && this.on.get(a.id) === toggleSig(a); }

  /** The draft now: answers, and the options switched on whose action still matches what the
   * user switched on (and, for an outward one, confirmed). */
  draft(questions: Question[], actions: CustomAction[]): Draft {
    return { answers: draftAnswers(questions, this.choices), options: actions.filter((a) => this.isOn(a)).map((a) => a.id) };
  }

  /**
   * A send carried `d`: its options went with it (one-shot) and switch off; its answers are
   * marked sent. Returns the rollback for a refusal: it restores exactly what this send spent,
   * and only if nothing happened since (no edit, no other send, no update from the agent) and
   * each option's action is still the one switched on. Returns whether it restored.
   */
  spend(d: Draft): (actions: CustomAction[]) => boolean {
    const spent = d.options.map((id) => [id, this.on.get(id)] as const).filter((x): x is readonly [string, string] => x[1] !== undefined);
    const wasSent = this.sentAnswers;
    for (const [id] of spent) this.on.delete(id);
    if (Object.keys(d.answers).length) this.sentAnswers = draftSig({ answers: d.answers, options: [] });
    this.bump();
    const t = this.token();
    return (actions: CustomAction[]) => {
      if (!this.fresh(t)) return false;
      for (const [id, sig] of spent) {
        const a = actions.find((x) => x.id === id);
        if (a && a.mode === "toggle" && toggleSig(a) === sig) this.on.set(id, sig);
      }
      this.sentAnswers = wasSent;
      this.bump();
      return true;
    };
  }
}

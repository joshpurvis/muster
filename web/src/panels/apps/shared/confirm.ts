// The second step of a deliberate action (Merge, an outward custom action). It is armed only
// once no key is held, so the key press that opened it, or its auto-repeat, can never also
// confirm; and any change to what it confirms (its `target`), or an invalid update, cancels it.

/** Keys that are only held to modify another (Shift for "merge anyway"); they never block arming. */
export const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]);

export class Gate {
  private held = new Set<string>();
  target: string | null = null;
  armed = false;

  /** `code` is the physical key (KeyboardEvent.code), so Shift+M and its "m" keyup match.
   * Returns true when the browser's default action must be prevented (a repeat while open). */
  keyDown(code: string, key: string, repeat: boolean): boolean {
    if (!MODIFIERS.has(key)) this.held.add(code);
    return this.target !== null && repeat;
  }

  keyUp(code: string) {
    this.held.delete(code);
    if (this.target !== null && this.held.size === 0) this.armed = true;
  }

  /** Focus left the app: keyups may never come. */
  blur() {
    this.held.clear();
    if (this.target !== null) this.armed = true;
  }

  openTarget(target: string) {
    this.target = target;
    this.armed = this.held.size === 0;
  }

  close() {
    this.target = null;
    this.armed = false;
  }

  get isOpen() { return this.target !== null; }

  /** A confirm may send only when armed and still about the same thing. */
  canConfirmTarget(target: string | null): boolean {
    return this.target !== null && this.armed && target !== null && target === this.target;
  }

  /** New input: returns true if it cancelled an open confirm. */
  inputTarget(target: string | null): boolean {
    if (this.target === null) return false;
    if (target !== null && target === this.target) return false;
    this.close();
    return true;
  }
}

// Mouse reports xterm sends to a program that tracks the mouse. The right button is Muster's (it
// opens the terminal menu), so no report of it may reach the program: not a press, a release, nor
// motion while it is held, in any encoding, with any modifiers.

/** xterm's CoreMouseButton.RIGHT: the button of a mouse event before it is encoded. */
export const RIGHT_BUTTON = 2;

// Button code bits (xterm's ctlseqs): low 2 bits the button (3: a release in the legacy encodings),
// 4/8/16 Shift/Alt/Ctrl, 32 motion, 64 wheel or buttons 4-7, 128 buttons 8-11.
const BUTTON_MASK = 3 | 64 | 128;

// Each matches one complete report and nothing else (anchored), so typed or pasted text is
// never mistaken for one unless it is exactly a report.
// deno-lint-ignore no-control-regex
const SGR = /^\x1b\[<(\d{1,6});\d{1,6};\d{1,6}[Mm]$/; // 1006 (and 1016 pixels)
// deno-lint-ignore no-control-regex
const URXVT = /^\x1b\[(\d{1,6});\d{1,6};\d{1,6}M$/; // 1015
// X10 / normal (one byte per value) and UTF-8 1005 (one character each, up to U+07FF).
// deno-lint-ignore no-control-regex
const LEGACY = /^\x1b\[M([\x20-\u07ff])[\x20-\u07ff][\x20-\u07ff]$/;

/** Is `data` exactly one mouse report of the right button (press, SGR release, or motion while
 * held, with any modifiers)? A legacy-encoding release carries no button (code 3); it is dropped
 * before encoding instead (see terms.ts). */
export function isRightButtonReport(data: string): boolean {
  let code: number;
  let m = SGR.exec(data);
  if (m) code = Number(m[1]);
  else if ((m = URXVT.exec(data))) code = Number(m[1]) - 32;
  else if ((m = LEGACY.exec(data))) code = m[1].charCodeAt(0) - 32;
  else return false;
  return code >= 0 && (code & BUTTON_MASK) === RIGHT_BUTTON;
}

import { isRightButtonReport } from "./mousereport.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

const S = String.fromCharCode;
const sgr = (code: number, up = false) => `\x1b[<${code};5;5${up ? "m" : "M"}`;
const urxvt = (code: number) => `\x1b[${code + 32};5;5M`;
const legacy = (code: number, col = 5, row = 5) => `\x1b[M${S(code + 32)}${S(col + 32)}${S(row + 32)}`;
const MODS = [0, 4, 8, 16, 4 | 8 | 16]; // none, Shift, Alt, Ctrl, all three

Deno.test("isRightButtonReport: right press, release and held motion, with any modifiers", () => {
  for (const mod of MODS) {
    for (const [name, enc] of [["sgr", sgr], ["urxvt", urxvt], ["legacy", legacy]] as const) {
      assertEquals(isRightButtonReport(enc(2 | mod)), true, `${name} press mod ${mod}`);
      assertEquals(isRightButtonReport(enc(2 | 32 | mod)), true, `${name} motion mod ${mod}`);
    }
    assertEquals(isRightButtonReport(sgr(2 | mod, true)), true, `sgr release mod ${mod}`);
  }
  // UTF-8 (1005): coordinates past 95 are characters above U+007F.
  assertEquals(isRightButtonReport(legacy(2, 300, 120)), true, "utf-8 press at col 300");
  assertEquals(isRightButtonReport(legacy(2 | 32, 1500, 9)), true, "utf-8 motion");
  // SGR pixel coordinates (1016) share the SGR shape.
  assertEquals(isRightButtonReport("\x1b[<34;1234;567M"), true, "sgr pixels motion");
});

Deno.test("isRightButtonReport: left, middle, wheel, plain motion and releases pass", () => {
  for (const mod of MODS) {
    for (const enc of [sgr, urxvt, legacy]) {
      for (const code of [0, 1, 32, 33, 35, 64, 65, 66, 67, 128, 130]) {
        assertEquals(isRightButtonReport(enc(code | mod)), false, `${enc(code | mod)}`);
      }
    }
    assertEquals(isRightButtonReport(sgr(0 | mod, true)), false, "sgr left release");
    assertEquals(isRightButtonReport(sgr(1 | mod, true)), false, "sgr middle release");
  }
  // A legacy release carries no button; it is filtered before encoding.
  assertEquals(isRightButtonReport(legacy(3)), false, "legacy release");
});

Deno.test("isRightButtonReport: keyboard and pasted text are never reports", () => {
  for (
    const d of [
      "",
      "a",
      "\r",
      "\x1b",
      "\x1b[A",
      "\x1b[1;5C",
      "\x1b[M", // incomplete
      '\x1b[M"%', // one byte short
      '\x1b[M"%%x', // a report followed by more text
      'x\x1b[M"%%', // text before it
      "paste \x1b[<2;5;5M here",
      "\x1b[<2;5;5M\x1b[<2;5;5m", // two reports in one chunk: not one report
      "\x1b[200~\x1b[<2;5;5M\x1b[201~", // bracketed paste of one
      "\x1b[<2;5M",
      "\x1b[<2;5;5X",
      "\x1b[2;5;5M", // urxvt code 2 - 32 < 0
      "\x1b[M\x01%%", // control byte as a code
    ]
  ) {
    assertEquals(isRightButtonReport(d), false, JSON.stringify(d));
  }
});

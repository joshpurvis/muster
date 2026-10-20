// The kit's colour tokens keep the contrast critique.md measured: text tokens at 4.5:1 or more on
// every surface they sit on, series colours at 3:1 on the canvas, and no series hue is the
// good-green or bad-red. Reads the tokens straight from kit.css, so an edit that breaks them fails.
import { check } from "../apps/shared/test_util.ts";
import { AVATARS, avatarColor } from "./parts.ts";

const css = await Deno.readTextFile(new URL("./kit.css", import.meta.url));

function tokens(selector: string): Record<string, string> {
  const start = css.indexOf(selector);
  check(start >= 0, `kit.css has ${selector}`);
  const body = css.slice(css.indexOf("{", start) + 1, css.indexOf("\n}", start));
  return Object.fromEntries([...body.matchAll(/--(mk-[\w-]+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
}

/** WCAG 2 contrast of two #rrggbb colours. */
export function contrast(a: string, b: string): number {
  const lum = (hex: string) => {
    const [r, g, bl] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
      .map((c) => c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [x, y] = [lum(a), lum(b)].sort((m, n) => n - m);
  return (x + 0.05) / (y + 0.05);
}

const themes = { dark: tokens(':root, [data-theme="dark"] {'), light: tokens('[data-theme="light"] {') };

Deno.test("kit text tokens keep 4.5:1 on every surface, in both themes", () => {
  for (const [name, t] of Object.entries(themes)) {
    for (const surface of ["mk-canvas", "mk-surface", "mk-raised", "mk-sunken"]) {
      for (const text of ["mk-text", "mk-text-2", "mk-text-3", "mk-accent", "mk-good", "mk-warn", "mk-bad", "mk-info"]) {
        const c = contrast(t[text], t[surface]);
        check(c >= 4.5, `${name}: ${text} ${t[text]} on ${surface} ${t[surface]} is ${c.toFixed(2)}:1`);
      }
    }
    check(contrast("#ffffff", t["mk-accent-fill"]) >= 4.5, `${name}: white on the accent fill`);
    check(contrast("#ffffff", t["mk-bad-fill"]) >= 4.5, `${name}: white on the danger fill (a primary danger button) is ${contrast("#ffffff", t["mk-bad-fill"]).toFixed(2)}:1`);
  }
});

Deno.test("series colours keep 3:1 on the canvas and are never the good or bad colour", () => {
  for (const [name, t] of Object.entries(themes)) {
    const series = Array.from({ length: 8 }, (_, i) => t[`mk-c${i + 1}`]);
    check(new Set(series).size === 8, `${name}: 8 different series colours`);
    for (const [i, c] of series.entries()) {
      check(contrast(c, t["mk-canvas"]) >= 3, `${name}: --mk-c${i + 1} ${c} on canvas is ${contrast(c, t["mk-canvas"]).toFixed(2)}:1`);
      check(c !== t["mk-good"] && c !== t["mk-bad"], `${name}: --mk-c${i + 1} is not a status colour`);
    }
  }
});

Deno.test("avatar colours keep white letters at 4.5:1", () => {
  for (const c of AVATARS) check(contrast("#ffffff", c) >= 4.5, `white on ${c} is ${contrast("#ffffff", c).toFixed(2)}:1`);
  check(avatarColor("dana") === avatarColor("Dana") && AVATARS.includes(avatarColor("lee")), "stable per person");
});

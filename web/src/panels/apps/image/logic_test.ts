import { fitScale, formatBytes, validateImage } from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

const png = "data:image/png;base64,iVBORw0KGgo=";

Deno.test("image input: a loaded path or an https url", () => {
  const r = validateImage({ path: "shots/a.png", src: png, mime: "image/png", bytes: 8, alt: "Login page", caption: "After" });
  check(r.ok);
  eq([r.value.src, r.value.source, r.value.alt, r.value.caption, r.value.bytes], [png, "shots/a.png", "Login page", "After", 8]);
  const u = validateImage({ url: "https://example.com/a.svg" });
  check(u.ok && u.value.src === "https://example.com/a.svg" && u.value.alt === "Image: a.svg", "no alt: named by its file");
  const c = validateImage({ url: "https://example.com/x/chart.png?v=2", caption: "Weekly CI minutes" });
  check(c.ok && c.value.alt === "Weekly CI minutes", "no alt: the caption describes it");
});

Deno.test("image input: clear errors for missing, mixed, unsafe or unloaded sources", () => {
  check(errorsOf(validateImage({}))[0].includes("needs path"));
  check(errorsOf(validateImage({ path: "a.png", url: "https://x.example/a.png" }))[0].includes("not both"));
  check(errorsOf(validateImage({ url: "http://x.example/a.png" }))[0].includes("https"));
  check(errorsOf(validateImage({ url: "https://user:pw@x.example/a.png" }))[0].includes("https"));
  check(errorsOf(validateImage({ url: "javascript:alert(1)" }))[0].includes("https"));
  check(errorsOf(validateImage({ path: "a.png" }))[0].includes("not loaded"));
  check(errorsOf(validateImage({ path: "a.png", src: "data:text/html;base64,PGI+" }))[0].includes("src"));
  check(errorsOf(validateImage({ path: "a.png", src: png, alt: 3 }))[0].startsWith("alt"));
  check(errorsOf(validateImage("x"))[0].includes("object"));
  const w = validateImage({ url: "https://x.example/a.png", width: 3 });
  check(w.ok && w.warnings[0].includes("width"));
});

Deno.test("image zoom and size helpers", () => {
  eq(fitScale(2000, 1000, 500, 500), 0.25);
  eq(fitScale(100, 100, 500, 500), 1);
  eq(fitScale(0, 0, 500, 500), 1);
  eq([formatBytes(900), formatBytes(2048), formatBytes(3 * 1024 * 1024)], ["900 B", "2.0 KiB", "3.0 MiB"]);
});

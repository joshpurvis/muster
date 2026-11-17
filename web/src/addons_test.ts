import { addonMessage, externalLink } from "./addons.ts";
function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) {
    throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
  }
}
const source = {} as Window;
const origin = "http://localhost:5173";
const receive = (data: unknown) => addonMessage({ source, origin, data }, source, origin);
Deno.test("add-on messages require the frame's exact source and origin", () => {
  const data = { muster: 1, type: "badge", count: 2 };
  equal(addonMessage({ source: {} as Window, origin, data }, source, origin), null);
  equal(addonMessage({ source, origin: "http://127.0.0.1:5173", data }, source, origin), null);
  equal(addonMessage({ source, origin: "null", data }, source, origin), null);
  equal(addonMessage({ source: null, origin, data }, null, origin), null);
  equal(receive(data), data);
});
Deno.test("add-on messages reject unknown types, fields and malformed shapes", () => {
  for (
    const data of [
      null,
      [],
      1,
      "badge",
      {},
      { muster: 2, type: "badge", count: 0 },
      { muster: 1, type: "rpc", method: "hello" },
      { muster: 1, type: "constructor" },
      { muster: 1, type: "badge" },
      { muster: 1, type: "badge", count: 2, extra: 0 },
      { muster: 1, type: "title", text: "x", html: true },
    ]
  ) equal(receive(data), null);
  for (const count of [-1, 1000, 0.5, "1", NaN, Infinity, null]) {
    equal(receive({ muster: 1, type: "badge", count }), null);
  }
  for (const count of [0, 999]) equal(receive({ muster: 1, type: "badge", count })?.type, "badge");
  for (const text of ["", "x".repeat(60), "🙂".repeat(60)]) {
    equal(receive({ muster: 1, type: "title", text })?.type, "title");
  }
  equal(receive({ muster: 1, type: "title", text: "x".repeat(61) }), null);
  equal(receive({ muster: 1, type: "title", text: 1 }), null);
  for (const text of ["", "x".repeat(301), 1]) {
    equal(receive({ muster: 1, type: "message-agent", agent: "alpha", text }), null);
  }
  equal(
    receive({ muster: 1, type: "message-agent", agent: "alpha", text: "x".repeat(300) })?.type,
    "message-agent",
  );
  equal(receive({ muster: 1, type: "message-agent", agent: "../beta", text: "hello" }), null);
});
Deno.test("add-on external links allow only bounded credential-free https URLs", () => {
  for (
    const url of [
      "http://example.test",
      "javascript:alert(1)",
      "file:///tmp/counter",
      "//example.test",
      "https://user:pass@example.test",
      "https://example.test/" + "a".repeat(2048),
    ]
  ) {
    equal(externalLink(url), null);
    equal(receive({ muster: 1, type: "open-link", url }), null);
  }
  equal(externalLink("https://example.test/path"), "https://example.test/path");
});

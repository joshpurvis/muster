import {
  ADDON_SANDBOX,
  addonMessage,
  addonMessageError,
  externalLink,
  pinnedLabel,
  reloadAddonFrame,
} from "./addons.ts";
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
Deno.test("add-on sandbox has exactly the three permitted capabilities", async () => {
  equal(ADDON_SANDBOX, "allow-scripts allow-same-origin allow-forms");
  const view = await Deno.readTextFile(new URL("./panels/AddonView.tsx", import.meta.url));
  equal(view.match(/sandbox=\{ADDON_SANDBOX\}/g)?.length, 1);
  equal(view.includes("allow-popups"), false);
});
Deno.test("frame receives only allowlisted message errors, never RPC details", () => {
  for (
    const text of [
      "Not sent: this agent isn't allowed.",
      "Not sent: limit of 30 an hour.",
      "Not sent: message must be 1-300 characters.",
      "Not sent.",
    ]
  ) equal(addonMessageError(new Error(text)), text);
  for (
    const error of [new Error("agent beta was removed"), new Error("Not sent. /tmp/daemon"), {
      message: "details",
    }, null]
  ) {
    equal(addonMessageError(error), "Not sent.");
  }
});
Deno.test("reload retries the same frame and its exact address", () => {
  let writes = 0;
  const url = "http://localhost:5173/?count=4";
  const frame = {
    get src() {
      return url;
    },
    set src(value) {
      equal(value, url);
      writes++;
    },
  };
  reloadAddonFrame(frame);
  equal(writes, 1);
  reloadAddonFrame(null);
});
Deno.test("Questions keeps open counts including zero; add-ons have their own count label", () => {
  equal(pinnedLabel("Questions", 3, false), "Questions, 3 open");
  equal(pinnedLabel("Questions", 0, false), "Questions, 0 open");
  equal(pinnedLabel("Counter", 4, true), "Counter, 4");
  equal(pinnedLabel("Counter", 0, true), "Counter");
});

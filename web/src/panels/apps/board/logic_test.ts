import { validateBoard } from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

const cost = JSON.parse(await Deno.readTextFile(new URL("../../../../dev/panel_examples/cost-breakdown.json", import.meta.url)));

Deno.test("board input: the toolkit's cost example passes as is", () => {
  const r = validateBoard(cost);
  check(r.ok, JSON.stringify(r));
  eq(r.warnings, []);
  eq(r.value.blocks.map((b) => b.kind), ["stats", "chart", "list", "meters", "callout", "actions"]);
});

Deno.test("board input: every problem is named with its path", () => {
  const e = errorsOf(validateBoard({
    title: "", min: 20, blocks: [
      { kind: "table" },
      { kind: "stats", items: [] },
      { kind: "chart", spec: { type: "pie", x: { values: [] }, series: [] } },
      { kind: "list", items: [{ title: "x", bar: 2, color: "red;x:y", icon: "<svg>" }] },
      { kind: "meters", items: [{ label: "a", value: 1, max: 0 }] },
      { kind: "callout" },
      { kind: "actions", items: [{ id: "closed", label: "x" }, { id: "go", label: "Go", key: "gg", data: { send: "x", nested: { a: 1 } } }] },
      { kind: "callout", text: "x", span: 2 },
    ],
  }));
  for (const want of [
    "title: must not be empty", "min: must be from 160 to 1200", "blocks[0].kind: must be one of stats, chart, list, meters, callout, actions",
    "blocks[1].items: needs at least 1 item", "blocks[2].spec.type: must be one of bar, line, scatter, area", "blocks[3].items[0].bar: must be from 0 to 1",
    "blocks[3].items[0].color: must be a kit colour token", "blocks[3].items[0].icon: must be an icon name", "blocks[4].items[0].max: must be more than 0",
    "blocks[5].text: is required", "blocks[6].items[0].id: must start with a letter", "blocks[6].items[1].key: is longer than 1",
    "blocks[6].items[1].data.send: names must be lowercase", "blocks[6].items[1].data.nested: must be a string", 'blocks[7].span: must be "full"',
  ]) check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  check(errorsOf(validateBoard({ title: "t", blocks: [] }))[0].startsWith("blocks: needs at least 1 item"));
});

Deno.test("board input: an unknown icon or field is a warning, never a blank panel", () => {
  const r = validateBoard({ title: "t", blocks: [{ kind: "callout", text: "x", icon: "unicorn", tone: "info", colour: "x" }] });
  check(r.ok && r.warnings.some((w) => w.includes('unknown icon "unicorn"')) && r.warnings.some((w) => w.includes("colour: unknown field")), JSON.stringify(r));
});

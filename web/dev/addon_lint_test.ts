// Public feature files must not contain names from the owner's private registry.
const root = new URL("../../", import.meta.url);
const base = Deno.env.get("XDG_DATA_HOME") || `${Deno.env.get("HOME")}/.local/share`;
const registry = `${base}/muster/addons.json`;
const extra = Deno.env.get("MUSTER_ADDON_LINT_NAMES");
const names: string[] = [];
let found = false;
try {
  const rows: { name: string }[] = JSON.parse(await Deno.readTextFile(registry));
  names.push(...rows.map((a) => a.name));
  found = true;
} catch (e) {
  if (!(e instanceof Deno.errors.NotFound)) throw e;
}
if (extra) {
  names.push(...(await Deno.readTextFile(extra)).split(/\r?\n/));
  found = true;
}
if (!found) console.log("skipped: no registry");
Deno.test("add-on public files keep registered names private", async () => {
  if (!found) return;
  const privateNames = [
    ...new Set(names.map((n) => n.trim().toLowerCase()).filter((n) => n && n !== "counter")),
  ];
  const files: string[] = JSON.parse(await Deno.readTextFile(new URL("./addon_files.json", import.meta.url)));
  const hits: string[] = [];
  for (const file of files) {
    const lines = (await Deno.readTextFile(new URL(file, root))).split("\n");
    lines.forEach((line, index) => {
      if (privateNames.some((name) => line.toLowerCase().includes(name))) hits.push(`${file}:${index + 1}`);
    });
  }
  if (hits.length) throw new Error(`Registered add-on name in public files:\n${hits.join("\n")}`);
  console.log(`passed: ${files.length} public files checked`);
});

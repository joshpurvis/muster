// Stamp the bytes actually built, so old open windows keep their own UI identity.
for (const path of Deno.args) {
  const source = await Deno.readTextFile(path);
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(source));
  const id = [...new Uint8Array(hash)].map((n) => n.toString(16).padStart(2, "0")).join("").slice(0, 16);
  if (!source.includes("muster-ui-build-placeholder")) {
    throw new Error(`missing metrics build marker: ${path}`);
  }
  await Deno.writeTextFile(path, source.replaceAll("muster-ui-build-placeholder", id));
}

import {
  ADDRESS_IDLE, addressStep, addressText, isPreviewUrl, type PreviewCheck, previewFrameSrc, previewNotice, previewRevision, RETRY_FOR_MS, RETRY_MS, startChecks,
} from "./preview.ts";

function check(value: unknown, message = "assertion failed") {
  if (!value) throw new Error(message);
}

Deno.test("only loopback http(s) URLs are framed", () => {
  for (const ok of ["http://localhost:5173/", "https://127.0.0.1:8443/a?b#c", "http://LOCALHOST:1/"]) {
    check(isPreviewUrl(ok), ok);
  }
  for (const bad of [
    "https://example.com/", "http://localhost.evil.com:5173/", "http://user@localhost:5173/", "http://localhost:5173@evil.com/",
    "javascript:alert(1)", "http://[::1]:3000/", "file:///etc/passwd", "ftp://localhost/", "not a url", "", 42, null, "http://192.168.1.2:5173/",
  ]) {
    check(!isPreviewUrl(bad), String(bad));
  }
});

Deno.test("the frame goes through the panel server's /preview page", () => {
  const src = previewFrameSrc("http://127.0.0.1:40001", "http://localhost:5173/a?b=1&c=2#d");
  check(src === "http://127.0.0.1:40001/preview?url=http%3A%2F%2Flocalhost%3A5173%2Fa%3Fb%3D1%26c%3D2%23d", src);
  check(new URL(src).searchParams.get("url") === "http://localhost:5173/a?b=1&c=2#d");
});

Deno.test("notices follow the check", () => {
  check(previewNotice(null, false) === null);
  check(previewNotice({ reachable: true, answered: true, status: 200 }, false) === null, "a working page has no notice");
  check(previewNotice({ reachable: true, answered: null }, false) === null, "https is not read");
  const down = previewNotice({ reachable: false }, true)!;
  check(down.kind === "down" && down.blocking && down.detail.includes("reloads by itself"), down.detail);
  check(previewNotice({ reachable: false }, false)!.detail.includes("press Reload"));
  const refused = previewNotice({ reachable: true, answered: true, status: 200, refused: "X-Frame-Options: DENY" }, false)!;
  check(refused.kind === "refused" && refused.detail.includes("X-Frame-Options: DENY") && refused.detail.includes("browser"));
  const leaves = previewNotice({ reachable: true, answered: true, status: 302, leaves_to: "https://login.example/" }, false)!;
  check(leaves.kind === "leaves" && leaves.detail.includes("https://login.example/"));
  const silent = previewNotice({ reachable: true, answered: false }, false)!;
  check(silent.kind === "silent" && !silent.blocking);
  // A refusal outranks a redirect; being down outranks both.
  check(previewNotice({ reachable: true, refused: "x", leaves_to: "y" }, false)!.kind === "refused");
  check(previewNotice({ reachable: false, refused: "x" }, false)!.kind === "down");
});

Deno.test("a frame reloads on data updates, not title-only ones", () => {
  check(previewRevision({ revision: 5, data_revision: 3 }) === 3, "title-only updates after data at 3");
  check(previewRevision({ revision: 5, data_revision: 5 }) === 5);
  check(previewRevision({ revision: 4 }) === 4, "older backends: every update");
});

/** A fake clock and timer queue for startChecks. */
function harness(answers: (PreviewCheck | Error)[]) {
  let t = 0;
  const timers: { at: number; run: () => void; id: number }[] = [];
  let ids = 0;
  const reports: { check: PreviewCheck | null; retrying: boolean }[] = [];
  let ups = 0, calls = 0;
  const stop = startChecks({
    check: () => {
      const a = answers[Math.min(calls++, answers.length - 1)];
      return a instanceof Error ? Promise.reject(a) : Promise.resolve(a);
    },
    report: (c, retrying) => reports.push({ check: c, retrying }),
    up: () => ups++,
    now: () => t,
    later: (run, ms) => { timers.push({ at: t + ms, run, id: ++ids }); return ids; },
    cancel: (id) => { const i = timers.findIndex((x) => x.id === id); if (i >= 0) timers.splice(i, 1); },
  });
  const settle = () => new Promise((r) => setTimeout(r, 0));
  /** Advance to the next timer and let its check settle. */
  const tick = async () => {
    await settle();
    const next = timers.sort((a, b) => a.at - b.at).shift();
    if (!next) return false;
    t = next.at;
    next.run();
    await settle();
    return true;
  };
  return { reports, tick, settle, stop, get ups() { return ups; }, get calls() { return calls; }, get pending() { return timers.length; } };
}

Deno.test("checks repeat while the server is down and reload once it answers", async () => {
  const h = harness([{ reachable: false }, { reachable: false }, { reachable: true, answered: true, status: 200 }]);
  await h.settle();
  check(h.reports.at(-1)!.retrying && h.reports.at(-1)!.check!.reachable === false, "down, retrying");
  await h.tick();
  await h.tick();
  check(h.ups === 1 && h.pending === 0, `reloads once it is up: ups ${h.ups}, pending ${h.pending}`);
});

Deno.test("a dropped connection during the retries is retried within the deadline, then given up", async () => {
  const lost = new Error("socket closed");
  const h = harness([{ reachable: false }, lost, lost, { reachable: true, answered: true }]);
  await h.settle();
  await h.tick();
  check(h.reports.at(-1)!.retrying && h.reports.at(-1)!.check?.reachable === false, "a failed check keeps the last result and keeps retrying");
  await h.tick();
  await h.tick();
  check(h.ups === 1, "recovers after the connection comes back");

  const g = harness([{ reachable: false }, lost]);
  await g.settle();
  while (await g.tick()) { /* run out the deadline */ }
  const last = g.reports.at(-1)!;
  check(!last.retrying && last.check?.reachable === false, "after the deadline it stops promising to reload");
  check(g.calls <= RETRY_FOR_MS / RETRY_MS + 2, `bounded checks: ${g.calls}`);
});

Deno.test("a working server is checked once; stop cancels a pending retry", async () => {
  const ok = harness([{ reachable: true, answered: true, status: 200 }]);
  await ok.settle();
  check(ok.calls === 1 && ok.pending === 0 && ok.reports.length === 1 && !ok.reports[0].retrying);
  const down = harness([{ reachable: false }]);
  await down.settle();
  check(down.pending === 1);
  down.stop();
  check(down.pending === 0, "stopped");
});

Deno.test("the address bar edits, submits, keeps a refused address and cancels", () => {
  const url = "http://localhost:5173/";
  let s = ADDRESS_IDLE;
  check(addressText(s, url) === url && addressText(s, null) === "");
  s = addressStep(s, { type: "focus", url });
  check(s.editing && addressText(s, url) === url, "focus starts from the current address");
  check(addressStep(s, { type: "blur", url }) === ADDRESS_IDLE, "leaving an untouched edit ends it");
  s = addressStep(s, { type: "input", text: "3000" });
  check(addressStep(s, { type: "blur", url }) === s, "typed text survives leaving the field");
  check(addressStep(s, { type: "focus", url: "http://localhost:9999/" }).draft === "3000", "refocus keeps the edit");
  s = addressStep(s, { type: "submit" });
  check(s.busy && addressStep(s, { type: "submit" }) === s, "one navigation at a time");
  check(addressStep(s, { type: "blur", url }) === s, "blur during a navigation keeps the edit");
  const refused = addressStep(s, { type: "refused", reason: "example.com is not this computer." });
  check(!refused.busy && refused.error.includes("not this computer") && addressText(refused, url) === "3000", "a refusal keeps the text");
  check(addressStep(refused, { type: "blur", url }) === refused, "a refusal stays visible after leaving the field");
  const typing = addressStep(refused, { type: "input", text: "3001" });
  check(typing.error === "" && typing.draft === "3001", "typing clears the reason");
  check(addressStep(typing, { type: "cancel" }) === ADDRESS_IDLE, "Escape restores the address");
  check(addressStep(addressStep(typing, { type: "submit" }), { type: "done" }) === ADDRESS_IDLE, "success ends the edit");
});

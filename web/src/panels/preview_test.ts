import {
  ADDRESS_IDLE, addressStep, addressText, isPreviewUrl, untouchedSinceSubmit, type PreviewCheck, previewFrameSrc, clock, previewNotice, previewRevision, previewStatus, RETRY_FOR_MS, RETRY_MS, startChecks,
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
  check(refused.kind === "refused" && refused.evidence === "X-Frame-Options: DENY" && refused.detail.includes("browser"));
  const leaves = previewNotice({ reachable: true, answered: true, status: 302, leaves_to: "https://login.example/" }, false)!;
  check(leaves.kind === "leaves" && leaves.evidence!.includes("https://login.example/"));
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

Deno.test("the address bar: typing, Enter on what it shows, a kept refusal, Escape", () => {
  const url = "http://localhost:5173/";
  let s = ADDRESS_IDLE;
  check(addressText(s, url) === url && addressText(s, null) === "");
  check(addressStep(s, { type: "blur", url }) === s, "focus and leave without typing: nothing changes");
  s = addressStep(s, { type: "input", text: "3000" });
  check(addressStep(s, { type: "blur", url }) === s, "typed text survives leaving the field");
  check(addressText(addressStep(s, { type: "input", text: url }), "x") === url && addressStep(addressStep(s, { type: "input", text: url }), { type: "blur", url }).dirty === false,
    "an edit back to the address ends on blur");
  s = addressStep(s, { type: "submit", id: 1, text: addressText(s, url) });
  check(s.pending === 1 && addressStep(s, { type: "submit", id: 2, text: "x" }) === s, "one navigation at a time");
  const refused = addressStep(s, { type: "refused", id: 1, reason: "example.com is not this computer." });
  check(!refused.pending && refused.error.includes("not this computer") && addressText(refused, url) === "3000", "a refusal keeps the text");
  check(addressStep(refused, { type: "blur", url }) === refused, "a refusal stays after leaving the field");
  const typing = addressStep(refused, { type: "input", text: "3001" });
  check(typing.error === "" && typing.draft === "3001", "typing clears the reason");
  check(addressStep(addressStep(typing, { type: "submit", id: 3, text: "3001" }), { type: "done", id: 3 }) === ADDRESS_IDLE, "success ends the edit");
});

Deno.test("after Escape, Enter submits the address the field shows (a reload), not an empty draft", () => {
  const url = "http://localhost:5173/";
  let s = addressStep(addressStep(ADDRESS_IDLE, { type: "input", text: "300" }), { type: "cancel" });
  check(addressText(s, url) === url, "Escape shows the address again");
  s = addressStep(s, { type: "submit", id: 1, text: addressText(s, url) });
  check(s.sent === url, `submits the shown address: ${JSON.stringify(s.sent)}`);
});

Deno.test("Escape during a navigation keeps it pending; its late answer does not touch newer text", () => {
  const url = "http://localhost:5173/";
  let s = addressStep(ADDRESS_IDLE, { type: "input", text: "example.com" });
  s = addressStep(s, { type: "submit", id: 1, text: "example.com" });
  s = addressStep(s, { type: "cancel" });
  check(s.pending === 1 && addressStep(s, { type: "submit", id: 2, text: url }) === s, "still one navigation at a time after Escape");
  const late = addressStep(s, { type: "refused", id: 1, reason: "nope" });
  check(!late.pending && late.error === "" && addressText(late, url) === url, "a refusal after Escape shows nothing against the address");
  const typed = addressStep(s, { type: "input", text: "4000" });
  const done = addressStep(typed, { type: "done", id: 1 });
  check(!done.pending && done.draft === "4000" && done.dirty, "a success after new typing keeps the new text");
  check(!untouchedSinceSubmit(typed), "the field is not released when the user moved on");
  check(addressStep(typed, { type: "done", id: 7 }) === typed, "an unknown completion changes nothing");
});

Deno.test("an untouched field follows the address when it changes elsewhere", () => {
  // Focus is not an edit: another window or the agent moves the panel; the field shows it.
  const s = ADDRESS_IDLE;
  check(addressText(s, "http://localhost:3000/") === "http://localhost:3000/");
  check(addressText(s, "http://localhost:4000/") === "http://localhost:4000/");
  check(addressStep(s, { type: "blur", url: "http://localhost:4000/" }) === s);
});

Deno.test("the address bar's dot and the countdown", () => {
  check(previewStatus(null, false, null) === "wait");
  check(previewStatus("http://localhost:5173/", true, null) === "ok");
  check(previewStatus("http://localhost:5173/", false, null) === "wait");
  check(previewStatus("http://localhost:5173/", true, previewNotice({ reachable: true, refused: "X-Frame-Options: DENY" }, false)) === "warn");
  check(previewStatus("http://localhost:5173/", false, previewNotice({ reachable: false }, true)) === "wait");
  check(previewNotice({ reachable: false }, true, "localhost:5173")!.title === "Nothing is running at localhost:5173 yet");
  check(clock(80_000) === "1:20" && clock(0) === "0:00" && clock(1) === "0:01");
});

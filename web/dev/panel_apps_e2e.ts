// Built-in panel apps against the MOCK daemon: image, chart, table and pr_decision open with
// sample data, react to real clicks and keys, and pr_decision's Merge → confirm sends exactly
// the event the panel showed. Screenshots of each app, dark and light, go to <shots>.
// Part of dev/e2e.ts; also runs alone (same flags as e2e.ts):
//
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots>,src,dev,panels \
//     --allow-write=<shots>,/tmp dev/panel_apps_e2e.ts http://127.0.0.1:17761/#token=dev <shots> <repo>
import { type Browser, launch } from "./cdp.ts";
import { chartSample, HEAD_SHA, lineSample, prSample, tableSample } from "./panel_samples.ts";

const M = "globalThis.__muster";
type Check = (cond: unknown, msg: string) => void;
export interface PanelAppsContext { a: Browser; repo: string; shots: string; ok: (msg: string) => void; check: Check }

export async function panelApps({ a, repo, shots, ok, check }: PanelAppsContext) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const hello = await R("hello", {});
  // A task of our own in the repo, so `image` can read a file from its worktree.
  const ws = await R("workspace.add", { path: repo });
  const ch = await R("channel.create", { workspace_id: ws.id, agent: "shell", name: "Panel apps", window_id: await a.eval(`${M}.windowId.value`), cols: 100, rows: 30 });
  await a.wait(`${M}.windowSessions.value.some((s) => s.channel_id === ${ch.id})`, 6000);
  const opener: number = await a.eval(`${M}.windowSessions.value.find((s) => s.channel_id === ${ch.id}).id`);
  const call = (method: string, args: Record<string, unknown>) => R(method, { from_session: opener, arguments: args });
  const events = async (pid: number) => (await call("panel.wait", { panel_id: pid, timeout_seconds: 0 })).events as any[];
  // Links go to the system browser; record them instead of opening tabs.
  await a.eval(`globalThis.open = (url) => { globalThis.__openedLinks = [...(globalThis.__openedLinks ?? []), url]; return null; }`);

  async function attach(pid: number) {
    await a.wait(`!!document.querySelector('[data-panel-view="${pid}"] iframe') && !document.querySelector('[data-panel-view="${pid}"] .panel-status')`, 8000);
    const doc: any = await a.send("DOM.getDocument", { depth: 0 });
    const node: any = await a.send("DOM.querySelector", { nodeId: doc.root.nodeId, selector: `[data-panel-id="${pid}"]` });
    const proxyId = (await a.send("DOM.describeNode", { nodeId: node.nodeId })).node.frameId;
    const frames: any[] = [];
    const collect = (n: any) => { frames.push(n.frame); for (const c of n.childFrames ?? []) collect(c); };
    collect((await a.send("Page.getFrameTree")).frameTree);
    let app = frames.find((f) => f.parentId === proxyId && f.url === "about:srcdoc");
    let session: string | undefined;
    if (!app) {
      const targets: any = await a.send("Target.getTargets");
      const child = targets.targetInfos.find((t: any) => t.type === "iframe" && t.parentFrameId === proxyId && t.url === "about:srcdoc");
      check(child, `panel ${pid}: app frame inside its proxy`);
      session = (await a.send("Target.attachToTarget", { targetId: child.targetId, flatten: true })).sessionId;
      app = { id: child.targetId };
    }
    const world: any = await a.send("Page.createIsolatedWorld", { frameId: app.id, worldName: `apps-${pid}` }, session);
    const inApp = async (expression: string): Promise<any> => {
      const r: any = await a.send("Runtime.evaluate", { expression, contextId: world.executionContextId, returnByValue: true, awaitPromise: true }, session);
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
      return r.result.value;
    };
    const waitIn = async (cond: string, ms = 4000) => {
      const end = Date.now() + ms;
      while (!(await inApp(`!!(${cond})`))) {
        if (Date.now() > end) throw new Error(`panel ${pid}: timed out waiting for ${cond}`);
        await a.sleep(50);
      }
    };
    const where = async (sel: string) => {
      const outer = (await a.rect(`[data-panel-id="${pid}"]`))!;
      const r = await inApp(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null;
        e.scrollIntoView({block: 'nearest'}); const r = e.getBoundingClientRect(); return {x: r.x + r.width / 2, y: r.y + r.height / 2}; })()`);
      check(r, `panel ${pid}: ${sel} exists`);
      return [outer.x + r.x, outer.y + r.y] as [number, number];
    };
    const click = async (sel: string) => { const [x, y] = await where(sel); await a.clickAt(x, y); await a.sleep(80); };
    const zoom = (mode: "fullscreen" | "inline") => inApp(`new Promise((resolve) => {
      const id = 'e2e-zoom-' + Math.random();
      const listen = (e) => { if (e.data?.id === id) { removeEventListener('message', listen); resolve(true); } };
      addEventListener('message', listen);
      parent.postMessage({jsonrpc: '2.0', id, method: 'ui/request-display-mode', params: {mode: '${mode}'}}, '*');
    })`);
    /** Zoomed, dark then light, and the page must not scroll sideways. */
    const shots2 = async (name: string) => {
      await zoom("fullscreen");
      for (const theme of ["dark", "light"]) {
        await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
        await waitIn(`document.documentElement.dataset.theme === '${theme}'`);
        await a.sleep(250);
        const r = (await a.rect(`[data-panel-view="${pid}"]`))!;
        await a.shot(`${shots}/panel-${name}-${theme}.png`, r);
      }
      await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
      await waitIn(`document.documentElement.dataset.theme === 'dark'`);
      check(await inApp(`document.documentElement.scrollWidth <= innerWidth + 1`), `${name}: no sideways scroll ${await inApp(`JSON.stringify([document.documentElement.scrollWidth, innerWidth, [...document.querySelectorAll('*')].filter((e) => e.getBoundingClientRect().right > innerWidth + 1).slice(0, 5).map((e) => e.tagName + '.' + e.getAttribute('class') + ':' + Math.round(e.getBoundingClientRect().right))])`)}`);
      await zoom("inline");
      await a.sleep(300); // the layout settles before the next click
    };
    return { inApp, waitIn, click, where, shots2, zoom };
  }

  const open = async (app: string, title: string, data: unknown) => {
    const pid = (await call("panel.open", { title, app, data })).panel_id as number;
    return { pid, ...(await attach(pid)) };
  };
  const close = (pid: number) => call("panel.close", { panel_id: pid });

  // ---- invalid input explains itself in the panel
  {
    const p = await open("chart", "Broken chart", { type: "pie", x: { values: [1, 2] }, series: [{ name: "a", values: [1] }] });
    await p.waitIn(`document.querySelector('.error')`);
    const text: string = await p.inApp(`document.querySelector('.error').textContent`);
    check(text.includes("type: must be one of bar, line, scatter, area") && text.includes("series[0].values: has 1 values"), `chart errors: ${text}`);
    await close(p.pid);
    ok("an invalid chart spec shows every problem in the panel");
  }

  // ---- image: a worktree file, zoom toggle; a URL's origin joins the panel CSP
  {
    const p = await open("image", "Icon", { path: "web/m/icon-512.png", alt: "Muster icon", caption: "The app icon, 512 px" });
    await p.waitIn(`document.querySelector('.stage img')?.naturalWidth === 512`);
    check(await p.inApp(`document.querySelector('.stage img').src.startsWith('data:image/png;base64,')`), "image arrives as a data URI");
    check(await p.inApp(`document.querySelector('.stage').classList.contains('fit')`), "starts zoomed to fit");
    await p.click(".bar button");
    check(await p.inApp(`document.querySelector('.stage').classList.contains('actual')`), "button switches to actual size");
    await a.key("z");
    check(await p.inApp(`document.querySelector('.stage').classList.contains('fit')`), "Z switches back to fit");
    check((await p.inApp(`document.querySelector('.caption').textContent`)) === "The app icon, 512 px", "caption shows");
    await p.shots2("image");
    await close(p.pid);
    const u = await call("panel.open", { title: "Remote", app: "image", data: { url: "https://example.invalid/a.png" } });
    const panel = (await R("hello", {})).panels.find((x: any) => x.id === u.panel_id);
    check(panel.csp.resourceDomains.includes("https://example.invalid"), `url origin declared: ${JSON.stringify(panel.csp)}`);
    const q = await attach(u.panel_id);
    await q.waitIn(`document.querySelector('.error')`, 8000);
    check((await q.inApp(`document.querySelector('.error').textContent`)).includes("resourceDomains"), "unreachable URL explains itself");
    await close(u.panel_id);
    ok("image: worktree file as a data URI, zoom by button and Z, URL origin declared, load errors explained");
  }

  // ---- chart: SVG with accessible title, hidden data table, hover and keyboard tooltip
  {
    const p = await open("chart", "CI minutes", chartSample);
    await p.waitIn(`document.querySelector('svg[role=img]')`);
    check((await p.inApp(`document.querySelector('svg title').textContent`)) === "CI minutes per day", "svg title");
    check((await p.inApp(`document.querySelector('svg desc').textContent`)).includes("Bar chart of 2 series"), "svg desc");
    check((await p.inApp(`document.querySelectorAll('rect.bar').length`)) === 13, "13 bars (one null skipped)");
    check((await p.inApp(`document.querySelectorAll('.sr-only table.data tbody tr').length`)) === 7, "hidden data table");
    const [x, y] = await p.where(`rect.bar[data-i="3"]`);
    await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
    await p.waitIn(`!document.querySelector('.tip').hidden`);
    const tip: string = await p.inApp(`document.querySelector('.tip').textContent`);
    check(tip.includes("Thu") && tip.includes("61") && tip.includes("110"), `hover tooltip: ${tip}`);
    await a.clickAt(x, y);
    await a.key("Right");
    check((await p.inApp(`document.querySelector('[aria-live]').textContent`)).startsWith("Fri: master 70"), "arrow keys move the tooltip");
    await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 5, y: 5 });
    await p.shots2("chart-bar");
    await close(p.pid);
    const l = await open("chart", "Latency", lineSample);
    await l.waitIn(`document.querySelectorAll('.marks path').length === 3`);
    await l.shots2("chart-line");
    await close(l.pid);
    ok("chart: bars and a gapped line render as SVG with title, desc, data table, hover and keyboard values");
  }

  // ---- table: agent sort, header click, filter, numbers in monospace
  {
    const p = await open("table", "Tests", tableSample);
    await p.waitIn(`document.querySelectorAll('tbody tr').length === 6`);
    const first = () => p.inApp(`document.querySelector('tbody tr td').textContent`);
    check((await first()) === "export_large_csv", "agent's sort: seconds descending");
    check((await p.inApp(`getComputedStyle(document.querySelector('td.num')).fontFamily`)).includes("mono"), "numbers in monospace");
    await p.click(`thead th:nth-child(1) button`);
    check((await first()) === "admin_can_unlock_ip", "header click sorts by test name");
    check((await p.inApp(`document.querySelector('thead th').getAttribute('aria-sort')`)) === "ascending", "aria-sort");
    await p.click("#filter");
    await a.type("login");
    await p.waitIn(`document.querySelectorAll('tbody tr').length === 2`);
    check((await p.inApp(`document.querySelector('.count').textContent`)) === "2 of 6 rows", "filter count");
    await a.key("Escape");
    await p.waitIn(`document.querySelectorAll('tbody tr').length === 6`);
    await p.shots2("table");
    await close(p.pid);
    ok("table: agent sort, header click sort, filter with count, Escape clears, numbers in monospace");
  }

  // ---- pr_decision: ask, failing-checks gate, request changes, merge → confirm → exact event
  {
    const p = await open("pr_decision", "PR #812", prSample);
    await p.waitIn(`document.querySelectorAll('.change').length === 2`);
    check((await p.inApp(`document.querySelector('h1').textContent`)).includes("Rate-limit failed logins"), "title");
    check((await p.inApp(`document.querySelectorAll('.diff tr.add').length`)) > 5, "diffs rendered with additions");
    await p.click("#ask");
    await a.type("Why check the limit before the user lookup?");
    await a.key("Ctrl+Enter");
    let got = await events(p.pid);
    check(got.length === 1 && got[0].kind === "ask" && got[0].payload._meta.muster.data.question === "Why check the limit before the user lookup?", `ask event: ${JSON.stringify(got)}`);
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, answer: { question: "Why check the limit before the user lookup?", text: "So a blocked IP cannot probe which emails exist." } } });
    await p.waitIn(`document.querySelector('.answer')?.textContent.includes('cannot probe')`);
    ok("pr_decision: Ask sends the question; the agent's answer shows inline");

    await call("panel.update", { panel_id: p.pid, data: { ...prSample, checks: { state: "failing", items: [{ name: "test (ubuntu)", state: "fail" }] } } });
    await p.waitIn(`document.querySelector('#merge').disabled`);
    await p.click("h1");
    await a.key("m");
    check((await p.inApp(`document.querySelector('.notice')?.textContent ?? ''`)).includes("Shift+M"), "m with failing checks explains the Shift override");
    check((await p.inApp(`document.querySelector('#merge-hint').textContent`)).includes("Shift+M"), "the override hint shows beside the disabled Merge");
    await a.key("Shift+M");
    await p.waitIn(`document.querySelector('.drawer .warn')?.textContent.includes('failing')`);
    await a.key("Escape");
    await p.waitIn(`!document.querySelector('.drawer')`);
    check((await events(p.pid)).length === 0, "the gate sent nothing");
    ok("pr_decision: failing checks disable Merge; Shift+M opens the confirm with a warning; Escape cancels");

    await call("panel.update", { panel_id: p.pid, data: prSample });
    await p.waitIn(`!document.querySelector('#merge').disabled`);
    await p.click("h1");
    await a.key("r");
    await a.type("Please add a test for the proxy header case.");
    await p.click("#request-send");
    got = await events(p.pid);
    check(got.length === 1 && got[0].kind === "request_changes" && got[0].payload._meta.muster.data.body === "Please add a test for the proxy header case.", `request_changes: ${JSON.stringify(got)}`);
    await call("panel.update", { panel_id: p.pid, data: prSample });
    await p.click("h1");
    await a.key("o");
    check((await a.eval(`globalThis.__openedLinks ?? []`)).includes(prSample.url), "O opens the PR in the system browser");
    ok("pr_decision: R sends a review comment; O opens GitHub through the host");

    await p.click("#merge");
    await p.waitIn(`document.querySelector('.drawer pre.command')`);
    const command: string = await p.inApp(`document.querySelector('.drawer pre.command').textContent`);
    check(command === `gh pr merge 812 --repo acme/api --squash --delete-branch --match-head-commit ${HEAD_SHA}`, `exact command shown: ${command}`);
    check((await events(p.pid)).length === 0, "Merge alone sends nothing");
    await p.shots2("pr-confirm");
    await p.waitIn(`document.querySelector('.drawer pre.command')`);
    // The proxy allows 3 messages per ~5.5 s of user activity: use them up with links, and the
    // refused Merge keeps the confirm open to try again.
    for (let i = 0; i < 3; i++) await a.key("o");
    await p.click("#confirm-merge");
    await p.waitIn(`document.querySelector('.notice.danger')?.textContent.includes('Try again') && document.activeElement?.id === 'confirm-merge'`);
    check((await events(p.pid)).length === 0, "a refused Merge sent nothing");
    await a.sleep(5600);
    await p.click("#confirm-merge");
    got = await events(p.pid);
    check(got.length === 1 && got[0].kind === "merge", `one merge event: ${JSON.stringify(got)} ${await p.inApp(`document.querySelector('.notice')?.textContent + '|' + !!document.querySelector('.drawer')`)}`);
    const data = got[0].payload._meta.muster.data;
    check(JSON.stringify(data) === JSON.stringify({ action: "merge", repo: "acme/api", number: 812, base: "master", method: "squash", delete_branch: true, head_sha: HEAD_SHA }), `merge payload: ${JSON.stringify(data)}`);
    check(String(got[0].payload.content.text).endsWith(command), "event text carries the command");
    check(await p.inApp(`document.querySelector('#merge').disabled`), "Merge disabled while the agent works");
    ok("pr_decision: Merge shows the exact gh command; a refused send keeps the confirm for a retry; Confirm sends one merge event with that PR, method, branch flag and head SHA");

    // Keyboard: method change, M, Enter.
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, status: "failed", status_detail: "Head branch was modified. Review and try the merge again." } });
    await p.waitIn(`document.querySelector('.status.danger')`);
    await p.inApp(`(() => { const s = document.querySelector('#method'); s.value = 'rebase'; s.dispatchEvent(new Event('change')); })()`);
    await a.sleep(5600);
    // A held Enter on the focused Merge button opens the confirm, and its auto-repeat must not
    // confirm; only a fresh press after release does.
    await p.inApp(`document.querySelector('#merge').focus()`);
    const enter = (type: string, autoRepeat = false) => a.send("Input.dispatchKeyEvent", {
      type, key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, autoRepeat, ...(type === "keyDown" ? { text: "\r", unmodifiedText: "\r" } : {}),
    });
    await enter("keyDown");
    await p.waitIn(`document.activeElement?.id === 'confirm-merge'`);
    for (let i = 0; i < 4; i++) { await enter("keyDown", true); await a.sleep(40); }
    check((await events(p.pid)).length === 0 && await p.inApp(`!!document.querySelector('.drawer pre.command')`), "a held Enter never confirms");
    await enter("keyUp");
    // A base retarget while confirming cancels the confirm.
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, base: "production", status: "failed", status_detail: "Head branch was modified. Review and try the merge again." } });
    await p.waitIn(`!document.querySelector('.drawer') && document.querySelector('.notice')?.textContent.includes('changed while you were confirming')`);
    check((await events(p.pid)).length === 0, "retargeting sent nothing");
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, status: "failed", status_detail: "Head branch was modified. Review and try the merge again." } });
    await p.waitIn(`document.querySelector('.status.danger')`);
    await p.click("h1");
    await a.key("m");
    await p.waitIn(`document.activeElement?.id === 'confirm-merge'`);
    await a.key("Enter");
    got = await events(p.pid);
    check(got.length === 1 && got[0].payload._meta.muster.data.method === "rebase", `keyboard merge: ${JSON.stringify(got)}`);
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, status: "merging" } });
    await p.waitIn(`document.querySelector('.status.info')?.textContent.includes('Merging')`);
    await call("panel.update", { panel_id: p.pid, data: { ...prSample, status: "merged", status_detail: "Merged as 9e1f2c4 into master; branch deleted." } });
    await p.waitIn(`document.querySelector('.status.success')?.textContent.includes('Merged as 9e1f2c4')`);
    check(await p.inApp(`document.querySelector('#merge').disabled && document.querySelector('#ask').disabled`), "merged: Merge and Ask are off");
    await p.shots2("pr-merged");
    ok("pr_decision: a held Enter cannot confirm; a base change cancels the confirm; M then Enter merges by keyboard with the chosen method; merging and merged statuses show prominently");

    await call("panel.update", { panel_id: p.pid, data: prSample });
    await p.waitIn(`!document.querySelector('.status')`);
    await p.shots2("pr-decision");
    // About 400 px of app, whatever else this window shows beside the zoomed group.
    await p.zoom("fullscreen");
    await a.sleep(300);
    const share = (await p.inApp("innerWidth")) / 1440;
    await a.size(Math.ceil(400 / share), 900);
    await a.sleep(400);
    const narrow: number = await p.inApp("innerWidth");
    check(narrow < 440, `narrow width reached: ${narrow}`);
    check(await p.inApp(`document.documentElement.scrollWidth <= innerWidth + 1`), `narrow: no sideways scroll at ${narrow} px`);
    await a.shot(`${shots}/panel-pr-decision-narrow-dark.png`, (await a.rect(`[data-panel-view="${p.pid}"]`))!);
    await p.zoom("inline");
    await a.size(1440, 900);
    await close(p.pid);
    ok("pr_decision: readable about 400 px wide");
  }
  await R("channel.archive", { id: ch.id }).catch(() => {});
  void hello;
}

if (import.meta.main) {
  const [url, shots, repo] = Deno.args;
  if (!url || !shots || !repo) {
    console.error("usage: panel_apps_e2e.ts <url#token=..> <shots-dir> <repo-path>");
    Deno.exit(2);
  }
  const a = await launch(9351, "/tmp/muster-cdp-panel-apps");
  let step = 0;
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("panel_apps_e2e runs against the mock daemon only");
    await panelApps({
      a, repo, shots,
      ok: (m) => console.log(`✓ ${++step}. ${m}`),
      check: (c, m) => { if (!c) throw new Error("check failed: " + m); },
    });
    const errors = a.console.filter((l) => /^(error|exception)/.test(l));
    if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
    console.log("panel apps e2e passed");
  } catch (e) {
    console.error("✗", e instanceof Error ? e.message : e);
    await a.shot(`${shots}/zz-panel-apps-failure.png`).catch(() => {});
    console.log(a.console.slice(-20).join("\n"));
    Deno.exitCode = 1;
  } finally {
    await a.close();
  }
}

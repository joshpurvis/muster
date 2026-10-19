// Built-in panel apps against the MOCK daemon: image, chart, table and pr_decision open with
// sample data, react to real clicks and keys, and pr_decision's Merge → confirm sends exactly
// the event the panel showed; then the open-PRs flow: pr_overview → open → pr_decision with
// pictures and a chart → hold → the user's own outward action, confirmed → back to the overview. Screenshots of each app, dark and light, go to <shots>.
// Part of dev/e2e.ts; also runs alone (same flags as e2e.ts):
//
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots>,src,dev,panels \
//     --allow-write=<shots>,/tmp dev/panel_apps_e2e.ts http://127.0.0.1:17761/#token=dev <shots> <repo>
import { type Browser, launch } from "./cdp.ts";
import { chartSample, HEAD_SHA, lineSample, overviewSample, prMediaSample, prSample, tableSample } from "./panel_samples.ts";

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
  /** "?" (Shift+/), which the key helper cannot type. */
  const question = async () => {
    for (const type of ["keyDown", "keyUp"]) {
      await a.send("Input.dispatchKeyEvent", { type, key: "?", code: "Slash", windowsVirtualKeyCode: 191, modifiers: 8,
        ...(type === "keyDown" ? { text: "?", unmodifiedText: "/" } : {}) });
    }
    await a.sleep(80);
  };

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

  // ---- open PRs: overview → open → decision with media → hold → custom outward action → back
  {
    const ov = await open("pr_overview", "Open PRs", overviewSample);
    await ov.waitIn(`document.querySelectorAll('.card').length === 4`);
    const order = () => ov.inApp(`[...document.querySelectorAll('.card')].map((c) => Number(c.id.slice(3)))`);
    const selectedNow = () => ov.inApp(`Number(document.querySelector('.card.selected')?.id.slice(3))`);
    check(JSON.stringify(await order()) === "[801,812,820,815]", `riskiest first: ${await order()}`);
    await ov.inApp(`(() => { const s = document.querySelector('#sort'); s.value = 'quick'; s.dispatchEvent(new Event('change')); })()`);
    check(JSON.stringify(await order()) === "[815,812,820,801]", `quick wins first: ${await order()}`);
    await ov.inApp(`(() => { const s = document.querySelector('#sort'); s.value = 'oldest'; s.dispatchEvent(new Event('change')); })()`);
    check(JSON.stringify(await order()) === "[801,812,815,820]", `oldest first: ${await order()}`);
    await ov.inApp(`(() => { const s = document.querySelector('#sort'); s.value = 'risk'; s.dispatchEvent(new Event('change')); })()`);
    await ov.click("h1");
    await question();
    await ov.waitIn(`document.querySelector('.shortcuts')?.textContent.includes('Hold: put aside for later') && document.activeElement?.id === 'shortcuts-close'`);
    await a.key("Escape");
    await ov.waitIn(`!document.querySelector('.shortcuts')`);
    await a.key("/");
    await a.type("failing");
    await ov.waitIn(`document.querySelectorAll('.card').length === 1 && document.querySelector('#pr-801')`);
    await a.key("Escape");
    await ov.waitIn(`document.querySelectorAll('.card').length === 4`);
    await ov.click("h1");
    await a.key("/");
    check(await ov.inApp(`document.activeElement?.id === 'filter'`), "/ focuses the filter");
    await a.type("login");
    await ov.waitIn(`document.querySelectorAll('.card').length === 1`);
    await a.key("Escape");
    await ov.waitIn(`document.querySelectorAll('.card').length === 4`);
    await ov.click("h1");
    check((await selectedNow()) === 812, "the filter's match stays selected after clearing it");
    await a.key("k");
    check((await selectedNow()) === 801 && await ov.inApp(`document.activeElement?.id === 'pr-801'`), "K moves up and focuses the card");
    await a.key("j");
    await a.key("Down");
    await a.key("Up");
    check((await selectedNow()) === 812, "J and the arrows move too");
    check((await ov.inApp(`document.querySelector('#pr-801 .chips').textContent`)).includes("Checks failing"), "checks badge");
    check((await ov.inApp(`document.querySelector('#pr-801 .meta').textContent`)).includes("Draft"), "draft flag");
    check((await events(ov.pid)).length === 0, "moving sends nothing");
    await ov.shots2("pr-overview");
    await ov.click("#pr-812 .title");
    await a.key("Enter");
    let got = await events(ov.pid);
    check(got.length === 1 && got[0].kind === "open" && JSON.stringify(got[0].payload._meta.muster.data) === JSON.stringify({ action: "open", repo: "acme/api", number: 812 }), `open event: ${JSON.stringify(got)}`);
    const status = (n: number, st: string, detail?: string) => ({ ...overviewSample, current: 812,
      prs: overviewSample.prs.map((p) => p.number === n ? { ...p, status: st, status_detail: detail } : p) });
    await call("panel.update", { panel_id: ov.pid, data: status(812, "reviewing") });
    await ov.waitIn(`document.querySelector('#pr-812 .badge.status')?.textContent === 'Reviewing' && document.querySelector('#pr-812 .badge.now')`);
    // The agent moving on never retargets the card the user is on.
    await a.key("k");
    check((await selectedNow()) === 801, "K moves to #801");
    await call("panel.update", { panel_id: ov.pid, data: { ...status(812, "reviewing"), current: 815 } });
    await ov.waitIn(`document.querySelector('#pr-815 .badge.now')`);
    check((await selectedNow()) === 801 && await ov.inApp(`document.activeElement?.id === 'pr-801'`), "the user's card stays selected when the agent's current changes");
    await call("panel.update", { panel_id: ov.pid, data: status(812, "reviewing") });
    await ov.waitIn(`document.querySelector('#pr-812 .badge.now')`);
    await a.key("j");
    check((await selectedNow()) === 812, "J back to #812");
    ok("pr_overview: cards sort riskiest / quick wins / oldest, filter with /, J/K move; Enter sends open; the agent's status shows on the card");

    const d = await open("pr_decision", "PR #812", prMediaSample);
    await d.waitIn(`document.querySelectorAll('.media .thumb img').length === 2 && [...document.querySelectorAll('.media .thumb img')].every((i) => i.naturalWidth > 0)`);
    check(await d.inApp(`document.querySelector('.media').compareDocumentPosition(document.querySelector('.change')) & Node.DOCUMENT_POSITION_FOLLOWING`), "media before the diffs");
    check(await d.inApp(`document.querySelector('.grid').compareDocumentPosition(document.querySelector('.media')) & Node.DOCUMENT_POSITION_FOLLOWING`), "risk, checks and reviews before the media");
    // The enlarged picture is modal: an open Merge confirm closes, the rest goes inert, Tab stays inside.
    await d.click("#merge");
    await d.waitIn(`document.querySelector('.drawer pre.command')`);
    // (The confirm drawer covers most of a small panel: reach the picture by keyboard.)
    await d.inApp(`document.querySelector('#media-0').focus()`);
    await a.key("Enter");
    await d.waitIn(`document.querySelector('.lightbox') && !document.querySelector('.drawer') && document.activeElement?.id === 'lightbox-close'`);
    check(await d.inApp(`document.querySelector('.actions').inert && document.querySelector('.head').inert && !document.querySelector('.lightbox').inert`), "everything behind the picture is inert");
    for (let i = 0; i < 5; i++) await a.key("Shift+Tab");
    check(await d.inApp(`!!document.activeElement?.closest('.lightbox')`), "Shift+Tab stays inside the picture view");
    await d.inApp(`document.querySelector('.lb-stage').focus()`);
    await a.key("m");
    await a.key("Enter");
    check(await d.inApp(`!document.querySelector('.drawer') && !!document.querySelector('.lightbox')`) && (await events(d.pid)).length === 0, "no merge behind the picture");
    await d.inApp(`document.querySelector('#lightbox-close').focus()`);
    await a.key("z");
    await d.waitIn(`document.querySelector('.lb-stage.actual') && document.querySelector('#lightbox-zoom').getAttribute('aria-pressed') === 'true'`);
    await a.key("z");
    await d.waitIn(`document.querySelector('.lb-stage.fit')`);
    // The agent replaces the pictures while one is enlarged: the view closes instead of showing another.
    await call("panel.update", { panel_id: d.pid, data: { ...prMediaSample, media: [prMediaSample.media[1], prMediaSample.media[2]] } });
    await d.waitIn(`!document.querySelector('.lightbox') && !document.querySelector('.actions').inert`);
    await call("panel.update", { panel_id: d.pid, data: prMediaSample });
    await d.waitIn(`document.querySelectorAll('.media .thumb img').length === 2`);
    await events(d.pid);
    ok("pr_decision: an enlarged picture is modal (confirm closed, background inert, Tab trapped, keys blocked), zooms to actual size with Z, and closes when the agent replaces it");
    await d.waitIn(`document.querySelector('.media-chart svg[role=img]')`);
    check((await d.inApp(`document.querySelectorAll('.media-chart rect.bar').length`)) === 6, "media chart drawn with the chart app's code");
    check((await d.inApp(`document.querySelector('.media-chart svg title').textContent`)) === "Bar chart", "chart title");
    // Clear of the sticky action bar at the bottom.
    await d.inApp(`document.querySelector('#media-0').scrollIntoView({ block: 'center' })`);
    await d.click("#media-0");
    await d.waitIn(`document.querySelector('.lightbox img')?.naturalWidth === 512 && document.activeElement?.id === 'lightbox-close'`);
    await a.key("Right");
    await d.waitIn(`document.querySelector('.lightbox img')?.alt === 'Retry-After countdown'`);
    await d.shots2("pr-decision-enlarged");
    await a.key("Escape");
    await d.waitIn(`!document.querySelector('.lightbox') && document.activeElement?.id === 'media-1'`);
    check((await events(d.pid)).length === 0, "looking at pictures sends nothing");
    check(await d.inApp(`!!document.querySelector('#skip') && !!document.querySelector('#hold') && /Your actions/.test(document.querySelector('.custom-group')?.textContent ?? '')`), "Skip, Hold, and the user's actions in their own group");
    await d.shots2("pr-decision-media");
    ok("pr_decision: pictures and a chart before the diffs; a click enlarges a picture in the panel, arrows step, Esc closes");

    await d.click("h1");
    await a.key("h");
    got = await events(d.pid);
    check(got.length === 1 && got[0].kind === "hold" && JSON.stringify(got[0].payload._meta.muster.data) === JSON.stringify({ action: "hold", repo: "acme/api", number: 812, head_sha: HEAD_SHA }), `hold event: ${JSON.stringify(got)}`);
    check(await d.inApp(`document.querySelector('#merge').disabled && document.querySelector('#skip').disabled`), "the agent has the decision: actions wait");
    await close(d.pid);
    await call("panel.update", { panel_id: ov.pid, data: { ...status(812, "held", "Put aside from the decision panel."), current: undefined } });
    await ov.waitIn(`document.querySelector('.group.g-held') && [...document.querySelectorAll('.card')].pop()?.id === 'pr-812'`);
    check((await ov.inApp(`document.querySelector('.repo').textContent`)).includes("1 held"), "header counts the held PR");
    check((await ov.inApp(`getComputedStyle(document.querySelector('#pr-812')).opacity`)) === "1", "a held card keeps full-contrast text");
    ok("pr_decision: H sends one hold event; the overview groups the held PR at the end");

    // The user's own actions: outward ones ask first, showing what the agent will do.
    await ov.click("h1");
    check((await selectedNow()) === 812, "still on the held PR");
    await a.key("Up");
    check((await selectedNow()) === 815, `the keys reach a card at the bottom (above the sticky bar): ${await selectedNow()}`);
    check(await ov.inApp(`document.querySelector('#pr-815').getBoundingClientRect().bottom <= document.querySelector('.actions').getBoundingClientRect().top`), "the selected card is not under the action bar");
    await a.key("q");
    await ov.waitIn(`document.querySelector('.drawer .outward-desc')?.textContent === 'Add the needs-qa label to this PR on GitHub.' && document.activeElement?.id === 'confirm-outward'`);
    check((await events(ov.pid)).length === 0, "an outward action sends nothing before the confirm");
    await ov.shots2("pr-overview-confirm");
    await ov.inApp(`document.querySelector('#confirm-outward').focus()`);
    await a.key("Enter");
    got = await events(ov.pid);
    check(got.length === 1 && got[0].kind === "custom" && JSON.stringify(got[0].payload._meta.muster.data) === JSON.stringify({ action: "custom", id: "needs-qa", repo: "acme/api", number: 815 }), `custom event: ${JSON.stringify(got)}`);
    check(String(got[0].payload.content.text).includes("Add the needs-qa label"), "the event text says what to do");
    if (await ov.inApp(`!!document.querySelector('#your-actions')`)) await ov.click("#your-actions");
    await ov.click("#act-later");
    got = await events(ov.pid);
    check(got.length === 1 && got[0].payload._meta.muster.data.id === "later", `a local action sends at once: ${JSON.stringify(got)}`);
    await call("panel.update", { panel_id: ov.pid, data: { ...status(812, "held"), prs: status(812, "held").prs.map((p) => p.number === 815 ? { ...p, status_detail: "Labelled needs-qa." } : p), current: undefined } });
    await ov.waitIn(`document.querySelector('#pr-815 .detail')?.textContent === 'Labelled needs-qa.'`);
    ok("custom actions: an outward one shows its description and needs Enter on Confirm; a local one sends at once; the agent's note shows on the card");

    // The proxy lets 3 messages through per ~5.5 s of activity: start a fresh budget.
    await a.sleep(5600);
    await ov.click("h1");
    await a.key("n");
    got = await events(ov.pid);
    check(got.length === 1 && got[0].kind === "next" && JSON.stringify(got[0].payload._meta.muster.data) === JSON.stringify({ action: "next", repo: "acme/api", number: 801, order: [820, 815] }), `next event: ${JSON.stringify(got)} ${await ov.inApp(`document.querySelector('.notice')?.textContent + '|' + document.activeElement?.id`)}`);
    await call("panel.update", { panel_id: ov.pid, data: { ...status(812, "held"), current: undefined } });
    await ov.waitIn(`!document.querySelector('.notice')`);
    // About 400 px of app.
    await ov.zoom("fullscreen");
    await a.sleep(300);
    const share = (await ov.inApp("innerWidth")) / 1440;
    await a.size(Math.ceil(400 / share), 900);
    await a.sleep(400);
    const narrow: number = await ov.inApp("innerWidth");
    check(narrow < 440, `narrow width reached: ${narrow}`);
    check(await ov.inApp(`document.documentElement.scrollWidth <= innerWidth + 1`), `overview narrow: no sideways scroll at ${narrow} px`);
    check(await ov.inApp(`document.querySelector('.actions').offsetHeight <= innerHeight * 0.35`), `overview narrow: the action bar takes at most about a third: ${await ov.inApp(`document.querySelector('.actions').offsetHeight + '/' + innerHeight`)}`);
    for (const theme of ["dark", "light"]) {
      await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
      await ov.waitIn(`document.documentElement.dataset.theme === '${theme}'`);
      await a.sleep(250);
      await a.shot(`${shots}/panel-pr-overview-narrow-${theme}.png`, (await a.rect(`[data-panel-view="${ov.pid}"]`))!);
    }
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await ov.zoom("inline");
    await a.size(1440, 900);
    await a.sleep(300);
    const d2 = await open("pr_decision", "PR #801", { ...prMediaSample, number: 801 });
    await d2.waitIn(`document.querySelector('.media-chart svg[role=img]')`);
    await d2.zoom("fullscreen");
    await a.sleep(300);
    await a.size(Math.ceil(400 / share), 900);
    await a.sleep(400);
    check(await d2.inApp(`document.documentElement.scrollWidth <= innerWidth + 1`), `decision with media narrow: no sideways scroll`);
    check(await d2.inApp(`document.querySelector('.actions').offsetHeight <= innerHeight * 0.35`), `decision narrow: the action bar takes at most about a third: ${await d2.inApp(`document.querySelector('.actions').offsetHeight + '/' + innerHeight`)}`);
    await d2.click("h1");
    await question();
    await d2.waitIn(`document.querySelector('.shortcuts')?.textContent.includes('Skip: move on')`);
    await a.shot(`${shots}/panel-pr-decision-shortcuts-narrow-dark.png`, (await a.rect(`[data-panel-view="${d2.pid}"]`))!);
    await a.key("Escape");
    await d2.waitIn(`!document.querySelector('.shortcuts')`);
    await d2.inApp(`document.querySelector('#media-0').scrollIntoView({ block: 'center' })`);
    await d2.click("#media-0");
    await d2.waitIn(`document.querySelector('.lightbox')`);
    await a.key("z");
    await d2.waitIn(`document.querySelector('.lb-stage.actual')`);
    await a.shot(`${shots}/panel-pr-decision-actual-size-narrow-dark.png`, (await a.rect(`[data-panel-view="${d2.pid}"]`))!);
    await a.key("Escape");
    await a.shot(`${shots}/panel-pr-decision-media-narrow-dark.png`, (await a.rect(`[data-panel-view="${d2.pid}"]`))!);
    await d2.zoom("inline");
    await a.size(1440, 900);
    await close(d2.pid);
    await close(ov.pid);
    ok("pr_overview: N sends next with the rest in the shown order; overview and decision with media are readable about 400 px wide");
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

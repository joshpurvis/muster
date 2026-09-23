// Visual states that need agents (approval banner, inbox, timeline) using dev/mock.ts.
//   deno task mock &  then:
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-write=<shots>,/tmp/muster-cdp-mock dev/shots-mock.ts <shots> <repo>
import { launch } from "./cdp.ts";

const [shots, repo] = Deno.args;
const b = await launch(9335, "/tmp/muster-cdp-mock");
const R = (m: string, p: unknown) =>
  b.eval(`globalThis.__muster.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
try {
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.size(1440, 900);
  await b.goto("http://127.0.0.1:17744/#token=dev");
  await b.wait(`!!document.querySelector('.topbar')`);
  const w: any = await R("workspace.add", { path: repo });
  const c1: any = await R("channel.create", {
    workspace_id: w.id,
    name: "fix-auth-redirect",
    agent: "claude",
    prompt: "Fix the OAuth redirect loop",
  });
  const c2: any = await R("channel.create", { workspace_id: w.id, name: "new-api", agent: "codex" });
  await R("channel.create", { workspace_id: w.id, name: "docs-refresh", agent: "shell" });
  await b.sleep(1500);
  await b.eval(
    `[...document.querySelectorAll('.ch-row')].find(r => r.textContent.includes('fix-auth')).click()`,
  );
  await b.sleep(800);
  const sid = await b.eval(
    `[...globalThis.__muster.sessions.value.values()].find(s => s.channel_id === ${c1.id}).id`,
  );
  await R("session.send", { id: sid, text: "please ask before running tests" });
  await R("channel.post", { id: c1.id, text: "Repro: log in with Google, get bounced back to /login." });
  const s2 = await b.eval(
    `[...globalThis.__muster.sessions.value.values()].find(s => s.channel_id === ${c2.id}).id`,
  );
  await R("session.send", { id: s2, text: "ask about the schema" });
  await b.sleep(5000);
  await b.key("Alt+d");
  await b.sleep(1200);
  await b.key("Alt+l");
  await b.sleep(900);
  await b.shot(`${shots}/m1-approval.png`);
  await b.key("Alt+i");
  await b.sleep(900);
  await b.shot(`${shots}/m2-inbox.png`);
  await b.key("a");
  await b.sleep(900);
  await b.shot(`${shots}/m3-inbox-after-approve.png`);
  await b.key("Alt+i");
  await b.size(1100, 760);
  await b.sleep(600);
  await b.shot(`${shots}/m4-1100.png`);
  await b.send("Emulation.setEmulatedMedia", {
    features: [{ name: "prefers-color-scheme", value: "light" }],
  });
  await b.sleep(600);
  await b.shot(`${shots}/m5-light.png`);
  console.log(b.console.filter((l) => /error|exception/i.test(l)).join("\n") || "no console errors");
} finally {
  await b.close();
}

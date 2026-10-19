// Called only by scripts/account-profiles-smoke.py, against its fake-Claude fixture.
import { launch } from "./cdp.ts";

const [url, base, port] = Deno.args;
if (!base?.startsWith("/tmp/muster-profiles-smoke-") || new URL(url).hostname !== "127.0.0.1") {
  throw new Error("This walkthrough requires the isolated profile smoke fixture");
}
const b = await launch(Number(port), `${base}/browser`);
const M = "globalThis.__muster";
const check = (condition: unknown, message: string) => {
  if (!condition) throw new Error(message);
};
const clickText = (selector: string, text: string) =>
  b.eval(`(() => {
  const item = [...document.querySelectorAll(${
    JSON.stringify(selector)
  })].find(e => e.textContent.trim() === ${JSON.stringify(text)});
  if (!item) throw new Error('Missing button: ' + ${JSON.stringify(text)});
  item.click();
})()`);
const fill = (selector: string, value: string) =>
  b.eval(`(() => {
  const input = document.querySelector(${JSON.stringify(selector)});
  if (!input) throw new Error('Missing input: ' + ${JSON.stringify(selector)});
  input.value = ${JSON.stringify(value)};
  input.dispatchEvent(new Event('input', {bubbles: true}));
})()`);

try {
  await b.size(1440, 1000);
  await b.goto(url);
  await b.wait(`${M}?.windowId.value != null`);
  const hello = await b.eval(`${M}.rpc.call('hello', {})`);
  check(
    hello.workspaces.some((w: { path: string }) => w.path === `${base}/repo`),
    "Wrong backend: refusing UI mutations",
  );
  // Never open native windows or touch backend lifecycle from a browser test.
  await b.eval(`(() => {
    const rpc = ${M}.rpc, call = rpc.call.bind(rpc);
    rpc.call = (method, params) => method.startsWith('daemon.') || method === 'window.show'
      ? Promise.reject(new Error('Lifecycle actions blocked by fixture')) : call(method, params);
  })()`);
  await b.click('[aria-label="Settings and more"]');
  await clickText('[role="menuitem"]', "Claude profiles…");
  await b.wait(`!!document.querySelector('[role="dialog"][aria-label="Claude profiles"]')`);
  await b.sleep(200);
  await b.shot(`${base}/profiles-desktop.png`);
  await fill('input[placeholder="Personal or Work"]', "UI profile");
  await fill('input[placeholder="~/.claude-work"]', `${base}/ui`);
  await clickText('button[type="submit"]', "Register profile");
  await b.wait(`document.querySelector('input[placeholder="Personal or Work"]').value === ''`);
  const profile = await b.eval(
    `${M}.rpc.call('account_profile.list', {}).then(profiles => profiles.find(p => p.name === 'UI profile'))`,
  );
  check(profile?.config_dir === `${base}/ui`, "Registration form did not save the directory reference");
  await b.size(720, 900);
  await b.sleep(200);
  await b.shot(`${base}/profiles-narrow.png`);
  check(
    await b.eval(`(() => {
    const d = document.querySelector('[role="dialog"]'), r = d.getBoundingClientRect();
    return r.left >= 0 && r.right <= innerWidth && r.bottom <= innerHeight && d.scrollWidth <= d.clientWidth;
  })()`),
    "Profile dialog overflows the viewport",
  );
  await clickText('[role="dialog"] button', "Done");
  await b.size(1440, 1000);
  await b.key("Alt+Shift+N");
  await b.wait(`!!document.querySelector('[role="dialog"][aria-label="New tab"]')`);
  await fill('[aria-label="New tab"] input', `${base}/repo`);
  await clickText('[aria-label="New tab"] button[role="radio"]', "Claude");
  await b.eval(`(() => {
    const label = [...document.querySelectorAll('[aria-label="New tab"] label')].find(e => e.querySelector('.field-label')?.textContent === 'Claude profile');
    const select = label.querySelector('select');
    select.value = ${JSON.stringify(String(profile.id))};
    select.dispatchEvent(new Event('change', {bubbles: true}));
  })()`);
  await b.sleep(200);
  await b.shot(`${base}/profile-selection.png`);
  await clickText('[aria-label="New tab"] button[type="submit"]', "Open tab");
  await b.wait(`${M}.focusedSession.value?.account_profile_id === ${profile.id}`, 15000);
  check(
    await b.eval(`${M}.focusedSession.value.kind === 'claude'`),
    "Selected profile was not applied to a Claude session",
  );
  check(!b.console.some((line) => line.startsWith("exception:")), b.console.join("\n"));
  console.log("Profile UI checks passed; screenshots:", base);
} finally {
  await b.close();
}

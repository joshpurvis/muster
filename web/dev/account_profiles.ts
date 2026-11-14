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
  check(
    await b.eval(
      `!document.querySelector('[aria-label="Claude profiles"]').textContent.includes('Workspace defaults')`,
    ),
    "Profile management must not expose saved workspace history",
  );
  await b.sleep(200);
  await b.shot(`${base}/profiles-desktop.png`);
  // Usage: the smoke left Work current, Home unconfirmed and Default's 5-hour window reset.
  await b.wait(`!!document.querySelector('[aria-label="Claude profiles"] .usage-table')`);
  const table = await b.eval(`document.querySelector('[aria-label="Claude profiles"] .usage-table').innerText`);
  check(
    table.includes("not confirmed fresh") && table.includes("Unknown") && /Work\s+25%/.test(table),
    `Usage table does not show the fixture's states honestly: ${table}`,
  );
  await b.shot(`${base}/usage-profiles.png`);
  // The dialog's tabs: it opens on Usage with that tab focused; arrow keys move between tabs and
  // show only the chosen panel.
  const D = `document.querySelector('[aria-label="Claude profiles"]')`;
  const tabState = () =>
    b.eval(`(() => {
    const sel = ${D}.querySelector('[role="tab"][aria-selected="true"]');
    const shown = [...${D}.querySelectorAll('[role="tabpanel"]')].filter((p) => !p.hidden).map((p) => p.id);
    return { selected: sel?.textContent, focused: document.activeElement === sel, shown, controls: sel?.getAttribute('aria-controls') };
  })()`);
  let t = await tabState();
  check(t.selected === "Usage" && t.focused, `The dialog must open on a focused Usage tab: ${JSON.stringify(t)}`);
  await b.eval(
    `document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }))`,
  );
  await b.sleep(100);
  t = await tabState();
  check(
    t.selected === "Profiles" && t.focused && t.shown.length === 1 && t.shown[0] === t.controls,
    `ArrowRight must select and focus Profiles and show only its panel: ${JSON.stringify(t)}`,
  );
  // Profile rows start collapsed and open one at a time.
  const heads = `${D}.querySelectorAll('.pf-head')`;
  const expanded = () => b.eval(`[...${heads}].filter((h) => h.getAttribute('aria-expanded') === 'true').length`);
  check((await expanded()) === 0, "Profile rows must start collapsed");
  await b.eval(`${heads}[1].click()`);
  await b.eval(`${heads}[2].click()`);
  await b.sleep(100);
  check(
    (await expanded()) === 1 && await b.eval(`${heads}[2].getAttribute('aria-expanded') === 'true'`),
    "Only one profile row may be open at a time",
  );
  await b.shot(`${base}/profiles-list.png`);
  await clickText('[aria-label="Claude profiles"] [role="tab"]', "Add profile");
  // The narrow window must fit the Add tab with its disclosure open too.
  await clickText('[aria-label="Claude profiles"] .dlg-more', "Registering ~/.claude itself?");
  await b.size(720, 900);
  await b.sleep(200);
  check(
    await b.eval(`(() => {
    const d = document.querySelector('[role="dialog"]'), r = d.getBoundingClientRect();
    return r.left >= 0 && r.right <= innerWidth && r.bottom <= innerHeight && d.scrollWidth <= d.clientWidth;
  })()`),
    "The Add profile tab overflows a narrow viewport",
  );
  await b.size(1440, 1000);
  await fill('input[placeholder="Personal or Work"]', "UI profile");
  await fill('input[placeholder="~/.claude-work"]', `${base}/ui`);
  await clickText('button[type="submit"]', "Register profile");
  await b.wait(`document.querySelector('input[placeholder="Personal or Work"]').value === ''`);
  const profile = await b.eval(
    `${M}.rpc.call('account_profile.list', {}).then(profiles => profiles.find(p => p.name === 'UI profile'))`,
  );
  check(profile?.config_dir === `${base}/ui`, "Registration form did not save the directory reference");
  check(
    await b.eval(`(() => {
    const tab = document.querySelector('[aria-label="Claude profiles"] [role="tab"][aria-selected="true"]');
    const open = document.querySelector('[aria-label="Claude profiles"] .pf-head[aria-expanded="true"]');
    return tab?.textContent === 'Profiles' && open?.textContent.includes('UI profile');
  })()`),
    "After registering, the dialog should show Profiles with the new profile open",
  );
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
  // A Claude account in the sidebar's Usage section opens the dialog on Usage, that account marked.
  await b.wait(`[...document.querySelectorAll('.us-opens')].some((r) => r.textContent.includes('Work'))`);
  await b.eval(`[...document.querySelectorAll('.us-opens')].find((r) => r.textContent.includes('Work')).click()`);
  await b.wait(`!!document.querySelector('[aria-label="Claude profiles"] .usage-table')`);
  const marked = await b.eval(
    `[...document.querySelectorAll('[aria-label="Claude profiles"] .usage-table tr.on .usage-name')].map((e) => e.textContent)`,
  );
  check(marked.includes("Work"), `The sidebar's Work row must open the dialog with Work marked: ${marked}`);
  await clickText('[role="dialog"] button', "Done");
  await b.key("Alt+Shift+N");
  await b.wait(`!!document.querySelector('[role="dialog"][aria-label="New channel"]')`);
  await fill('[aria-label="New channel"] input', `${base}/repo`);
  await clickText('[aria-label="New channel"] button[role="radio"]', "Claude");
  check(
    await b.eval(`document.querySelector('[aria-label="New channel"] select').value === 'default'`),
    "New Tab must explicitly select Default even when the folder has a saved profile",
  );
  await b.click('[aria-label="New channel"] button[type="submit"]');
  await b.wait(
    `${M}.focusedSession.value?.kind === 'claude' && !document.querySelector('[role="dialog"][aria-label="New channel"]')`,
    15000,
  );
  check(
    await b.eval(`${M}.focusedSession.value.account_profile_id == null`),
    "New Tab silently inherited a hidden workspace profile instead of its visible Default choice",
  );
  await b.key("Alt+Shift+N");
  await b.wait(`!!document.querySelector('[role="dialog"][aria-label="New channel"]')`);
  await fill('[aria-label="New channel"] input', `${base}/repo`);
  await clickText('[aria-label="New channel"] button[role="radio"]', "Claude");
  // The capacity table and a suggestion the human may take or ignore: Default stays selected.
  await b.wait(`!!document.querySelector('[aria-label="New channel"] .usage-suggestion')`);
  check(
    await b.eval(
      `document.querySelector('[aria-label="New channel"] .usage-suggestion').textContent.includes('Suggested: Work')`,
    ),
    "New Tab does not suggest the account with known room",
  );
  check(
    await b.eval(`document.querySelector('[aria-label="New channel"] select').value === 'default'`),
    "A suggestion must never change the selection by itself",
  );
  // The capacity table opens from the Usage pill (closed by default).
  check(
    await b.eval(`!document.querySelector('[aria-label="New channel"] .usage-table')`),
    "The usage table starts closed",
  );
  await clickText('[aria-label="New channel"] button.pill-toggle', "Usage");
  await b.wait(`!!document.querySelector('[aria-label="New channel"] .usage-table')`);
  check(
    await b.eval(`[...document.querySelectorAll('[aria-label="New channel"] button.pill-toggle')]
      .find((e) => e.textContent.trim() === 'Usage')?.getAttribute('aria-pressed') === 'true'`),
    "The Usage pill reads as pressed while the table shows",
  );
  check(
    await b.eval(`/Work/.test(document.querySelector('[aria-label="New channel"] .usage-table').innerText)`),
    "The usage table lists the profiles",
  );
  await b.sleep(150);
  await b.shot(`${base}/usage-new-tab.png`);
  await clickText('[aria-label="New channel"] button', "Use Work");
  const workId = await b.eval(
    `${M}.rpc.call('account_profile.list', {}).then(profiles => profiles.find(p => p.name === 'Work').id)`,
  );
  await b.wait(`document.querySelector('[aria-label="New channel"] select').value === ${JSON.stringify(String(workId))}`);
  check(
    await b.eval(`document.activeElement === document.querySelector('[aria-label="New channel"] select')`),
    "Taking the suggestion must leave focus on the profile picker",
  );
  check(
    await b.eval(`!!document.querySelector('[aria-label="New channel"] .usage-suggestion[role="status"]')`),
    "The suggestion's status region must stay mounted",
  );
  await b.sleep(150);
  await b.shot(`${base}/usage-new-tab-chosen.png`);
  await b.eval(`(() => {
    const select = document.querySelector('[aria-label="New channel"] select[aria-label="Claude profile"]');
    select.value = ${JSON.stringify(String(profile.id))};
    select.dispatchEvent(new Event('change', {bubbles: true}));
  })()`);
  await b.sleep(200);
  await b.shot(`${base}/profile-selection.png`);
  await b.size(520, 800);
  await b.sleep(100);
  await b.shot(`${base}/profile-selection-narrow.png`);
  await b.eval(`document.documentElement.dataset.theme = 'light'`);
  await b.shot(`${base}/profile-selection-light.png`);
  await b.eval(`document.documentElement.dataset.theme = 'dark'`);
  await b.click('[aria-label="New channel"] button[type="submit"]');
  await b.wait(`${M}.focusedSession.value?.account_profile_id === ${profile.id}`, 15000);
  check(
    await b.eval(`${M}.focusedSession.value.kind === 'claude'`),
    "Selected profile was not applied to a Claude session",
  );
  // A long profile name in a narrow dialog: the suggestion wraps instead of pushing "Use" out.
  const longName = "Work account for the very long client engagement name that fills eighty chars";
  await b.eval(`${M}.rpc.call('account_profile.update', {id: ${workId}, name: ${JSON.stringify(longName)}})`);
  await b.size(400, 800);
  await b.key("Alt+Shift+N");
  await b.wait(`!!document.querySelector('[role="dialog"][aria-label="New channel"]')`);
  await fill('[aria-label="New channel"] input', `${base}/repo`);
  await clickText('[aria-label="New channel"] button[role="radio"]', "Claude");
  await b.wait(`!!document.querySelector('[aria-label="New channel"] .usage-suggestion button')`);
  check(
    await b.eval(`(() => {
    const d = document.querySelector('[role="dialog"]').getBoundingClientRect();
    const r = document.querySelector('[aria-label="New channel"] .usage-suggestion button').getBoundingClientRect();
    return r.left >= d.left && r.right <= d.right && document.documentElement.scrollWidth <= innerWidth;
  })()`),
    "The Use button must stay inside a narrow dialog with a long profile name",
  );
  await b.eval(`document.querySelector('[aria-label="New channel"] .usage-suggestion').scrollIntoView()`);
  await b.sleep(150);
  await b.shot(`${base}/usage-new-tab-narrow-long-name.png`);
  // Light theme, through the app's own preference.
  await b.eval(`${M}.prefs.value = {...${M}.prefs.value, theme: 'light'}`);
  await b.wait(`document.documentElement.dataset.theme === 'light'`);
  await b.size(1440, 1000);
  await b.sleep(250);
  await b.shot(`${base}/usage-new-tab-light.png`);
  await b.key("Escape");
  await b.click('[aria-label="Settings and more"]');
  await clickText('[role="menuitem"]', "Claude profiles…");
  await b.wait(`!!document.querySelector('[aria-label="Claude profiles"] .usage-table')`);
  await b.sleep(250);
  await b.shot(`${base}/usage-profiles-light.png`);
  await b.eval(`${M}.prefs.value = {...${M}.prefs.value, theme: 'dark'}`);
  check(!b.console.some((line) => line.startsWith("exception:")), b.console.join("\n"));
  console.log("Profile UI checks passed; screenshots:", base);
} finally {
  await b.close();
}

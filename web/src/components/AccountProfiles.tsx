import type { ComponentChildren } from "preact";
import { useEffect, useId, useRef, useState } from "preact/hooks";
import { accountProfiles, accountProfilesSupported, accountUsage, codexUsage, meta, overlay, prefs, setPref, toast } from "../state.ts";
import { rpc } from "../rpc.ts";
import type { AccountProfile, AccountUsage, CodexUsage, ProfileChoice } from "../types.ts";
import {
  accountBars,
  type AccountBars,
  asOf,
  codexBars,
  lastObserved,
  parseProfileChoice,
  peakUsage,
  sameChoice,
  suggestionView,
  type UsageBar,
  type WindowCell,
  windowCell,
} from "../account_profiles.ts";
import { IconChart, IconChevron, IconChevronDown, IconUser } from "../icons.tsx";
import * as A from "../actions.ts";
import { contextMenu } from "./Menu.tsx";
import { MoreOptions, PillSelect, PillToggle } from "./DialogKit.tsx";
import { cx, tildify } from "../util.ts";

/** Keeps `accountUsage` current while mounted (statuses age on the backend's clock: a figure
 * that was current goes stale) and returns a clock that ticks the "seen … ago" texts. */
function useUsageClock(): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    let alive = true;
    const refresh = () => {
      rpc.call<AccountUsage>("account_usage.get", {}).then((u) => {
        if (alive) accountUsage.value = u;
      }).catch(() => {});
      // Older backends have no codex_usage.get: Codex is then simply not shown.
      rpc.call<CodexUsage | null>("codex_usage.get", {}).then((u) => {
        if (alive) codexUsage.value = u;
      }).catch(() => {});
    };
    refresh();
    const poll = setInterval(refresh, 60_000);
    const tick = setInterval(() => setNow(Date.now()), 30_000);
    return () => {
      alive = false;
      clearInterval(poll);
      clearInterval(tick);
    };
  }, []);
  return now;
}

function UsageCell({ cell }: { cell: WindowCell }) {
  return (
    <td class={cx("usage-cell", !cell.current && "usage-uncertain", cell.high && "usage-high")}>
      <span class="usage-main">{cell.main}</span>
      {cell.detail && <span class="usage-detail">{cell.detail}</span>}
    </td>
  );
}

/** Each account's last reported usage. Unknown, stale and unconfirmed figures say so. */
export function UsageTable({ usage, now, selected, account }: {
  usage: AccountUsage;
  now: number;
  /** Highlighted row (the new session's current choice). */
  selected?: ProfileChoice;
  /** Highlighted account (every row on it): the one clicked in the sidebar. */
  account?: string;
}) {
  return (
    <div class="usage-table-wrap">
      <table class="usage-table">
        <thead>
          <tr>
            <th scope="col">Profile</th>
            <th scope="col">5-hour</th>
            <th scope="col">Weekly</th>
            <th scope="col">Last observed</th>
          </tr>
        </thead>
        <tbody>
          {usage.rows.map((r) => (
            <tr
              key={String(r.choice)}
              class={cx(
                (selected !== undefined ? sameChoice(r.choice, selected) : r.account === account) && "on",
                !r.eligible && "usage-off",
              )}
            >
              <th scope="row" title={r.ineligible_reason ?? undefined}>
                <span class="usage-name">{r.name}</span>
                {r.same_account_as.length > 0 && (
                  <span class="usage-note">same account as {r.same_account_as.join(", ")}</span>
                )}
                {!r.eligible && (
                  <span class="usage-note">{r.enabled ? "can't start here now" : "disabled"}</span>
                )}
              </th>
              <UsageCell cell={windowCell(r.five_hour, now)} />
              <UsageCell cell={windowCell(r.seven_day, now)} />
              <td class="usage-seen">{lastObserved(r, now)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** The sidebar's bottom-left Usage section: a small table, one line per account (every Claude
 * login, then Codex), its windows as columns named once in the heading. */
export function UsageCard() {
  const now = useUsageClock();
  const open = prefs.value.usageCard;
  const claude = accountUsage.value?.rows.length ? accountBars(asOf(accountUsage.value, now), now) : [];
  const codex = codexBars(codexUsage.value, now, accountUsage.value?.stale_after_ms ?? 30 * 60_000);
  const accounts = codex ? [...claude, codex] : claude;
  if (!accounts.length) return null;
  const cols = [...new Set(accounts.flatMap((a) => a.bars.map((b) => b.label)))];
  return (
    <section class={cx("us", open && "open")} aria-label="Usage">
      <div class="ap-feed-head" {...contextMenu(usageMenu(open))}>
        <button type="button" class="ap-feed-toggle" aria-expanded={open} onClick={() => setPref("usageCard", !open)}>
          <span class="ap-chev" aria-hidden="true"><IconChevron size={11} /></span>
          <IconChart size={13} />
          Usage
        </button>
        <Peak accounts={accounts} />
        {open && <span class="us-cols" aria-hidden="true">{cols.map((c) => <span key={c}>{c}</span>)}</span>}
      </div>
      {open && (
        <div class="us-list" role="list">
          {accounts.map((a) => (
            <UsageRow key={a.key} a={a}>
              <span class="us-name">{a.name}</span>
              {cols.map((c) => {
                const b = a.bars.find((x) => x.label === c);
                return b ? <Meter key={c} bar={b.bar} /> : <span key={c} class="um" />;
              })}
            </UsageRow>
          ))}
        </div>
      )}
    </section>
  );
}

const usageMenu = (open: boolean) => () => [
  { label: open ? "Hide usage" : "Show usage", run: () => setPref("usageCard", !open) },
  ...(accountProfilesSupported.value ? [{ label: "Claude profiles…", run: () => openProfiles() }] : []),
  "sep" as const,
  { label: "Hide sidebar", keys: "Alt+G", run: A.toggleSidebar },
];

const openProfiles = (account?: string) => (overlay.value = { kind: "account_profiles", account });

/** One window of one account: a thin bar and its figure. A figure that is not current is drawn
 * hatched and grey. Its story is the hover tip; keyboard and screen readers get it from the row. */
function Meter({ bar }: { bar: UsageBar }) {
  return (
    <span class={cx("um", !bar.current && "um-stale", `um-${bar.level}`)} data-tip={bar.tip}>
      <span class="um-track"><span class="um-fill" style={{ width: `${bar.fill ?? 0}%` }} /></span>
      <span class="um-pct">{bar.label}</span>
    </span>
  );
}

/** One account as a list row. Tab reaches it (↑/↓ move between accounts), its tip then shows
 * every window's story, and a screen reader hears the same text as its label. A Claude row
 * opens the Claude profiles dialog (the full usage table); Codex has no profiles, so its row
 * only informs. */
function UsageRow({ a, children }: { a: AccountBars; children: ComponentChildren }) {
  const opens = a.tool === "claude" && accountProfilesSupported.value;
  const story = `${a.name} — ${a.bars.map((b) => b.bar.tip).join("; ")}`;
  return (
    <div role="listitem" class="us-item">
      <div
        class={cx("us-row", opens && "us-opens")}
        tabIndex={0}
        role={opens ? "button" : "group"}
        aria-haspopup={opens ? "dialog" : undefined}
        aria-label={story}
        data-tip={opens ? `${story}. Click for Claude profiles.` : story}
        data-tip-focus
        onClick={opens ? () => openProfiles(a.key) : undefined}
        onKeyDown={(e) => {
          if (opens && (e.key === "Enter" || e.key === " ")) {
            e.preventDefault();
            openProfiles(a.key);
            return;
          }
          if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
          e.preventDefault();
          const item = (e.currentTarget as HTMLElement).parentElement;
          const next = e.key === "ArrowDown" ? item?.nextElementSibling : item?.previousElementSibling;
          (next?.firstElementChild as HTMLElement | null)?.focus();
        }}
      >
        <span class="us-row-in" aria-hidden="true">{children}</span>
      </div>
    </div>
  );
}

/** What a collapsed heading still tells: the fullest current figure. CSS hides it while the
 * list shows. */
function Peak({ accounts }: { accounts: AccountBars[] }) {
  const p = peakUsage(accounts);
  if (!p) return null;
  return (
    <span class={cx("us-peak", `um-${p.level}`)} data-tip={p.tip}>
      <span class="sr-only">{p.tip}</span>
      <span aria-hidden="true">{p.name} <b>{p.label}</b></span>
    </span>
  );
}

/** The new-session flow's suggestion and, on request (the Usage pill), the capacity table.
 * Choosing stays with the human: "Use" changes the picker and moves focus to it. The status
 * region stays mounted, so screen readers hear the new text instead of losing it with a removed
 * node. Only a suggestion to switch shows unasked; "keep" and "none" show with the table. */
export function ProfileUsage({ value, onUse, table }: {
  value: ProfileChoice | undefined;
  onUse: (choice: ProfileChoice) => void;
  /** The Usage pill is on. */
  table: boolean;
}) {
  const now = useUsageClock();
  if (!accountProfilesSupported.value || !accountProfiles.value.length || !accountUsage.value) return null;
  const usage = asOf(accountUsage.value, now);
  const s = suggestionView(usage, value);
  return (
    <div class="usage-hint">
      {table && <UsageTable usage={usage} now={now} selected={value} />}
      <div class={cx("usage-suggestion", s.kind !== "switch" && "usage-none")} role="status">
        {s.kind === "switch" && (
          <>
            <span>
              <strong>Suggested: {s.name}.</strong> {s.reason}
            </span>
            <button class="btn btn-sm" type="button" onClick={() => onUse(s.choice)}>
              Use {s.name}
            </button>
          </>
        )}
        {table && s.kind === "keep" && (
          <span>
            <strong>{s.name} has the most known room.</strong> {s.reason}
          </span>
        )}
        {table && s.kind === "none" && <span>{s.reason}</span>}
      </div>
    </div>
  );
}

/** The Claude profile pill (`null` on a backend without profiles), and, while the choice is
 * still open and profiles exist, a Usage pill that shows each account's capacity. */
export function ProfilePill({ value, onChange, disabled = false, tip, selectRef, usage, onUsage }: {
  value: ProfileChoice | undefined;
  onChange: (choice: ProfileChoice | undefined) => void;
  disabled?: boolean;
  /** More hover text after the verification hint; also read to screen readers. */
  tip?: string;
  selectRef?: { current: HTMLSelectElement | null };
  usage?: boolean;
  onUsage?: (on: boolean) => void;
}) {
  const id = useId();
  if (!accountProfilesSupported.value) return null;
  const profiles = accountProfiles.value;
  const missing = typeof value === "number" && !profiles.some((p) => p.id === value);
  // The fixed-profile note (Edit agent) comes after the verification hint, never instead of it.
  const hint = ["Verify your account with /status inside Claude Code.", tip].filter(Boolean).join(" ");
  return (
    <>
      <PillSelect
        icon={<IconUser size={13} />}
        label="Claude profile"
        value={String(value ?? "default")}
        disabled={disabled}
        tip={hint}
        describedBy={`${id}-hint`}
        selectRef={selectRef}
        onChange={(v) => onChange(parseProfileChoice(v))}
        options={[
          { value: "default", label: "Default · ordinary Claude environment", short: "Default profile" },
          ...(missing ? [{ value: String(value), label: `Unavailable profile #${value}`, disabled: true }] : []),
          ...profiles.map((p) => ({
            value: String(p.id),
            label: p.name + (p.enabled ? "" : " (disabled)"),
            disabled: !p.enabled,
          })),
        ]}
      />
      <span id={`${id}-hint`} class="sr-only">
        Default is the ordinary Claude environment. {hint}
      </span>
      {profiles.length > 0 && !disabled && onUsage && (
        <PillToggle
          label="Usage"
          on={!!usage}
          onChange={onUsage}
          tip="Each profile's last reported 5-hour and weekly usage"
        />
      )}
    </>
  );
}
type ModalT = (p: { children: ComponentChildren; class?: string; label: string }) => ComponentChildren;

type ProfilesTab = "usage" | "profiles" | "add";
const TABS: { id: ProfilesTab; label: string }[] = [
  { id: "usage", label: "Usage" },
  { id: "profiles", label: "Profiles" },
  { id: "add", label: "Add profile" },
];
/** Which profile row is open: a profile id, Default, or none. */
type OpenRow = number | "default" | null;

/** One action with what it does written beside it (no hidden tooltip). */
function ActionLine({ children, hint }: { children: ComponentChildren; hint: ComponentChildren }) {
  return (
    <div class="pf-action">
      {children}
      <span class="field-hint">{hint}</span>
    </div>
  );
}

const LOGIN_CHANGED_HINT = "Signed in to a different account in this folder? Forgets its usage until its next session gets a reply.";

/** A profile as a collapsed row (name, folder, notes); open, its settings and actions. */
function ProfileRow({ profile, open, onToggle }: { profile: AccountProfile; open: boolean; onToggle: () => void }) {
  const [name, setName] = useState(profile.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const id = useId();
  const others = accountProfiles.value.filter((p) => p.id !== profile.id);
  const sameAs = profile.account_group ? others.filter((p) => p.account_group === profile.account_group) : [];
  const run = async (work: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await work();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
      // The inline error goes with the row if it is closed meanwhile; the toast stays.
      toast("error", `${profile.name}: ${msg}`);
    } finally {
      setBusy(false);
    }
  };
  const update = (patch: { name?: string; enabled?: boolean }) =>
    run(async () => {
      const next = await rpc.call<AccountProfile>("account_profile.update", { id: profile.id, ...patch });
      accountProfiles.value = accountProfiles.value.map((p) => p.id === next.id ? next : p);
      setName(next.name);
    });
  const setSameAccount = (value: string) =>
    run(async () => {
      await rpc.call<AccountProfile>("account_profile.same_account", {
        id: profile.id,
        with: value ? Number(value) : null,
      });
      accountProfiles.value = await rpc.call<AccountProfile[]>("account_profile.list", {});
    });
  const loginChanged = () =>
    run(async () => {
      accountUsage.value = await rpc.call<AccountUsage>("account_profile.login_changed", {
        account_profile: profile.id,
      });
      accountProfiles.value = await rpc.call<AccountProfile[]>("account_profile.list", {});
      toast("success", `Forgot ${profile.name}'s usage. It reappears after its next session gets a reply.`);
    });
  return (
    <div class={cx("pf-item", open && "open")}>
      <button
        type="button"
        class="pf-head"
        id={`pf-head-${profile.id}`}
        aria-expanded={open}
        aria-controls={open ? `${id}-body` : undefined}
        aria-label={`${profile.name}, ${profile.config_dir}${profile.enabled ? "" : ", disabled"}${
          sameAs.length ? `, same account as ${sameAs.map((p) => p.name).join(", ")}` : ""
        }`}
        onClick={onToggle}
      >
        <span class="pf-name">{profile.name}</span>
        <span class="pf-path">{tildify(profile.config_dir, meta.value.home)}</span>
        {!profile.enabled && <span class="pf-note">Disabled</span>}
        {sameAs.length > 0 && <span class="pf-note">same account as {sameAs.map((p) => p.name).join(", ")}</span>}
        <IconChevron size={12} class="pf-chev" />
      </button>
      {open && (
        <form
          class="pf-body"
          id={`${id}-body`}
          onSubmit={(e) => {
            e.preventDefault();
            update({ name: name.trim() });
          }}
        >
          <div class="field-row pf-rename">
            <label class="field">
              <span class="field-label">Name</span>
              <input value={name} maxLength={80} disabled={busy} onInput={(e) => setName(e.currentTarget.value)} />
            </label>
            <button class="btn" type="submit" disabled={busy || !name.trim() || name.trim() === profile.name}>
              Rename
            </button>
          </div>
          {others.length > 0 && (
            <label class="field">
              <span class="field-label">Signs in to</span>
              <span class="profile-select">
                <select
                  value={String(sameAs[0]?.id ?? "")}
                  disabled={busy}
                  onChange={(e) => setSameAccount(e.currentTarget.value)}
                >
                  <option value="">Its own Claude account</option>
                  {others.map((p) => <option key={p.id} value={p.id}>The same account as {p.name}</option>)}
                </select>
                <IconChevronDown size={14} />
              </span>
            </label>
          )}
          <code class="profile-directory">{profile.config_dir}</code>
          <ActionLine
            hint={profile.enabled
              ? "Blocks new starts and restarts. Running sessions continue."
              : "Disabled: new starts and restarts are blocked. Running sessions continue."}
          >
            <button class="btn" type="button" disabled={busy} onClick={() => update({ enabled: !profile.enabled })}>
              {profile.enabled ? "Disable" : "Enable"}
            </button>
          </ActionLine>
          <ActionLine hint={LOGIN_CHANGED_HINT}>
            <button class="btn" type="button" disabled={busy} onClick={loginChanged}>Login changed</button>
          </ActionLine>
          {error && <p class="field-hint field-bad" role="alert">{error}</p>}
        </form>
      )}
    </div>
  );
}

/** Default: Muster's own Claude environment, ~/.claude unless Muster itself was started with
 * CLAUDE_CONFIG_DIR set (sessions then inherit that folder). */
function DefaultRow({ open, onToggle, hasUsage }: { open: boolean; onToggle: () => void; hasUsage: boolean }) {
  const [busy, setBusy] = useState(false);
  const id = useId();
  const forget = async () => {
    setBusy(true);
    try {
      accountUsage.value = await rpc.call<AccountUsage>("account_profile.login_changed", {
        account_profile: "default",
      });
      // Profiles on Default's folder lose their same-account setting too.
      accountProfiles.value = await rpc.call<AccountProfile[]>("account_profile.list", {});
      toast("success", "Forgot the usage of Default's folder.");
    } catch (e) {
      toast("error", e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class={cx("pf-item", open && "open")}>
      <button
        type="button"
        class="pf-head"
        aria-expanded={open}
        aria-controls={open ? `${id}-body` : undefined}
        aria-label="Default, Muster's own Claude setup"
        onClick={onToggle}
      >
        <span class="pf-name">Default</span>
        <span class="pf-path pf-path-plain">Muster's own Claude setup</span>
        <IconChevron size={12} class="pf-chev" />
      </button>
      {open && (
        <div class="pf-body" id={`${id}-body`}>
          <span class="field-hint">
            Sessions without a profile start with the Claude setup Muster itself was started with:{" "}
            <code>~/.claude</code>, or the folder in <code>CLAUDE_CONFIG_DIR</code> if that was set.
          </span>
          <ActionLine hint={hasUsage ? LOGIN_CHANGED_HINT : "No usage to forget yet."}>
            <button class="btn" type="button" disabled={busy || !hasUsage} onClick={forget}>Login changed</button>
          </ActionLine>
        </div>
      )}
    </div>
  );
}

/** Claude profiles in three tabs: usage (what you look at most), the profiles themselves (rows
 * that open one at a time), and registering a new one. Always opens on Usage; `account` (from a
 * sidebar click) highlights that account's rows. */
export function AccountProfilesDialog({ Modal, close, account }: {
  Modal: ModalT;
  close: () => void;
  account?: string;
}) {
  const now = useUsageClock();
  const id = useId();
  const [tab, setTab] = useState<ProfilesTab>("usage");
  const [openRow, setOpenRow] = useState<OpenRow>(null);
  const [howOpen, setHowOpen] = useState(false);
  const [defaultOpen, setDefaultOpen] = useState(false);
  const [name, setName] = useState("");
  const [directory, setDirectory] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const tabsRef = useRef<HTMLDivElement>(null);
  // Account keys follow row order, which a rename can change: remember the clicked account's
  // profiles and find their account again in each new snapshot.
  const [marked] = useState(() =>
    account ? accountUsage.peek()?.rows.filter((r) => r.account === account).map((r) => r.choice) ?? [] : []
  );
  useEffect(() => {
    tabsRef.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.focus();
  }, []);
  const usage = accountUsage.value ? asOf(accountUsage.value, now) : null;
  const def = usage?.rows.find((r) => r.choice === "default");
  const defaultHasUsage = !!def && (def.five_hour.status !== "unknown" || def.seven_day.status !== "unknown");
  const toggle = (row: Exclude<OpenRow, null>) => setOpenRow((o) => o === row ? null : row);
  const create = async () => {
    setBusy(true);
    setError("");
    try {
      const next = await rpc.call<AccountProfile>("account_profile.create", {
        name: name.trim(),
        config_dir: directory.trim(),
      });
      accountProfiles.value = [...accountProfiles.value.filter((p) => p.id !== next.id), next];
      setName("");
      setDirectory("");
      toast("success", `Registered ${next.name}. Authentication stays in Claude Code.`);
      setTab("profiles");
      setOpenRow(next.id);
      setTimeout(() => document.getElementById(`pf-head-${next.id}`)?.focus(), 0);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const tabKeys = (e: KeyboardEvent) => {
    const i = TABS.findIndex((t) => t.id === tab);
    const j = e.key === "ArrowRight" ? (i + 1) % TABS.length
      : e.key === "ArrowLeft" ? (i + TABS.length - 1) % TABS.length
      : e.key === "Home" ? 0
      : e.key === "End" ? TABS.length - 1
      : -1;
    if (j < 0) return;
    e.preventDefault();
    setTab(TABS[j].id);
    setTimeout(() => document.getElementById(`${id}-tab-${TABS[j].id}`)?.focus(), 0);
  };
  const panel = (t: ProfilesTab) => ({
    role: "tabpanel" as const,
    id: `${id}-panel-${t}`,
    "aria-labelledby": `${id}-tab-${t}`,
    hidden: tab !== t,
  });
  return (
    <Modal class="dialog dialog-scroll dlg-calm profiles-dialog" label="Claude profiles">
      <div class="dlg-head">
        <h2>Claude profiles</h2>
        <div class="dlg-tabs" role="tablist" aria-label="Claude profiles" ref={tabsRef} onKeyDown={tabKeys}>
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              id={`${id}-tab-${t.id}`}
              aria-selected={tab === t.id}
              aria-controls={`${id}-panel-${t.id}`}
              tabIndex={tab === t.id ? 0 : -1}
              class={cx("dlg-tab", tab === t.id && "on")}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
      </div>
      <div class="dlg-body pf-panels">
        <div {...panel("usage")} class="pf-panel">
          {usage
            ? (
              <UsageTable
                usage={usage}
                now={now}
                account={usage.rows.find((r) => marked.some((c) => sameChoice(c, r.choice)))?.account}
              />
            )
            : <p class="field-hint">No usage yet. It appears after a Claude session Muster started gets a reply.</p>}
          <p class="field-hint">From Claude sessions Muster starts, updated after each reply.</p>
          <MoreOptions open={howOpen} onToggle={setHowOpen} label="How usage is read">
            <p class="field-hint">
              Muster reads the status line of the Claude sessions it starts, after each reply. An account nobody
              used here stays Unknown: Muster never asks Claude about it. Your own status line keeps showing as
              before.
            </p>
          </MoreOptions>
        </div>
        <div {...panel("profiles")} class="pf-panel">
          <p class="field-hint">
            A profile is a Claude config folder that sessions start in. Muster never reads or changes login files.
          </p>
          <div class="pf-list">
            <DefaultRow open={openRow === "default"} onToggle={() => toggle("default")} hasUsage={defaultHasUsage} />
            {accountProfiles.value.map((p) => (
              <ProfileRow key={p.id} profile={p} open={openRow === p.id} onToggle={() => toggle(p.id)} />
            ))}
          </div>
          {accountProfiles.value.length === 0 && (
            <div class="pf-empty">
              <span class="field-hint">No other profiles yet. Every session uses Default.</span>
              <button class="btn btn-sm" type="button" onClick={() => setTab("add")}>Add a profile</button>
            </div>
          )}
        </div>
        <form
          {...panel("add")}
          class="pf-panel"
          onSubmit={(e) => {
            e.preventDefault();
            create();
          }}
        >
          <ol class="pf-steps">
            <li>
              In a terminal, run <code>CLAUDE_CONFIG_DIR=/absolute/path claude</code> and sign in as usual.
            </li>
            <li>Register that folder here.</li>
          </ol>
          <label class="field">
            <span class="field-label">Name</span>
            <input
              value={name}
              maxLength={80}
              placeholder="Personal or Work"
              disabled={busy}
              onInput={(e) => setName(e.currentTarget.value)}
            />
          </label>
          <label class="field">
            <span class="field-label">Folder</span>
            <input
              value={directory}
              placeholder="~/.claude-work"
              spellcheck={false}
              disabled={busy}
              onInput={(e) => setDirectory(e.currentTarget.value)}
            />
            <span class="field-hint">Must already exist. Its location stays fixed.</span>
          </label>
          <MoreOptions open={defaultOpen} onToggle={setDefaultOpen} label="Registering ~/.claude itself?">
            <p class="field-hint">
              Sign in with plain <code>claude</code> (or <code>env -u CLAUDE_CONFIG_DIR claude</code>) instead of
              step 1. A profile on <code>~/.claude</code> starts its sessions without{" "}
              <code>CLAUDE_CONFIG_DIR</code>, so Claude Code uses <code>~/.claude</code> and{" "}
              <code>~/.claude.json</code>.
            </p>
          </MoreOptions>
          {error && <p class="field-hint field-bad" role="alert">{error}</p>}
        </form>
      </div>
      <div class="dlg-foot">
        {tab === "add" && (
          <button
            class="btn btn-primary"
            type="submit"
            form={`${id}-panel-add`}
            disabled={busy || !name.trim() || !directory.trim()}
          >
            {busy ? "Registering…" : "Register profile"}
          </button>
        )}
        <button class="btn" type="button" onClick={close}>Done</button>
      </div>
    </Modal>
  );
}

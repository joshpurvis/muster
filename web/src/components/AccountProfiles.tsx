import type { ComponentChildren } from "preact";
import { useEffect, useId, useState } from "preact/hooks";
import { accountProfiles, accountProfilesSupported, accountUsage, toast } from "../state.ts";
import { rpc } from "../rpc.ts";
import type { AccountProfile, AccountUsage, ProfileChoice } from "../types.ts";
import {
  asOf,
  lastObserved,
  parseProfileChoice,
  sameChoice,
  suggestionView,
  type WindowCell,
  windowCell,
} from "../account_profiles.ts";
import { IconChevronDown, IconUser } from "../icons.tsx";
import { PillSelect, PillToggle } from "./DialogKit.tsx";
import { cx } from "../util.ts";

/** Keeps `accountUsage` current while mounted (statuses age on the backend's clock: a figure
 * that was current goes stale) and returns a clock that ticks the "seen … ago" texts. */
function useUsageClock(): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    let alive = true;
    const refresh = () =>
      rpc.call<AccountUsage>("account_usage.get", {}).then((u) => {
        if (alive) accountUsage.value = u;
      }).catch(() => {});
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
export function UsageTable({ usage, now, selected }: {
  usage: AccountUsage;
  now: number;
  /** Highlighted row (the new session's current choice). */
  selected?: ProfileChoice;
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
                selected !== undefined && sameChoice(r.choice, selected) && "on",
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
  /** Hover text; also read to screen readers. */
  tip?: string;
  selectRef?: { current: HTMLSelectElement | null };
  usage?: boolean;
  onUsage?: (on: boolean) => void;
}) {
  const id = useId();
  if (!accountProfilesSupported.value) return null;
  const profiles = accountProfiles.value;
  const missing = typeof value === "number" && !profiles.some((p) => p.id === value);
  const hint = tip ?? "Verify your account with /status inside Claude Code.";
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

function ProfileRow({ profile }: { profile: AccountProfile }) {
  const [name, setName] = useState(profile.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const others = accountProfiles.value.filter((p) => p.id !== profile.id);
  const sameAs = profile.account_group
    ? others.find((p) => p.account_group === profile.account_group)?.id ?? ""
    : "";
  const run = async (work: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await work();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
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
    <form
      class="profile-row"
      onSubmit={(e) => {
        e.preventDefault();
        update({ name: name.trim() });
      }}
    >
      <div class="field-row">
        <label class="field">
          <span class="field-label">Profile name{profile.enabled ? "" : " · disabled"}</span>
          <input
            value={name}
            maxLength={80}
            disabled={busy}
            onInput={(e) => setName(e.currentTarget.value)}
          />
        </label>
        <button class="btn" type="submit" disabled={busy || !name.trim() || name.trim() === profile.name}>
          Rename
        </button>
        <button
          class="btn"
          type="button"
          disabled={busy}
          onClick={() => update({ enabled: !profile.enabled })}
        >
          {profile.enabled ? "Disable" : "Enable"}
        </button>
      </div>
      <code class="profile-directory">{profile.config_dir}</code>
      {!profile.enabled && (
        <span class="field-hint">New starts and restarts are blocked. Running sessions continue.</span>
      )}
      {others.length > 0 && (
        <div class="field-row profile-account-row">
          <label class="field">
            <span class="field-label">Signs in to</span>
            <span class="profile-select">
              <select
                value={String(sameAs)}
                disabled={busy}
                onChange={(e) => setSameAccount(e.currentTarget.value)}
              >
                <option value="">Its own Claude account</option>
                {others.map((p) => <option key={p.id} value={p.id}>The same account as {p.name}</option>)}
              </select>
              <IconChevronDown size={14} />
            </span>
          </label>
          <button
            class="btn"
            type="button"
            disabled={busy}
            title="You signed in to a different account in this directory: forget its usage"
            onClick={loginChanged}
          >
            Login changed
          </button>
        </div>
      )}
      {others.length === 0 && (
        <div class="profile-account-row">
          <button class="btn" type="button" disabled={busy} onClick={loginChanged}>Login changed</button>
        </div>
      )}
      {error && <p class="field-hint field-bad" role="alert">{error}</p>}
    </form>
  );
}

/** The profiles dialog's usage overview, including Default. */
function UsageSection() {
  const now = useUsageClock();
  const [busy, setBusy] = useState(false);
  if (!accountUsage.value) return null;
  const usage = asOf(accountUsage.value, now);
  const forgetDefault = async () => {
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
  const def = usage.rows.find((r) => r.choice === "default");
  const hasDefault = !!def && (def.five_hour.status !== "unknown" || def.seven_day.status !== "unknown");
  return (
    <section class="profile-row usage-section" aria-label="Usage">
      <h3>Usage</h3>
      <p class="field-hint">
        From the status line of Claude sessions Muster starts, after each reply. An account nobody used here
        stays Unknown: Muster never asks Claude about it. Your own status line keeps showing as before.
      </p>
      <UsageTable usage={usage} now={now} />
      {hasDefault && (
        <div class="profile-account-row">
          <button class="btn btn-sm" type="button" disabled={busy} onClick={forgetDefault}>
            Default's login changed
          </button>
        </div>
      )}
    </section>
  );
}

export function AccountProfilesDialog({ Modal, close }: { Modal: ModalT; close: () => void }) {
  const [name, setName] = useState("");
  const [directory, setDirectory] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
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
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal class="dialog dialog-scroll dlg-calm profiles-dialog" label="Claude profiles">
      <div class="dlg-head">
        <h2>Claude profiles</h2>
        <span class="muted">
          Choose which Claude configuration directory a session uses. Muster never reads or changes login
          files.
        </span>
      </div>
      <div class="dlg-body">
        <UsageSection />
        {accountProfiles.value.map((p) => <ProfileRow key={p.id} profile={p} />)}
        <form
          class="profile-row"
          onSubmit={(e) => {
            e.preventDefault();
            create();
          }}
        >
          <h3>Register a directory</h3>
          <p class="field-hint">
            First, run <code>CLAUDE_CONFIG_DIR=/absolute/path claude</code>{" "}
            in your terminal and sign in normally. Then register that directory here. Its location stays
            fixed. For the default <code>~/.claude</code> folder, sign in with plain <code>claude</code> (or
            {" "}
            <code>env -u CLAUDE_CONFIG_DIR claude</code>) instead; its sessions start without{" "}
            <code>CLAUDE_CONFIG_DIR</code>, so Claude Code uses <code>~/.claude</code> and{" "}
            <code>~/.claude.json</code>.
          </p>
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
            <span class="field-label">Existing configuration directory</span>
            <input
              value={directory}
              placeholder="~/.claude-work"
              spellcheck={false}
              disabled={busy}
              onInput={(e) => setDirectory(e.currentTarget.value)}
            />
          </label>
          {error && <p class="field-hint field-bad" role="alert">{error}</p>}
          <button class="btn btn-primary" type="submit" disabled={busy || !name.trim() || !directory.trim()}>
            {busy ? "Registering…" : "Register profile"}
          </button>
        </form>
      </div>
      <div class="dlg-foot">
        <button class="btn" type="button" onClick={close}>Done</button>
      </div>
    </Modal>
  );
}

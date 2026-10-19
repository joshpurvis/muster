import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { accountProfiles, accountProfilesSupported, toast } from "../state.ts";
import { rpc } from "../rpc.ts";
import type { AccountProfile, ProfileChoice } from "../types.ts";
import { parseProfileChoice } from "../account_profiles.ts";
import { IconChevronDown } from "../icons.tsx";

export function ProfileSelect({ value, onChange, disabled = false }: {
  value: ProfileChoice | undefined;
  onChange: (choice: ProfileChoice | undefined) => void;
  disabled?: boolean;
}) {
  if (!accountProfilesSupported.value) return null;
  const profiles = accountProfiles.value;
  const missing = typeof value === "number" && !profiles.some((p) => p.id === value);
  return (
    <label class="field">
      <span class="field-label">Claude profile</span>
      <span class="profile-select">
        <select
          value={value ?? "default"}
          disabled={disabled}
          onChange={(e) => onChange(parseProfileChoice(e.currentTarget.value))}
        >
          <option value="default">Default · ordinary Claude environment</option>
          {missing && <option value={value} disabled>Unavailable profile #{value}</option>}
          {profiles.map((p) => (
            <option key={p.id} value={p.id} disabled={!p.enabled}>
              {p.name}
              {p.enabled ? "" : " (disabled)"}
            </option>
          ))}
        </select>
        <IconChevronDown size={14} />
      </span>
      <span class="field-hint">Verify your account with /status inside Claude Code.</span>
    </label>
  );
}

type ModalT = (p: { children: ComponentChildren; class?: string; label: string }) => ComponentChildren;

function ProfileRow({ profile }: { profile: AccountProfile }) {
  const [name, setName] = useState(profile.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const update = async (patch: { name?: string; enabled?: boolean }) => {
    setBusy(true);
    setError("");
    try {
      const next = await rpc.call<AccountProfile>("account_profile.update", { id: profile.id, ...patch });
      accountProfiles.value = accountProfiles.value.map((p) => p.id === next.id ? next : p);
      setName(next.name);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
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
      {error && <p class="field-hint field-bad" role="alert">{error}</p>}
    </form>
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
    <Modal class="dialog dialog-scroll profiles-dialog" label="Claude profiles">
      <div class="dlg-head">
        <h2>Claude profiles</h2>
        <span class="muted">
          Choose which Claude configuration directory a session uses. Muster never reads or changes login
          files.
        </span>
      </div>
      <div class="dlg-body">
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
            fixed. A profile at the default <code>~/.claude</code> folder starts its sessions without{" "}
            <code>CLAUDE_CONFIG_DIR</code>, like Default.
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

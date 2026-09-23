// Per-channel agent permission profile: the controls (New Channel dialog and the Permissions
// dialog), a one-line summary, and the header chip shown when a channel is more permissive
// than the defaults. Only the human sets this; agents (MCP) can't.
import type { ComponentChildren } from "preact";
import { useLayoutEffect, useRef, useState } from "preact/hooks";
import { channelById, overlay, sessionsByChannel } from "../state.ts";
import * as A from "../actions.ts";
import type { Channel, ClaudeMode, CodexSandbox, Permissions } from "../types.ts";
import { DEFAULT_PERMISSIONS } from "../types.ts";
import { IconChevron, IconCodex, IconGlobe, IconShield, IconShieldAlert, IconSpark } from "../icons.tsx";
import { Spinner } from "./common.tsx";
import { cx } from "../util.ts";

interface Opt<T> {
  v: T;
  label: string;
  /** Short form for summaries and the header chip. */
  short: string;
  hint: string;
  danger?: string;
}

// Least to most permissive, left to right.
export const CODEX_OPTS: Opt<CodexSandbox>[] = [
  {
    v: "read_only",
    label: "Read only",
    short: "read only",
    hint: "Reads files and runs read-only commands. No edits, no network.",
  },
  {
    v: "workspace_write",
    label: "Workspace",
    short: "workspace",
    hint: "Edits files in the channel's folder. No network (Codex's default).",
  },
  {
    v: "workspace_write_network",
    label: "Workspace + network",
    short: "workspace + network",
    hint:
      "Edits the channel's folder and may use the network, including muster itself as you (it can read your muster token).",
  },
  {
    v: "full_access",
    label: "Full access",
    short: "full access",
    hint: "No sandbox.",
    danger: "No sandbox: Codex can change anything your user can, anywhere on this machine.",
  },
];

export const CLAUDE_OPTS: Opt<ClaudeMode>[] = [
  { v: "plan", label: "Plan", short: "plan", hint: "Explores and proposes a plan. No edits." },
  {
    v: "default",
    label: "Your default",
    short: "your default",
    hint: "No override: uses your own Claude Code setting (permissions.defaultMode).",
  },
  {
    v: "accept_edits",
    label: "Accept edits",
    short: "accept edits",
    hint: "Edits files without asking; still asks before other commands.",
  },
  {
    v: "bypass",
    label: "Bypass",
    short: "bypass",
    hint: "No permission prompts.",
    danger: "No permission prompts: Claude runs every tool and command without asking.",
  },
];

export function perms(c: Channel | undefined | null): Permissions {
  return { ...DEFAULT_PERMISSIONS, ...(c?.permissions ?? {}) };
}

const codexOpt = (v: CodexSandbox) => CODEX_OPTS.find((o) => o.v === v) ?? CODEX_OPTS[1];
const claudeOpt = (v: ClaudeMode) => CLAUDE_OPTS.find((o) => o.v === v) ?? CLAUDE_OPTS[1];

/** "Codex: workspace + network · Claude: default" */
export function summary(p: Permissions): string {
  return `Codex: ${codexOpt(p.codex_sandbox).short} · Claude: ${claudeOpt(p.claude_mode).short}`;
}

export function isDefault(p: Permissions): boolean {
  return p.codex_sandbox === DEFAULT_PERMISSIONS.codex_sandbox &&
    p.claude_mode === DEFAULT_PERMISSIONS.claude_mode;
}

/** More permissive than the defaults (read only / plan are less). */
export function isElevated(p: Permissions): boolean {
  return p.codex_sandbox === "workspace_write_network" || p.codex_sandbox === "full_access" ||
    p.claude_mode === "accept_edits" || p.claude_mode === "bypass";
}

export function isDangerous(p: Permissions): boolean {
  return p.codex_sandbox === "full_access" || p.claude_mode === "bypass";
}

function Seg<T extends string>(
  { label, icon, opts, value, onChange }: {
    label: string;
    icon: ComponentChildren;
    opts: Opt<T>[];
    value: T;
    onChange: (v: T) => void;
  },
) {
  const cur = opts.find((o) => o.v === value) ?? opts[0];
  const move = (e: KeyboardEvent) => {
    const i = opts.findIndex((o) => o.v === value);
    const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!d) return;
    e.preventDefault();
    const next = opts[(i + d + opts.length) % opts.length];
    onChange(next.v);
    const group = e.currentTarget as HTMLElement;
    requestAnimationFrame(() => group.querySelector<HTMLElement>(".seg-btn.on")?.focus());
  };
  return (
    <div class="field perm-field">
      <span class="field-label perm-label">
        {icon} {label}
      </span>
      <div class="seg seg-lg perm-seg" role="radiogroup" aria-label={label} onKeyDown={move}>
        {opts.map((o) => (
          <button
            type="button"
            role="radio"
            aria-checked={o.v === value}
            tabIndex={o.v === value ? 0 : -1}
            class={cx("seg-btn", o.v === value && "on", o.danger && "seg-danger")}
            onClick={() => onChange(o.v)}
          >
            {o.danger && <IconShieldAlert size={13} />}
            {o.label}
          </button>
        ))}
      </div>
      {cur.danger
        ? (
          <span class="perm-warn" role="note">
            <IconShieldAlert size={13} /> {cur.danger}
          </span>
        )
        : <span class="field-hint">{cur.hint}</span>}
    </div>
  );
}

/** The two segmented controls. */
export function PermissionFields(
  { value, onChange }: { value: Permissions; onChange: (p: Permissions) => void },
) {
  return (
    <div class="perm-fields">
      <Seg
        label="Codex sandbox"
        icon={<IconCodex size={13} />}
        opts={CODEX_OPTS}
        value={value.codex_sandbox}
        onChange={(v) => onChange({ ...value, codex_sandbox: v })}
      />
      <Seg
        label="Claude permission mode"
        icon={<IconSpark size={13} />}
        opts={CLAUDE_OPTS}
        value={value.claude_mode}
        onChange={(v) => onChange({ ...value, claude_mode: v })}
      />
    </div>
  );
}

/** Collapsible "Permissions" section for the New Channel dialog. */
export function PermissionsSection(
  { value, onChange }: { value: Permissions; onChange: (p: Permissions) => void },
) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (open) ref.current?.scrollIntoView({ block: "nearest" });
  }, [open]);
  return (
    <div class={cx("perm-section", open && "open")} ref={ref}>
      <button
        type="button"
        class="perm-disclosure"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <span class="perm-disc-chev" aria-hidden="true">
          <IconChevron size={12} />
        </span>
        <span class="field-label">Permissions</span>
        <span class={cx("perm-summary", isDangerous(value) && "danger", isElevated(value) && "elevated")}>
          {isElevated(value) &&
            (isDangerous(value) ? <IconShieldAlert size={12} /> : <IconGlobe size={12} />)}
          {summary(value)}
        </span>
      </button>
      {open && (
        <div class="perm-body">
          <PermissionFields value={value} onChange={onChange} />
          <span class="field-hint perm-note">
            Applies to every agent in the channel, including ones other agents start. Agents can't change it through muster's tools.
          </span>
        </div>
      )}
    </div>
  );
}

/** Dialog opened from the channel menu, header chip or command palette. */
export function PermissionsDialog(
  { channelId, Modal, close }: {
    channelId: number;
    Modal: (p: { children: ComponentChildren; class?: string; label: string }) => ComponentChildren;
    close: () => void;
  },
) {
  const c = channelById.value.get(channelId);
  const [value, setValue] = useState<Permissions>(perms(c));
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const first = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => first.current?.querySelector<HTMLElement>(".seg-btn.on")?.focus(), []);
  if (!c) return null;
  const before = perms(c);
  const changed = before.codex_sandbox !== value.codex_sandbox || before.claude_mode !== value.claude_mode;
  const running = (sessionsByChannel.value.get(c.id) ?? []).filter((s) =>
    s.state !== "exited" &&
    ((s.kind === "codex" && before.codex_sandbox !== value.codex_sandbox) ||
      (s.kind === "claude" && before.claude_mode !== value.claude_mode))
  );
  const save = async () => {
    if (!changed) return close();
    setBusy(true);
    setErr(null);
    try {
      await A.setPermissions(c.id, value);
      overlay.value = null;
      A.focusActiveTerminalSoon();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <Modal class="dialog perm-dlg" label="Channel permissions">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
      >
        <div class="dlg-head">
          <h2>Permissions</h2>
          <span class="muted">
            What agents in <b>#{c.name}</b> may do. Applies to new and restarted sessions.
          </span>
        </div>
        <div class="dlg-body" ref={first}>
          <PermissionFields value={value} onChange={setValue} />
          {running.length > 0 && (
            <span class="field-hint perm-note">
              {running.length === 1
                ? `${running[0].name} keeps its current permissions until it is restarted.`
                : `${running.length} running sessions keep their current permissions until restarted.`}
            </span>
          )}
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          {!isDefault(value) && (
            <button
              type="button"
              class="btn btn-ghost dlg-foot-left"
              onClick={() => setValue({ ...DEFAULT_PERMISSIONS })}
            >
              Reset to defaults
            </button>
          )}
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button
            type="submit"
            class={cx("btn", isDangerous(value) && changed ? "btn-danger" : "btn-primary")}
            disabled={busy}
          >
            {busy ? <Spinner size={12} /> : null} Save
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** Header chip, only when the channel is more permissive than the defaults. */
export function PermissionChip({ c }: { c: Channel }) {
  const p = perms(c);
  if (!isElevated(p)) return null;
  const danger = isDangerous(p);
  const parts: string[] = [];
  if (p.codex_sandbox === "full_access") parts.push("Codex: full access");
  else if (p.codex_sandbox === "workspace_write_network") parts.push("Network");
  if (p.claude_mode === "bypass") parts.push("Claude: bypass");
  else if (p.claude_mode === "accept_edits") parts.push("Accept edits");
  return (
    <button
      type="button"
      class={cx("chip chip-perm", danger ? "danger" : "elevated")}
      data-tip={`${summary(p)} · click to change`}
      onClick={() => (overlay.value = { kind: "permissions", channelId: c.id })}
    >
      {danger
        ? <IconShieldAlert size={12} />
        : p.codex_sandbox === "workspace_write_network"
        ? <IconGlobe size={12} />
        : <IconShield size={12} />}
      <span>{parts.join(" · ")}</span>
    </button>
  );
}

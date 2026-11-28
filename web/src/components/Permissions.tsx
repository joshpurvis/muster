// Per-tab agent permission profile: the controls (New tab dialog and the Permissions dialog), a
// one-line summary, and the tab's elevated-permissions badge.
// Only the human sets this; agents (MCP) can't.
import type { ComponentChildren } from "preact";
import { useId, useLayoutEffect, useRef, useState } from "preact/hooks";
import { channelById, overlay, sessionsByChannel, workspaceById } from "../state.ts";
import { taskLabel } from "../model.ts";
import * as A from "../actions.ts";
import type { AntigravityMode, Channel, ClaudeMode, CodexSandbox, Permissions } from "../types.ts";
import { CLAUDE_MODES, DEFAULT_PERMISSIONS, storedPermissions } from "../types.ts";
import { IconAntigravity, IconCodex, IconGlobe, IconShield, IconShieldAlert, IconSpark } from "../icons.tsx";
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
    hint: "Edits files in the tab's folder. No network (Codex's default).",
  },
  {
    v: "workspace_write_network",
    label: "Workspace + network",
    short: "workspace + network",
    hint:
      "Edits the tab's folder and may use the network, including muster itself as you (it can read your muster token).",
  },
  {
    v: "full_access",
    label: "Full access",
    short: "full access",
    hint: "No sandbox.",
    danger: "No sandbox: Codex can change anything your user can, anywhere on this machine.",
  },
];

const claudeLabels: Record<ClaudeMode, Omit<Opt<ClaudeMode>, "v">> = {
  plan: { label: "Plan", short: "plan", hint: "Explores and proposes a plan. No edits." },
  default: {
    label: "Your default",
    short: "your default",
    hint: "Uses this account's Claude Code setting (permissions.defaultMode).",
  },
  accept_edits: {
    label: "Accept edits",
    short: "accept edits",
    hint: "Edits files without asking; still asks before other commands.",
  },
  auto: {
    label: "Auto",
    short: "auto",
    hint: "Runs without asking. Claude blocks actions it judges risky.",
  },
  bypass: {
    label: "Bypass",
    short: "bypass",
    hint: "No permission prompts.",
    danger: "No permission prompts: Claude runs every tool and command without asking.",
  },
};
export const CLAUDE_OPTS = CLAUDE_MODES.map((v) => ({ v, ...claudeLabels[v] }));

export const ANTIGRAVITY_OPTS: Opt<AntigravityMode>[] = [
  { v: "plan", label: "Plan", short: "plan", hint: "Reads and plans. No edits." },
  { v: "accept_edits", label: "Accept edits", short: "accept edits",
    hint: "Edits files in the tab's folder without asking. Asks before commands." },
  { v: "auto", label: "Auto", short: "auto",
    hint: "Edits and commands Antigravity judges safe run without asking; the rest ask. Antigravity's default." },
  { v: "full_access", label: "Full access", short: "full access", hint: "Never asks.",
    danger: "No prompts: Antigravity can run anything your user can." },
];

export function perms(c: Channel | undefined | null): Permissions {
  return storedPermissions(c?.permissions);
}

const antigravityOpt = (v: AntigravityMode) => ANTIGRAVITY_OPTS.find((o) => o.v === v) ?? ANTIGRAVITY_OPTS[2];
const codexOpt = (v: CodexSandbox) => CODEX_OPTS.find((o) => o.v === v) ?? CODEX_OPTS[1];
const claudeOpt = (v: ClaudeMode) => claudeLabels[v] ?? claudeLabels[DEFAULT_PERMISSIONS.claude_mode];

/** "Codex: workspace + network · Claude: auto" */
export function summary(p: Permissions): string {
  return `Codex: ${codexOpt(p.codex_sandbox).short} · Claude: ${claudeOpt(p.claude_mode).short}` +
    (antigravityOpt(p.antigravity_mode).v === "auto" ? "" : ` · ${kindSummary(p, "antigravity")}`);
}

export function isDefault(p: Permissions): boolean {
  return p.codex_sandbox === DEFAULT_PERMISSIONS.codex_sandbox &&
    p.claude_mode === DEFAULT_PERMISSIONS.claude_mode && p.antigravity_mode === DEFAULT_PERMISSIONS.antigravity_mode;
}

/** Profiles marked with the elevated-permissions badge. */
export function isElevated(p: Permissions): boolean {
  return p.codex_sandbox === "workspace_write_network" ||
    p.claude_mode === "accept_edits" || p.claude_mode === "auto" || isDangerous(p);
}

export function isDangerous(p: Permissions): boolean {
  return p.codex_sandbox === "full_access" || p.claude_mode === "bypass" || p.antigravity_mode === "full_access";
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
  const warningId = useId();
  const cur = opts.find((o) => o.v === value) ?? opts[0];
  const message = useRef<HTMLSpanElement>(null);
  const previous = useRef(value);
  useLayoutEffect(() => {
    if (value !== previous.current && cur.danger) message.current?.scrollIntoView({ block: "center" });
    previous.current = value;
  }, [value, cur.danger]);
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
            aria-describedby={o.v === value && o.danger ? warningId : undefined}
            tabIndex={o.v === value ? 0 : -1}
            class={cx("seg-btn", o.v === value && "on", o.danger && "seg-danger")}
            onClick={() => onChange(o.v)}
          >
            {o.danger && <IconShieldAlert size={13} />}
            {o.label}
          </button>
        ))}
      </div>
      <span ref={message} aria-live="polite" aria-atomic="true" id={warningId}
        class={cur.danger ? "perm-warn" : "field-hint"}>
        {cur.danger && <IconShieldAlert size={13} />} {cur.danger ?? cur.hint}
      </span>
    </div>
  );
}

/** The segmented controls (`only`: just the one that applies, for a named agent's tab). */
export function PermissionFields(
  { value, onChange, only }: {
    value: Permissions;
    onChange: (p: Permissions) => void;
    only?: "codex" | "claude" | "antigravity";
  },
) {
  return (
    <div class="perm-fields">
      {(!only || only === "codex") && (
        <Seg
          label="Codex sandbox"
          icon={<IconCodex size={13} />}
          opts={CODEX_OPTS}
          value={value.codex_sandbox}
          onChange={(v) => onChange({ ...value, codex_sandbox: v })}
        />
      )}
      {(!only || only === "claude") && (
        <Seg
          label="Claude permission mode"
          icon={<IconSpark size={13} />}
          opts={CLAUDE_OPTS}
          value={value.claude_mode}
          onChange={(v) => onChange({ ...value, claude_mode: v })}
        />
      )}
      {(!only || only === "antigravity") && (
        <Seg
          label="Antigravity mode"
          icon={<IconAntigravity size={13} />}
          opts={ANTIGRAVITY_OPTS}
          value={value.antigravity_mode}
          onChange={(v) => onChange({ ...value, antigravity_mode: v })}
        />
      )}
    </div>
  );
}

/** "Claude: plan" / "Codex: read only": the setting that applies to `kind`. */
export function kindSummary(p: Permissions, kind: "codex" | "claude" | "antigravity"): string {
  if (kind === "antigravity") return `Antigravity: ${antigravityOpt(p.antigravity_mode).short}`;
  return kind === "codex"
    ? `Codex: ${codexOpt(p.codex_sandbox).short}`
    : `Claude: ${claudeOpt(p.claude_mode).short}`;
}

/** The Permissions pill of the New channel and New agent dialogs: shows the current choice
 * ("Permissions" while it is the default), amber for elevated permissions, red
 * when dangerous. Opens `PermissionsPanel` under the pill row. */
export function PermissionsPill(
  { value, only, open, onToggle, controls }: {
    value: Permissions;
    only?: "codex" | "claude" | "antigravity";
    open: boolean;
    onToggle: (open: boolean) => void;
    /** Id of the panel it opens. */
    controls: string;
  },
) {
  const kinds = only ? [only] : (["codex", "claude", "antigravity"] as const);
  const changed = kinds.filter((k) => kindSummary(value, k) !== kindSummary(DEFAULT_PERMISSIONS, k));
  const text = changed.length ? changed.map((k) => kindSummary(value, k)).join(" · ") : "Permissions";
  return (
    <button
      type="button"
      class={cx("pill perm-pill", open && "on", isElevated(value) && "elevated", isDangerous(value) && "danger")}
      aria-expanded={open}
      aria-controls={controls}
      aria-label={`Permissions: ${only ? kindSummary(value, only) : summary(value)}`}
      onClick={() => onToggle(!open)}
    >
      {isDangerous(value)
        ? <IconShieldAlert size={13} />
        : isElevated(value)
        ? <IconGlobe size={13} />
        : <IconShield size={13} />}
      <span class="pill-text">{text}</span>
    </button>
  );
}

/** What `PermissionsPill` opens: the control(s) and a note on who they apply to. */
export function PermissionsPanel(
  { value, onChange, only, note, id }: {
    value: Permissions;
    onChange: (p: Permissions) => void;
    only?: "codex" | "claude" | "antigravity";
    note?: string;
    id: string;
  },
) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => ref.current?.scrollIntoView({ block: "nearest" }), []);
  return (
    <div class="pill-panel perm-panel" id={id} ref={ref} role="group" aria-label="Permissions">
      <PermissionFields value={value} onChange={onChange} only={only} />
      <span class="field-hint perm-note">
        {note ??
          "Applies to every agent in the tab, including ones other agents start. Agents can't change it through muster's tools."}
      </span>
    </div>
  );
}

/** Dialog opened from the tab menu, the tab's badge or the command palette. */
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
  const changed = before.codex_sandbox !== value.codex_sandbox || before.claude_mode !== value.claude_mode ||
    before.antigravity_mode !== value.antigravity_mode;
  const running = (sessionsByChannel.value.get(c.id) ?? []).filter((s) =>
    s.state !== "exited" &&
    ((s.kind === "codex" && before.codex_sandbox !== value.codex_sandbox) ||
      (s.kind === "claude" && before.claude_mode !== value.claude_mode) ||
      (s.kind === "antigravity" && before.antigravity_mode !== value.antigravity_mode))
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
    <Modal class="dialog dialog-scroll dlg-calm perm-dlg" label="Tab permissions">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
      >
        <div class="dlg-head">
          <h2>Permissions</h2>
          <span class="muted">
            What agents in <b>{taskLabel(c, workspaceById.value.get(c.workspace_id)).text}</b>{" "}
            may do. Applies to new and restarted sessions.
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

/** A tab's permission badge, for elevated permissions: amber globe
 * (network) or shield (accept edits or auto), red warning shield (full access, bypass). Opens the
 * Permissions dialog. */
export function PermBadge({ c }: { c: Channel }) {
  const p = perms(c);
  if (!isElevated(p)) return null;
  const danger = isDangerous(p);
  return (
    <button
      type="button"
      class={cx("tab-perm", danger ? "danger" : "elevated")}
      data-tip={`${summary(p)} · click to change`}
      aria-label={`Permissions: ${summary(p)}`}
      onPointerDown={(e) => e.stopPropagation()}
      onClick={(e) => {
        e.stopPropagation();
        overlay.value = { kind: "permissions", channelId: c.id };
      }}
    >
      {danger
        ? <IconShieldAlert size={12} />
        : p.codex_sandbox === "workspace_write_network"
        ? <IconGlobe size={12} />
        : <IconShield size={12} />}
    </button>
  );
}

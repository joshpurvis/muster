// Small building blocks for the calm dialog layout (docs/UI.md "Dialogs"): property pills in a
// row under the main fields, and a "More options" disclosure for settings few people change.
import type { ComponentChildren, Ref } from "preact";
import { IconChevron, IconChevronDown } from "../icons.tsx";
import { cx } from "../util.ts";

/** One entry of a `PillSelect`: `short` is what the closed pill shows (the list shows `label`). */
export interface PillOption {
  value: string;
  label: string;
  short?: string;
  disabled?: boolean;
  /** Options with the same group, side by side, go in one `<optgroup>`. */
  group?: string;
}

/** A native select drawn as a small pill: optional icon and key ("Model"), the chosen option's
 * short text, a chevron. The select itself lies transparent over the pill, so the pill is as
 * wide as its text (a select is as wide as its longest option), while clicks, keys, the option
 * list and screen readers are the platform's own. `label` is the accessible name; `tip` the
 * hover text (pass the same words in `describedBy` for keyboard users). */
export function PillSelect(
  {
    icon,
    k,
    label,
    value,
    options,
    onChange,
    disabled,
    tip,
    describedBy,
    selectRef,
    id,
    class: c,
    selectClass,
  }: {
    icon?: ComponentChildren;
    k?: string;
    label: string;
    value: string;
    options: PillOption[];
    onChange: (v: string) => void;
    disabled?: boolean;
    tip?: string;
    describedBy?: string;
    selectRef?: Ref<HTMLSelectElement>;
    id?: string;
    class?: string;
    selectClass?: string;
  },
) {
  const cur = options.find((o) => o.value === value);
  const groups: { label?: string; items: PillOption[] }[] = [];
  for (const o of options) {
    const last = groups[groups.length - 1];
    if (last && last.label === o.group) last.items.push(o);
    else groups.push({ label: o.group, items: [o] });
  }
  const item = (o: PillOption) => (
    <option key={o.value} value={o.value} disabled={o.disabled}>{o.label}</option>
  );
  return (
    <span class={cx("pill pill-select", disabled && "disabled", c)} data-tip={tip}>
      {icon}
      {k && <span class="pill-k" aria-hidden="true">{k}</span>}
      <span class="pill-value" aria-hidden="true">{cur ? cur.short ?? cur.label : value}</span>
      <IconChevronDown size={12} />
      <select
        ref={selectRef}
        id={id}
        class={cx("pill-native", selectClass)}
        value={value}
        disabled={disabled}
        aria-label={label}
        aria-describedby={describedBy}
        onChange={(e) => onChange(e.currentTarget.value)}
      >
        {groups.map((g) =>
          g.label === undefined
            ? g.items.map(item)
            : <optgroup key={g.label} label={g.label}>{g.items.map(item)}</optgroup>
        )}
      </select>
    </span>
  );
}

/** A pill that is on or off (`aria-pressed`). */
export function PillToggle(
  { icon, label, on, onChange, disabled, tip, describedBy }: {
    icon?: ComponentChildren;
    label: string;
    on: boolean;
    onChange: (on: boolean) => void;
    disabled?: boolean;
    tip?: string;
    describedBy?: string;
  },
) {
  return (
    <button
      type="button"
      class={cx("pill pill-toggle", on && "on")}
      aria-pressed={on}
      aria-describedby={describedBy}
      disabled={disabled}
      data-tip={tip}
      onClick={() => onChange(!on)}
    >
      {icon}
      {label}
    </button>
  );
}

/** "More options ›" with a one-line summary of what is inside, so a closed section still says
 * what it holds. */
export function MoreOptions(
  { open, onToggle, summary, label = "More options", children }: {
    open: boolean;
    onToggle: (open: boolean) => void;
    summary?: ComponentChildren;
    label?: string;
    children: ComponentChildren;
  },
) {
  return (
    <div class={cx("dlg-more-section", open && "open")}>
      <button type="button" class="dlg-more" aria-expanded={open} onClick={() => onToggle(!open)}>
        <IconChevron size={12} class="dlg-more-chev" />
        <span>{label}</span>
        {!open && summary && <span class="dlg-more-summary">{summary}</span>}
      </button>
      {open && <div class="dlg-more-body">{children}</div>}
    </div>
  );
}

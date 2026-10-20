// `board` app: a dashboard from plain JSON (no HTML, no script from the agent). Input checks
// here mirror src/panel_apps.rs `check_board`, with the same limits; unknown fields are refused
// in both. docs/MCP.md "board".
import { eventBytes, fits, MAX_EVENT, Problems, type Result } from "../shared/validate.ts";
import { color_, type ChartSpec, type Stat, validateChart, validateStat } from "../chart/logic.ts";
import { ICONS } from "../../kit/icons.ts";

export const KINDS = ["stats", "chart", "list", "meters", "callout", "actions"] as const;
export type Kind = typeof KINDS[number];
export const TONES = ["good", "warn", "bad", "info", "accent"] as const;
export type Tone = typeof TONES[number];
/** Event kinds an action may send (as the host's bridge accepts), minus Muster's own. */
export const ACTION_ID = /^[a-zA-Z][\w/-]{0,79}$/;
const RESERVED_IDS = ["closed", "overflow", "navigated"];
/** data-* names an action's data may use: lowercase, not the kit's own attributes. */
export const DATA_KEY = /^[a-z][a-z0-9_]{0,30}$/;
const RESERVED_DATA = ["send", "text", "confirm", "state", "sent", "busy", "key"];

export interface ListItem { title: string; sub?: string; icon?: string; tone?: Tone; badge?: string; bar?: number; value?: string | number; color?: string }
export interface MeterItem { label: string; value: number; max: number; text?: string; of?: string; mark?: number; markLabel?: string; tone?: Tone; color?: string }
export interface ActionItem { id: string; label: string; primary?: boolean; danger?: boolean; confirm?: string; icon?: string; key?: string; data?: Record<string, string | number | boolean> }
interface Common { span?: "full"; title?: string; aside?: string }
export type Block =
  | Common & { kind: "stats"; items: Stat[] }
  | Common & { kind: "chart"; spec: ChartSpec; height?: number }
  | Common & { kind: "list"; items: ListItem[] }
  | Common & { kind: "meters"; note?: string; items: MeterItem[] }
  | Common & { kind: "callout"; tone?: Exclude<Tone, "accent">; icon?: string; text: string }
  | Common & { kind: "actions"; items: ActionItem[] };
export interface Board { title: string; scope?: string; takeaway?: string; source?: string; min?: number; blocks: Block[] }

export const MAX_BLOCKS = 40;

/** The sentence the agent reads when an action is pressed. */
export const actionText = (label: string) => `Pressed "${label}"`;

function iconName(p: Problems, o: Record<string, unknown>, at: string): string | undefined {
  const v = p.string(o, "icon", at, { max: 20 });
  if (v === undefined) return undefined;
  if (!/^[a-zA-Z]{1,20}$/.test(v)) { p.add(`${at}.icon`, "must be an icon name such as check, alert or chart"); return undefined; }
  if (!Object.hasOwn(ICONS, v)) p.warnings.push(`${at}.icon: unknown icon ${JSON.stringify(v)}, drawn as "?"; known: ${Object.keys(ICONS).join(", ")}`);
  return v;
}

function numberIn(p: Problems, o: Record<string, unknown>, key: string, at: string, lo: number, hi: number, required = false): number | undefined {
  const v = p.number(o, key, at, required);
  if (v !== undefined && (v < lo || v > hi)) { p.add(at ? `${at}.${key}` : key, `must be from ${lo} to ${hi}`); return undefined; }
  return v;
}

function shortValue(p: Problems, o: Record<string, unknown>, at: string): string | number | undefined {
  const v = o.value;
  if (v === undefined || v === null) return undefined;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && fits(v, 40)) return v;
  p.add(`${at}.value`, "must be a number or a string of at most 40 characters");
  return undefined;
}

function block(p: Problems, raw: unknown, at: string, ids: Set<string>, keys: Set<string>): Block | undefined {
  const o = p.object(raw, at);
  if (!o) return undefined;
  const kind = p.oneOf(o, "kind", at, KINDS, true);
  if (o.span !== undefined && o.span !== null && o.span !== "full") p.add(`${at}.span`, 'must be "full" (or left out for one column)');
  const common: Common = { span: o.span === "full" ? "full" : undefined, title: p.string(o, "title", at, { max: 200 }), aside: p.string(o, "aside", at, { max: 100 }) };
  const base = ["kind", "span", "title", "aside"];
  const items = (max: number) => p.array(o, "items", at, { required: true, max, min: 1 }) ?? [];
  switch (kind) {
    case "stats": {
      p.only(o, at, [...base, "items"]);
      const list = items(8).map((s, i) => validateStat(p, s, `${at}.items[${i}]`, true)).filter((s): s is Stat => !!s);
      return { ...common, kind, items: list };
    }
    case "chart": {
      p.only(o, at, [...base, "spec", "height"]);
      const height = numberIn(p, o, "height", at, 80, 800);
      if (o.spec === undefined || o.spec === null) { p.add(`${at}.spec`, "is required (the chart app's spec)"); return undefined; }
      const r = validateChart(o.spec);
      if (!r.ok) { r.errors.forEach((e) => p.errors.push(`${at}.spec.${e}`)); return undefined; }
      r.warnings.forEach((w) => p.warnings.push(`${at}.spec.${w}`));
      return { ...common, kind, spec: r.value, height };
    }
    case "list": {
      p.only(o, at, [...base, "items"]);
      const list: ListItem[] = [];
      items(100).forEach((it, i) => {
        const ia = `${at}.items[${i}]`;
        const io = p.object(it, ia);
        if (!io) return;
        p.only(io, ia, ["title", "sub", "icon", "tone", "badge", "bar", "value", "color"]);
        const title = p.string(io, "title", ia, { required: true, max: 200 });
        const item: ListItem = {
          title: title ?? "", sub: p.string(io, "sub", ia, { max: 300 }), icon: iconName(p, io, ia), tone: p.oneOf(io, "tone", ia, TONES),
          badge: p.string(io, "badge", ia, { max: 40 }), bar: numberIn(p, io, "bar", ia, 0, 1), value: shortValue(p, io, ia), color: color_(p, io, ia),
        };
        if (title !== undefined) list.push(item);
      });
      return { ...common, kind, items: list };
    }
    case "meters": {
      p.only(o, at, [...base, "note", "items"]);
      const note = p.string(o, "note", at, { max: 300 });
      const list: MeterItem[] = [];
      items(20).forEach((it, i) => {
        const ia = `${at}.items[${i}]`;
        const io = p.object(it, ia);
        if (!io) return;
        p.only(io, ia, ["label", "value", "max", "text", "of", "mark", "markLabel", "tone", "color"]);
        const label = p.string(io, "label", ia, { required: true, max: 100 });
        const value = p.number(io, "value", ia, true);
        const max = p.number(io, "max", ia, true);
        if (max !== undefined && max <= 0) p.add(`${ia}.max`, "must be more than 0");
        const item = {
          text: p.string(io, "text", ia, { max: 40 }), of: p.string(io, "of", ia, { max: 60 }), mark: p.number(io, "mark", ia),
          markLabel: p.string(io, "markLabel", ia, { max: 100 }), tone: p.oneOf(io, "tone", ia, TONES), color: color_(p, io, ia),
        };
        if (label !== undefined && value !== undefined && max !== undefined && max > 0) list.push({ label, value, max, ...item });
      });
      return { ...common, kind, note, items: list };
    }
    case "callout": {
      p.only(o, at, [...base, "tone", "icon", "text"]);
      const text = p.string(o, "text", at, { required: true, max: 1000 });
      const tone = p.oneOf(o, "tone", at, ["good", "warn", "bad", "info"] as const);
      const ic = iconName(p, o, at);
      return text === undefined ? undefined : { ...common, kind, tone, icon: ic, text };
    }
    case "actions": {
      p.only(o, at, [...base, "items"]);
      const list: ActionItem[] = [];
      items(8).forEach((it, i) => {
        const ia = `${at}.items[${i}]`;
        const io = p.object(it, ia);
        if (!io) return;
        p.only(io, ia, ["id", "label", "primary", "danger", "confirm", "icon", "key", "data"]);
        const id = p.string(io, "id", ia, { required: true, max: 80 });
        if (id !== undefined && (!ACTION_ID.test(id) || RESERVED_IDS.includes(id))) p.add(`${ia}.id`, "must start with a letter and use letters, digits, _, - or / (at most 80), and not be closed, overflow or navigated");
        else if (id !== undefined && ids.has(id)) p.add(`${ia}.id`, `${JSON.stringify(id)} is used twice`);
        if (id !== undefined) ids.add(id);
        const label = p.string(io, "label", ia, { required: true, max: 60 });
        const key = p.string(io, "key", ia, { max: 1 });
        if (key !== undefined && !/^[a-zA-Z0-9]$/.test(key)) p.add(`${ia}.key`, "must be one letter or digit");
        else if (key !== undefined && keys.has(key.toLowerCase())) p.add(`${ia}.key`, `${JSON.stringify(key)} is used twice`);
        if (key !== undefined) keys.add(key.toLowerCase());
        let data: ActionItem["data"];
        if (io.data !== undefined && io.data !== null) {
          const d = p.object(io.data, `${ia}.data`);
          if (d) {
            const entries = Object.entries(d);
            if (entries.length > 10) p.add(`${ia}.data`, "has more than 10 fields");
            data = {};
            for (const [k, v] of entries) {
              if (!DATA_KEY.test(k) || RESERVED_DATA.includes(k)) p.add(`${ia}.data.${k}`, "names must be lowercase letters, digits or _ (at most 31), and not one of the kit's own");
              else if (!(typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v)) || (typeof v === "string" && fits(v, 500)))) p.add(`${ia}.data.${k}`, "must be a string (at most 500 characters), number or true/false");
              else data[k] = v;
            }
          }
        }
        const item: ActionItem = {
          id: id ?? "", label: label ?? "", primary: p.boolean(io, "primary", ia), danger: p.boolean(io, "danger", ia),
          confirm: p.string(io, "confirm", ia, { max: 200 }), icon: iconName(p, io, ia), key, data,
        };
        if (id !== undefined && label !== undefined) {
          const size = eventBytes({ kind: id, text: actionText(label), data: data ?? {} });
          if (size > MAX_EVENT) p.add(ia, `would send an event of ${size} bytes; at most 16 KiB (shorten its data)`);
          list.push(item);
        }
      });
      return { ...common, kind, items: list };
    }
    default: return undefined;
  }
}

export function validateBoard(data: unknown): Result<Board> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.only(o, "", ["title", "scope", "takeaway", "source", "min", "blocks"]);
  const title = p.string(o, "title", "", { required: true, max: 200 });
  const scope = p.string(o, "scope", "", { max: 200 });
  const takeaway = p.string(o, "takeaway", "", { max: 600 });
  const source = p.string(o, "source", "", { max: 300 });
  const min = numberIn(p, o, "min", "", 160, 1200);
  const ids = new Set<string>(), keys = new Set<string>();
  const blocks = (p.array(o, "blocks", "", { required: true, max: MAX_BLOCKS, min: 1 }) ?? [])
    .map((b, i) => block(p, b, `blocks[${i}]`, ids, keys)).filter((b): b is Block => !!b);
  return p.result({ title: title ?? "", scope, takeaway, source, min, blocks });
}

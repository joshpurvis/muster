// Input checks for built-in panel apps: collect every problem with its path, so the panel can
// show the agent's mistakes all at once instead of a blank screen.

/** Length in Unicode code points, the way Muster's backend counts (Rust `chars().count()`), so
 * text the backend accepted is never refused here. UTF-16 `.length` is never smaller. */
export function chars(v: string): number {
  if (v.length < 2) return v.length;
  let n = 0;
  for (const _ of v) n++;
  return n;
}

/** Whether `v` is at most `max` code points (cheap when its UTF-16 length already fits). */
export const fits = (v: string, max: number) => v.length <= max || chars(v) <= max;

export type Result<T> = { ok: true; value: T; warnings: string[] } | { ok: false; errors: string[] };

export class Problems {
  errors: string[] = [];
  warnings: string[] = [];

  add(path: string, text: string) { this.errors.push(`${path}: ${text}`); }

  object(v: unknown, path: string): Record<string, unknown> | undefined {
    if (v && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
    this.add(path, "must be an object");
    return undefined;
  }

  /** Optional unless `required`; returns undefined when absent or wrong. */
  string(o: Record<string, unknown>, key: string, path: string, opt: { required?: boolean; max?: number; nonEmpty?: boolean } = {}): string | undefined {
    const v = o[key];
    const at = path ? `${path}.${key}` : key;
    if (v === undefined || v === null) {
      if (opt.required) this.add(at, "is required");
      return undefined;
    }
    if (typeof v !== "string") { this.add(at, "must be a string"); return undefined; }
    if ((opt.nonEmpty ?? opt.required) && !v.trim()) { this.add(at, "must not be empty"); return undefined; }
    const max = opt.max ?? 10_000;
    if (!fits(v, max)) { this.add(at, `is longer than ${max} characters`); return undefined; }
    return v;
  }

  oneOf<T extends string>(o: Record<string, unknown>, key: string, path: string, options: readonly T[], required = false): T | undefined {
    const v = o[key];
    const at = path ? `${path}.${key}` : key;
    if (v === undefined || v === null) {
      if (required) this.add(at, `is required (one of ${options.join(", ")})`);
      return undefined;
    }
    if (typeof v !== "string" || !options.includes(v as T)) {
      this.add(at, `must be one of ${options.join(", ")} (got ${JSON.stringify(v)?.slice(0, 40)})`);
      return undefined;
    }
    return v as T;
  }

  number(o: Record<string, unknown>, key: string, path: string, required = false): number | undefined {
    const v = o[key];
    const at = path ? `${path}.${key}` : key;
    if (v === undefined || v === null) {
      if (required) this.add(at, "is required");
      return undefined;
    }
    if (typeof v !== "number" || !Number.isFinite(v)) { this.add(at, "must be a finite number"); return undefined; }
    return v;
  }

  boolean(o: Record<string, unknown>, key: string, path: string): boolean | undefined {
    const v = o[key];
    if (v === undefined || v === null) return undefined;
    if (typeof v !== "boolean") { this.add(path ? `${path}.${key}` : key, "must be true or false"); return undefined; }
    return v;
  }

  array(o: Record<string, unknown>, key: string, path: string, opt: { required?: boolean; max: number; min?: number }): unknown[] | undefined {
    const v = o[key];
    const at = path ? `${path}.${key}` : key;
    if (v === undefined || v === null) {
      if (opt.required) this.add(at, "is required");
      return undefined;
    }
    if (!Array.isArray(v)) { this.add(at, "must be an array"); return undefined; }
    if (v.length > opt.max) { this.add(at, `has ${v.length} items; at most ${opt.max}`); return undefined; }
    if (opt.min && v.length < opt.min) { this.add(at, `needs at least ${opt.min} item${opt.min === 1 ? "" : "s"}`); return undefined; }
    return v;
  }

  /** Unknown keys are warnings, not errors: a typo is reported without hiding the panel. */
  known(o: Record<string, unknown>, path: string, keys: readonly string[]) {
    for (const k of Object.keys(o)) {
      if (!keys.includes(k)) this.warnings.push(`${path ? path + "." : ""}${k}: unknown field, ignored`);
    }
  }

  /** Unknown keys are errors: for inputs whose backend check refuses them too (the board). */
  only(o: Record<string, unknown>, path: string, keys: readonly string[]) {
    for (const k of Object.keys(o)) {
      if (!keys.includes(k)) this.add(`${path ? path + "." : ""}${k}`, `unknown field (it takes ${keys.join(", ")})`);
    }
  }

  result<T>(value: T): Result<T> {
    return this.errors.length ? { ok: false, errors: this.errors } : { ok: true, value, warnings: this.warnings };
  }
}

/** An https URL without credentials, or undefined. */
export function httpsUrl(v: string): string | undefined {
  try {
    const u = new URL(v);
    if (u.protocol !== "https:" || u.username || u.password || !u.hostname) return undefined;
    return u.href;
  } catch {
    return undefined;
  }
}

/** Muster refuses panel events over 16 KiB of JSON (src/panels.rs `MAX_EVENT`). */
export const MAX_EVENT = 16 * 1024;

/** UTF-8 size of the `ui/message` params an event becomes: what the backend measures. */
export function eventBytes(event: { kind: string; text: string; data: object }): number {
  const params = { role: "user", content: { type: "text", text: event.text }, _meta: { muster: { kind: event.kind, data: event.data } } };
  return new TextEncoder().encode(JSON.stringify(params)).length;
}

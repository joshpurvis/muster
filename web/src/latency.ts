// Typing latency monitor: the pure half (matching keys to stages, stats, report text). The live
// half, which takes the timestamps and draws the overlay, is latency_monitor.ts.

/** A key counts as "first after a pause" when the terminal saw no keydown for this long. */
export const IDLE_MS = 1000;

/** A key whose output never shows up (a password prompt) is dropped after this. */
export const STALE_MS = 3000;

/** One keystroke typed into a terminal. Times are performance.now() ms; NaN until reached. */
export interface KeySample {
  sid: number;
  /** Since the previous keydown in this terminal; Infinity for its first key. */
  idle: number;
  /** keydown reaches our listener after the event's own timeStamp; null when that is unusable. */
  queue: number | null;
  /** keydown (capturing on the terminal's element, so before xterm's own listener). */
  t0: number;
  /** The input bytes are handed to the WebSocket. */
  t1: number;
  /** The first output frame for this session after t1: usually the echo, but output is not
   * matched to keys, so other output or one frame echoing several keys counts too. */
  t2: number;
  /** xterm has parsed that frame (write callback). */
  t3: number;
  /** xterm's next render of this terminal after t3. */
  t4: number;
  /** The next animation frame after t4. Not a measured presentation time: the paint happens
   * somewhere between t4 and t5. */
  t5: number;
  /** Largest gap between animation frames in the second before t0; null when not probed. */
  frameGap: number | null;
  webgl: boolean;
}

export interface Deltas {
  ui: number;
  rtt: number;
  parse: number;
  render: number;
  paint: number;
  total: number;
}

export function deltas(s: KeySample): Deltas {
  return {
    ui: s.t1 - s.t0,
    rtt: s.t2 - s.t1,
    parse: s.t3 - s.t2,
    render: s.t4 - s.t3,
    paint: s.t5 - s.t4,
    total: s.t5 - s.t0,
  };
}

export function isIdleKey(s: KeySample): boolean {
  return s.idle >= IDLE_MS;
}

export function median(xs: number[]): number {
  if (!xs.length) return NaN;
  const v = [...xs].sort((a, b) => a - b);
  const m = v.length >> 1;
  return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
}

export interface Summary {
  n: number;
  median: number;
  max: number;
}

function summary(xs: number[]): Summary {
  return { n: xs.length, median: median(xs), max: xs.length ? Math.max(...xs) : NaN };
}

/** Totals of first-after-pause keys against keys typed in a burst. */
export function summarize(samples: KeySample[]): { idle: Summary; burst: Summary } {
  const idle: number[] = [];
  const burst: number[] = [];
  for (const s of samples) (isIdleKey(s) ? idle : burst).push(s.t5 - s.t0);
  return { idle: summary(idle), burst: summary(burst) };
}

/** Milliseconds for a column: one decimal under 100, whole above; "-" when unknown. */
export function fmtMs(v: number | null): string {
  if (v === null || !Number.isFinite(v)) return "-";
  return v < 100 ? v.toFixed(1) : String(Math.round(v));
}

/** Browser family from a user agent: the native window is WebKitGTK. */
export function uaFamily(ua: string): string {
  if (/Firefox\//.test(ua)) return "Firefox";
  if (/Chrom(e|ium)\//.test(ua)) return "Chromium";
  if (/AppleWebKit\//.test(ua)) return /Linux|X11/.test(ua) ? "WebKitGTK" : "WebKit";
  return "other";
}

const COLS = ["idle", "evt", "ui", "rtt", "parse", "rend", "paint", "total"];
const REPORT_COLS = [...COLS, "gap", "gl"];

function cells(s: KeySample, full: boolean): string[] {
  const d = deltas(s);
  const idle = Number.isFinite(s.idle) ? (s.idle >= 10000 ? ">10s" : fmtMs(s.idle)) : "first";
  const c = [idle, fmtMs(s.queue), fmtMs(d.ui), fmtMs(d.rtt), fmtMs(d.parse), fmtMs(d.render)];
  c.push(fmtMs(d.paint), fmtMs(d.total));
  if (full) c.push(fmtMs(s.frameGap), s.webgl ? "y" : "n");
  return c;
}

function table(rows: string[][]): string {
  return rows.map((r) => r.map((c) => c.padStart(6)).join("")).join("\n");
}

/** The overlay's rows (newest last). */
export function formatTable(samples: KeySample[], full = false): string {
  return table([full ? REPORT_COLS : COLS, ...samples.map((s) => cells(s, full))]);
}

export function formatSummary(samples: KeySample[]): string {
  const { idle, burst } = summarize(samples);
  const line = (name: string, s: Summary) =>
    `${name.padEnd(16)}n=${s.n} median ${fmtMs(s.median)} ms, max ${fmtMs(s.max)} ms`;
  return `${line(`after ≥${IDLE_MS / 1000}s idle:`, idle)}\n${line("in a burst:", burst)}`;
}

/** The plain-text report the Copy button puts on the clipboard. */
export function formatReport(env: string, samples: KeySample[], last = 50): string {
  return [
    "Muster typing latency",
    env,
    "",
    formatSummary(samples),
    "",
    "ms: idle = since the previous key, evt = keydown event to our listener, ui = to WebSocket,",
    "rtt = to the first output frame, parse = xterm parse, rend = to xterm's render, paint = to",
    "the next animation frame, total = keydown to the next frame after the first output (output",
    "is not matched to keys, and the real paint is somewhere before that frame), gap = largest",
    "frame gap in the second before the key, gl = WebGL renderer.",
    formatTable(samples.slice(-last), true),
  ].join("\n");
}

/** Matches each key to the stages after it, per session. Pure: callers pass the clock readings
 * (latency_monitor.ts feeds it from the live hooks). Each stage takes the first event after the
 * previous one; keys older than STALE_MS are dropped at every stage, never counted. */
export class KeyTracker {
  /** Bumped by reset() and clear(): callbacks scheduled before then are ignored. */
  gen = 0;
  done: KeySample[] = [];
  readonly pending = new Map<number, KeySample[]>();
  private lastKey = new Map<number, number>();

  constructor(private keep = 200) {}

  /** This session's keys still in flight at t; stale ones are dropped. */
  private live(sid: number, t: number): KeySample[] {
    const q = (this.pending.get(sid) ?? []).filter((s) => t - s.t0 <= STALE_MS);
    if (q.length) this.pending.set(sid, q);
    else this.pending.delete(sid);
    return q;
  }

  private inFlight(s: KeySample, t: number): boolean {
    return t - s.t0 <= STALE_MS && !!this.pending.get(s.sid)?.includes(s);
  }

  keydown(sid: number, t0: number, k: { queue: number | null; frameGap: number | null; webgl: boolean }) {
    // A key that never sent bytes was a shortcut (Ctrl+Shift+C…): drop it.
    const q = this.live(sid, t0).filter((s) => !Number.isNaN(s.t1));
    const prev = this.lastKey.get(sid);
    this.lastKey.set(sid, t0);
    q.push({
      sid,
      idle: prev === undefined ? Infinity : t0 - prev,
      queue: k.queue,
      t0,
      t1: NaN,
      t2: NaN,
      t3: NaN,
      t4: NaN,
      t5: NaN,
      frameGap: k.frameGap,
      webgl: k.webgl,
    });
    this.pending.set(sid, q);
  }

  /** A user's bytes left: they belong to the newest key, if it sent none yet. */
  sent(sid: number, t: number) {
    const q = this.live(sid, t);
    const s = q[q.length - 1];
    if (s && Number.isNaN(s.t1)) s.t1 = t;
  }

  /** An output frame arrived: the keys it is the first output for (pass them to parsed()). */
  output(sid: number, t: number): KeySample[] {
    const waiting = this.live(sid, t).filter((s) => !Number.isNaN(s.t1) && Number.isNaN(s.t2));
    for (const s of waiting) s.t2 = t;
    return waiting;
  }

  parsed(gen: number, keys: KeySample[], t: number) {
    if (gen !== this.gen) return;
    for (const s of keys) if (this.inFlight(s, t)) s.t3 = t;
  }

  /** The terminal rendered: the keys this is the first render for (pass them to painted()). */
  render(sid: number, t: number): KeySample[] {
    const rendered = this.live(sid, t).filter((s) => !Number.isNaN(s.t3) && Number.isNaN(s.t4));
    for (const s of rendered) s.t4 = t;
    return rendered;
  }

  /** The next frame: these keys are done. Returns whether any still counted. */
  painted(gen: number, sid: number, keys: KeySample[], t: number): boolean {
    if (gen !== this.gen) return false;
    const q = this.live(sid, t);
    const finished = keys.filter((s) => q.includes(s));
    if (!finished.length) return false;
    for (const s of finished) s.t5 = t;
    const left = q.filter((s) => !finished.includes(s));
    if (left.length) this.pending.set(sid, left);
    else this.pending.delete(sid);
    this.done.push(...finished);
    if (this.done.length > this.keep) this.done = this.done.slice(-this.keep);
    return true;
  }

  /** The terminal is gone. */
  dispose(sid: number) {
    this.pending.delete(sid);
    this.lastKey.delete(sid);
  }

  /** Forget keys in flight and the idle baseline; callbacks already scheduled become no-ops. */
  reset() {
    this.gen++;
    this.pending.clear();
    this.lastKey.clear();
  }

  /** reset(), and forget the finished keys too. */
  clear() {
    this.reset();
    this.done = [];
  }
}

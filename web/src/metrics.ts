// Content-free local samples. Build stamping replaces this literal in the finished bundle.
export const UI_BUILD = "muster-ui-build-placeholder";
export type Sample = { name: string; value: number };
export class SampleBuffer {
  samples: Sample[] = [];
  dropped = 0;
  add(name: string, value: number) {
    if (!Number.isFinite(value) || value < 0 || value > 1e12) return;
    if (this.samples.length >= 255) this.dropped++;
    else this.samples.push({ name, value });
  }
  take(): Sample[] {
    const samples = this.samples;
    if (this.dropped) samples.push({ name: "ui.samples_dropped", value: this.dropped });
    this.samples = [];
    this.dropped = 0;
    return samples;
  }
}

const buffer = new SampleBuffer();
let enabled = false;
let timer = 0;
let busy = false;
const listeners = new Set<(enabled: boolean) => void>();
const samplers = new Set<() => void>();
export function metricsEnabled() {
  return enabled;
}
export function onMetricsEnabled(fn: (enabled: boolean) => void) {
  listeners.add(fn);
}
export function onMetricsFlush(fn: () => void) {
  samplers.add(fn);
}
export function measure(name: string, value: number) {
  if (enabled) buffer.add(name, value);
}
function setEnabled(value: boolean) {
  if (enabled === value) return;
  enabled = value;
  if (!enabled) buffer.take();
  for (const fn of listeners) fn(enabled);
}

export function configureMetrics(
  value: boolean,
  send: (body: unknown) => Promise<{ enabled: boolean }> | null,
) {
  setEnabled(value);
  if (timer) return;
  const ua = navigator.userAgent;
  const renderer = /Chrom(e|ium)/.test(ua)
    ? "chromium"
    : /Firefox/.test(ua)
    ? "firefox"
    : /AppleWebKit/.test(ua)
    ? "webkit"
    : "other";
  // This heartbeat also learns about enable/disable without reloading a window. No rAF loop.
  timer = setInterval(async () => {
    if (busy) return;
    if (enabled) { for (const sample of samplers) sample(); }
    const samples = buffer.take();
    const pending = send({ build: UI_BUILD, renderer, samples });
    if (!pending) {
      buffer.dropped += samples.length;
      return;
    }
    busy = true;
    try {
      setEnabled((await pending).enabled);
    } catch {
      buffer.dropped += samples.length;
    } finally {
      busy = false;
    }
  }, 15000);
}

// Typing latency monitor (palette: "Typing latency monitor"): times each key typed into a
// terminal, from keydown to the next animation frame after the first output that follows it
// (see KeyTracker in latency.ts), and shows the last few in a small overlay.
// Off by default and not kept across reloads. While off nothing is installed: no listeners, no
// animation frame loop (terms.ts only checks for a null probe).

import { formatReport, formatSummary, formatTable, KeyTracker, uaFamily } from "./latency.ts";
import { type LatencyProbe, setLatencyProbe } from "./terms.ts";
import { isNative } from "./native.ts";
import { toast } from "./state.ts";
import { copyText } from "./util.ts";

const SHOWN = 10;
const KEPT = 200;
/** Frame times kept for the frame-gap probe. */
const FRAME_WINDOW_MS = 1000;
const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock", "Super", "Hyper"]);

let on = false;
let overlay: HTMLDivElement | null = null;
let body: HTMLPreElement | null = null;
let framesBtn: HTMLButtonElement | null = null;

// Frame-gap probe: a rAF loop recording frame times. It keeps the page producing frames, which
// may itself hide a slow first frame after idle, so the overlay can turn it off to compare.
let framesOn = true;
let frameReq = 0;
const frames: number[] = [];

function frameLoop() {
  const t = performance.now();
  frames.push(t);
  let i = 0;
  while (i < frames.length - 1 && frames[i] < t - FRAME_WINDOW_MS - 100) i++;
  if (i) frames.splice(0, i);
  frameReq = requestAnimationFrame(frameLoop);
}

function startFrames() {
  if (!frameReq) frameReq = requestAnimationFrame(frameLoop);
}

function stopFrames() {
  cancelAnimationFrame(frameReq);
  frameReq = 0;
  frames.length = 0;
}

/** Largest gap between frames in the second before t, counting the time since the last one. */
function frameGap(t: number): number | null {
  if (!frameReq || !frames.length) return null;
  let max = t - frames[frames.length - 1];
  for (let i = frames.length - 1; i > 0 && frames[i] > t - FRAME_WINDOW_MS; i--) {
    max = Math.max(max, frames[i] - frames[i - 1]);
  }
  return max;
}

const tracker = new KeyTracker(KEPT);
/** Pending next-frame callbacks of rendered keys, cancelled when the monitor turns off. */
const paintReqs = new Set<number>();

const probe: LatencyProbe = {
  keydown(sid, ev, webgl) {
    if (MODIFIERS.has(ev.key) || ev.isComposing) return;
    const t0 = performance.now();
    // timeStamp is on performance.now()'s clock; a platform that fills it otherwise is ignored.
    const lag = t0 - ev.timeStamp;
    tracker.keydown(sid, t0, { queue: lag >= 0 && lag < 10000 ? lag : null, frameGap: frameGap(t0), webgl });
  },
  sent(sid) {
    tracker.sent(sid, performance.now());
  },
  output(sid) {
    if (!tracker.pending.has(sid)) return undefined;
    const waiting = tracker.output(sid, performance.now());
    if (!waiting.length) return undefined;
    const gen = tracker.gen;
    return () => tracker.parsed(gen, waiting, performance.now());
  },
  render(sid) {
    if (!tracker.pending.has(sid)) return;
    const rendered = tracker.render(sid, performance.now());
    if (!rendered.length) return;
    const gen = tracker.gen;
    const req = requestAnimationFrame(() => {
      paintReqs.delete(req);
      if (tracker.painted(gen, sid, rendered, performance.now())) draw();
    });
    paintReqs.add(req);
  },
  dispose(sid) {
    tracker.dispose(sid);
  },
};

function env(): string {
  const ua = navigator.userAgent;
  return [
    `${uaFamily(ua)}${isNative ? " (native window)" : ""}`,
    `devicePixelRatio ${devicePixelRatio}`,
    `frame-gap probe ${frameReq ? "on" : "off"}`,
    ua,
  ].join(" · ");
}

function draw() {
  if (!body) return;
  body.textContent = `${formatTable(tracker.done.slice(-SHOWN))}\n${formatSummary(tracker.done)}`;
}

function button(label: string, title: string, run: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.title = title;
  // Keep the terminal focused: the next key typed should still be measured.
  b.addEventListener("mousedown", (ev) => ev.preventDefault());
  b.addEventListener("click", run);
  return b;
}

function copyReport() {
  // copyText's fallback copies through a hidden textarea, which takes the focus: give it back to
  // the terminal (as its own Ctrl+Shift+C does), unless the user has moved on meanwhile.
  const prev = document.activeElement as HTMLElement | null;
  copyText(formatReport(env(), tracker.done)).then((ok) => {
    const now = document.activeElement;
    if (prev?.isConnected && (!now || now === document.body)) prev.focus();
    if (ok) toast("success", "Latency report copied", 1800);
    else toast("error", "Copy failed: the clipboard isn't available here");
  });
}

function framesLabel() {
  if (framesBtn) framesBtn.textContent = `Frames: ${framesOn ? "on" : "off"}`;
}

function mount() {
  overlay = document.createElement("div");
  overlay.className = "latmon";
  overlay.setAttribute("aria-label", "Typing latency monitor");
  const head = document.createElement("div");
  head.className = "latmon-head";
  const title = document.createElement("span");
  title.textContent = "Typing latency (ms)";
  framesBtn = button(
    "",
    "Frame-gap probe: a constant animation frame loop. It may hide a slow first frame after idle; turn it off to compare.",
    () => {
      framesOn = !framesOn;
      if (framesOn) startFrames();
      else stopFrames();
      framesLabel();
    },
  );
  framesLabel();
  head.append(
    title,
    framesBtn,
    button("Copy", "Copy a plain-text report", copyReport),
    button("Clear", "Forget the measured keys and those still in flight", () => {
      tracker.clear();
      draw();
    }),
    button("×", "Turn the monitor off", () => setLatencyMonitor(false)),
  );
  body = document.createElement("pre");
  body.className = "latmon-body";
  overlay.append(head, body);
  document.body.appendChild(overlay);
  draw();
}

export function latencyMonitorOn(): boolean {
  return on;
}

export function setLatencyMonitor(v: boolean) {
  if (v === on) return;
  on = v;
  if (on) {
    mount();
    if (framesOn) startFrames();
    setLatencyProbe(probe);
    return;
  }
  setLatencyProbe(null);
  stopFrames();
  for (const req of paintReqs) cancelAnimationFrame(req);
  paintReqs.clear();
  tracker.reset();
  overlay?.remove();
  overlay = body = framesBtn = null;
}

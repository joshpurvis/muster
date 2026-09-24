import { Terminal } from "@xterm/xterm";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { FRAME_OUTPUT, FRAME_REPLAY } from "../rpc.ts";
import type { SessionInfo } from "../types.ts";
import { bufferText, type Cell, FONT_FAMILY, phoneGrid, scrollPosition } from "./logic.ts";

export function cellSize(fontSize: number, term?: Terminal): Cell {
  const cell = (term as any)?._core?._renderService?.dimensions?.css?.cell;
  if (cell?.width > 0 && cell?.height > 0) return cell;
  const ctx = document.createElement("canvas").getContext("2d");
  if (ctx) ctx.font = `${fontSize}px ${FONT_FAMILY}`;
  return { width: ctx?.measureText("W").width || fontSize * 0.6, height: Math.ceil(fontSize * 1.2) * 1.15 };
}

function theme() {
  const css = getComputedStyle(document.documentElement);
  const color = (key: string) => css.getPropertyValue(key).trim();
  const result: Record<string, string> = {
    background: color("--term-bg"),
    foreground: color("--term-fg"),
    cursor: color("--accent"),
    selectionBackground: color("--term-selection"),
  };
  const ansi = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
  ];
  ansi.forEach((name, i) => result[name] = color(`--ansi-${i}`));
  return result;
}

export class PhoneTerminal {
  readonly term: Terminal;
  private queue: Uint8Array[] = [];
  private raf = 0;
  private momentum = 0;
  private panX = 0;
  private panY = 0;
  private scrollRemainder = 0;
  private disposed = false;
  private selecting = false;
  private pinned = true;
  private observer: ResizeObserver;
  private media = matchMedia("(prefers-color-scheme: light)");
  private events = new AbortController();
  private onTheme = () => this.term.options.theme = theme();

  constructor(
    readonly sid: number,
    private host: HTMLElement,
    private surface: HTMLElement,
    info: SessionInfo,
    fontSize: number,
    private closedHeight: () => number,
    private onBottom: (bottom: boolean) => void,
    private onFont: (size: number) => void,
  ) {
    this.term = new Terminal({
      cols: info.cols,
      rows: info.rows,
      allowProposedApi: true,
      disableStdin: true,
      fontFamily: FONT_FAMILY,
      fontSize,
      lineHeight: 1.15,
      scrollback: 10000,
      cursorBlink: false,
      drawBoldTextInBrightColors: false,
      theme: theme(),
    });
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";
    this.term.open(host);
    const textarea = this.term.textarea!;
    textarea.readOnly = true;
    textarea.setAttribute("inputmode", "none");
    textarea.tabIndex = -1;
    this.term.attachCustomKeyEventHandler(() => false);
    this.term.onScroll(() => this.render());
    this.observer = new ResizeObserver(() => {
      if (this.selecting) return;
      if (this.pinned) {
        this.bottom();
        return;
      }
      this.panY = Math.min(this.panY, this.maxPan());
      this.render();
    });
    this.observer.observe(surface);
    this.media.addEventListener("change", this.onTheme);
    this.installTouch();
    this.render();
  }

  private cell() {
    return cellSize(this.term.options.fontSize!, this.term);
  }
  private maxPan() {
    return Math.max(0, this.term.rows * this.cell().height - this.surface.clientHeight);
  }
  private atBottom() {
    const b = this.term.buffer.active;
    return b.viewportY >= b.baseY && this.panY >= this.maxPan() - 1;
  }
  private position() {
    return this.term.buffer.active.viewportY * this.cell().height + this.panY + this.scrollRemainder;
  }
  private scrollTo(position: number) {
    const b = this.term.buffer.active;
    const h = this.cell().height;
    const target = scrollPosition(position, b.baseY, h, this.maxPan());
    this.panY = target.pan;
    this.scrollRemainder = target.remainder;
    this.term.scrollLines(target.row - b.viewportY);
    this.render();
  }
  private render() {
    if (this.disposed) return;
    const cell = this.cell();
    this.panX = Math.max(0, Math.min(this.panX, this.term.cols * cell.width - this.surface.clientWidth));
    this.host.style.width = `${Math.ceil(this.term.cols * cell.width)}px`;
    this.host.style.transform = `translate(${-this.panX}px, ${-this.panY}px)`;
    this.pinned = this.atBottom();
    this.onBottom(this.pinned);
  }
  bottom() {
    this.scrollTo(Infinity);
  }
  grid() {
    return phoneGrid(this.surface.clientWidth, this.closedHeight(), this.cell());
  }
  update(info: SessionInfo) {
    if (info.cols === this.term.cols && info.rows === this.term.rows) return;
    const pinned = this.atBottom();
    this.term.resize(info.cols, info.rows);
    if (pinned) this.bottom();
    else this.render();
  }
  fontSize(size: number) {
    if (size === this.term.options.fontSize) return;
    const pinned = this.atBottom();
    this.term.options.fontSize = size;
    if (pinned) this.bottom();
    else this.render();
  }
  select(active: boolean) {
    this.selecting = active;
    cancelAnimationFrame(this.momentum);
  }
  snapshot() {
    const buffer = this.term.buffer.active;
    const all = bufferText(buffer);
    const first = Math.min(buffer.length - 1, buffer.viewportY + Math.floor(this.panY / this.cell().height));
    const last = first + Math.ceil(this.surface.clientHeight / this.cell().height);
    return {
      ...all,
      offset: all.offsets[first] ?? 0,
      screen: bufferText(buffer, first, last).text,
      cell: this.cell(),
    };
  }
  receive(type: number, payload: Uint8Array) {
    if (type === FRAME_REPLAY) {
      cancelAnimationFrame(this.raf);
      this.raf = 0;
      this.queue = [];
      this.term.write("\x1bc");
      this.term.write(payload.slice(), () => {
        if (!this.disposed) this.bottom();
      });
    } else if (type === FRAME_OUTPUT) {
      this.queue.push(payload.slice());
      if (!this.raf) this.raf = requestAnimationFrame(() => this.flush());
    }
  }
  private flush() {
    this.raf = 0;
    if (this.disposed || !this.queue.length) return;
    const pinned = this.atBottom();
    const position = this.position();
    const data = new Uint8Array(this.queue.reduce((n, p) => n + p.length, 0));
    let offset = 0;
    for (const part of this.queue) {
      data.set(part, offset);
      offset += part.length;
    }
    this.queue = [];
    this.term.write(data, () => {
      if (!this.disposed) this.scrollTo(pinned ? Infinity : position);
    });
  }

  private installTouch() {
    const points = new Map<number, { x: number; y: number }>();
    let last = { x: 0, y: 0 }, axis = "", velocity = 0, time = 0, distance = 0, font = 11;
    const spacing = () => {
      const [a, b] = [...points.values()];
      return a && b ? Math.hypot(a.x - b.x, a.y - b.y) : 0;
    };
    const options = { signal: this.events.signal };
    this.surface.addEventListener("pointerdown", (e) => {
      if (this.selecting) return;
      e.preventDefault();
      cancelAnimationFrame(this.momentum);
      this.surface.setPointerCapture(e.pointerId);
      points.set(e.pointerId, { x: e.clientX, y: e.clientY });
      last = { x: e.clientX, y: e.clientY };
      axis = "";
      velocity = 0;
      time = e.timeStamp;
      if (points.size === 2) {
        distance = spacing();
        font = this.term.options.fontSize!;
      }
    }, options);
    this.surface.addEventListener("pointermove", (e) => {
      if (!points.has(e.pointerId)) return;
      const point = { x: e.clientX, y: e.clientY };
      points.set(e.pointerId, point);
      if (points.size >= 2) {
        this.onFont(Math.max(7, Math.min(20, Math.round(font * spacing() / Math.max(1, distance) * 2) / 2)));
      } else {
        const dx = last.x - point.x, dy = last.y - point.y;
        if (!axis && Math.hypot(dx, dy) >= 2) axis = Math.abs(dx) > Math.abs(dy) ? "x" : "y";
        if (axis === "x") {
          this.panX += dx;
          this.render();
        } else if (axis === "y") {
          this.scrollTo(this.position() + dy);
          velocity = dy / Math.max(8, e.timeStamp - time);
        }
      }
      last = point;
      time = e.timeStamp;
    }, options);
    const end = (e: PointerEvent) => {
      points.delete(e.pointerId);
      if (points.size) {
        last = [...points.values()][0];
        axis = "";
        velocity = 0;
        return;
      }
      if (e.type === "pointercancel" || axis !== "y" || e.timeStamp - time > 100) return;
      let previous = performance.now();
      const step = (now: number) => {
        const dt = Math.min(32, now - previous);
        previous = now;
        this.scrollTo(this.position() + velocity * dt);
        velocity *= Math.pow(0.94, dt / 16);
        if (Math.abs(velocity) > 0.02) this.momentum = requestAnimationFrame(step);
      };
      this.momentum = requestAnimationFrame(step);
    };
    this.surface.addEventListener("pointerup", end, options);
    this.surface.addEventListener("pointercancel", end, options);
    this.surface.addEventListener("wheel", (e) => {
      e.preventDefault();
      this.scrollTo(this.position() + e.deltaY);
    }, { ...options, passive: false });
  }
  dispose() {
    this.disposed = true;
    cancelAnimationFrame(this.raf);
    cancelAnimationFrame(this.momentum);
    this.events.abort();
    this.observer.disconnect();
    this.media.removeEventListener("change", this.onTheme);
    this.term.dispose();
  }
}

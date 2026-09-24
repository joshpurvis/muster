import { needsHuman, type SessionInfo } from "../types.ts";

export const FONT_FAMILY =
  '"JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Menlo, ui-monospace, monospace';
export const COLLAPSED_HEIGHT = 68;
export interface Grid {
  cols: number;
  rows: number;
}
export interface Cell {
  width: number;
  height: number;
}

export function normalizeCode(code: string): string {
  const clean = code.replace(/[\s-]/g, "");
  return clean.length === 64 ? clean.toLowerCase() : clean.toUpperCase();
}

export function sessionOrder(a: SessionInfo, b: SessionInfo): number {
  return Number(needsHuman(b.state)) - Number(needsHuman(a.state)) || b.state_since - a.state_since ||
    a.id - b.id;
}

export function phoneGrid(width: number, closedHeight: number, cell: Cell): Grid {
  return {
    cols: Math.max(2, Math.min(500, Math.floor(width / Math.max(1, cell.width)))),
    rows: Math.max(
      1,
      Math.min(300, Math.floor((closedHeight - COLLAPSED_HEIGHT) / Math.max(1, cell.height))),
    ),
  };
}

export function scrollPosition(position: number, base: number, height: number, overflow: number) {
  const y = Math.max(0, Math.min(position, base * height + overflow));
  const row = Math.min(base, Math.floor(y / height));
  const pan = Math.min(overflow, y - row * height);
  return { row, pan, remainder: y - row * height - pan };
}

export interface BufferLine {
  isWrapped: boolean;
  translateToString(trimRight?: boolean): string;
}
export interface TextBuffer {
  length: number;
  getLine(index: number): BufferLine | undefined;
}

/** Keep an offset per physical row so selection opens at the current scroll position. */
export function bufferText(buffer: TextBuffer, start = 0, end = buffer.length) {
  let text = "";
  const offsets: number[] = [];
  for (let i = start; i < Math.min(end, buffer.length); i++) {
    const line = buffer.getLine(i);
    if (i > start && !line?.isWrapped) text += "\n";
    offsets.push(text.length);
    text += line?.translateToString(!buffer.getLine(i + 1)?.isWrapped) ?? "";
  }
  return { text, offsets };
}

export function relativeTime(since: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

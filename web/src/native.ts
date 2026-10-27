// The native window (src/gui.rs) and what the page may ask of its window. The native window's
// init script sets `__musterNative.ipc` to the version of the `window.ipc` messages it
// understands (`parse_ipc`); windows opened by older builds, and browsers, have none.

const g = globalThis as unknown as {
  __musterNative?: { ipc?: number; secret?: string };
  ipc?: { postMessage?: (body: string) => void };
};

/** This page runs in a native window that takes messages. */
export const isNative = (g.__musterNative?.ipc ?? 0) >= 1 && typeof g.ipc?.postMessage === "function";

function post(msg: Record<string, unknown>): boolean {
  if (!isNative) return false;
  try {
    g.ipc!.postMessage!(JSON.stringify({ ...msg, secret: g.__musterNative?.secret }));
    return true;
  } catch {
    return false;
  }
}

/** Tell the native window which Muster window this page shows, so that reloading a frozen page
 * from the native side (src/gui.rs, its watchdog) brings the same window back. */
export function tellNativeWindow(id: number) {
  if ((g.__musterNative?.ipc ?? 0) >= 3) post({ window: id });
}

/** Show or hide the native window's system title bar. */
export function setNativeDecorations(on: boolean) {
  post({ decorations: on });
}

/** Can "Close window" work here? The native window always; a browser lets a page close only a
 * window a script opened or one with a single history entry (Muster's `--app` windows). */
export function canCloseWindow(): boolean {
  return isNative || !!opener || history.length <= 1;
}

/** Close this window (only this one; its tabs stay in their window for a later `muster`).
 * Resolves false when the browser refused. */
export function closeWindow(): Promise<boolean> {
  if (post({ close: true })) return Promise.resolve(true);
  close();
  // window.close() doesn't say whether it worked; a page that is still here wasn't closed.
  return new Promise((resolve) => setTimeout(() => resolve(closed), 300));
}

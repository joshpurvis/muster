/** The server embeds the very same frame-src as the response header. Keep this snapshot for
 * the page's lifetime: a later hello or registry change cannot widen the loaded CSP. */
export function framePolicy(content: string) {
  const sources = content.split("; ").find((part) => part.startsWith("frame-src "))?.slice(10).split(" ") ??
    [];
  const panel = sources.find((s) => /^http:\/\/127\.0\.0\.1:[0-9]+\/proxy$/.test(s));
  const addons = new Set(
    sources.filter((s) => /^https?:\/\/(localhost|127\.0\.0\.1):[0-9]+$/.test(s))
      .map((s) => new URL(s).origin),
  );
  return { panel: panel ? new URL(panel).origin : "", addons };
}
export const loadedFramePolicy = framePolicy(
  typeof document === "undefined"
    ? ""
    : document.querySelector<HTMLMetaElement>('meta[name="muster-frame-policy"]')?.content ?? "",
);
/** Reuse the window's note-saving guard, including its existing prompt when storage fails. */
export function refreshFramePolicy(loaded: string, current: string | undefined, reload: () => void): boolean {
  if (!loaded || !current || loaded === current) return false;
  reload();
  return true;
}

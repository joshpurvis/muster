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
/** Offer a manual reload when the panel server moves outside this page's fixed CSP.
 * Never reload on reconnect: Questions answers and Memory edits may exist only in this page. */
export function needsFramePolicyReload(loaded: string, current: string | undefined): boolean {
  return !!loaded && !!current && loaded !== current;
}

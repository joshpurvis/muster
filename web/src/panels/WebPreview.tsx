// The web_preview panel (docs/MCP.md "Built-in apps"): an agent's local dev server, live. Not an
// MCP App: the host draws the bar (address, Reload, Open in browser) and frames the panel
// server's /preview page, whose CSP holds the inner frame to the dev server's origin
// (src/web_preview.rs). Cross-origin frames say little about failures, so the backend's
// check (`panel.preview_check`) and a load timeout drive the notices (preview.ts).
import { useEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { helloEpoch } from "../state.ts";
import { focusGroup } from "../actions.ts";
import { IconGlobe, IconRestart } from "../icons.tsx";
import type { Panel, PanelResource } from "../types.ts";
import { panelOriginUrl, panelReloads, usePanelPlacement } from "./placement.ts";
import {
  isPreviewUrl, type Notice, type PreviewCheck, previewFrameSrc, previewNotice, RETRY_FOR_MS, RETRY_MS, SLOW_MS, SLOW_NOTICE,
} from "./preview.ts";

/** Sandboxed, but a real page: its own origin, scripts, forms and popups (which open in the
 * system browser in the native window). Never top navigation. Matches web/panels/preview.html. */
const SANDBOX = "allow-scripts allow-same-origin allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-downloads";

export function WebPreview({ panel }: { panel: Panel }) {
  const iframe = useRef<HTMLIFrameElement | null>(null);
  const { ref, paneId, split, dim } = usePanelPlacement(panel.id, iframe);
  const origin = panelOriginUrl();
  // The address, and the panel revision it came with: every panel_update loads the page again
  // (even at the same address, for servers that do not reload by themselves).
  const [shown, setShown] = useState<{ url: string | null; revision: number }>({ url: null, revision: 0 });
  const url = shown.url;
  const [error, setError] = useState("");
  // Bumped by Reload (and by the server coming up): a new frame loads the page afresh.
  const [load, setLoad] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [slow, setSlow] = useState(false);
  const [check, setCheck] = useState<PreviewCheck | null>(null);
  const [retrying, setRetrying] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const reloads = panelReloads.value.get(panel.id) ?? 0;
  const epoch = helloEpoch.value;
  const frameKey = `${url}#${shown.revision}#${load}#${reloads}`;

  useEffect(() => {
    let live = true;
    rpc.call<PanelResource>("panel.resource", { id: panel.id }).then((r) => {
      if (!live) return;
      // The backend accepts only these; refuse anything else here too rather than frame it.
      const next = isPreviewUrl(r.data.url) ? r.data.url : null;
      setError(next ? "" : "This preview's address is not a local dev server, so Muster will not show it here.");
      setShown((old) => old.url === next && old.revision === r.panel.revision ? old : { url: next, revision: r.panel.revision });
    }).catch((e) => { if (live) setError(`Could not load the panel: ${String(e?.message ?? e)}`); });
    return () => { live = false; };
  }, [panel.id, panel.revision, reloads, epoch]);

  // Each new frame: a load timeout, and the backend's check, repeated while nothing answers.
  useEffect(() => {
    if (!url || !origin) return;
    let live = true;
    setLoaded(false);
    setSlow(false);
    setCheck(null);
    setDismissed(false);
    setRetrying(false);
    const slowTimer = setTimeout(() => { if (live) setSlow(true); }, SLOW_MS);
    const until = Date.now() + RETRY_FOR_MS;
    let retry = 0;
    let wasDown = false;
    const run = () => {
      rpc.call<PreviewCheck>("panel.preview_check", { id: panel.id }).then((c) => {
        if (!live) return;
        if (c.reachable && wasDown) {
          // The server came up: load the page now rather than leave a browser error showing.
          setLoad((n) => n + 1);
          return;
        }
        wasDown = !c.reachable;
        const again = wasDown && Date.now() < until;
        setRetrying(again);
        setCheck(c);
        if (again) retry = setTimeout(run, RETRY_MS);
      }).catch(() => { /* An older backend, or the panel closed: the frame alone it is. */ });
    };
    run();
    return () => {
      live = false;
      clearTimeout(slowTimer);
      clearTimeout(retry);
    };
  }, [frameKey, origin]);

  useEffect(() => {
    // Focus moving into the cross-origin frame blurs this window; select the panel's group.
    const blur = () => {
      if (iframe.current && document.activeElement === iframe.current && paneId.current) focusGroup(paneId.current);
    };
    addEventListener("blur", blur);
    return () => removeEventListener("blur", blur);
  }, []);

  const reload = () => setLoad((n) => n + 1);
  const openInBrowser = () => { if (url) globalThis.open(url, "_blank", "noopener,noreferrer"); };
  const notice: Notice | null = (dismissed ? null : previewNotice(check, retrying)) ?? (slow && !loaded ? SLOW_NOTICE : null);
  const problem = !origin ? "The backend did not provide a separate panel origin." : error;

  return (
    <div
      ref={ref}
      class={dim ? "panel-view dim" : split ? "panel-view ring" : "panel-view"}
      data-panel-view={panel.id}
      data-web-preview=""
      onPointerDown={() => { if (paneId.current) focusGroup(paneId.current); }}
    >
      <div class="preview-bar">
        <span class="preview-kind">Web preview</span>
        <span class="preview-url" title={url ?? ""}>{url ?? ""}</span>
        <button type="button" class="btn btn-sm" onClick={reload} disabled={!url} title="Load the page again">
          <IconRestart size={13} /> Reload
        </button>
        <button type="button" class="btn btn-sm" onClick={openInBrowser} disabled={!url} title="Open this address in your web browser">
          <IconGlobe size={13} /> Open in browser
        </button>
      </div>
      {notice && !notice.blocking && !problem && (
        <div class="preview-hint" role="status" data-preview-notice={notice.kind}>
          <span><strong>{notice.title}</strong> {notice.detail}</span>
        </div>
      )}
      <div class="panel-body">
        <div class="panel-frame-host">
          {url && origin && (
            <iframe
              key={frameKey}
              ref={iframe}
              class="panel-frame preview-frame"
              title={panel.title}
              data-panel-id={panel.id}
              name={`muster-panel-${panel.id}`}
              sandbox={SANDBOX}
              allow="clipboard-write; fullscreen"
              referrerPolicy="no-referrer"
              src={previewFrameSrc(origin, url)}
              onLoad={() => setLoaded(true)}
              onFocus={() => { if (paneId.current) focusGroup(paneId.current); }}
            />
          )}
        </div>
        {problem
          ? (
            <div class="panel-status" role="alert">
              <p>{problem}</p>
            </div>
          )
          : notice?.blocking && (
            <div class="panel-status" role="alert" data-preview-notice={notice.kind}>
              <p><strong>{notice.title}</strong></p>
              <p class="panel-status-detail">{notice.detail}</p>
              <p class="panel-status-detail preview-status-url">{url}</p>
              <div class="preview-actions">
                <button type="button" class="btn" onClick={reload}>Reload</button>
                <button type="button" class="btn" onClick={openInBrowser}>Open in browser</button>
                {notice.kind !== "down" && <button type="button" class="btn" onClick={() => setDismissed(true)}>Show the page anyway</button>}
              </div>
            </div>
          )}
      </div>
    </div>
  );
}

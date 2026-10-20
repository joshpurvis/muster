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
  isPreviewUrl, type Notice, type PreviewCheck, previewFrameSrc, previewNotice, previewRevision, SLOW_MS, SLOW_NOTICE, startChecks,
} from "./preview.ts";

/** Sandboxed, but a real page: its own origin, scripts, forms and popups (which open in the
 * system browser in the native window). Never top navigation. Matches web/panels/preview.html. */
const SANDBOX = "allow-scripts allow-same-origin allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-downloads";

export function WebPreview({ panel }: { panel: Panel }) {
  const iframe = useRef<HTMLIFrameElement | null>(null);
  const { ref, paneId, split, dim } = usePanelPlacement(panel.id, iframe);
  const origin = panelOriginUrl();
  // The address, and the data revision it came with: each data update loads the page again
  // (even at the same address, for servers that do not reload by themselves); a title-only
  // update does not.
  const [shown, setShown] = useState<{ url: string | null; revision: number }>({ url: null, revision: 0 });
  const url = shown.url;
  const [error, setError] = useState("");
  // Bumped by Reload (and by the server coming up): a new frame loads the page afresh.
  const [load, setLoad] = useState(0);
  const reloads = panelReloads.value.get(panel.id) ?? 0;
  const epoch = helloEpoch.value;
  const frameKey = `${url}#${shown.revision}#${load}#${reloads}`;
  // Per-frame state is stamped with the frame it belongs to, so a new frame starts clean
  // without a reset that could land after its own load event.
  const [loadedKey, setLoadedKey] = useState("");
  const [slowKey, setSlowKey] = useState("");
  const [dismissedKey, setDismissedKey] = useState("");
  const [checked, setChecked] = useState<{ key: string; check: PreviewCheck | null; retrying: boolean }>({ key: "", check: null, retrying: false });
  const current = checked.key === frameKey ? checked : { check: null, retrying: false };

  useEffect(() => {
    let live = true;
    rpc.call<PanelResource>("panel.resource", { id: panel.id }).then((r) => {
      if (!live) return;
      // The backend accepts only these; refuse anything else here too rather than frame it.
      const next = isPreviewUrl(r.data.url) ? r.data.url : null;
      const revision = previewRevision(r.panel);
      setError(next ? "" : "This preview's address is not a local dev server, so Muster will not show it here.");
      setShown((old) => old.url === next && old.revision === revision ? old : { url: next, revision });
    }).catch((e) => { if (live) setError(`Could not load the panel: ${String(e?.message ?? e)}`); });
    return () => { live = false; };
  }, [panel.id, panel.revision, reloads, epoch]);

  // Each new frame: a load timeout, and the backend's check, repeated while nothing answers.
  useEffect(() => {
    if (!url || !origin) return;
    const key = frameKey;
    const slowTimer = setTimeout(() => setSlowKey(key), SLOW_MS);
    const stop = startChecks({
      check: () => rpc.call<PreviewCheck>("panel.preview_check", { id: panel.id }),
      report: (check, retrying) => setChecked({ key, check, retrying }),
      // The server came up: load the page now rather than leave a browser error showing.
      up: () => setLoad((n) => n + 1),
    });
    return () => {
      clearTimeout(slowTimer);
      stop();
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
  const loaded = loadedKey === frameKey;
  const notice: Notice | null = (dismissedKey === frameKey ? null : previewNotice(current.check, current.retrying))
    ?? (slowKey === frameKey && !loaded ? SLOW_NOTICE : null);
  const problem = !origin ? "The backend did not provide a separate panel origin." : error;

  return (
    <div
      ref={ref}
      class={dim ? "panel-view dim" : split ? "panel-view ring" : "panel-view"}
      data-panel-view={panel.id}
      data-web-preview=""
      onPointerDown={() => { if (paneId.current) focusGroup(paneId.current); }}
      // Tabbing onto the bar's buttons selects this panel's group, like a click, so group
      // commands (Alt+W) act on it.
      onFocusCapture={() => { if (paneId.current) focusGroup(paneId.current); }}
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
              onLoad={() => setLoadedKey(frameKey)}
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
                {notice.kind !== "down" && <button type="button" class="btn" onClick={() => setDismissedKey(frameKey)}>Hide this notice</button>}
              </div>
            </div>
          )}
      </div>
    </div>
  );
}

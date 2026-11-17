import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import {
  type Addon,
  ADDON_SANDBOX,
  addonLabels,
  addonMessage,
  addonMessageError,
  addonTitle,
  externalLink,
  THEME_TOKENS,
} from "../addons.ts";
import { addonTab } from "../layout/tree.ts";
import { focusGroup, reloadWindow } from "../actions.ts";
import { loadedFramePolicy } from "../frame_policy.ts";
import { focusedTabId } from "../state.ts";
import { rpc } from "../rpc.ts";
import { usePanelPlacement } from "./placement.ts";

export function AddonView({ addon }: { addon: Addon }) {
  const frame = useRef<HTMLIFrameElement>(null);
  const tab = addonTab(addon.id);
  const [visible, setVisible] = useState(false);
  const { ref, paneId, dim } = usePanelPlacement(tab, frame, tab, setVisible);
  const focused = focusedTabId.value === tab && visible;
  const origin = new URL(addon.url).origin;
  const lastLink = useRef(-Infinity);
  const post = (type: string, fields = {}) =>
    frame.current?.contentWindow?.postMessage({ muster: 1, type, ...fields }, origin);
  const theme = () => {
    const root = document.documentElement, css = getComputedStyle(root);
    post("theme", {
      mode: root.dataset.theme === "light" ? "light" : "dark",
      tokens: Object.fromEntries(THEME_TOKENS.map((k) => [k, css.getPropertyValue(k).trim()])),
    });
  };
  useLayoutEffect(() => {
    const receive = async (e: MessageEvent) => {
      const m = addonMessage(e, frame.current?.contentWindow ?? null, origin);
      if (!m) return;
      if (m.type === "badge" || m.type === "title") {
        addonLabels.value = {
          ...addonLabels.value,
          [addon.id]: {
            ...addonLabels.value[addon.id],
            ...(m.type === "badge" ? { count: m.count } : { title: m.text }),
          },
        };
      } else if (m.type === "open-link") {
        if (performance.now() - lastLink.current < 2000) return;
        lastLink.current = performance.now();
        globalThis.open(externalLink(m.url)!, "_blank", "noopener,noreferrer");
      } else {
        try {
          await rpc.call("addon.message", { id: addon.id, origin, agent: m.agent, text: m.text });
          post("message-agent-result", { ok: true });
        } catch (e) {
          post("message-agent-result", { ok: false, error: addonMessageError(e) });
        }
      }
    };
    const focus = () => {
      if (document.activeElement === frame.current && paneId.current) focusGroup(paneId.current);
    };
    const observer = new MutationObserver(theme);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme", "style"],
    });
    addEventListener("message", receive);
    addEventListener("blur", focus);
    return () => {
      observer.disconnect();
      removeEventListener("message", receive);
      removeEventListener("blur", focus);
    };
  }, [addon.id, origin]);
  useEffect(() => {
    post("visible", { visible });
  }, [visible]);
  useEffect(() => {
    if (focused) post("focus");
  }, [focused]);
  return (
    <div ref={ref} class={`panel-view${dim ? " dim" : ""}`}>
      <div class="panel-body">
        <div class="panel-frame-host">
          {!loadedFramePolicy.addons.has(origin)
            ? (
              <div class="panel-status" role="status">
                <p>Reload the window to open this add-on.</p>
                <button type="button" class="btn" onClick={reloadWindow}>Reload</button>
              </div>
            )
            : (
              <iframe
                ref={frame}
                class="panel-frame"
                data-addon-id={addon.id}
                data-addon-tab={tab}
                title={addonTitle(addon)}
                src={addon.url}
                sandbox={ADDON_SANDBOX}
                referrerPolicy="no-referrer"
                onLoad={() => {
                  theme();
                  post("visible", { visible });
                  if (focused) post("focus");
                }}
              />
            )}
        </div>
      </div>
    </div>
  );
}

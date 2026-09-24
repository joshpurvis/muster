import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { SessionInfo } from "../types.ts";
import { load, save } from "../util.ts";
import { act, canInteract, errorText, notify, ready } from "./store.ts";
import { Approval, Icon } from "./ui.tsx";

const KEYS = [
  ["Esc", "\x1b"],
  ["⇧Tab", "\x1b[Z"],
  ["Tab", "\t"],
  ["↑", "\x1b[A"],
  ["↓", "\x1b[B"],
  ["←", "\x1b[D"],
  ["→", "\x1b[C"],
  ["Enter", "\r"],
  ["^C", "\x03"],
  ["1", "1"],
  ["2", "2"],
  ["3", "3"],
  ["⌫", "\x7f"],
];

export function Composer(
  { session: s, onHeight }: { session: SessionInfo; onHeight: (height: number) => void },
) {
  const [draft, setDraft] = useState(() => load<string>(`m.draft.${s.id}`, ""));
  const [collapsed, setCollapsed] = useState(false);
  const [options, setOptions] = useState(false);
  const [sending, setSending] = useState(false);
  const card = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const timer = useRef(0);
  const held = useRef(false);
  const reason = !ready.value
    ? "Disconnected — reconnecting…"
    : s.state === "exited"
    ? "Session exited"
    : !canInteract.value
    ? "This device has view access only"
    : "";
  const disabled = !!reason;
  useLayoutEffect(() => {
    const ro = new ResizeObserver(() => onHeight(card.current?.getBoundingClientRect().height ?? 0));
    if (card.current) ro.observe(card.current);
    return () => ro.disconnect();
  }, [onHeight]);
  useLayoutEffect(() => {
    if (!input.current) return;
    input.current.style.height = "auto";
    input.current.style.height = `${Math.min(144, input.current.scrollHeight)}px`;
  }, [draft, collapsed]);
  useEffect(() => () => clearTimeout(timer.current), []);
  const send = async (mode?: "interrupt") => {
    if (disabled || sending || !draft.trim()) return;
    const text = draft;
    setSending(true);
    setOptions(false);
    try {
      const result = await act<{ delivered: boolean; queued: number }>(s, "session.send", { text, mode });
      setDraft((current) => {
        const next = current === text ? "" : current;
        save(`m.draft.${s.id}`, next);
        return next;
      });
      notify(result.delivered ? "Sent" : `Queued (${result.queued})`);
    } catch (e) {
      notify(errorText(e));
    } finally {
      setSending(false);
    }
  };
  const key = (data: string) =>
    act(s, "session.input", { data, keys: true }).catch((e) => notify(errorText(e)));
  return (
    <div class={`composer-wrap ${collapsed ? "collapsed" : ""}`} ref={card}>
      {collapsed
        ? (
          <button
            type="button"
            class="compose-expand primary"
            aria-label="Expand composer"
            onClick={() => {
              setCollapsed(false);
              requestAnimationFrame(() => input.current?.focus());
            }}
          >
            <Icon name="compose" />
          </button>
        )
        : (
          <section class="composer" aria-label="Message composer">
            <div class="composer-handle">
              <span>
                {s.queued > 0
                  ? `${s.queued} queued`
                  : s.state === "needs_approval"
                  ? "Approval needed"
                  : s.state === "asking_question"
                  ? "Your answer"
                  : "Message"}
              </span>
              <button
                type="button"
                class="collapse-button"
                aria-label="Minimise composer"
                onClick={() => {
                  input.current?.blur();
                  setCollapsed(true);
                }}
              >
                <Icon name="down" />
              </button>
            </div>
            {(s.state === "needs_approval" || s.state === "asking_question") && (
              <div class="approval-prompt">
                <p>{s.state_detail || "The agent is waiting for you."}</p>
                {s.state === "needs_approval"
                  ? <Approval session={s} />
                  : <small>Use 1 / 2 / 3, then Enter to answer.</small>}
              </div>
            )}
            <div class="key-chips" aria-label="Terminal keys">
              {KEYS.map(([label, data]) => (
                <button
                  type="button"
                  key={label}
                  disabled={disabled}
                  onPointerDown={(e) => e.preventDefault()}
                  onClick={() => key(data)}
                >
                  {label}
                </button>
              ))}
              <button
                type="button"
                class="stop"
                disabled={disabled}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => act(s, "session.interrupt").catch((e) => notify(errorText(e)))}
              >
                Stop
              </button>
            </div>
            {options && (
              <button
                type="button"
                class="interrupt-send"
                disabled={disabled || sending || !draft.trim()}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => send("interrupt")}
              >
                Interrupt &amp; send
              </button>
            )}
            <div class="input-row">
              <textarea
                ref={input}
                value={draft}
                rows={1}
                placeholder={s.kind === "shell" ? "Command…" : `Message ${s.name}…`}
                aria-label={s.kind === "shell" ? "Command" : `Message ${s.name}`}
                readOnly={s.state === "exited"}
                autoCapitalize="sentences"
                autoCorrect="on"
                spellcheck
                onInput={(e) => {
                  setDraft(e.currentTarget.value);
                  save(`m.draft.${s.id}`, e.currentTarget.value);
                }}
              />
              <button
                type="button"
                class="icon-button send-options"
                aria-label="Send options"
                aria-expanded={options}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => setOptions(!options)}
              >
                <Icon name="more" />
              </button>
              <button
                type="button"
                class="send-button primary"
                aria-label="Send message"
                disabled={disabled || sending || !draft.trim()}
                onPointerDown={(e) => {
                  e.preventDefault();
                  held.current = false;
                  timer.current = setTimeout(() => {
                    held.current = true;
                    setOptions(true);
                  }, 500);
                }}
                onPointerUp={() => clearTimeout(timer.current)}
                onPointerCancel={() => clearTimeout(timer.current)}
                onPointerLeave={() => clearTimeout(timer.current)}
                onClick={() => {
                  if (!held.current) send();
                }}
              >
                <Icon name="send" />
              </button>
            </div>
            {reason && <p class="composer-disabled">{reason}</p>}
          </section>
        )}
    </div>
  );
}

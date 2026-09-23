// Folder path input with `fs.complete` suggestions (Tab completes, ↑↓ pick). Used by the New
// tab dialog and the empty window.
import { useEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { IconFolder } from "../icons.tsx";
import { cx } from "../util.ts";

export function FolderInput(
  { value, onChange, autoFocus, inputRef, placeholder }: {
    value: string;
    onChange: (v: string) => void;
    autoFocus?: boolean;
    inputRef?: { current: HTMLInputElement | null };
    placeholder?: string;
  },
) {
  const [sugg, setSugg] = useState<string[]>([]);
  const [sel, setSel] = useState(-1);
  const [touched, setTouched] = useState(false);
  const own = useRef<HTMLInputElement>(null);
  const ref = inputRef ?? own;
  const seq = useRef(0);

  useEffect(() => {
    if (autoFocus) {
      ref.current?.focus();
      ref.current?.select();
    }
  }, []);

  // Suggest only once the user types (the prefilled folder needs none).
  useEffect(() => {
    if (!touched) return;
    const prefix = value || "~/";
    const my = ++seq.current;
    const t = setTimeout(() => {
      rpc.call<{ entries: string[] }>("fs.complete", { prefix })
        .then((r) => {
          if (my !== seq.current) return;
          setSugg(r.entries.slice(0, 50));
          setSel(-1);
        })
        .catch(() => my === seq.current && setSugg([]));
    }, 90);
    return () => clearTimeout(t);
  }, [value, touched]);

  const complete = (v: string) => {
    onChange(v.endsWith("/") ? v : v + "/");
    setTouched(true);
    ref.current?.focus();
  };

  // Common prefix of suggestions for Tab completion.
  const common = (() => {
    if (!sugg.length) return "";
    let p = sugg[0];
    for (const s of sugg) while (!s.startsWith(p)) p = p.slice(0, -1);
    return p;
  })();

  const showList = touched && sugg.length > 0 &&
    !(sugg.length === 1 && sugg[0].replace(/\/$/, "") === value.replace(/\/$/, ""));

  return (
    <div class="path-box">
      <input
        ref={ref}
        value={value}
        placeholder={placeholder ?? "~/projects/my-repo"}
        spellcheck={false}
        autoComplete="off"
        aria-label="Folder"
        onInput={(e) => {
          setTouched(true);
          onChange((e.target as HTMLInputElement).value);
        }}
        onKeyDown={(e) => {
          if (e.key === "Tab" && !e.shiftKey && !e.ctrlKey && !e.altKey) {
            if (!touched) setTouched(true);
            if (sel >= 0 && sugg[sel]) complete(sugg[sel]);
            else if (sugg.length === 1) complete(sugg[0]);
            else if (common && common.length > value.length) onChange(common);
            else if (sugg.length) setSel(0);
            else return; // nothing to complete: Tab moves on
            e.preventDefault();
          } else if (e.key === "ArrowDown" && showList) {
            e.preventDefault();
            setSel((sel + 1) % sugg.length);
          } else if (e.key === "ArrowUp" && showList) {
            e.preventDefault();
            setSel((sel - 1 + sugg.length) % sugg.length);
          } else if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && showList && sel >= 0 && sugg[sel]) {
            e.preventDefault();
            e.stopPropagation();
            complete(sugg[sel]);
          } else if (e.key === "Escape" && showList && sel >= 0) {
            e.preventDefault();
            e.stopPropagation();
            setSel(-1);
          }
        }}
      />
      {showList && (
        <div class="path-sugg" role="listbox">
          {sugg.map((s, i) => (
            <div
              key={s}
              role="option"
              aria-selected={i === sel}
              class={cx("path-item", i === sel && "sel")}
              onMouseDown={(e) => {
                e.preventDefault();
                complete(s);
              }}
            >
              <IconFolder size={13} />
              <span>{s}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

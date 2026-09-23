// Custom context / dropdown menus: keyboard navigable, submenus, never native.
import { signal } from "@preact/signals";
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { IconChevron } from "../icons.tsx";
import { keyLabel } from "../keys.ts";
import { cx } from "../util.ts";

export type MenuItem =
  | {
    label: string;
    icon?: ComponentChildren;
    keys?: string;
    hint?: string;
    danger?: boolean;
    disabled?: boolean;
    checked?: boolean;
    /** Single key that activates the item while the menu is open. */
    accel?: string;
    run?: () => void;
    submenu?: MenuItem[];
  }
  | "sep"
  | { header: string };

interface MenuState {
  x: number;
  y: number;
  items: MenuItem[];
  /** Align right edge to x (for buttons at the right of the screen). */
  alignRight?: boolean;
  onClose?: () => void;
  minWidth?: number;
  /** Item index to preselect (keyboard-opened menus). */
  select?: number;
  /** Top of the anchor element: the menu flips above it when there's no room below. */
  anchorTop?: number;
}

export const menu = signal<MenuState | null>(null);

export function openMenu(state: MenuState) {
  menu.value = state;
}

/** Open a menu anchored under an element. */
export function openMenuAt(el: Element, items: MenuItem[], opts: Partial<MenuState> = {}) {
  const r = el.getBoundingClientRect();
  const alignRight = opts.alignRight ?? r.left > innerWidth * 0.6;
  menu.value = {
    x: alignRight ? r.right : r.left,
    y: r.bottom + 4,
    anchorTop: r.top,
    items,
    alignRight,
    ...opts,
  };
}

export function closeMenu() {
  const m = menu.value;
  menu.value = null;
  m?.onClose?.();
}

function isAction(it: MenuItem): it is Exclude<MenuItem, "sep" | { header: string }> {
  return it !== "sep" && !("header" in it);
}

function MenuList(
  { items, x, y, alignRight, depth, onDone, minWidth, autoFocus, select, anchorTop }: {
    items: MenuItem[];
    x: number;
    y: number;
    alignRight?: boolean;
    depth: number;
    onDone: () => void;
    minWidth?: number;
    autoFocus?: boolean;
    select?: number;
    anchorTop?: number;
  },
) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x, y, ready: false });
  const [sel, setSel] = useState(-1);
  const [sub, setSub] = useState<{ index: number; x: number; y: number; kb: boolean } | null>(
    null,
  );

  useLayoutEffect(() => {
    const el = ref.current!;
    const r = el.getBoundingClientRect();
    let nx = alignRight ? x - r.width : x;
    let ny = y;
    if (nx + r.width > innerWidth - 6) nx = innerWidth - r.width - 6;
    if (nx < 6) nx = 6;
    if (ny + r.height > innerHeight - 6) {
      ny = anchorTop !== undefined && anchorTop - r.height - 4 >= 6
        ? anchorTop - r.height - 4
        : Math.max(6, innerHeight - r.height - 6);
    }
    setPos({ x: nx, y: ny, ready: true });
    if (autoFocus) setSel(items.findIndex((it) => isAction(it) && !it.disabled));
    if (select !== undefined) setSel(select);
  }, [x, y]);

  // Focus once visible (visibility:hidden elements can't take focus).
  useLayoutEffect(() => {
    if (pos.ready && (depth === 0 || autoFocus)) ref.current?.focus();
  }, [pos.ready]);

  const actionable = items.map((it, i) => (isAction(it) && !it.disabled ? i : -1)).filter((i) => i >= 0);

  const openSub = (i: number, kb = false) => {
    const row = ref.current?.querySelector(`[data-i="${i}"]`);
    if (!row) return;
    const r = row.getBoundingClientRect();
    setSub({ index: i, x: r.right - 2, y: r.top - 5, kb });
  };

  const activate = (i: number) => {
    const it = items[i];
    if (!isAction(it) || it.disabled) return;
    if (it.submenu) {
      openSub(i, true);
      return;
    }
    onDone();
    it.run?.();
  };

  const onKey = (e: KeyboardEvent) => {
    // Keys reach this list only while it has focus, so an open submenu here was opened by
    // hover and isn't focused. Enter / → move into it; anything else closes it and is handled.
    if (sub) {
      if (e.key === "ArrowRight" || e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        e.stopPropagation();
        openSub(sub.index, true);
        return;
      }
      setSub(null);
      if (e.key === "ArrowLeft" || e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        if (e.key === "ArrowLeft" || depth > 0) return;
      }
    }
    const pos = actionable.indexOf(sel);
    if (e.key === "ArrowDown") {
      setSel(actionable[(pos + 1) % actionable.length]);
    } else if (e.key === "ArrowUp") {
      setSel(actionable[(pos - 1 + actionable.length) % actionable.length]);
    } else if (e.key === "Enter" || e.key === " ") {
      if (sel >= 0) activate(sel);
    } else if (e.key === "ArrowRight") {
      const it = items[sel];
      if (sel >= 0 && isAction(it) && it.submenu) openSub(sel, true);
    } else if (e.key === "ArrowLeft" && depth > 0) {
      return; // bubbles to SubMenu, which returns to the parent list
    } else if (e.key === "Escape") {
      onDone();
    } else if (e.key === "Tab") {
      onDone();
    } else {
      // Type-ahead: first item starting with the letter.
      const k = e.key.toLowerCase();
      if (k.length !== 1) return;
      const acc = items.findIndex((it) => isAction(it) && !it.disabled && it.accel === k);
      if (acc >= 0) {
        e.preventDefault();
        e.stopPropagation();
        activate(acc);
        return;
      }
      const hit = actionable.find((i) => {
        const it = items[i];
        return isAction(it) && it.label.toLowerCase().startsWith(k);
      });
      if (hit !== undefined) setSel(hit);
      else return;
    }
    e.preventDefault();
    e.stopPropagation();
  };

  return (
    <>
      <div
        ref={ref}
        class="menu"
        role="menu"
        tabIndex={-1}
        onKeyDown={onKey}
        style={{
          left: pos.x + "px",
          top: pos.y + "px",
          visibility: pos.ready ? "visible" : "hidden",
          minWidth: minWidth ? minWidth + "px" : undefined,
        }}
        onMouseLeave={() => !sub && setSel(-1)}
      >
        {items.map((it, i) => {
          if (it === "sep") return <div key={i} class="menu-sep" />;
          if ("header" in it) return <div key={i} class="menu-header">{it.header}</div>;
          return (
            <div
              key={i}
              data-i={i}
              role="menuitem"
              aria-disabled={it.disabled}
              class={cx(
                "menu-item",
                i === sel && "sel",
                it.danger && "danger",
                it.disabled && "disabled",
                sub?.index === i && "open",
              )}
              onMouseEnter={() => {
                // Keyboard follows the pointer: the list under the mouse takes focus.
                if (!ref.current?.contains(document.activeElement)) ref.current?.focus();
                setSel(i);
                if (it.submenu) openSub(i);
                else setSub(null);
              }}
              onClick={(e) => {
                e.stopPropagation();
                activate(i);
              }}
            >
              <span class="menu-icon">{it.checked ? "✓" : it.icon}</span>
              <span class="menu-label">{it.label}</span>
              {it.hint && <span class="menu-hint">{it.hint}</span>}
              {it.accel && !it.keys && (
                <span class="menu-keys">
                  <kbd>{it.accel.toUpperCase()}</kbd>
                </span>
              )}
              {it.keys && (
                <span class="menu-keys">
                  {keyLabel(it.keys).map((k, j) => <kbd key={j}>{k}</kbd>)}
                </span>
              )}
              {it.submenu && <IconChevron size={13} class="menu-sub" />}
            </div>
          );
        })}
      </div>
      {sub && (() => {
        const it = items[sub.index];
        if (!isAction(it) || !it.submenu) return null;
        return (
          <SubMenu
            key={`${sub.index}:${sub.kb}`}
            items={it.submenu}
            x={sub.x}
            y={sub.y}
            depth={depth + 1}
            kb={sub.kb}
            onDone={onDone}
            onBack={() => {
              setSub(null);
              ref.current?.focus();
            }}
          />
        );
      })()}
    </>
  );
}

function SubMenu(
  props: {
    items: MenuItem[];
    x: number;
    y: number;
    depth: number;
    kb: boolean;
    onDone: () => void;
    onBack: () => void;
  },
) {
  const wrap = useRef<HTMLDivElement>(null);
  return (
    <div
      ref={wrap}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft" || e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          props.onBack();
        }
      }}
    >
      <MenuList
        items={props.items}
        x={props.x}
        y={props.y}
        depth={props.depth}
        onDone={props.onDone}
        autoFocus={props.kb}
      />
    </div>
  );
}

export function MenuHost() {
  const m = menu.value;
  useEffect(() => {
    if (!m) return;
    const prev = document.activeElement as HTMLElement | null;
    const down = (e: PointerEvent) => {
      if (!(e.target as HTMLElement).closest?.(".menu")) closeMenu();
    };
    const blur = () => closeMenu();
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        closeMenu();
      }
    };
    addEventListener("pointerdown", down, true);
    addEventListener("blur", blur);
    addEventListener("resize", blur);
    addEventListener("keydown", key, true);
    return () => {
      removeEventListener("pointerdown", down, true);
      removeEventListener("blur", blur);
      removeEventListener("resize", blur);
      removeEventListener("keydown", key, true);
      // Restore focus to where it was (e.g. a terminal), unless something else took it.
      if (prev && (document.activeElement === document.body || !document.activeElement)) {
        prev.focus?.();
      }
    };
  }, [m]);
  if (!m) return null;
  return (
    <div class="menu-layer">
      <MenuList
        items={m.items}
        x={m.x}
        y={m.y}
        alignRight={m.alignRight}
        depth={0}
        onDone={closeMenu}
        minWidth={m.minWidth}
        select={m.select}
        anchorTop={m.anchorTop}
      />
    </div>
  );
}

// Renders a split tree with draggable gutters.
import { useRef } from "preact/hooks";
import {
  dragGutter,
  equalize,
  findPane,
  type Layout,
  type LayoutState,
  setSizes,
  type SplitNode,
} from "./tree.ts";
import { updateLayout } from "../state.ts";
import { Pane } from "./Pane.tsx";
import { cx } from "../util.ts";

const MIN_PX = 120;
const GUTTER = 1; // visible px; hit area is widened in CSS

export function SplitView({ ch, layout }: { ch: number; layout: LayoutState }) {
  if (layout.zoom) {
    const p = findPane(layout.root, layout.zoom);
    if (p) {
      return (
        <div class="split-root">
          <Pane ch={ch} pane={p} focused zoomed />
        </div>
      );
    }
  }
  return (
    <div class="split-root">
      <Node ch={ch} node={layout.root} path={[]} focus={layout.focus} />
    </div>
  );
}

function Node(
  { ch, node, path, focus }: { ch: number; node: Layout; path: number[]; focus: string },
) {
  if (node.type === "pane") {
    return <Pane ch={ch} pane={node} focused={node.id === focus} zoomed={false} />;
  }
  return <Split ch={ch} node={node} path={path} focus={focus} />;
}

function Split(
  { ch, node, path, focus }: { ch: number; node: SplitNode; path: number[]; focus: string },
) {
  const ref = useRef<HTMLDivElement>(null);
  const out = [];
  for (let i = 0; i < node.children.length; i++) {
    const c = node.children[i];
    const key = c.type === "pane" ? c.id : "s" + i + ":" + firstPaneId(c);
    out.push(
      <div
        key={key}
        class="split-child"
        style={{ flex: `${node.sizes[i]} 1 0px` }}
      >
        <Node ch={ch} node={c} path={[...path, i]} focus={focus} />
      </div>,
    );
    if (i < node.children.length - 1) {
      out.push(
        <Gutter
          key={"g" + key}
          ch={ch}
          dir={node.dir}
          path={path}
          index={i}
          sizes={node.sizes}
          container={ref}
        />,
      );
    }
  }
  return <div ref={ref} class={cx("split", "split-" + node.dir)}>{out}</div>;
}

function firstPaneId(n: Layout): string {
  return n.type === "pane" ? n.id : firstPaneId(n.children[0]);
}

function Gutter(
  { ch, dir, path, index, sizes, container }: {
    ch: number;
    dir: "row" | "col";
    path: number[];
    index: number;
    sizes: number[];
    container: { current: HTMLDivElement | null };
  },
) {
  const onDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    const box = container.current!.getBoundingClientRect();
    const gutters = (sizes.length - 1) * GUTTER;
    const total = (dir === "row" ? box.width : box.height) - gutters;
    const start = dir === "row" ? e.clientX : e.clientY;
    const startSizes = [...sizes];
    const minFrac = MIN_PX / Math.max(total, 1);
    document.body.classList.add(dir === "row" ? "resizing-col" : "resizing-row");
    el.classList.add("dragging");
    let raf = 0;
    let last = 0;
    const move = (ev: PointerEvent) => {
      last = (dir === "row" ? ev.clientX : ev.clientY) - start;
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        const next = dragGutter(startSizes, index, last / total, minFrac);
        updateLayout(ch, (l) => setSizes(l, path, next));
      });
    };
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      el.classList.remove("dragging");
      document.body.classList.remove("resizing-col", "resizing-row");
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };
  return (
    <div
      class={cx("gutter", "gutter-" + dir)}
      role="separator"
      aria-orientation={dir === "row" ? "vertical" : "horizontal"}
      onPointerDown={onDown}
      onDblClick={() => updateLayout(ch, (l) => equalize(l, path))}
    />
  );
}

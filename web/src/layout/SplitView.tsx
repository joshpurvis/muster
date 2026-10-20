// Renders the window's split tree of groups with draggable gutters.
import { useRef } from "preact/hooks";
import {
  dragGutter,
  equalize,
  findPane,
  type Layout,
  type LayoutState,
  setSizes,
  type SplitNode,
  topRightPane,
} from "./tree.ts";
import { fullscreen, shownAlone, updateLayout } from "../state.ts";
import { Group } from "./Pane.tsx";
import { cx } from "../util.ts";

const MIN_PX = 120;
const GUTTER = 1; // visible px; hit area is widened in CSS

export function SplitView({ layout }: { layout: LayoutState }) {
  // Hide other groups during zoom (or fullscreen, which shows one group alone the same way)
  // without unmounting apps or losing their local state.
  const zoom = shownAlone.value;
  const corner = zoom ?? topRightPane(layout.root);
  return (
    <div class="split-root">
      <Node node={layout.root} path={[]} focus={layout.focus} corner={corner} zoom={zoom} />
    </div>
  );
}

function Node(
  { node, path, focus, corner, zoom }: { node: Layout; path: number[]; focus: string; corner: string; zoom: string | null },
) {
  if (node.type === "pane") {
    const full = fullscreen.value === node.id;
    return (
      <Group
        pane={node}
        focused={node.id === focus}
        zoomed={node.id === zoom && !full}
        fullscreen={full}
        multi={path.length > 0}
        controls={node.id === corner}
      />
    );
  }
  return <Split node={node} path={path} focus={focus} corner={corner} zoom={zoom} />;
}

function Split(
  { node, path, focus, corner, zoom }: { node: SplitNode; path: number[]; focus: string; corner: string; zoom: string | null },
) {
  const ref = useRef<HTMLDivElement>(null);
  const out = [];
  for (let i = 0; i < node.children.length; i++) {
    const c = node.children[i];
    const key = c.type === "pane" ? c.id : "s" + i + ":" + firstPaneId(c);
    // A group shown alone takes the whole box: its siblings are hidden, and its own share
    // (a fraction below 1) would otherwise leave the rest of the box empty.
    const hidden = !!zoom && !findPane(c, zoom);
    out.push(
      <div
        key={key}
        class="split-child"
        style={{ flex: zoom ? "1 1 0px" : `${node.sizes[i]} 1 0px`, display: hidden ? "none" : undefined }}
      >
        <Node node={c} path={[...path, i]} focus={focus} corner={corner} zoom={zoom} />
      </div>,
    );
    if (!zoom && i < node.children.length - 1) {
      out.push(
        <Gutter
          key={"g" + key}
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
  { dir, path, index, sizes, container }: {
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
        updateLayout((l) => setSizes(l, path, next));
      });
    };
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      el.removeEventListener("lostpointercapture", up);
      if (raf) {
        // Apply the last position the pending frame would have drawn.
        cancelAnimationFrame(raf);
        raf = 0;
        const next = dragGutter(startSizes, index, last / total, minFrac);
        updateLayout((l) => setSizes(l, path, next));
      }
      el.classList.remove("dragging");
      document.body.classList.remove("resizing-col", "resizing-row");
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
    el.addEventListener("lostpointercapture", up);
  };
  return (
    <div
      class={cx("gutter", "gutter-" + dir)}
      role="separator"
      aria-orientation={dir === "row" ? "vertical" : "horizontal"}
      onPointerDown={onDown}
      onDblClick={() => updateLayout((l) => equalize(l, path))}
    />
  );
}

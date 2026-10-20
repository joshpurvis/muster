// The image app: the picture on a soft stage with a real shadow (so a white screenshot neither
// melts into a light theme nor glares in a dark one), zoom controls on the picture (shared/viewer.ts:
// Fit, − and +, 100%, Ctrl+wheel, drag to pan), then the caption with the file's facts as chips.
import { startApp } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { freshView, type Viewer, viewer, type ViewState } from "../shared/viewer.ts";
import { formatBytes, type ImageInput, validateImage } from "./logic.ts";

const root = document.getElementById("app")!;
let current: ImageInput | null = null;
let warnings: string[] = [];
let view: ViewState = freshView();
let shownSrc = "";
let v: Viewer | null = null;

function render() {
  if (!current) return;
  const img = current;
  // A new picture starts at Fit; an update of the same one keeps its zoom and scroll.
  if (img.src !== shownSrc) view = freshView();
  shownSrc = img.src;
  v?.destroy();
  const picture = h("img", { src: img.src, alt: img.alt, decoding: "async" });
  v = viewer(picture, view, {
    stageClass: "stage img-stage",
    label: "Image; + and − zoom, 0 fits, 1 is actual size, drag or arrow keys scroll",
    ids: { zoomIn: "zoom-in", zoomOut: "zoom-out", fit: "zoom-fit", actual: "zoom-actual" },
  });
  const { stage, controls, readout } = v;
  readout.classList.add("img-zoom");
  readout.hidden = true;
  const dims = h("span", { class: "mk-chip dims", hidden: true });
  picture.addEventListener("load", () => {
    if (picture.naturalWidth) { dims.textContent = `${picture.naturalWidth} × ${picture.naturalHeight}`; dims.hidden = false; }
    readout.hidden = false;
  });
  picture.addEventListener("error", () => {
    readout.hidden = true;
    stage.replaceChildren(h("div", { class: "error", role: "alert" },
      h("strong", null, "The image could not be shown. "),
      img.src.startsWith("data:")
        ? "The file is not a valid image of its type."
        : "Check that the URL is reachable and that its origin is declared in csp.resourceDomains."));
  });
  controls.classList.add("img-tools");
  mount(root,
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("figure", { class: "img-figure" },
      h("div", { class: "stage-wrap" }, stage, controls, readout),
      h("figcaption", { class: "cap" },
        img.caption ? h("span", { class: "caption" }, img.caption) : h("span"),
        h("span", { class: "meta" },
          h("span", { class: "mk-chip source", title: img.source }, img.source),
          dims,
          img.bytes !== undefined ? h("span", { class: "mk-chip" }, formatBytes(img.bytes)) : null))),
  );
}

addEventListener("keydown", (e) => {
  if (current) v?.key(e);
});

startApp({
  name: "Muster Image",
  input: (data) => {
    const r = validateImage(data);
    if (!r.ok) { current = null; v?.destroy(); v = null; problems(root, "This image panel's input is invalid", r.errors); return; }
    current = r.value;
    warnings = r.warnings;
    render();
  },
});

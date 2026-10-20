// The image app: the picture on a soft stage with a real shadow (so a white screenshot neither
// melts into a light theme nor glares in a dark one), Fit / Actual size on the picture, then the
// caption with the file's facts as chips.
import { startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { formatBytes, type ImageInput, validateImage } from "./logic.ts";

const root = document.getElementById("app")!;
let fit = true;
let current: ImageInput | null = null;
let warnings: string[] = [];
let apply = () => {};

function render() {
  if (!current) return;
  const img = current;
  const picture = h("img", { src: img.src, alt: img.alt, decoding: "async", draggable: "false" });
  const stage = h("div", { class: "stage img-stage", tabindex: "0", "aria-label": "Image; Z toggles actual size" }, picture);
  const fitBtn = h("button", { type: "button", id: "zoom-fit", onclick: () => setFit(true) }, "Fit");
  const actualBtn = h("button", { type: "button", id: "zoom-actual", title: "Actual size (Z)", onclick: () => setFit(false) }, "Actual size ", h("kbd", null, "Z"));
  const zoom = h("span", { class: "img-zoom", "aria-hidden": "true" });
  const dims = h("span", { class: "mk-chip dims", hidden: true });
  const updateZoom = () => {
    if (!picture.naturalWidth) { zoom.hidden = true; return; }
    zoom.hidden = false;
    zoom.textContent = `${Math.round((picture.clientWidth / picture.naturalWidth) * 100)}%`;
  };
  apply = () => {
    stage.className = `stage img-stage ${fit ? "fit" : "actual"}`;
    fitBtn.setAttribute("aria-pressed", String(fit));
    actualBtn.setAttribute("aria-pressed", String(!fit));
    requestAnimationFrame(updateZoom);
  };
  picture.addEventListener("load", () => {
    if (picture.naturalWidth) { dims.textContent = `${picture.naturalWidth} × ${picture.naturalHeight}`; dims.hidden = false; }
    updateZoom();
  });
  picture.addEventListener("error", () => {
    stage.replaceChildren(h("div", { class: "error", role: "alert" },
      h("strong", null, "The image could not be shown. "),
      img.src.startsWith("data:")
        ? "The file is not a valid image of its type."
        : "Check that the URL is reachable and that its origin is declared in csp.resourceDomains."));
  });
  picture.addEventListener("click", () => setFit(!fit));
  new ResizeObserver(updateZoom).observe(stage);
  mount(root,
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("figure", { class: "img-figure" },
      h("div", { class: "stage-wrap" }, stage,
        h("div", { class: "img-tools mk-seg", role: "group", "aria-label": "Zoom" }, fitBtn, actualBtn),
        zoom),
      h("figcaption", { class: "cap" },
        img.caption ? h("span", { class: "caption" }, img.caption) : h("span"),
        h("span", { class: "meta" },
          h("span", { class: "mk-chip source", title: img.source }, img.source),
          dims,
          img.bytes !== undefined ? h("span", { class: "mk-chip" }, formatBytes(img.bytes)) : null))),
  );
  apply();
}

function setFit(next: boolean) { fit = next; apply(); }

addEventListener("keydown", (e) => {
  if (typing(e) || e.ctrlKey || e.metaKey || e.altKey || !current) return;
  if (e.key === "z" || e.key === "Z") { setFit(!fit); e.preventDefault(); }
});

startApp({
  name: "Muster Image",
  input: (data) => {
    const r = validateImage(data);
    if (!r.ok) { current = null; problems(root, "This image panel's input is invalid", r.errors); return; }
    current = r.value;
    warnings = r.warnings;
    render();
  },
});

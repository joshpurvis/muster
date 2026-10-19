import { startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { formatBytes, type ImageInput, validateImage } from "./logic.ts";

const root = document.getElementById("app")!;
let fit = true;
let current: ImageInput | null = null;
let warnings: string[] = [];

function render() {
  if (!current) return;
  const img = current;
  const picture = h("img", { src: img.src, alt: img.alt, decoding: "async", draggable: "false" });
  const stage = h("div", { class: "stage", tabindex: "0", "aria-label": "Image; Z toggles zoom" }, picture);
  const size = h("span", { class: "muted dims" });
  const label = h("span");
  const toggle = h("button", { type: "button", title: "Toggle zoom (Z)", onclick: () => setFit(!fit) }, label, " ", h("kbd", null, "Z"));
  apply = () => {
    stage.className = `stage ${fit ? "fit" : "actual"}`;
    label.textContent = fit ? "Actual size" : "Zoom to fit";
    toggle.setAttribute("aria-pressed", String(!fit));
  };
  picture.addEventListener("load", () => {
    if (picture.naturalWidth) size.textContent = `${picture.naturalWidth} × ${picture.naturalHeight}`;
  });
  picture.addEventListener("error", () => {
    stage.replaceChildren(h("div", { class: "error", role: "alert" },
      h("strong", null, "The image could not be shown. "),
      img.src.startsWith("data:")
        ? "The file is not a valid image of its type."
        : "Check that the URL is reachable and that its origin is declared in csp.resourceDomains."));
  });
  picture.addEventListener("click", () => setFit(!fit));
  mount(root, 
    h("header", { class: "bar" },
      h("span", { class: "source mono", title: img.source }, img.source),
      img.bytes !== undefined ? h("span", { class: "muted" }, formatBytes(img.bytes)) : null,
      size,
      h("span", { class: "spacer" }),
      toggle),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    stage,
    img.caption ? h("p", { class: "caption" }, img.caption) : null,
  );
  apply();
}

let apply = () => {};
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

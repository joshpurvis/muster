// `image` app input. Muster reads `path` when the panel opens or updates and adds `src` (a
// data URI), `mime` and `bytes`; `url` is shown directly from a declared https origin.
import { httpsUrl, Problems, type Result } from "../shared/validate.ts";

export interface ImageInput {
  src: string;
  /** What the panel names as the source: the worktree path or the URL. */
  source: string;
  alt: string;
  caption?: string;
  bytes?: number;
}

const DATA_URI = /^data:image\/(png|jpeg|gif|webp|svg\+xml);base64,[A-Za-z0-9+/]+=*$/;

export function validateImage(data: unknown): Result<ImageInput> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["path", "url", "alt", "caption", "src", "mime", "bytes"]);
  const path = p.string(o, "path", "", { max: 4096, nonEmpty: true });
  const url = p.string(o, "url", "", { max: 4096, nonEmpty: true });
  const alt = p.string(o, "alt", "", { max: 1000 });
  const caption = p.string(o, "caption", "", { max: 2000 });
  const src = p.string(o, "src", "", { max: 16 * 1024 * 1024 });
  const bytes = p.number(o, "bytes", "");
  let shown = "";
  let source = "";
  if (path !== undefined && url !== undefined) p.add("data", "give path or url, not both");
  else if (path !== undefined) {
    if (src === undefined) p.add("path", "the image file was not loaded (Muster reads it when the panel opens)");
    else if (!DATA_URI.test(src)) p.add("src", "is not a supported image data URI");
    else { shown = src; source = path; }
  } else if (url !== undefined) {
    const safe = httpsUrl(url);
    if (!safe) p.add("url", "must be an https URL without a user name or password");
    else { shown = safe; source = safe; }
  } else if (!p.errors.length) p.add("data", "needs path (a file in the task worktree) or url (https)");
  return p.result({ src: shown, source, alt: alt ?? "", caption, bytes });
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}

/** Scale that fits `w`×`h` inside the box without enlarging past actual size. */
export function fitScale(w: number, h: number, boxW: number, boxH: number): number {
  if (!(w > 0 && h > 0 && boxW > 0 && boxH > 0)) return 1;
  return Math.min(1, boxW / w, boxH / h);
}

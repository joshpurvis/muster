import { isPreviewUrl, previewFrameSrc, previewNotice } from "./preview.ts";

function check(value: unknown, message = "assertion failed") {
  if (!value) throw new Error(message);
}

Deno.test("only loopback http(s) URLs are framed", () => {
  for (const ok of ["http://localhost:5173/", "https://127.0.0.1:8443/a?b#c", "http://[::1]:3000/", "http://LOCALHOST:1/"]) {
    check(isPreviewUrl(ok), ok);
  }
  for (const bad of [
    "https://example.com/", "http://localhost.evil.com:5173/", "http://user@localhost:5173/", "http://localhost:5173@evil.com/",
    "javascript:alert(1)", "file:///etc/passwd", "ftp://localhost/", "not a url", "", 42, null, "http://192.168.1.2:5173/",
  ]) {
    check(!isPreviewUrl(bad), String(bad));
  }
});

Deno.test("the frame goes through the panel server's /preview page", () => {
  const src = previewFrameSrc("http://127.0.0.1:40001", "http://localhost:5173/a?b=1&c=2#d");
  check(src === "http://127.0.0.1:40001/preview?url=http%3A%2F%2Flocalhost%3A5173%2Fa%3Fb%3D1%26c%3D2%23d", src);
  check(new URL(src).searchParams.get("url") === "http://localhost:5173/a?b=1&c=2#d");
});

Deno.test("notices follow the check", () => {
  check(previewNotice(null, false) === null);
  check(previewNotice({ reachable: true, answered: true, status: 200 }, false) === null, "a working page has no notice");
  check(previewNotice({ reachable: true, answered: null }, false) === null, "https is not read");
  const down = previewNotice({ reachable: false }, true)!;
  check(down.kind === "down" && down.blocking && down.detail.includes("reloads by itself"), down.detail);
  check(previewNotice({ reachable: false }, false)!.detail.includes("press Reload"));
  const refused = previewNotice({ reachable: true, answered: true, status: 200, refused: "X-Frame-Options: DENY" }, false)!;
  check(refused.kind === "refused" && refused.detail.includes("X-Frame-Options: DENY") && refused.detail.includes("browser"));
  const leaves = previewNotice({ reachable: true, answered: true, status: 302, leaves_to: "https://login.example/" }, false)!;
  check(leaves.kind === "leaves" && leaves.detail.includes("https://login.example/"));
  const silent = previewNotice({ reachable: true, answered: false }, false)!;
  check(silent.kind === "silent" && !silent.blocking);
  // A refusal outranks a redirect; being down outranks both.
  check(previewNotice({ reachable: true, refused: "x", leaves_to: "y" }, false)!.kind === "refused");
  check(previewNotice({ reachable: false, refused: "x" }, false)!.kind === "down");
});

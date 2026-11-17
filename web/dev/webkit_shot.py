#!/usr/bin/env python3
"""Screenshots of the desktop UI in WebKitGTK, the engine of Muster's desktop window.

Headless Chromium draws some things differently from the desktop window (native <select> padding,
forced `line-height: normal` on form controls), so a picture from it can look right while the
window shows the bug. Use this for every desktop UI picture that goes in a PR, docs/screenshots or a
design review (AGENTS.md, "Screenshots of the UI"). The phone UI is not drawn by this engine on a
phone; it keeps headless Chromium with mobile emulation (docs/ANDROID.md).

Normally run through web/dev/webkit_shot.sh, which starts an isolated mock and stops it after:

  python3 -I web/dev/webkit_shot.py <url> <steps.json> <out-dir> [--size WxH] [--theme dark|light]

<url> must be the mock on a loopback address (http://127.0.0.1:<port>/#token=dev): the tool asks
its /api/health first and refuses anything that isn't the mock, so it can't open a real Muster.
Needs PyGObject with the Cairo bindings (python-gobject, python-cairo), WebKitGTK 2.40 or newer
with the 4.1 API (webkit2gtk-4.1) and a graphical session (Wayland or X11); nothing is shown on
screen.

steps.json is a list of steps, run in order. Each may have:
  "js":     an async function body run in the page (`await` works; `return` a value to print it
            as `RESULT <n> <json>`). Fails the run if it throws or takes over "timeout" seconds.
  "until":  a JS expression; waits until it is truthy (up to "timeout" seconds, default 10).
  "settle": seconds to wait after the step before the picture (default 0.4).
  "shot":   file name without extension; saves <out-dir>/<shot>.png of the visible window.
  "crop":   with "shot": a CSS selector; the picture is cut to that element (plus "pad" px).
Example (web/dev/webkit_shots/palette.json): open the command palette and take a picture.

Exit status: 0 when every step ran, 1 on a JS error, 2 when an "until" timed out.
"""
import argparse
import json
import os
import sys
import time
import urllib.request
from urllib.parse import urlparse

# Software rendering: the GL paths abort in sandboxes and headless sessions, and they don't
# change layout. Must be set before WebKit loads.
os.environ.setdefault("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
os.environ.setdefault("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
# The theme comes from --theme only (see Page): a GTK_THEME like "Adwaita:dark" would override it.
os.environ.pop("GTK_THEME", None)

import gi  # noqa: E402

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
import cairo  # noqa: E402
from gi.repository import GLib, Gtk, WebKit2  # noqa: E402


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def parse_args():
    p = argparse.ArgumentParser(description="Desktop UI screenshots in WebKitGTK")
    p.add_argument("url")
    p.add_argument("steps")
    p.add_argument("out")
    p.add_argument("--size", default="1600x1000", help="window size in CSS px, WxH (default 1600x1000)")
    p.add_argument("--theme", choices=["dark", "light"], default="dark",
                   help="the system colour scheme the page sees (Muster's theme setting 'system')")
    a = p.parse_args()
    u = urlparse(a.url)
    if u.hostname not in ("127.0.0.1", "localhost", "::1"):
        p.error("the URL must be the mock on a loopback address, never a real backend")
    try:
        a.w, a.h = (int(n) for n in a.size.lower().split("x"))
    except ValueError:
        p.error("--size takes WxH, e.g. 1600x1000")
    if not (100 <= a.w <= 8000 and 100 <= a.h <= 8000):
        p.error("--size: each side must be 100 to 8000 px")
    # The mock's /api/health says version "mock"; a real Muster (even the owner's live one on
    # loopback) says its own version and is refused.
    try:
        # Never via a proxy, and never following a redirect to some other listener.
        direct = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with direct.open(f"{u.scheme}://{u.netloc}/api/health", timeout=5) as r:
            health = json.load(r)
    except (OSError, ValueError) as e:
        p.error(f"no mock answered at {u.netloc}: {e}")
    if health.get("version") != "mock":
        p.error(f"{u.netloc} is not the mock (version {health.get('version')!r}); use web/dev/webkit_shot.sh")
    return a


class Page:
    def __init__(self, url, w, h, theme):
        # WebKitGTK answers prefers-color-scheme from the GTK theme, so pin both the theme and
        # its dark variant: the desktop's own theme (e.g. a "-dark" one) would win otherwise.
        settings = Gtk.Settings.get_default()
        settings.set_property("gtk-theme-name", "Adwaita")
        settings.set_property("gtk-application-prefer-dark-theme", theme == "dark")
        self.win = Gtk.OffscreenWindow()
        self.win.set_default_size(w, h)
        # Ephemeral: nothing (localStorage, cookies) is kept between runs or written to disk, so a
        # theme or layout from an earlier run can't leak into this one.
        self.view = WebKit2.WebView.new_with_context(WebKit2.WebContext.new_ephemeral())
        self.view.set_size_request(w, h)
        self.win.add(self.view)
        self.win.show_all()
        self.failed = None
        self.view.connect("load-failed", self._load_failed)
        self.view.load_uri(url)
        end = time.monotonic() + 15
        while self.view.is_loading() and time.monotonic() < end:
            self.spin(0.05)
        if self.failed:
            raise RuntimeError(f"the page did not load: {self.failed}")
        if self.view.is_loading():
            raise RuntimeError("the page did not finish loading within 15 s")
        res = self.view.get_main_resource()
        status = res.get_response().get_status_code() if res and res.get_response() else 0
        if status != 200:
            raise RuntimeError(f"the page answered HTTP {status} (is web/dist built?)")
        self.spin(1.0)

    def _load_failed(self, _view, _event, uri, error):
        self.failed = f"{uri}: {error.message}"
        return False

    @staticmethod
    def spin(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            Gtk.main_iteration_do(False)
            time.sleep(0.005)

    def run(self, body, timeout=10):
        """Run `body` as an async function in the page; return (answered, value). A `return` of
        nothing gives (True, UNDEFINED); `return null` gives (True, None)."""
        box = {}

        def done(view, res):
            try:
                v = view.call_async_javascript_function_finish(res)
                box["v"] = UNDEFINED if v is None or v.is_undefined() else json.loads(v.to_json(0))
            except GLib.Error as e:
                box["err"] = e.message

        self.view.call_async_javascript_function(body, -1, None, None, None, None, done)
        end = time.monotonic() + timeout
        while not box and time.monotonic() < end:
            Gtk.main_iteration_do(False)
            time.sleep(0.005)
        if "err" in box:
            raise RuntimeError(f"{box['err']} in: {body[:200]}")
        if not box:
            raise TimeoutError(f"no answer within {timeout:g} s from: {body[:200]}")
        return box["v"]

    def snapshot(self, timeout=15):
        box = {}

        def done(view, res):
            try:
                box["s"] = view.get_snapshot_finish(res)
            except GLib.Error as e:
                box["err"] = e.message

        self.view.get_snapshot(WebKit2.SnapshotRegion.VISIBLE, WebKit2.SnapshotOptions.NONE, None, done)
        end = time.monotonic() + timeout
        while not box and time.monotonic() < end:
            Gtk.main_iteration_do(False)
            time.sleep(0.005)
        if "err" in box:
            raise RuntimeError(f"snapshot failed: {box['err']}")
        if not box:
            raise RuntimeError(f"no snapshot within {timeout} s")
        return box["s"]


UNDEFINED = object()


def crop(surface, rect, pad, css_width):
    """Cut `surface` to the CSS-px `rect` grown by `pad`, kept inside the picture. On a HiDPI
    display the snapshot has more pixels than CSS px: scale by its width over `css_width`."""
    sw, sh = surface.get_width(), surface.get_height()
    k = sw / css_width
    x0, y0 = max(0, int((rect["x"] - pad) * k)), max(0, int((rect["y"] - pad) * k))
    x1 = min(sw, int((rect["x"] + rect["width"] + pad) * k + 0.999))
    y1 = min(sh, int((rect["y"] + rect["height"] + pad) * k + 0.999))
    if x1 <= x0 or y1 <= y0:
        raise RuntimeError(f"crop area is empty: {rect}")
    out = cairo.ImageSurface(cairo.FORMAT_ARGB32, x1 - x0, y1 - y0)
    ctx = cairo.Context(out)
    ctx.set_source_surface(surface, -x0, -y0)
    ctx.paint()
    return out


def main():
    a = parse_args()
    with open(a.steps) as f:
        steps = json.load(f)
    os.makedirs(a.out, exist_ok=True)
    try:
        page = Page(a.url, a.w, a.h, a.theme)
    except RuntimeError as e:
        print(f"FAILED: {e}", file=sys.stderr)
        return 1
    print("WEBKIT", f"{WebKit2.get_major_version()}.{WebKit2.get_minor_version()}.{WebKit2.get_micro_version()}",
          f"{a.w}x{a.h}", a.theme)
    for i, s in enumerate(steps):
        try:
            if s.get("js"):
                r = page.run(s["js"], s.get("timeout", 10))
                if r is not UNDEFINED:
                    print("RESULT", i, json.dumps(r))
            if s.get("until"):
                end = time.monotonic() + s.get("timeout", 10)
                while True:
                    left = end - time.monotonic()
                    try:
                        if left > 0 and page.run(f"return !!({s['until']});", left):
                            break
                    except TimeoutError:
                        pass
                    if time.monotonic() >= end:
                        print(f"STEP {i} TIMEOUT waiting for {s['until']}", file=sys.stderr)
                        return 2
                    page.spin(0.1)
            page.spin(s.get("settle", 0.4))
            if s.get("shot"):
                surface = page.snapshot()
                if s.get("crop"):
                    rect = page.run(
                        f"const r = document.querySelector({json.dumps(s['crop'])})?.getBoundingClientRect();"
                        " return r && {x: r.x, y: r.y, width: r.width, height: r.height};")
                    if rect is UNDEFINED or not rect:
                        raise RuntimeError(f"nothing matches crop selector {s['crop']}")
                    width = page.run("return innerWidth;")
                    surface = crop(surface, rect, s.get("pad", 0), width)
                path = os.path.join(a.out, s["shot"] + ".png")
                surface.write_to_png(path)
                print("SHOT", path)
        except (RuntimeError, TimeoutError) as e:
            print(f"STEP {i} FAILED: {e}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

# Parked review findings: fix/link-activation-token (28 Nov 2026)

One review (correctness + reliability): P1 0, P2 3, P3 3. One P3 fixed (agent-panel docs). Parked:

- P2: `link_activation` (src/gui.rs) waits on GTK's Wayland token roundtrip on the UI thread with no timeout; if the
  Wayland connection drops mid-request the window could stall. The window is already lost in that case.
- P2: a touchscreen tap that opens a `_blank` link on release may give GTK an older serial, so the compositor marks
  the browser urgent instead of raising it. Mouse clicks are unaffected.
- P2: on GNOME, `xdg-open` hands off to `gio open`, which drops the token when it opens a running D-Bus-activated
  browser. Hyprland and other generic desktops run the desktop entry directly and keep it (checked 28 Nov with a fake
  handler: the token arrived unchanged). Fix if a GNOME user hits it: launch through GIO with the window's context.
- P3: on X11/XWayland a startup notification starts and is never completed if the opener fails; the desktop shows
  launch feedback until its timeout.
- P3: the Wayland probe test checks the token reaches the opener, not that the new-window handler asks for it.

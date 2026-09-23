# Android remote support: implementation plan

Status: planning only. Starts after the Linux desktop redesign (PLAN.md §0) stabilises. Method
names follow docs/PROTOCOL.md; `session.wait/input/output` and `window.*` are being implemented
in other worktrees and may not be in this checkout yet.

## Goals and non-goals

Goals: from a phone on the tailnet, see which agents need you, approve or deny, send a prompt
(typed or dictated), poke keys, and watch the terminal. Installable as a PWA. No new binary.

Non-goals (for the Tailscale/PWA track): a native app, a relay or cloud service, Tailscale
Funnel or any public exposure, multi-user, editing files, a full keyboard terminal on the phone,
iOS testing. See "Paid version (later)" for what comes after.

## Architecture

```
 phone (Chrome / PWA)                   laptop
 ┌─────────────────────────┐   HTTPS    ┌──────────────┐  HTTP   ┌───────────────────────┐
 │ https://host.tn.ts.net/m/│──────────►│ tailscaled   │────────►│ muster daemon         │
 │  mobile.tsx (Preact)     │ WireGuard │ tailscale    │ 127.0.0.1│  127.0.0.1:<port>      │
 │  rpc.ts  (wss, cookie)   │◄──────────│ serve :443   │◄────────│  server.rs routes      │
 │  terms.ts (xterm, ro)    │           │ adds X-Fwd-*,│         │  + /m/ assets          │
 │  sw.js   (Web Push)      │           │ Tailscale-*  │         │  + /api/device/*       │
 └─────────────────────────┘           └──────────────┘         │  + push sink (notify)  │
        ▲  push (encrypted)                                       └──────────┬────────────┘
        └──────────── FCM ◄── daemon POST (VAPID) ───────────────────────────┘
```

The daemon keeps binding 127.0.0.1 (src/main.rs:303-316). `tailscale serve --bg --https=443
http://127.0.0.1:<port>` publishes it to the tailnet only. The port is 7433 unless busy or
overridden (src/main.rs:305-315), so `muster pair` prints the exact `tailscale serve` command
for the running daemon and the docs recommend pinning `MUSTER_PORT=7433`. Everything the phone
does goes through the existing protocol; only auth, the asset path and a few methods are new.

## Security model

Today: anyone who presents the client token is the user. The token sits in `client_token`
(src/paths.rs:60) and `daemon.json`, both written 0600 (src/paths.rs:88-97), browsers get it through a single-use
launch code, and `/api/exchange` checks that the connecting socket belongs to the daemon's uid
via `/proc/net/tcp` (src/server.rs:127-154, 183-207). `origin_allowed` (src/server.rs:51-67)
accepts an absent `Origin`, `127.0.0.1`/`localhost`, `tauri://`, `wry://` and a dev list.

What changes behind `tailscale serve`:

- The TCP peer is tailscaled, not the browser. `socket_owner` reports tailscaled's uid (root
  in a normal install), which is not the daemon's uid, so `/api/exchange` refuses. That must
  not be the only guard: with a userspace tailscaled under the same uid it would pass. Rule:
  `/api/exchange` and `/api/launch-code` keep the *local* origin list only; the remote allowlist
  below never applies to them.
- The browser's `Origin` is `https://<host>.<tailnet>.ts.net`, so `/ws` is refused today. A
  persisted `remote_origins` allowlist replaces the `MUSTER_ALLOW_ORIGIN` dev hatch
  (src/server.rs:66) for these routes.
- tailscale serve adds `Tailscale-User-Login`, `Tailscale-User-Name` and `X-Forwarded-For` and
  strips client-supplied copies. Remote peers cannot forge them; any local process, including
  other local users, can by connecting to 127.0.0.1 directly. So the daemon honours those
  headers only when `socket_owner` says the peer socket belongs to the proxy uid
  (`MUSTER_PROXY_UID`, default 0); otherwise they are ignored and the request counts as
  "local/unknown" for logging and rate limits. Identity headers pin and log; they never
  authenticate. The device token is the credential.
- **Proxied requests get a smaller daemon.** `tailscale serve` publishes the whole listener,
  not just `/m/`. Without a rule, `wss://host.ts.net/ws?token=<client token>` from any tailnet
  peer (no `Origin`, which `origin_allowed` accepts, src/server.rs:52-53) is full access, and
  `/mcp` and `/hook/*` session tokens (src/server.rs:156-168) become remote credentials. Both
  tokens are readable by unsandboxed agents (docs/PROTOCOL.md:226-231). Rule: a request is
  *proxied* when the peer socket belongs to the proxy uid **or** it carries any
  `X-Forwarded-For`/`Tailscale-*` header (forging those locally only makes a request more
  restricted, so that is safe). Proxied requests may only use `/m/*`, `/api/device/*`, and
  `/ws` or `/api/rpc` with a device cookie; client tokens, session tokens, `/mcp`, `/hook`,
  `/api/exchange`, `/api/launch-code` and `/` all answer `404`/`401` there. Requests carrying
  `Tailscale-Funnel-Request` are refused outright. Belt and braces: mount only the needed
  paths (`tailscale serve --bg --https=443 --set-path=/m http://127.0.0.1:<port>/m`, and the
  same for `/ws`, `/api/device`, `/api/rpc`), which `muster pair` prints.
- `?token=` in the `/ws` query (src/server.rs:70-86) would work over TLS but leaks into proxy
  logs and depends on `sessionStorage`, which is per browsing session (guess: an installed PWA
  relaunched after Chrome's process was killed starts a new one). Device auth uses an
  `HttpOnly; Secure; SameSite=Strict; Path=/` cookie instead; the page never sees the token.
  `SameSite` is not the CSRF boundary: `ts.net` is on the public suffix list, so every host in
  one tailnet is same-site with every other. The boundary is the strict `Origin` check below.

### Threats and mitigations

| Threat | Mitigation |
|---|---|
| Lost/stolen phone | Token HttpOnly (no copy in page storage); 30-day sliding expiry; `muster devices revoke` closes live sockets and every later request fails; default scope `interact`, so no spawning or permission changes. Be honest: `interact` can approve and prompt agents, i.e. run code as you. Optional later: WebAuthn user verification before `approve_always`/`manage`. |
| Tailnet peers (your other devices, shared nodes) | Reach the HTTPS port, get `401` without a cookie. Pairing: one pending pairing at a time (`muster pair` replaces it); 64-hex code in the QR and a 9-char base32 alias (45 bits) for typing, both consumed atomically under the hub lock; 2-minute TTL; single use; the pending pairing is burnt after 3 wrong attempts and `muster pair` says so; 5 attempts/min per source (the trusted forwarded IP, else one "local" bucket); pending pairings live in memory only. Recommend a Tailscale ACL allowing only your phone to `host:443`. Never Funnel. |
| Shared tailnet (other people's users) | Same as above, plus pinning: the `Tailscale-User-Login` seen at pairing (when the headers are trusted) is stored; later requests that carry a *different* login are refused. Requests with no login header (tagged nodes, local) skip the check. A login identifies a user, not a phone; the token still does that. |
| Token leakage (backups, logs, screenshots) | Tokens stored as SHA-256 hashes in SQLite; never in URLs; QR expires in 2 minutes; daemon.log records pairings, refusals and revocations with device id and source, never tokens. |
| Web page compromise (XSS) | Terminal bytes render in xterm; names via Preact. `/m/` responses only (not the desktop, whose inline theme script at web/index.html:10 would break) get `default-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'` (xterm's DOM renderer injects styles). |
| Stale approval | A reconnect flushes queued calls (web/src/rpc.ts:151-153, :231-238) and `session.approve` acts on whatever prompt is on screen now (src/inject.rs:491). Mobile passes `state_since` and the daemon rejects a mismatch; approvals are never queued while offline. |
| Local processes / agents | Unchanged from today (docs/PROTOCOL.md:226-231). `device.*` accepts only the client token, never a cookie. |

## Protocol additions

Store: a new entry in `MIGRATIONS` (src/store.rs:25) adds
`devices(id, name, token_hash, scopes, ts_login, created_at, last_seen_at, last_source,
expires_at, revoked_at)`, `remote_origins(origin, added_at)` and, in phase 2,
`push_subscriptions(device_id, endpoint, p256dh, auth, created_at)`. `token_hash` is
`UNIQUE`, `scopes` is a JSON array, `p256dh`/`auth` are stored as received. Foreign keys are
off (src/store.rs:141), so revocation deletes subscriptions explicitly. `size_owner` (below) is
runtime state, not persisted: no column, null after a daemon restart.

**Principal.** Every connection and HTTP request resolves to `Principal::Local` (client token)
or `Principal::Device{id, scopes}` (cookie). The device row is re-read at each request, so a
revoked or expired device fails on its next call even if a socket is still open; in-flight
calls finish. Prompts already in the injection queue stay queued (they were authorised when
sent); `muster devices revoke` prints how many are pending.

**Cookie rules.** A cookie is accepted only when `?token=`/`Authorization` is absent and the
request carries an `Origin` that is present, not `null`, and in `remote_origins` or the local
list. Bearer requests keep today's rules (absent `Origin` allowed, for the CLI,
docs/PROTOCOL.md:14). Every cookie-authenticated `POST` (`pair`, `logout`, `rpc`) also needs
`X-Muster: 1`; the WebSocket cannot add headers, so its protection is the strict `Origin`
check (`SameSite` only helps against sites outside the tailnet).

| Route | Auth | Purpose |
|---|---|---|
| `POST /api/device/pair` | pairing code | `{code, name?}` → `200 {device: Device}` plus `Set-Cookie: muster_device=<64 hex>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000`. `403` on any failure, same body as `/api/exchange`. |
| `GET /api/device/me` | device cookie | `200 {device}` or `401`. The page calls it on launch: browsers hide a WebSocket handshake's `401`, so this is how the PWA learns it is unpaired. Also renews the cookie and `last_seen_at` when older than a day (the sliding 30 days). |
| `POST /api/device/logout` | device cookie | Revokes this device, clears the cookie. |
| `GET /ws` | client token or device cookie | Cookie path per the rules above. |
| `POST /api/rpc` | client token or device cookie | Same. |
| `GET /m`, `/m/`, `/m/*` | none | `/m` redirects to `/m/`; the SPA fallback (src/assets.rs:58-62) serves `m/index.html` for extension-less paths under `m/`, with `no-cache` like `index.html` (src/assets.rs:26-32). |

`Device` = `{id, name, scopes, ts_login?, created_at, last_seen_at, last_source, expires_at,
revoked_at?}`.

| Method | Params | Result |
|---|---|---|
| `device.pair_code` | `{ttl_s? = 120, name?, scopes? = ["view","interact"], url?}` | `{code, short_code, url, expires_at}`. Local only. `url` defaults to `https://<Self.DNSName from tailscale status --json, trailing dot stripped>/m/#pair=<code>`; its origin is inserted into `remote_origins`. HTTPS certificates must be enabled for the tailnet first; `muster pair` says so when `tailscale serve` fails. |
| `device.list` | `{}` | `Device[]`. Local only. |
| `device.revoke` | `{id}` or `{all: true}` | `{revoked, queued_prompts}`. Closes that device's sockets, deletes its push subscriptions. Local only. |
| `session.keys` | `{id, keys: string[]}` | `{}`. Names from docs/CLI.md:35 (`muster keys` is specified there but not yet implemented; the key table lives in the daemon and both use it), encoded using the session's current application-cursor mode. `session.input` stays for raw bytes. |
| `session.approve` | `{id, decision, state_since?}` | As today; with `state_since`, `{error}` unless it equals the session's current `state_since` (src/model.rs:229). |
| `session.attach` | `{id, replay?: "full" \| "screen" \| {since: {offset, generation, epoch}}}` | Default `full` (unchanged for existing clients). Right before the `0x03` frame, on the same per-connection outbox (src/rpc.rs:481-490), the daemon sends a connection-scoped message `{"event":"attach_info","data":{id, kind: "full"\|"screen"\|"delta", offset, generation, epoch}}`; it is not a hub broadcast, so it cannot reach other clients or race the frame. `full`/`screen` replace the terminal (client resets, web/src/terms.ts:295-303); `delta` appends and the client skips the reset. `screen` = the mode prefix every replay already gets (`replay_prefix`, src/pty.rs:862-873) plus the vt100 parser's formatted screen; bounded by rows×cols. `offset` = raw bytes ingested at the moment of the snapshot (`OutBuf.total`, src/pty.rs:97-98), taken under the same lock as the snapshot and the subscription (src/pty.rs:8-10); it excludes the prefix and the up-to-64 KiB the ring drops after wrapping (src/pty.rs:397-404). The client advances it by the `0x01` payload bytes it receives, which are raw. `since` yields `delta` only when `generation` and `epoch` match and `offset ≥ total − ring.len()` and the range is ≤ 64 KiB; otherwise `screen`. `epoch` is random per daemon start (generations restart at zero, src/hub.rs:260). The attachment remembers its requested mode: the restart- and lag-triggered replays in `resend_replay` (src/rpc.rs:573) send `screen` instead of `full` for such attachments and carry `attach_info` too. |

**Scopes.** Default deny; `manage` ⊃ `interact` ⊃ `view`. Enforced for text requests in
`dispatch` (src/rpc.rs:84), for `session.attach/detach` in `handle_request` (src/rpc.rs:459-
503) and for binary `0x02` input frames (src/rpc.rs:430-436), which need `interact`.

| Scope | Methods |
|---|---|
| `view` | `hello`, `session.attach/detach/screen/output/wait`, `channel.timeline/diff` |
| `interact` | `session.send/keys/input/approve/interrupt/clear_queue/resize`, `channel.mark_read/post` |
| `manage` | `channel.create` (a `permissions` field is rejected: the channel gets defaults), `channel.archive/rename`, `session.spawn` (`extra_args` rejected), `session.kill/restart/remove/rename`, `workspace.add`, `fs.complete` |
| never for devices | `device.*`, `channel.set_permissions/set_layout/reorder/move`, `window.*`, `workspace.remove` |

Events: `Prompt` already records `from` (src/model.rs:249); `Approval` (src/model.rs:264,
written at src/inject.rs:513-518) gains `from: "device:<id>"`.

CLI: `muster pair [--name N] [--scope view|interact|manage] [--ttl S] [--url U]` prints the
`tailscale serve` command for the current port, the QR (unicode half blocks) and the short
code, then waits and reports "paired: <name>" or "burnt"; `muster devices [--json]`;
`muster devices revoke <id|name|--all>`.

## Frontend structure

Separate entry, not responsive: `web/src/mobile.tsx` bundled to `dist/m/app.js` by a second
`deno bundle` in web/deno.json's `build` task (web/deno.json:8), which also copies
`web/m/index.html`, `manifest.webmanifest`, `sw.js` and icons; `check` gains the new entry. The
desktop app is keyboard-first, holds a window via `window.claim`, and manages split trees; none
of that belongs on a phone, and a mobile client must not claim a window.

Shared as-is: `types.ts`, `util.ts` (with a `muster.m.` key prefix, web/src/util.ts:97, so the
two UIs never share `localStorage`), `actions.ts` (`approve` web/src/actions.ts:324, `send`
:334). Not shared: `state.ts`. Its `applyHello` selects a channel and its effects mark channels
read (web/src/state.ts:433, :524-553), which would clear desktop unread state from the phone.
The mobile store is a small file with the session and channel maps and the event switch; it
calls `channel.mark_read` only when the user opens a session. Shared with changes:

- `rpc.ts`: a `Transport` seam (see Paid version) and an auth source: under `/m/` no
  `?token=` is added (:125-126). New `suspend()`/`resume()` so a deliberate close does not
  schedule a reconnect (:161-175), and a 30 s `ping` while visible, since a frozen socket can
  still report `OPEN` and `kick()` (:112-117) would skip it. Backoff (:179-184) is reused.
- `terms.ts`: a `resizePolicy` gate in `sendResize` (:305-316) that all paths go through:
  `onResize` (:230), `fitNow` (:365-375), `attach` (:406-419) and `reattachAll` (:422-433).
  On mobile it is `"explicit"`: only the `follow`-mode pre-action resize goes out. `attach`
  passes the mobile replay mode. `disableStdin` stays true regardless of the `connStatus`
  effect (:580-583), `term.textarea` gets `readOnly` and `inputmode="none"`, and a transparent
  overlay takes pointer events (scroll, pinch, selection via long-press) so the textarea is
  never focused: not confident `readOnly` alone keeps the Android keyboard down, since xterm
  can recreate the textarea. Note `disableStdin` also silences xterm's answers to colour/mode queries
  (docs/PROTOCOL.md:78-80); a session whose only viewer is the phone behaves like an
  unattached one, which is already a supported state. Leaving a session calls
  `session.detach` and `dispose` (`unmount`, :395-404, keeps terminals cached; the phone
  shows one at a time).

Screens (`web/m/index.html` sets `viewport-fit=cover, interactive-widget=resizes-content` so
Chrome shrinks the layout viewport for the keyboard instead of the page and `visualViewport`
maths do not double-offset):

0. **Pair**: on launch, `#pair=<code>` in the hash → `POST /api/device/pair` (then strip the
   hash); otherwise `GET /api/device/me`; on `401` show a short-code input and the hint to run
   `muster pair`. Shown again whenever `me` returns `401` ("unpaired").
1. **Sessions**: one row per live session, state dot, name, tab name, `state_detail`, time in
   state; `needs_human()` first (src/model.rs:53-57), then most recent. Live via
   `session_upserted`, no pull-to-refresh. Approve / Deny inline on rows that need approval.
2. **Session**: xterm read-only in a horizontally scrollable, pinch-zoomable container; a fixed
   bottom bar positioned with `visualViewport` (`offsetTop + height`), containing: the approval
   row (Approve / Always / Deny → `session.approve` with `state_since`, hidden otherwise), the
   chip row (Esc, Enter, Tab, Shift+Tab, ← ↑ ↓ →, Ctrl-C, 1 2 3 → `session.keys`; plus
   **Stop** → `session.interrupt`, which sends Esc to agents and Ctrl-C to shells,
   src/agents/claude.rs:456, src/agents/shell.rs:97), and a textarea with
   autocorrect/dictation/paste whose Send goes through `session.send` (bracketed paste, then
   Enter, src/agents/mod.rs:91-102; queued while busy, src/inject.rs:1-10). A queued reply shows
   "queued (n)" from `SendOutcome` (src/inject.rs:39-45). Long-press Send offers "Interrupt
   and send". A settings sheet holds `termSize` and notification opt-in.

## Terminal width

The daemon's resize is "last resizer wins" (src/rpc.rs:197-207 → src/pty.rs:460), and the
desktop re-sends a size on attach and reconnect (web/src/terms.ts:406-433) but otherwise only
when its own `cols/rows` change (:305-316, `sentCols/sentRows`), so an implicit phone resize
would leave the desktop pane wrong until its next layout change.

- **Option A, keep the desktop size.** Phone renders the session's real grid, shrinks the font
  to fit the width when that stays legible (≥ 9 px), otherwise pans. No protocol change; no
  reflow on the desktop. Cost: 160-column output is small or needs panning on a 6" screen.
- **Option B, ownership follows the last interaction.** Phone resizes the PTY when it acts;
  the desktop takes it back on pane focus or keypress. Every handoff makes the TUI redraw on
  both screens.

Decision: **B is the default, A is a per-client setting.** Both run on the same protocol.

- **Setting.** `termSize: "follow" | "keep"` (default `follow`), a per-viewer pref in
  `localStorage` (desktop: `muster.prefs`, web/src/state.ts:226-231; mobile: its own
  `muster.m.prefs`; `localStorage` survives PWA relaunches). A client choice, so it lives in
  the client, not the `devices` row; the daemon never knows the mode.
- **Protocol.** `SessionInfo` gains `size_owner: string | null` (`"local"` or
  `"device:<id>"`), set by `session.resize` from the caller's principal. `resize_session`
  already emits `session_upserted` on a size change (src/hub.rs:1534-1538 via :636); it must
  also emit when only `size_owner` changes. A-mode clients simply never call `session.resize`.
- **Phone, `follow`.** Before `session.send`, `session.keys` or `session.approve`, if
  `size_owner` is not this device, call `session.resize` with the grid that fits the phone
  (`estimateGrid`, web/src/terms.ts:570-576, after `setFontSize` with the mobile size and
  with the mobile host padding instead of the desktop constants at :548-549), then act.
  Viewing never resizes.
- **Phone, `keep`.** Never resizes; renders the real `cols/rows`, fits the font when legible,
  else pans and pinch-zooms.
- **Desktop, both modes.** Re-sends its fit size when the pane gets focus or a key is typed
  (`focusin` :231, `onData` :224) and `size_owner` is not `"local"`, resetting
  `sentCols/sentRows` (:309) first so the dedupe does not swallow it. A keypress costs nothing
  when the desktop already owns the size.
- **Everyone.** When `session_upserted` changes `cols/rows` for a shown session the client
  does not own, it sets `sentCols/sentRows` to the new size *first* and then calls
  `term.resize`; otherwise `onResize` (:230) would echo a `session.resize` and flip
  `size_owner` straight back to the desktop. The desktop pane keeps that smaller grid until its
  own next fit (pane resize, font change) or focus/keypress, which is the intended take-back.
  The redraw is the accepted cost of B and the reason A stays available.

## Notifications

| | Web Push | ntfy |
|---|---|---|
| Needs | HTTPS (have it), `sw.js`, VAPID keypair generated once into the data dir, `web-push` crate, outbound HTTPS from the daemon to the push service (FCM for Chrome Android) | one HTTP POST to ntfy.sh or a self-hosted ntfy, the ntfy app on the phone |
| Privacy | Payload encrypted end to end (RFC 8291); the push service sees the subscription, timing and length (pad payloads to a fixed size) | ntfy.sh sees "#channel · session: Needs approval: Bash(…)" unless self-hosted |
| Tap | Deep link into the PWA, `/m/#s=<id>` | Opens a URL in the browser |
| Delivery | Best effort (RFC 8030): set `TTL: 300`, `Urgency: high`; Doze can delay | Comparable, app keeps its own socket |
| Effort | ~2-3 days | ~half a day |

Recommend Web Push: no extra app, encrypted, opens the right session, works while the tab is
frozen. Protocol: `push.key {}` → `{public_key}`; `push.subscribe {endpoint, p256dh, auth}`
(device only, replaces this device's row); `push.unsubscribe {}`. Endpoint rules: `https`
only, host on an allowlist (`fcm.googleapis.com`, `*.push.services.mozilla.com`,
`*.notify.windows.com`, extendable via config), no redirects followed, 5 s timeout; rows
deleted on 404/410, on revoke and on expiry. Implementation: refactor `notify.rs` so
`decide()` (src/notify.rs:63-106, with its 60 s / 5 s dedup) feeds sinks; `Note`
(src/notify.rs:29-32) gains `session_id`, `state_since` and `channel_id`; the desktop worker
(:118-146) is one sink, push another; `MUSTER_NO_NOTIFY` (:35) then disables only the desktop
sink. The service worker uses `tag = session id` so a newer alert replaces an older one and
drops an alert whose `state_since` is older than one it already showed. Keep ntfy as a
one-function fallback (`MUSTER_NTFY_URL`) for people who self-host it.

## Reconnect and battery

Android Chrome may freeze or discard a background tab at any time; timers do not run while
frozen, so nothing can be "done after 30 s". Policy: on `visibilitychange` → hidden (and
`freeze`), `rpc.suspend()` closes the socket at once and the shown session is detached; on
visible, `resume()` reconnects (`hello`, `/api/device/me`) and reattaches with
`replay: {since}` from the last `attach_info` offset plus counted `0x01` bytes; the daemon
answers `delta` when that is ≤ 64 KiB and in the same generation/epoch, else `screen`. So a
quick app switch costs a `hello` and a small delta; a long absence costs a screen. Only the
session on screen is ever attached. Push covers alerts while hidden. The 2 MiB `full` replay
(src/pty.rs:29) is never requested by the phone. (`max_message_size` at src/server.rs:87 is
the inbound limit, unrelated.)

## Milestones

| Phase | Work | Effort | Done when |
|---|---|---|---|
| 0 Approve from phone | `remote_origins`, proxied-request policy and proxy-uid header trust, cookie auth, `Principal` + scope checks on all three paths, `devices` table, `device.*`, `/api/device/me`, `muster pair/devices`, `/m/` sessions screen with Approve/Deny (+`state_since`), CSP, ts_login pinning | 4-5 days | Phone on the tailnet approves a Claude Code permission prompt; a stale approve is refused; `muster devices revoke` disconnects it within a second and the PWA shows "unpaired". |
| 1 Terminal + composer | Read-only xterm, `resizePolicy`, `session.keys`, chip row, textarea via `session.send`, visualViewport bar, `attach_info` + `screen` replay, `size_owner` + `termSize` setting | 4-5 days | A dictated multi-line prompt to a busy agent is delivered once idle; Esc interrupts; output is live; in `follow` the PTY follows the phone on send and returns to the desktop on its next keypress. |
| 2 Notifications | VAPID, `sw.js`, `push.*`, endpoint rules, notify sinks | 2-3 days | Screen-off phone shows the alert typically within ~10 s of `needs_approval`; tapping opens that session; revoke deletes the subscription. |
| 3 Replay economy | `delta` replay, suspend/resume, ping | 2 days | Return after 10 min background on a busy session transfers < 100 KiB (`delta` ≤ 64 KiB or a `screen`), measured in devtools. |
| 4 Polish | `manage` scope UI (new tab), install prompt, WebAuthn gate (if wanted), transport/auth seams tidy enough for the paid track | ongoing | Per item. |

## Paid version (later)

A native Android app that works without Tailscale, for people who will not set up a tailnet.
Nothing in phases 0-4 is thrown away if two seams exist from the start:

1. **Transport.** Frontend: `rpc.ts` talks to a `Transport` interface (`open(auth)`,
   `sendText`, `sendBinary`, `onMessage`, `onClose`) instead of `new WebSocket` directly
   (web/src/rpc.ts:125-134). `WsTransport` now; `IrohTransport` later, a thin bridge to a Tauri
   command that owns the connection in Rust. Daemon: `ws_session`'s per-connection loop
   (src/rpc.rs:346) takes a generic `Stream<Message> + Sink<Message>` plus a `Principal`, so an
   iroh connection feeds the same dispatcher, scope checks and attach/forwarding code. Auth is a
   `Principal` resolver per transport (cookie or client token for HTTP, node key for iroh).
2. **UI.** `mobile.tsx` uses nothing from `location`, cookies or the service worker directly;
   auth and push registration go through an `Env` object the entry point supplies. The same
   bundle then drops into a Tauri v2 mobile WebView unchanged; only `Env` differs.

Design sketch:

- **Connectivity: iroh** (QUIC, NAT hole punching, relay fallback, end-to-end encrypted to the
  desktop node's public key). n0's public relays first; self-hosted relay later. The daemon
  opens an iroh endpoint with ALPN `muster/1` and carries the same JSON/binary framing.
- **Pairing:** QR of the desktop's iroh node id plus a pairing code, checked by the same
  `device.*` machinery; the device row stores the phone's node key instead of a token hash.
- **Push:** FCM through a small push server that only ever sees encrypted payloads (the
  desktop encrypts to a key the app generated). Same `notify.rs` sink interface as Web Push.
- **Licensing (idea):** free companion app on Play, paid desktop licence sold via Stripe or
  Paddle; the daemon checks the licence at pairing time (offline grace). Guess to verify: Play
  Billing takes 15 % on the first $1M and its rules on out-of-app purchases keep changing, so
  confirm the "free app, licence bought elsewhere" shape is allowed before building it.
- **Terms:** re-read the Claude Code and Codex terms (PLAN.md §3) before charging money for
  anything that drives those CLIs; PLAN.md §3 also forbids their names in branding.
- **Ops:** relay availability, push server uptime and key rotation, Play listing and review,
  crash reporting, support. This is a service, not a binary.
- **iOS later:** APNs, Apple Developer Program ($99/yr), Tauri iOS target, TestFlight.

Rough effort: iroh transport + pairing 1-2 weeks; Tauri app shell, build, signing 2-3 weeks;
push server + FCM 1 week; store listing and policy work days; about 1.5-2 months total on top
of the Tailscale/PWA phases.

## Test plan

- Rust unit tests: token hashing and `token_eq`; scope enforcement as a table over every
  method name plus attach/detach and a `0x02` frame from a `view` device; pairing TTL, single
  use, long/short alias consumed atomically, burn after 3 failures, concurrent redemption (one
  wins); origin rules per auth kind (bearer without `Origin` still works, cookie without or
  with `null` `Origin` fails); proxy-uid gating of identity headers; ts_login mismatch refused;
  `state_since` mismatch on approve; revoke during an open socket (next call fails) and
  expiry during a live socket; `session.keys` encoding in both cursor modes; replay kinds:
  `delta` vs `screen` vs `full`, ring rollover, generation and epoch mismatch, size cap.
- Rust integration test (tokio-tungstenite, which can set `Cookie` and `Origin`): pair →
  `hello` and a `0x02` frame over `/ws` with the cookie → `device.revoke` → socket closed,
  `/api/device/me` `401`; a proxied request (forged `X-Forwarded-For`) with the client token
  gets `401`. `scripts/smoke.ts` covers the HTTP side only (`fetch` can send `Cookie`, the
  browser `WebSocket` cannot) and re-runs the existing bearer CLI paths as a regression.
- Web: `deno test` for viewport math, offset counting and the replay-kind handling; headless
  Chromium via `web/dev/cdp.ts` with `Emulation.setDeviceMetricsOverride` (412×915, mobile,
  touch) against `dev/mock.ts` for screenshots of the screens; the keyboard-open layout is
  approximated by shrinking the emulated viewport height (do not resize the desktop window
  under Hyprland). Headless cannot exercise the Android IME or push; those are device-only.
- Real device, each phase: run the `tailscale serve` line `muster pair` prints; scan with the
  camera; Chrome → "Install app"; checklist: approve, deny, approve after the prompt changed
  (refused), dictate a prompt to a busy agent, Esc, Ctrl-C in a shell, background 10 min then
  return (network panel shows a delta or screen, not megabytes), airplane mode on/off, revoke
  from the laptop and confirm "unpaired", second device pairing, wrong code ×3 then burnt,
  `follow` resize and desktop take-back. Use `chrome://inspect` over USB for console and
  network sizes.

## Open questions

1. Width is decided (B default, A as a setting). Should `follow` also resize on plain
   viewing after a long press on the terminal, or only on send/keys/approve as planned?
2. Default scope for `muster pair`: `interact` (recommended) or also `manage` (spawn tabs)?
3. Web Push (daemon makes outbound HTTPS to Google's push endpoint) or self-hosted ntfy?
4. Pin devices to `Tailscale-User-Login` as planned (enforced only when the header is present
   and trusted)? It makes tailscale serve the only proxy that gets pinning.
5. 30-day sliding expiry acceptable, or a fixed lifetime?
6. Require phone unlock (WebAuthn user verification) before `approve_always` or `manage` calls?
7. Should the desktop notification be suppressed when the phone has the session open? (Plan says no.)
8. Paid track: is the free-companion-app + paid-desktop-licence split the model to design the
   pairing check around, and Stripe or Paddle (merchant of record, handles VAT)?

# muster CLI

Everything the UI can do to a running agent, from a shell or from another agent. The CLI talks
to the daemon over `POST /api/rpc` with the client token from the discovery file (see
docs/PROTOCOL.md). It starts the daemon when needed, except for `ls`/`status`, which just report
that nothing is running (`ls` prints `muster daemon: not running`, or `[]` with `--json`, and
exits 0). The design follows herdr's `agent list/read/prompt/wait` commands.

## Targets

Commands that act on a session take a **target**:

| Form | Meaning |
|---|---|
| `12` | session id 12 |
| `.` | this session (`$MUSTER_SESSION_ID`; error outside a muster session) |
| `fix-auth` | exact session name, else exact tab (channel) name, case-insensitive |
| `fix` | unique case-insensitive prefix of a session name or tab name |
| `tab/name` | session `name` inside tab `tab` (either part may be a prefix) |
| `fix src/main.rs` | a string that is exactly a session or tab name is taken whole, even with a `/` in it |

A tab name resolves to that tab's agent session when it has exactly one live agent (claude or
codex), else its only live session. Archived tabs and removed sessions are never matched. An
exited session only matches by id or exact name, and an exact name matches exited sessions only
when no live session has it. All digits is always an id (never a name or prefix). Ambiguous targets fail with exit code 2 and
list the candidates (`id  tab/name  kind  state`).

## Commands

| Command | What it does |
|---|---|
| `muster` | Open a new window (reclaims a closed window with tabs first). The native window runs in its own detached process, so the command returns at once. |
| `muster open [path] [--window N]` | Open a window: window N, else a new one. With a path: add the folder (`workspace.add`), create a window unless `--window` is given (`window.create`), open a shell tab there (`channel.create` with `agent: "shell"`), then open the UI on that window. A shell rather than an empty task: since v2 a tab is a session, so a task without one has no tab and no window would show it. |
| `muster ls [--all] [--json]` (alias `list`) | Table of live sessions: `ID  WIN  TAB  NAME  KIND  STATE  FOR  DETAIL`. `FOR` is the time in state (`3m`). `--all` includes exited sessions. `WIN` is the session's own window (a tab is a session, so a task's sessions may be in different windows), or `hidden` for a named agent whose tab was closed (it still runs). `--json` prints `SessionInfo[]` (which carry `window_id`) with `channel_name` added. |
| `muster read <target> [-n LINES] [--screen] [--json]` | Print the last LINES (default 50) lines of output as plain text. `--screen` prints the visible screen instead. Trailing blank lines are dropped. `--json` prints `{id, lines}`. |
| `muster send <target> [TEXT...] [--interrupt] [--wait] [--until STATE]... [--timeout SECS]` | Send a prompt through the injection queue (bracketed paste, then Enter; queued while the agent is busy). TEXT words are joined by spaces; with no TEXT or `-`, stdin is read. `--interrupt` interrupts a busy agent first. `--wait` blocks until the agent has started and then reached a settled state (the default `until` set, see `wait`), prints the final state line (`id  tab/name  state  detail`) and exits 0, or 124 on timeout. `--until` implies `--wait`; `--timeout` (default 600) applies to the whole wait. Without `--wait` it prints `tab/name: sent` or `tab/name: queued (N waiting)`. Text starting with `-` goes after `--`. |
| `muster wait <target> [--until STATE]... [--timeout SECS] [--json]` | Block until the session is in one of the states. Default: idle, needs_approval, asking_question, error, stuck or exited. `needs_human` is accepted as a group. Prints the state line and exits 0 when matched, 124 on timeout (default 600 s). `--json` prints the `session.wait` reply. |
| `muster keys <target> KEY...` | Send keys: `enter`, `esc`, `tab`, `space`, `backspace`, `up/down/left/right`, `ctrl+<x>`, `alt+<x>` (a character or key name), `shift+tab`, `home`, `end`, `delete`, `pageup`, `pagedown`, modified cursor keys (`ctrl+left`, `alt+up`, `ctrl+shift+end`: xterm `CSI 1;m X`), a single character (itself), or `text:<literal>`. Names are case-insensitive. All keys go in one `session.input` with `keys: true`, so unmodified cursor keys follow the terminal's cursor mode. |
| `muster approve <target> [--always \| --deny]` | Answer the permission prompt. |
| `muster interrupt <target>` | Interrupt (Esc for agents, Ctrl-C for shells). |
| `muster new [PATH] [--profile PROFILE] [--kind claude\|codex\|shell\|none] [--worktree] [--base REF] [--name NAME] [--window N] [--prompt TEXT] [--json] [-- AGENT_ARGS...]` | New tab in PATH (default: cwd), default kind claude (`--agent` is the flag's old name, still accepted), no worktree unless `--worktree` (`--base` needs it). No `--name`: auto-named. `--window N` puts the tab (the task's first session) in window N, which must exist. No `--window`: inside a muster session, that session's window, else the daemon's default. `--kind none` creates a task without a session: it has no tab, so no window shows it until a session starts in it; the daemon closes a task when its last session is removed, never one that never had one. Arguments after `--` go to the agent CLI of the tab's first session, after muster's permission flags (`channel.create` `extra_args`), and again on its restarts: `muster new . --kind codex --prompt "hi" -- -m gpt-6-sol -c model_reasoning_effort=xhigh`. They need an agent (not `--kind none`), and only an allowlist of flags is accepted (model, effort, resume and similar; see docs/PROTOCOL.md "Channel permissions"): anything that could change the agent's permissions is refused, set the tab's permissions instead. Prints the new session id; `--json` prints `{channel, session}`. |
| `muster move <target> --window N` | Show the session (tab) in window N (`session.set_window`, with `from_window_id` so a concurrent move isn't overridden). Window N must exist. The session stays in its tab (task); the task's window follows once all its sessions are in N (or none is left in the task's window). Prints `id  tab/name: window A -> N`. The window isn't opened or raised. |
| `muster rename <target> <name> [--tab]` | Rename the session, or its tab with `--tab`. |
| `muster kill <target>` / `muster restart <target>` | Kill or restart the session. `muster kill` also stops a named agent whose tab was closed (hidden). |
| `muster events [--json] [--session TARGET] [--tab NAME]` | Stream daemon events (`GET /api/events`) until Ctrl-C, so scripts don't poll. Default: one line per session state change, `HH:MM:SS  ID  TAB/NAME  FROM -> TO  DETAIL` (local time of the change; `FROM` is `new` for a new session, `TO` is `removed` for a removed one). Each message to a named agent adds a line `HH:MM:SS  msg ID  FROM -> TO  origin=O hop=N  FIRST LINE` (with `refused: WHY` or `replied`). `--json`: every `session_upserted`, `session_removed`, `channel_upserted`, `channel_removed`, `window_upserted`, `window_removed`, `agent_upserted`, `agent_removed` and `agent_message` event, one JSON object per line exactly as the daemon sent it, flushed per line (plus `lagged`, below). `--session` keeps one session's events (and its tab's, its tab's window's and its own window's); `--tab` (exact tab name, else unique prefix) keeps that tab's, including a session moving out of it. Both are resolved once, at the start. Never starts the daemon: exit 1 when none runs. When the daemon stops, it prints `muster: the daemon stopped` to stderr and exits 0. After `lagged` (the reader fell behind) it re-reads the state and says so on stderr. |
| `muster pair [--name N] [--scope view\|interact\|manage] [--ttl SECS] [--url URL]` | Pair a phone (docs/ANDROID.md). Prints the `tailscale serve` commands when tailscale doesn't already proxy to this daemon's port, a QR code of `https://<host>/m/#pair=<code>` (needs `qrencode`), and the short code, then waits: `paired: <name> (device N)` and exit 0, or exit 1 when the code is burnt (three wrong codes), replaced or expired. Default scope `interact` (approve, prompt, keys); `manage` also opens and closes tabs. `--url` overrides the tailscale name. |
| `muster devices [--json]` | Paired devices: `ID  NAME  ACCESS  LAST`. Never starts the daemon. |
| `muster devices revoke <id\|name>` / `--all` | Revoke: the device's connections close at once and it must pair again. |
| `muster agent new <name> [--profile PROFILE] [--kind claude\|codex] [--model M] [--effort E] [--persona TEXT \| --persona-file F] [--accepts ORIGINS] [--autostart] [--claude-mode MODE] [--codex-sandbox SANDBOX]` | Create an agent (PLAN.md §10, `agent.create`): its home dir `<data>/agents/<name>/` with `CLAUDE.md` / `AGENTS.md` from the persona and `memory/`. Default kind claude. `--accepts` is a comma list of `human`, `schedule`, `webhook` (default: `human` for claude, all three for codex). `--autostart`: start it whenever the daemon starts. `--claude-mode plan\|default\|accept_edits\|bypass` / `--codex-sandbox read_only\|workspace_write\|workspace_write_network\|full_access`: its tab's profile (the other half keeps its default). Prints `agent NAME (KIND, accepts …): home DIR`. |
| `muster agent edit <name> [--model M] [--effort E] [--persona TEXT \| --persona-file F] [--accepts ORIGINS] [--autostart on\|off] [--claude-mode MODE] [--codex-sandbox SANDBOX]` | Change an agent (`agent.update`): only the given fields change; an empty `--model ""` / `--effort ""` clears it. A new persona rewrites its persona file (memory/ untouched). One permission flag keeps the other half as it is. Model, effort and permissions apply from its next start. Prints `updated NAME (…)`. |
| `muster agent ls [--json]` (alias `list`) | `NAME  KIND  MODEL  ACCEPTS  SESSION  STATE`. `--json` prints `agent.list`. |
| `muster agent rm <name>` | Remove the agent and its session; the home dir (persona, memory) stays. |
| `muster agent start <name> [--fresh]` | Start the agent's session (cwd = its home dir, in the agent's own tab in the "agents" workspace), restart it if it exited, or report the running one. Claude agents resume their last conversation; `--fresh` starts a new one (restarting a running agent). Prints the session id (`… (already running)`). An agent whose tab was closed keeps running hidden; this doesn't show it (the agents panel does). |
| `muster ask <agent> [TEXT...] [--timeout SECS] [--json]` | Ask an agent and print its reply (`agent.ask`): starts it if needed, delivers with an envelope, waits for this message's turn to end (default 600 s, max 1800). TEXT as for `send` (none or `-`: stdin). Inside a muster session (`$MUSTER_SESSION_ID`) the ask is that session's: an agent's runs MCP `ask_agent` (its origin and hop, permission check, rate limit); outside one it is the human's (`human`, hop 0). There is no flag to set the origin. Exit 0 with a reply, 124 on timeout (the message stays delivered), 1 when refused or the agent stopped without a reply (a prompt for the human, an error). If the agent reported no final message for this message, the end of its terminal is printed after a note on stderr that it may belong to another prompt (exit 0). `--json` prints the `agent.ask` reply. |
| `muster note ls/new/read/write/append/rename/open/close/rm` | Plain-text notes shared with agents. See "Notes" below. |
| `muster pr list --repo OWNER/NAME [--limit N]` / `muster pr snapshot --repo OWNER/NAME NUMBER [--media-dir DIR] [--max-diff-bytes N]` | Read-only GitHub fetches for PR reviews, printed as JSON; no daemon needed. See "PR snapshots" below. |
| `muster queue TARGET [--clear\|--resume]` | Inspect waiting prompts and interrupted delivery records as JSON. After inspecting the terminal, `--resume` forgets only the interrupted record and lets the waiting prompts continue (the interrupted prompt is not resent); `--clear` discards all the queue records. Neither undoes commands or erases terminal input. |
| `muster ui-install PATH` | Stage and activate a complete built UI directory, without restarting either service. Refresh the window to load it (open windows offer a "Reload" banner within a few seconds). Previous release directories are retained for rollback and existing pages. Refuses incompatible manifests and symlinked content. `MUSTER_WEB_DIR` on the backend overrides installed releases. |
| `muster reload` | Replace the application backend while its terminal host and terminal processes keep running. Clients reconnect to the same endpoint. A window's "Restart backend" banner runs this from the path the backend was started from (`daemon.reload`). |
| `muster stop` | Explicitly stop both backend and terminal host, terminating sessions. A window's "Shut down Muster…" does the same (`daemon.shutdown`). |
| `muster status` / `url` / `daemon` / `hook` | As before. With the native window on Linux, `status` also prints a `next native window:` line: the WebKitGTK renderer a window opened from this shell would use and why, and the fallback marker's path when there is one (README.md, "Native window renderer"). |

New sessions receive `$MUSTER_BIN` pointing to `<data>/bin/muster`; agent hooks use the
same stable executable path. Successful backend startup atomically switches it to a
private copy of that build, so callers survive deletion of the original checkout.
The first startup requires both executables side by side; backend-only updates can
reuse the saved host binary. This never restarts a running host. Executable snapshots
under `<data>/application-releases` are retained rather than pruned during updates.

Inside a muster session, `send`, `keys`, `ask` and `new` pass `$MUSTER_SESSION_ID` to the daemon
as `from_session`, so what an agent does through the CLI is attributed to it: `send` and `ask` run
as its MCP `send_message` / `ask_agent` (same guards and rate limits), and `keys` and `new` carry its
provenance (docs/MCP.md "Agents"). From a shell tab they carry that shell's provenance, which is the
human's unless an agent typed into it.

Output: human tables go to stdout, errors to stderr as `muster: <message>`. Exit codes: 0 ok,
1 error (including a `wait`/`send --wait` whose session exited before reaching the state),
2 bad or ambiguous target, 124 timeout.

`send --wait` on a shell: shells only show as working while a foreground job runs (sampled
every 250 ms), so a command that finishes at once is taken as started after about 1.5 s
(agents: 10 s).

## Panels

Run inside a Muster session (`MUSTER_SESSION_ID` is required). Commands use the same
task-scoped backend and limits as the MCP tools and print JSON.

```sh
muster panel open --title Review --app markdown --data '{"markdown":"# Review"}'
muster panel open --title Screenshot --app image --data '{"path":"shots/login.png","alt":"Login page"}'
muster panel open --title Preview --app custom --html-path ./review.html --data '{}'
muster panel update 1 --data '{"markdown":"# Updated"}' --title 'Updated review'
muster panel list
muster panel wait 1 --timeout-seconds 300
muster panel event 1 42
muster panel close 1
```

Open/update also accept `--html '<html>…</html>'` instead of `--html-path`. Open accepts
`--csp '{"resourceDomains":["https://cdn.example"]}'`; fetching remains blocked. HTML paths
must stay in the task cwd. Omit the ID on `wait` for any panel in the task; timeout is 0–1800
seconds (default 300). A timeout prints `{events: [], remaining: 0, timed_out: true}` and exits
successfully; a wait fails if the session moves to another task or restarts. `event` fetches
the full event that a panel prompt showed only as a placeholder (panel ID, then event ID).

## Notes

Plain-text notes you and agents share (docs/UI.md "Notes", docs/PROTOCOL.md "Notes"). Work
inside or outside a session; inside one, writes are attributed to that session's agent and share its MCP rate limit (a shell
tab counts as you). `<note>` is an id (all digits), an exact title (case-insensitive) or a unique
title prefix; ambiguous or unknown ones exit 2.

| Command | What it does |
|---|---|
| `muster note ls [--json]` (alias `list`) | `ID  REV  BYTES  OPEN  UPDATED  BY  TITLE`. `--json` prints `note.list`. |
| `muster note new [--title T] [TEXT...]` | Create a note (empty without TEXT; `-` reads stdin). Prints its id. Not shown until opened. |
| `muster note read <note> [--json]` | Print the text exactly (no newline added). `--json` prints the whole note, with its `rev`. |
| `muster note write <note> --rev N [TEXT...]` | Replace the text (none or `-`: stdin, as is). `--rev` is the revision you read; refused when the note changed since. Prints `note ID: rev R`. |
| `muster note append <note> [TEXT...]` | Add at the end, on a new line (none or `-`: stdin, trailing newlines dropped). Never conflicts. |
| `muster note rename <note> <title>` | Rename it. |
| `muster note open <note> [--window N]` | Show its tab in window N, else beside this session (needs a session). |
| `muster note close <note>` | Close its tab; the note stays. |
| `muster note rm <note>` | Delete it for good. Refused from an agent's session. |

```sh
muster note new --title "Release checklist" "- [ ] changelog"
echo "- [ ] tag v0.4" | muster note append release
muster note read release --json | jq .rev     # then: muster note write release --rev 3 - < plan.txt
```

## Agent skills

Muster ships two agent skills (in `skills/`, embedded in the binary): `muster-pr-review`, the
PR decision flow (review one snapshot, open a `pr_decision` panel, merge only on its Merge
event; for "help me review the open PRs", a `pr_overview` panel of every open PR first), and
`muster-panels`, a short guide to panels, the built-in apps and custom MCP Apps.
An agent loads one when the request matches its description ("review PR 812", "chart this").
Both only work inside a Muster session and say so otherwise.

```sh
muster skills install            # Claude Code (default folder and every profile) and Codex
muster skills install --claude   # or --codex; --dry-run shows the changes, writes nothing
muster skills status
```

A skill runs in your current agent, so the PR's text enters its context; for PRs from people
you don't trust, start a separate agent from the PR reviewer template (New agent) to keep it
isolated. Loading a skill never widens what you allowed the agent to do.

Targets: Claude Code `$CLAUDE_CONFIG_DIR/skills`, else `~/.claude/skills`, plus
`<folder>/skills` for every Claude profile (`muster profile ls`, disabled ones included), since
a session started under a profile reads skills from that profile's folder. The profiles are
read from Muster's database, read-only, so the daemon need not run. Each real folder is one
target: a profile that points at the default folder shares its line, labelled
`claude (default, Name)`; profile lines read `claude (Name)`. A profile folder that is missing
is reported as skipped and never created. A profile folder is only written to if its stored
path is absolute, has no control characters, resolves (symlinks followed) to a directory inside
your home other than home itself, and holds `settings.json`, `projects/`, `plugins/` or
`.claude.json` (checked by name only, never opened); anything else is reported as "skipped (not
a Claude config folder under your home)". Each folder is resolved once when planning and its
identity pinned: if it is moved or replaced before Muster writes, that target stops with
"changed while installing". Names and paths are printed with control characters escaped. Codex: `$CODEX_HOME/skills`, else `~/.codex/skills`
(where Codex's own skill-creator puts user skills); profiles do not change Codex's folder.
After adding a profile, run `muster skills install` again to cover it.
Each skill is `<target>/<name>/SKILL.md` (mode 0600, folders 0700), with a `.muster-installed`
marker beside it holding the SHA-256 of what Muster wrote. `install` lists each file as new,
updated, unchanged or locally modified. It writes nothing at all if a file was edited since
Muster wrote it, or Muster did not write it (`--force` replaces those), or if the `skills`
folder, a skill folder, SKILL.md or the marker is a symlink (even a dangling one), a special
file or larger than 256 KiB (refused even with `--force`: move it away). Each file is written
through an exclusively created, randomly named temporary file and a rename, SKILL.md before
its marker, and is checked again just before: a file that changed since the plan is left
alone and reported as "changed while installing". If a write fails, every file's result is
still printed and the command exits 1. Running it again is a no-op. `status` shows each skill
and target as missing, current, outdated (Muster's older version), locally modified or
blocked. The marker only records what Muster wrote; it is not proof of origin. Nothing else in those folders is touched. Start
a new agent session afterwards to pick the skills up.

## PR snapshots

`muster pr list` and `muster pr snapshot` fetch what a PR review needs from GitHub in one go,
in parallel and without the model, and print one JSON object that fits the `pr_overview` and
`pr_decision` panels (docs/MCP.md, "Built-in apps"). The agent adds only its judgement:
summary, risk, notes, questions. They never write to GitHub (no merge, review, comment or
label), need no running daemon, and use `gh` (logged in with `gh auth login`) and `curl`.

```sh
muster pr list --repo acme/api                  # open PRs, newest first (at most 200)
muster pr snapshot --repo acme/api 812          # one PR, pinned to its head commit
muster pr snapshot --repo acme/api 812 --verbose --max-diff-bytes 200000
```

`muster pr list --repo OWNER/NAME [--limit N] [--parallel N] [--verbose]` prints
`{repo, total, truncated, prs: [{number, title, author, url, created_at, draft, additions,
deletions, files, checks, review, labels, head_sha, base}]}`. `total` counts every open PR,
`truncated` is true when `prs` holds fewer (`--limit`, 1 to 200, default 200). `checks` is
`passing`, `failing`, `pending` or `none` (GitHub's rollup for the head commit); `review` is
`APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, `NONE` or `""`; at most 20 labels.

`muster pr snapshot --repo OWNER/NAME NUMBER [--media-dir DIR] [--max-diff-bytes N]
[--parallel N] [--verbose]` reads the PR's head, base and the repo's merge settings first,
then fetches everything else for exactly that head SHA: the diff from the merge base
(`compare/BASE_SHA...HEAD_SHA`, not `gh pr diff`, which follows the moving head), the check
runs and commit statuses of the head commit, and the pictures in the description. Then it
reads the PR again and fails with exit 3 if the head moved or the base branch was changed
meanwhile (new commits on the base branch do not count: the diff is from the merge base).
It prints:

```
{repo, number, title, author, url, state, draft, base, base_sha, head, head_sha,
 mergeable, merge_method_default, delete_branch_default,
 checks: {state, items: [{name, state, url?}]}, reviews: {decision, items: [{author, state}]},
 body, files: [{file, status, previous_file?, additions, deletions, binary, diff, truncated, note?}],
 files_complete, media: [{kind: "image", path, alt}], media_dir, media_skipped: [{url, reason}],
 fetched_at, checks_complete, checks_total?}
```

- `state` is `OPEN`, `MERGED` or `CLOSED`; `mergeable` is GitHub's (`MERGEABLE`,
  `CONFLICTING`, `UNKNOWN`). `merge_method_default` is GitHub's default for you when the repo
  allows it, else the first allowed of squash, merge, rebase; `delete_branch_default` is the
  repo's "delete head branches" setting.
- `checks.items[].state` is `pass`, `fail` (failure, timed out, action required, startup
  failure), `pending` (not completed), `skipping` (skipped, neutral, stale) or `cancel`;
  check runs are named `Workflow / job`. `checks.state` is `failing` if any fails, else
  `pending` if any is pending, else `passing`, and `none` without checks. All checks are
  read, 100 per request, up to 2000 (20 requests); `checks_complete` is false when there were
  more. Then `checks.state` is never `passing` or `none`: `failing` if a failure was already
  seen, else `pending`. `items` holds at most 200 (failing and pending first); `checks_total`
  (GitHub's count) appears when there are more, or when the list is incomplete. Only https
  `url`s are kept.
- `reviews.items` is each reviewer's latest review; `decision` is GitHub's review decision or `""`.
- `files[].diff` is that file's unified diff with its git header lines. All diffs together
  stay within `--max-diff-bytes` (default 400000), shared fairly: small files stay whole, the
  big ones are cut at a line end and marked `truncated`; one file's diff is at most 100000
  bytes (pr_decision's limit). `additions`/`deletions` always count the whole file. A file
  with no diff lines gets a `note` saying why: "renamed from OLD, content unchanged",
  "copied from OLD, content unchanged", "file mode changed only", "new empty file", "deleted
  empty file" or "binary file: no text diff". Only if GitHub refuses the diff as too large
  (HTTP 406/422), the files come from its JSON API instead (any other error, a rate limit
  above all, stands). That API lists at most 300 files (`files_complete` false when that
  limit may have been hit), and leaves out the text of huge files (empty `diff`, `truncated`,
  note "GitHub left out this file's diff (too large)"). It never says which files are
  binary, so `binary` is false there and a file without lines gets a note such as "new empty
  file (or a binary file: GitHub's file list doesn't say)".
- `body` is the raw description. All PR text is data: nothing from it reaches a command line.
- Pictures: only `https://github.com/user-attachments/assets/<id>` and
  `https://<name>.githubusercontent.com/<path>` URLs from the description (Markdown images,
  `<img>` tags, bare attachment links) are downloaded, the first 4, in parallel, each with
  curl at most 1.5 MB and 20 s, https only (redirects too, at most 5), and kept only if they
  are PNG, JPEG, GIF or WebP. For an attachment, GitHub's signed link from the rendered
  description is used, so private repos work too. That link is a short-lived credential: it
  goes to curl as a config on stdin (`curl -q ... -K -`), never on a command line (where
  other local users could read it in the process list), and is never printed. The pictures go
  into a new `.muster-pr-XXXXXXXX` directory (0700, random name) in the current directory, or
  `--media-dir DIR` (inside the current directory; its parent directories must exist and none
  may be a symlink; DIR itself must not exist yet; created 0700). Directories are opened one
  by one from the current directory without following symlinks, and the pictures are written
  as `1.png` and so on (0600, created exclusively, never through a symlink). `media[].path` is
  relative to the current directory, ready for pr_decision's `media`; `media_dir` is null
  when nothing was saved (the directory is then removed). Every other picture URL, and every
  failed download, is listed in `media_skipped` with the reason. Delete the directory after
  the review.
- On exit 3 or 4 the pictures are removed again.

GitHub limits: at most 4 requests to GitHub at once per command (`--parallel N` lowers it;
picture downloads count too). On GitHub's secondary rate limit it waits (its Retry-After,
else 2, 4, 8 s; at most 3 retries; a wait over 120 s stops instead); when the hourly limit
is used up it stops at once with `{"error": "rate_limited", "detail", "reset_at"}`.
A snapshot makes about 4 API requests (the PR, the diff, the checks, the PR again; one more
per 100 checks, up to 20 for checks, one more when the diff is too large) plus at most 4 picture downloads. A
list makes one request per 100 PRs for the numbers and one per 25 for the cards, run side by
side.

Exit codes: 0 ok; 2 bad arguments (repo must match `^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`, the
number a positive whole number; nothing is fetched); 3 `{"error": "head_moved", detail,
head_sha, head_sha_now, base, base_now, state}`: run it again; 4 the PR is not open: the JSON
still has its `state`, `merged_at`, `merge_commit` and `closed_at` (closed at the start:
with the identity fields only; closed while reading: with the full snapshot); 5 gh failed:
`{"error", "detail"}` with error `gh_missing`, `gh_auth` (run `gh auth login`), `not_found`,
`rate_limited` or `gh_failed`. stdout is only the JSON; stderr is for humans (`--verbose`
adds each request's time).

## Claude profiles

Profiles are Muster-owned names and references to existing Claude configuration directories.
Muster never logs in, opens authentication files, copies configuration, or deletes a profile
directory. Set up each subscription login directly in the official Claude Code CLI first:

```sh
CLAUDE_CONFIG_DIR="$HOME/.claude-personal" claude
CLAUDE_CONFIG_DIR="$HOME/.claude-work" claude
```

For the default `~/.claude` folder, sign in with plain `claude` (or `env -u CLAUDE_CONFIG_DIR
claude`) instead: Muster starts that profile's sessions without `CLAUDE_CONFIG_DIR`, and setting
it, even to `~/.claude`, makes Claude Code keep a separate `~/.claude/.claude.json`.

Then register and select the directories:

```sh
muster profile add Personal "$HOME/.claude-personal"
muster profile add Work "$HOME/.claude-work"
muster profile ls --json
muster new . --profile Work
muster profile default . Personal
muster new . --profile default
muster agent new reviewer --profile Work
```

`--profile` accepts an exact name (case-insensitive), numeric profile ID, or `default`.
Without it, `muster new` inherits the calling Claude session's profile before the workspace
default. Outside a Claude session it uses the workspace default, or the ordinary inherited
Claude environment. `--profile default` explicitly selects that ordinary environment. The
flag is only valid for Claude. Named agents default to the ordinary environment and keep
one fixed profile for their lifetime, including fresh conversations and replacement sessions.

| Command | Meaning |
|---|---|
| `muster profile ls [--json]` | List references, including disabled profiles. Does not check login status. |
| `muster profile add NAME DIRECTORY` | Register an existing directory. Relative CLI paths are resolved locally; duplicate canonical paths are refused. |
| `muster profile rename PROFILE NAME` | Change the display label. Directory and session bindings stay fixed. |
| `muster profile disable PROFILE` | Prevent new launches and restarts; running sessions remain usable. |
| `muster profile enable PROFILE` | Allow new launches again. |
| `muster profile default PATH PROFILE` | Set a workspace default for future Claude tasks; `default` clears it. |

Use `/status` inside Claude Code to verify the actual account. A profile label identifies a
directory, not a checked identity. Known authentication or endpoint environment overrides
block named-profile launches with an explanation; no values are printed and nothing is
silently unset. Existing/default sessions preserve their original environment behavior.
Profile bindings survive session restart and backend recovery. Changing workspace defaults
never changes an existing session. To use a different directory, create a new session or agent.
Usage reporting, automatic placement, and account handoff are not implemented yet.

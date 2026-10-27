# Claude account profiles and machine placement

Status: local profiles implemented, 2026-10-18. Usage display and placement suggestions
(manual choice plus a recommendation) implemented, 2026-10-26. Automatic placement and handoff
remain deferred. This expands the account and
quota-aware scheduling ideas in [PLAN.md](../PLAN.md). The existing terminal backend remains
the starting point.

Muster should let one person use two Claude accounts through one interface, initially on one
machine and eventually across multiple backend machines. Each Claude process uses a selected
account profile. Muster can report observed usage, suggest where to start a task, and later
offer a deliberate handoff when an account reaches a limit.

Keep machine selection separate from account selection. A second machine is useful for
execution capacity and environment differences, but is not required to use a second account.
Neither an inference proxy nor an Agent SDK migration is required for this plan.

Implementation constraint: run the official Claude Code normally. Muster manages only its
own profile labels and directory references, and selects a directory through the child
process environment. It never opens authentication files, invokes login or logout, or
creates, copies, edits, deletes, refreshes, or inspects provider credentials. The human signs
in and verifies the active account inside Claude Code. A profile identifies a configuration
directory, not a verified account identity.

## Local implementation

- Register an **existing** configuration directory through `muster profile add NAME DIR` or
  **Settings and more → Claude profiles**. Muster stores its canonical path, label, enabled
  state and ID in its own database. Directory contents are never opened. Duplicate paths
  (including symlink aliases) are refused. Renaming changes only the label; changing the path
  requires registering a new profile. Disable retains historical references and never stops
  a running process. There is no profile-directory deletion operation. The one exception
  to "never opened": `muster skills install` writes Muster's own skills into every profile's
  `skills/` folder (and only there), so profile sessions find them; see
  [CLI.md](CLI.md#agent-skills). The other: at each Claude launch Muster reads the `statusLine`
  key of the settings files that session would use, its profile's `settings.json` included, to
  keep showing the user's own status line beside its usage reporter, never through a link (see
  "Usage display and suggestions" below). Nothing else in those files is kept.
- Select a profile with `muster new --profile NAME`, the New Tab dialog, or
  `muster agent new NAME --profile PROFILE`. Named-agent profile bindings are fixed at
  creation, including for replacement sessions and `--fresh`. Create another named agent
  to use another profile; switching an existing agent is part of the deferred handoff work.
  The New Tab dialog starts with explicit **Default**; choose the profile for each new
  session. Profile settings do not show the backend's saved workspace history. Workspace
  defaults remain available through the CLI for launches that omit an explicit choice.
- New Claude sessions resolve selection in this order: explicit choice, calling/parent
  Claude session, workspace default, ordinary inherited Claude environment. Named agents
  always supply their own pinned choice. `"default"` explicitly chooses the ordinary
  environment; an omitted/null API field inherits. Non-Claude sessions reject a profile
  choice and do not apply workspace profile defaults.
- The resolved profile ID is persisted on the session. Restart, backend recovery and moving
  a session to another workspace do not re-resolve it. A disabled, missing or redirected
  directory blocks a launch; restart checks before interrupting the previous run. An
  already running named agent remains usable after its profile is disabled.
- Known inherited authentication/endpoint overrides are refused for named-profile launches,
  with errors containing variable names only. Muster never unsets them silently or reads
  settings files to determine effective authentication. Explicit Default preserves existing
  behavior. Profile/project/managed settings and external login changes still belong to
  Claude Code: the human verifies the active account with `/status`.

A named agent can also run on OpenRouter (docs/PROTOCOL.md "OpenRouter"): that is the owner's
own OpenRouter key from the backend's environment, not a Claude credential, and goes only into
that agent's sessions as `ANTHROPIC_AUTH_TOKEN`. The inherited-override check above runs on the
inherited environment before those variables are added, so a pinned profile still supplies
`CLAUDE_CONFIG_DIR` (settings, history) while the requests go to OpenRouter.

The supported isolation target is a Claude subscription login in each directory. Claude's
documentation notes that **keyless Console sign-ins are not isolated by `CLAUDE_CONFIG_DIR`**.
Muster does not claim to verify which authentication method or account a directory uses.

Implementation: `src/account_profiles.rs`, store migration v13, the shared hub launch path,
CLI/RPC and desktop controls. The terminal-host protocol is unchanged. Tests use only empty
or synthetic profile directories and fake Claude processes. Run
`bash scripts/isolated.sh python3 scripts/account-profiles-smoke.py --browser` after building
the workspace binaries and web assets; it creates its own backend, data, home and port.

## Usage display and suggestions (implemented)

Steps 2 and 3 of the implementation sequence below: `src/usage.rs` (observations, freshness,
ranking, status-line composition), `src/statusline.rs` (the reporter), store migration v22.

**Source.** Claude Code's documented status-line JSON, read on 2026-10-26 at
<https://code.claude.com/docs/en/statusline>: `rate_limits.five_hour` and
`rate_limits.seven_day`, each `used_percentage` (0 to 100) and `resets_at` (Unix epoch
seconds). The page says `rate_limits` "appears only for claude.ai Pro and Max subscribers ... and
only after the first API response in the session", that each window "may be independently
absent", and that Claude Code "drops a window once its `resets_at` time passes". The status line
runs at session start and resume, after each assistant message, on `/compact`, permission-mode
and vim-mode changes, on an optional `refreshInterval` timer and when a window's reset passes;
updates are debounced (300 ms) and a new update cancels a run still in flight. The command runs
through `/bin/sh` (checked against `claude` 2.1.306 in a throwaway run: temporary HOME and
configuration folder, a fake API key, no network namespace, no prompt sent) and its output is
shown only after it exits 0. The same runs showed that a `statusLine` passed with `--settings`
replaces the one in the user's and the project's settings, so Muster must run the user's own,
and that it does not run before the folder is trusted (the trust dialog showed, the command never
ran), the same rule Claude Code applies to a project's own status line. The composed command
described below ran there too: the user's line showed with its input intact, both with a working
reporter and with one that fails.

**Reporter.** Every Claude session Muster starts gets a status line in its own `--settings`
file. Muster finds your own status line the way Claude Code would without Muster (docs:
<https://code.claude.com/docs/en/settings>, "Settings precedence" and "Where Claude Code keeps the
local file in a git repository", read 2026-10-26), the first file with a `statusLine`:

1. project local settings, `.claude/settings.local.json` at the repository root (for a linked
   worktree, the main checkout's root), but in the session's folder outside a git repository,
   when the root is the home folder, or when the root, its `.git` or its `.claude` is not owned
   by you;
2. a `.claude/settings.local.json` an older Claude Code left in the session's folder;
3. shared project settings, `.claude/settings.json` in the session's folder;
4. user settings, `settings.json` in the session's configuration folder (`CLAUDE_CONFIG_DIR`,
   else `~/.claude`).

A `CLAUDE_CONFIG_DIR` that is relative or empty, or (without one) a relative `HOME`, is one
Muster can't place: Claude Code would resolve it against the session's own working directory,
not the backend's, and an empty value's meaning isn't documented. Such a session's status line is
left to Claude Code (even when a project file decides it), its reports are filed nowhere, and
Default shows unknown while the backend itself runs with one. Profiles always launch with an
absolute folder.

The repository root is found by reading `.git` entries, never by running git (a repository's own
git config can run commands). Every file is opened beneath its (canonical) folder without
following a link at any step, must be a plain file with one link and at most 1 MiB, and a name
that looks like a credentials file is refused outright; so a repository whose
`.claude/settings.json` links to `~/.claude/.credentials.json` is never read through. Only the
`statusLine` key is kept; the rest of each file is parsed past, never stored, logged or sent.
This is the one read of a profile folder's contents, and only of `settings.json`. When a file
exists but can't be read that way, or isn't valid JSON, or the status line is not a command, or
a worktree belongs to a bare repository, or the configuration folder can't be placed (above),
Muster is unsure what Claude Code will show and leaves the session's status line to Claude Code
entirely: no usage from that session. Every other option
of your status line (`padding`, `refreshInterval`, `hideVimModeIndicator`, ...) is copied
unchanged. Managed settings outrank `--settings`, so a managed status line keeps running and
Muster gets no usage from it.

The composed command is a small shell script that runs your command itself, as its last step
(`exec`, with the same input through a here-document), so its output and exit code are exactly
Claude Code's. In the background it hands a copy of the input to `muster statusline claude`,
with that process's output discarded. Whatever happens to the reporter (an older Muster build
that has no `statusline` after a reload back, a missing binary, a crash, a slow backend), your
line is unaffected. The reporter POSTs only the two windows and `cost.total_api_duration_ms`
(before the run's first response, when there are no windows yet, the API time alone) to
`POST /usage/claude` with the run's session token, connects only to a loopback address (the
same check as hook callbacks: a `MUSTER_URL` naming another host is refused before it is looked
up), gives up after 600 ms, and is never in the hook path, the event log or agent state. Without
a status line of your own only the reporter runs: Claude Code then shows an empty status row,
and (per its docs) a configured status line hides most footer keyboard hints.
`MUSTER_USAGE_REPORTER=off` in the backend's environment disables the reporter for sessions
started afterwards. A session started before this version has no reporter until it restarts.

Known limit: your status line is looked up when the session starts. Editing or removing
`statusLine` during a session keeps showing the command it started with, and adding one leaves
the line empty, where Claude Code alone would pick the change up; restart the session to pick up
a changed status line. (Looking it up again on every run would make your line wait on a Muster
process each time, which the composition above is built to avoid.)

**Attribution and freshness.** The daemon maps the token to its session and current run
(generation). At launch Muster records the configuration folder that run really uses (its
profile's, or Default's `CLAUDE_CONFIG_DIR` / `~/.claude` at the time), and observations are
filed per folder: one folder, one login. Nothing in the payload is trusted to name an account or
a folder (a session can still misreport its own folder's numbers); a run without a launch record
reports nothing. Claude Code redraws the status line with its cached numbers for many reasons
(a timer, a mode change, `/rename`, which also writes the transcript), and a callback can be lost
(during a backend reload) so its numbers first arrive in a redraw much later. Neither the arrival
time nor the transcript time says when the numbers were fetched. The one piece of evidence tied
to an API response is the run's API time: a report whose `cost.total_api_duration_ms` is higher
than the run's previous report's came with a response received after that previous report, so it
is newly fetched data, dated by that previous report (the earliest it can be from; refreshed by
redraws at most every 30 seconds). A report delivered late is therefore never younger than it
can be: a lost callback followed by a redraw an hour later is dated before the hour and reads as
stale. The reporter's API-time-only report at a run's start is the baseline that lets the run's
first response count. Without an earlier report to compare with (a session started before this
version, a first report whose start report was lost) a report is "not confirmed fresh". A fresh
report replaces an observation only if not older than it. Anything else (a redraw, changed numbers
without more API time, API time that went down, which becomes the new baseline) never replaces or
renews a confirmed observation or another run's; it is kept, as "not confirmed fresh", only where
nothing was stored or over the run's own unconfirmed numbers. The price of dating by the previous
report: the first answer after a long pause reads as old as the pause until the next response (a
`refreshInterval` narrows that). Each window keeps its own time, so partial reports never age the
other. Display states: **current** (fetched within
30 minutes, reset not passed), **stale** (older), **not confirmed fresh**, **reset** (its reset
time passed: unknown, not zero) and **unknown** (nothing reported). The window re-judges its last
snapshot by its own timestamps every 30 seconds, so a window that can't reach Muster turns
figures stale or reset on time and drops a suggestion older than 2.5 minutes. Accounts nobody used
in Muster stay unknown: Muster never probes.

**One account, several rows.** Default and a profile on the same folder are one account
automatically; Default shows the folder its new sessions would use now, so after the backend
restarts with another `CLAUDE_CONFIG_DIR` it no longer shows the old folder's figures. The human
can declare two profiles one account ("Signs in to: the same account as …",
`muster profile same-account`); their newest observations are shared and never added together.
"Login changed" (`muster profile login-changed`, for a profile or `default`) forgets the folder's
observations and the same-account declaration of every profile on that folder.

**Suggestion policy.** Among rows that can start a task now (enabled, folder present, no
conflicting inherited authentication), one per account, only accounts whose 5-hour and weekly
windows are both current are compared. The one whose fuller window is lowest is suggested, unless
it is at 90% or more. The reason names its numbers, the other compared accounts' numbers, and the
accounts with no current usage ("not compared"). Unknown, stale, unconfirmed or reset usage never
counts as room. The new-session flow shows the table and the suggestion with a **Use NAME**
button; the selection never changes by itself, and there is no automatic choice or rollover. No
workspace restrictions on eligible profiles exist yet, so none are applied.

Shown in **Settings and more → Claude profiles** (Usage table, including Default) and under the
profile picker of New Tab and the named-agent dialog (when at least one profile is registered).
CLI: `muster profile usage [--json]`. RPC and endpoint details: [PROTOCOL.md](PROTOCOL.md#claude-account-profiles).
`scripts/account-profiles-smoke.py --browser` drives the reporter through a fake Claude that runs
its status-line command like Claude Code does.

The rest of this document describes the full roadmap, including deferred features.

## Account profiles

Claude Code documents concurrent account logins through separate `CLAUDE_CONFIG_DIR`
directories. Each directory has its own settings, session history, and Claude account login.
The user signs in through the official CLI for each profile. See [Log in with multiple
accounts](https://code.claude.com/docs/en/iam#log-in-with-multiple-accounts).

For example, these are manual setup commands, not commands for Muster to run automatically:

```sh
CLAUDE_CONFIG_DIR="$HOME/.claude-account-a" claude auth login
CLAUDE_CONFIG_DIR="$HOME/.claude-account-b" claude auth login
```

Muster stores a profile's name and configuration directory, then sets `CLAUDE_CONFIG_DIR`
on the child process when starting a session. It does not change the daemon's environment
or switch the machine's global Claude login. Two sessions can therefore use different
accounts at the same time.

A profile whose folder is Claude Code's default, `$HOME/.claude` (compared as canonical paths,
so a symlink or trailing slash still matches), is the exception: its sessions start with
`CLAUDE_CONFIG_DIR` removed, including any value the daemon inherited, so Claude Code uses
`~/.claude` and `~/.claude.json`. Setting it, even to `~/.claude`, makes Claude Code keep a
separate state file at `~/.claude/.claude.json` instead. Sign in to that folder with plain
`claude` (or `env -u CLAUDE_CONFIG_DIR claude`), not `CLAUDE_CONFIG_DIR=~/.claude claude`. HOME is
the session's own (else the daemon user's home); an empty or relative HOME never counts, and
the profile then keeps its explicit `CLAUDE_CONFIG_DIR`. A `CLAUDE_CONFIG_DIR` in the daemon's
own environment does not make its folder "the default": a profile there gets the same value
set.

Preserve the credential boundary in PLAN.md: Muster never reads, copies, stores, refreshes,
or proxies Claude credentials or tokens. Claude Code owns authentication. Profile setup
must detect or explain conflicting API-key and gateway environment settings so an inherited
configuration cannot silently select different billing or an unintended endpoint.

Profiles also separate user settings, plugins, and history. Offer explicit settings setup
where needed; do not copy whole profile directories to share configuration, since they can
contain credentials. Project configuration and Muster's per-session hooks and MCP settings
continue to work through the existing launch path.

## Machine and session model

| Object | Responsibility | Suggested metadata |
| --- | --- | --- |
| Machine | Own processes, files, worktrees, and locally configured profiles | ID, name, connection state, workspace availability |
| Claude profile | Select a login and configuration on one machine | ID, machine ID, label, local config directory, enabled state |
| Account group | Associate profiles known by the user to use the same account | Opaque ID and label; no credentials |
| Session | Own one running Claude conversation | Existing session fields plus machine ID and profile ID |
| Usage observation | Record capacity last reported by Claude Code | Profile ID, session ID, observation time, available windows and reset times |

An account group is optional initially. It becomes useful if the same account is configured
on multiple machines: those profiles share account limits and must not appear as independent
capacity. Associate them explicitly rather than inspecting credential files.
Add this association before capacity recommendations: two profiles on the same machine can
also use the same account. A user-reported login change must invalidate its usage observations
and account-group membership; a directory binding is not proof of account identity.

Persist the selected profile on the session so restarting it does not silently change
accounts. A disabled or missing profile requires a visible selection or setup action. Existing
sessions without a profile retain the current default-login behavior until explicitly changed.

Workspace preferences can restrict eligible profiles and machines. An automatic suggestion
must respect these preferences, including tasks that should stay on a particular account.

## Usage observations from Claude Code

Use Claude Code's documented status-line JSON as the first source. It exposes
`rate_limits.five_hour` and `rate_limits.seven_day`, each with `used_percentage` and
`resets_at`. These are subscription usage windows, separate from conversation context usage.
See [Available status-line data](https://code.claude.com/docs/en/statusline#available-data).

A small local reporter receives this JSON and forwards the relevant fields to Muster using
the session's existing authenticated callback mechanism. Attribute reports using the daemon's
session-to-profile mapping, not an account label supplied in the payload. Integrate with an
existing user status-line command without silently replacing its visible output.

Muster displays the available windows, reset times, and when capacity was last observed.
The fields can be independently absent and appear only after the session's first API
response. Re-running a status-line script does not guarantee newly fetched account usage.
Consequently:

- Missing data means unknown, not zero usage or unlimited capacity.
- Old observations remain visibly stale and lose recommendation priority.
- An elapsed reset invalidates the old window; it does not establish fresh zero usage.
- Reports from sessions using the same account are not added together. Use the newest
  applicable observation, preserving each window's timestamp when reports are partial.
  Store callback receipt time separately from evidence of fresh provider data. A cached
  redraw from an idle session must not replace a newer observation or renew its freshness.
  If the source cannot establish freshness, retain that uncertainty instead of labeling it
  as newly fetched data. Attribute callbacks to the specific run as well as its session.
- Percentages express headroom within a plan, not interchangeable token balances across
  plans. Model-specific limits and activity outside Muster can affect availability.

An unused profile may have no current observation. Accept that limitation in the first
version instead of extracting OAuth tokens or making background inference calls to probe it.

Before adding the reporter, prototype composition with existing status-line commands and
settings precedence. Preserve their stdin and visible output, bound reporting time, handle
cancellation, and keep reporter failure from breaking the user's status line. Coalesce
duplicate telemetry and keep it out of agent activity-state detection.

## Choosing where a new task runs

Start with manual selection and a recommendation. The new-session flow shows the chosen
machine and profile, with defaults from the workspace. A capacity comparison might look like
this; these numbers are illustrative:

| Destination | Profile | Five-hour usage | Weekly usage | Last observed |
| --- | --- | --- | --- | --- |
| Desktop | Account A | 92% | 70% | 1 minute ago |
| Backend | Account B | 25% | 40% | 4 minutes ago |

Recommend a destination only after checking that it is online, the workspace and required
environment are available, and the profile is allowed for the task. Explain the suggestion
with the observed limits and freshness. Unknown or stale usage cannot establish that a
profile has more available capacity.

An optional `Auto` selection can later choose among explicitly allowed destinations for a
new task. Define and test the ranking policy before enabling it; consider both usage windows,
freshness, machine readiness, and account preferences. Show the selected destination and
record the reason. Automatic placement must not turn an explicit profile choice into a
fallback pool.

Once started, the session stays pinned to its machine and profile. Restart and resume use
that selection unless the human requests a handoff.

## Handling a limit during a task

When a session reports a usage limit, show the affected profile and the reset time when
available. Keep its work and terminal accessible, and explain whether Claude is still waiting
or the process has exited. Do not kill the process, replay prompts, or replace credentials
merely because a report suggests that a limit was reached.

Offer a human-selected continuation on another eligible profile. Treat it as a new process
and an explicit handoff:

1. Stop or interrupt the original task and confirm it is no longer modifying the worktree.
2. Preserve the working files and collect available task context: the original request,
   progress, changed files, verification results, and remaining work. Do not require another
   model response from the exhausted account to create this context.
3. Launch Claude Code under the selected profile in the same worktree and provide the
   handoff context. Link the old and new sessions and record the account change.

Implement these as durable, retryable states: context prepared, original run stopped and
verified, replacement launch pending, replacement confirmed. An interrupted turn is not
proof that background processes have exited. Reuse the existing process-cleanup checks and
retain an uncertain launch for reconciliation instead of retrying it as a second process.
A failed replacement leaves the files and handoff context available for deliberate retry.

Separate profiles have separate conversation histories. Do not assume `--resume` can move a
conversation between accounts, or copy profile directories to make it work. Full transcript
continuation is a separate compatibility investigation; files plus an explicit handoff are
the initial approach.

Automatic quota rollover is deferred. Documented multiple-account support does not by itself
establish permission for automatic quota pooling, and moving accounts onto separate machines
does not resolve that question. Revisit the current [authentication and credential
rules](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)
before expanding this behavior.

## Multiple backend machines

Keep one logical Muster coordinator and one client view. Each backend runs a local worker
that owns its PTYs, worktrees, profile paths, and Claude processes. The coordinator routes
session operations and aggregates status and usage observations. The local machine is the
first worker; transport and worker supervision are future implementation work.

Credentials remain on the machine where the user logged in. The coordinator needs profile
identifiers and availability metadata, not credentials. One profile per machine is a valid
initial setup, but the model should also allow multiple profiles on a worker.

Persist machine ownership with each session. A disconnected worker is unavailable, not proof
that its processes stopped. Do not automatically start a replacement that might duplicate
work still running on that worker. Reconnection should reconcile existing sessions.

Starting a new task remotely requires the repository and environment to be present there.
Moving an existing task additionally requires transferring changed and untracked files,
re-establishing dependencies and relevant local state, and ensuring the original process
has stopped. Cross-machine handoff is deferred until these mechanics are implemented.

## Implementation sequence

1. **Local profiles.** Add profile records, manual selection, workspace defaults, and
   per-session `CLAUDE_CONFIG_DIR`. Preserve selection across restarts. Keep the existing
   terminal backend and official login flow.
2. **Usage display.** Add the authenticated status-line reporter, stored observations,
   reset times, and freshness indicators. Show unknown data honestly. *Done 2026-10-26.*
3. **Placement suggestions.** Suggest among eligible local profiles. Add optional selection
   for new tasks only after defining its ranking policy and checking current provider rules.
   *Suggestions done 2026-10-26 (manual choice plus a recommendation); the optional automatic
   selection is not built.*
4. **Local handoff.** Add a human-selected continuation under another profile, using the
   existing worktree and recorded task context. Ensure there is only one active writer.
5. **Remote workers.** Add machine ownership, authenticated transport, reconciliation, and
   worker-local profiles. Extend the same placement model to remote machines.
6. **Remote handoff.** Add explicit transfer of working files and task context after remote
   workspace management is reliable.

Likely integration points are `src/model.rs` and `src/store.rs` for profile and observation
records, `src/hub.rs` and `src/adapters/claude.rs` for launch selection and reporting, and
`src/rpc.rs` plus the client protocol for selection and status. These are proposed changes,
not additions to the current API contract.

## Validation criteria

- One daemon can run two Claude sessions using different profile directories without
  changing the daemon environment or the global login. Each CLI reports the intended account.
- Restarting a session preserves its profile, and existing default-login sessions still work.
- Reporter callbacks cannot attribute usage to a different session or profile. Missing,
  stale, partial, and reset observations produce the intended display and ranking behavior.
- Profiles sharing an account are not counted as separate capacity. Workspace restrictions
  and explicit selections take precedence over suggestions.
- A handoff preserves changed files and task context without two agents writing to the same
  worktree. A disconnected remote worker cannot trigger duplicate execution.
- Muster never handles provider credentials. Automated checks use fake processes and sample
  usage payloads. Every Muster CLI call in tests sets temporary `MUSTER_RUNTIME_DIR`,
  `MUSTER_DATA_DIR`, and `MUSTER_PORT`; tests never contact the real daemon.

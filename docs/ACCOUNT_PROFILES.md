# Claude account profiles and machine placement

Status: local profiles implemented, 2026-10-18. Usage reporting,
placement suggestions and handoff remain deferred. This expands the account and
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
  a running process. There is no profile-directory deletion operation.
- Select a profile with `muster new --profile NAME`, the New Tab dialog, or
  `muster agent new NAME --profile PROFILE`. Named-agent profile bindings are fixed at
  creation, including for replacement sessions and `--fresh`. Create another named agent
  to use another profile; switching an existing agent is part of the deferred handoff work.
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

The supported isolation target is a Claude subscription login in each directory. Claude's
documentation notes that **keyless Console sign-ins are not isolated by `CLAUDE_CONFIG_DIR`**.
Muster does not claim to verify which authentication method or account a directory uses.

Implementation: `src/account_profiles.rs`, store migration v13, the shared hub launch path,
CLI/RPC and desktop controls. The terminal-host protocol is unchanged. Tests use only empty
or synthetic profile directories and fake Claude processes. Run
`bash scripts/isolated.sh python3 scripts/account-profiles-smoke.py --browser` after building
the workspace binaries and web assets; it creates its own backend, data, home and port.

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
   reset times, and freshness indicators. Show unknown data honestly.
3. **Placement suggestions.** Suggest among eligible local profiles. Add optional selection
   for new tasks only after defining its ranking policy and checking current provider rules.
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

# More powerful scheduling: plan

## Plain English

Phases 1, 2 and 3 of this plan are built: schedules with history, pause and skip, downtime and busy rules, alerts and replies into notes; then triggers (a note you allow, a branch moving, a PR or CI failure on GitHub, a local trigger URL), "Then…" steps that hand a reply to the next agent, and a daily dollar cap for OpenRouter agents; then agents proposing schedules that you approve with a click (a one-off reminder for itself up to a day ahead needs no approval), and a trigger for files appearing in a folder you chose (see "Status"). Phone alerts for schedules wait for the phone's push alerts. Before it, Muster couldn't run anything on a schedule, though the pieces that make it safe were in place: every message records whether a person, a schedule or a webhook started it, agents choose which of those they accept, chains stop after 4 hops, an agent takes at most 200 messages a day, and a "Background worker" template exists. This plan adds real schedules (every weekday at 9:00, or once at 15:30 tomorrow) with a run history, pause and skip, sensible behaviour after downtime or when the agent is busy, and alerts when a run fails. After that come triggers that aren't about time (a note changes, a PR opens, a commit lands, one agent finishes so the next starts) and spend caps for pay-per-use agents. Last, agents get a guarded way to propose schedules for themselves. Unattended work goes to Codex or OpenRouter agents by default. A Claude subscription agent only runs on a schedule if you opt in, and even then with Claude Code's own limits.

## Status

**Phase 1 is built** (2026-10-20, branch `feature/schedules`): features 1 to 6, plus quiet hours
and the count caps from 7, with the owner's decisions below. Code in `src/schedules/`; how it
behaves is in docs/PROTOCOL.md "Schedules", docs/CLI.md "Schedules" and docs/UI.md "Agents
panel". Choices made while building it:

- Cron lines are parsed by Muster itself (`src/schedules/cron.rs`, five fields with ranges,
  steps, lists, names and the `@daily`-style macros); time zones and daylight saving come from the
  `jiff` crate, which reads the system's time-zone database.
- Every schedule runs at most every 15 minutes, for every agent (custom cron lines too).
- "If missed: Auto" runs once for one-offs and anything slower than hourly, and skips for
  hourly-or-faster schedules. A slot up to 10 minutes late (a reload, a short sleep) still counts as
  on time.
- The busy rule waits at most an hour in line; a sent run waits at most an hour for its reply.
- The per-agent unattended cap defaults to 24 messages a day and counts every message whose
  origin isn't the human's, at any hop.
- Quiet hours are one window for the whole machine (default 23:00 to 07:00, its local time), and
  each schedule chooses whether it respects them. Run now ignores quiet hours and the
  schedule's own daily cap, but not the agent's caps.
- "Fresh each run" never restarts within two minutes of the human typing into the agent.
- A slot less than 15 minutes after the last run (its slot, or when its prompt was really
  submitted) is skipped when it comes, even where a daylight-saving change bunches a cron line's
  times.
- Cron's day rule is cronie's: a day field starting with `*` is a wildcard, and then both day
  fields must match.
- A schedule's prompt that waits in the agent's queue is checked again right before it is typed
  and before Enter; paused, removed, expired or out of time, it is taken back, never typed.
- `{last_reply}` is quoted and labelled as earlier output (data, not instructions).
- Each run is one row in a small runs table (time, outcome, reason) pointing at its agent
  message, which keeps the text and the reply.

**Phase 2 is built** (2026-10-21, branch `feature/schedules-phase2`): chaining (8), triggers
beyond time (9) and the OpenRouter spend cap (the rest of 7), with the owner's decisions below and
one more: note triggers are opt-in per note. Code in `src/schedules/triggers.rs`,
`src/schedules/run.rs` (steps) and `src/spend.rs`; how it behaves is in docs/PROTOCOL.md
"Schedules" (Phase 2), docs/CLI.md "Triggers and Then" and docs/UI.md "Agents panel" and "Notes".
Choices made while building it:

- A trigger is a schedule whose `when` is an event (`note`, `git`, `github`, `url`), so it has the
  same history, pause, alerts, reply note, caps and dialog. Its runs have origin `webhook`
  ("Triggered") and sender `trigger:<id>`. An event is held on the schedule and run by the
  scheduler like a due slot; events close together merge into one run.
- **Note triggers are opt-in** (the owner's word): a note fires nothing until the human switches
  "Allow triggers" on for it (off by default; the note's status line, its tab menu, the dialog,
  or `muster note triggers`). Only `note.set_triggers` writes it, refused from agents' sessions and
  shells they drive; no write of the text touches it. guess: stricter than asked, edits by *any*
  unattended work (a session on a scheduled or triggered chain) never fire a note trigger, not
  only the trigger's own runs, so two triggers can't feed each other; edits by the trigger's own
  agent never count either. Debounce default 2 minutes; at most 10 runs a day per note.
- Every trigger keeps the 15-minute floor: an event within 15 minutes of its last run waits (it
  isn't dropped). A trigger's own daily cap defaults to 10.
- Git is polled from the ref files every 15 s (no inotify, no hooks): cheap, and it works the same
  for worktrees and packed refs. A move while Muster was off fires once at start.
- GitHub uses `gh api` REST calls (pulls, actions/runs) through the PR runner, default every 5
  minutes (2 to 60); the first poll is a baseline; backoff doubles to an hour.
- A trigger URL is `POST /api/trigger/<name>` with a Bearer secret (64 hex), loopback only; the
  secret never appears in `schedule.list` (only `trigger.secret`, the human's).
- "Then…" steps: a success step and a failure step per level (failure may just notify), nested up
  to 4 levels in the API (the UI and CLI offer one level). Each is a run of kind `chain` one hop
  further, so the 4-hop limit bounds chains. Steps don't count toward the schedule's daily cap or
  floor and don't hold back its next run.
- Spend is read from OpenRouter's `GET /api/v1/key` (`usage_daily` for the overall cap; per agent,
  rises in `usage` split among the OpenRouter agents working in between). Chosen over price ×
  tokens from the CLIs (they don't report OpenRouter's prices) and the generation endpoint (needs
  ids the CLIs don't show). "Today" is OpenRouter's UTC day. Without a reading in 15 minutes,
  unattended OpenRouter work is refused too (spend unknown). guess: one credit is one dollar.
- The spend cap applies to every non-human message to an OpenRouter agent (agent asks on an
  unattended chain included), not only schedule runs.

**Phase 3 is built** (2026-10-26, branch `feature/schedules-phase3`): agents propose schedules
(10) and the file-change trigger (11), with the owner's decisions (question 3, answer b; the
Phase 2 opt-in rule applied to files). Code in `src/schedules/agent.rs` (the MCP tools, the
estimate) and `src/schedules/triggers.rs` (files); how it behaves is in docs/MCP.md "Schedules",
docs/PROTOCOL.md "Schedules" (Phase 3), docs/CLI.md "Agents' proposals and reminders" and
docs/UI.md "Agents panel". Choices made while building it:

- A proposal is an ordinary schedule with `made_by` and no next run: nothing can make it due
  (every write keeps it empty), and Run now and Skip next refuse it. Approval is a human-only RPC
  (`schedule.approve`, refused from an agent's session or a shell an agent drives) that checks it
  again as if made now. No tool approves, resumes, edits or makes a schedule directly: to change
  one an agent proposes a new one. A proposal nobody approves goes after 7 days.
- Only named agents propose, and never while handling scheduled or triggered work (their
  session's provenance isn't the human's): so a reminder can't set the next reminder, and a
  schedule can't breed schedules. An ask from another agent on a human-rooted chain is the
  human's work and may propose.
- Reminders: for the caller only, one-off, 5 minutes to 24 hours ahead, at most 3 waiting, 15
  minutes apart, 10 a day (deleted ones count); a Claude subscription agent keeps quiet hours on
  and a time inside them is refused up front. Proposals: at most 3 waiting, 10 a day; at most 5
  live schedules per agent it made (the plan's "at most 5 active schedules"). guess: these
  numbers; the owner may want them tighter or looser.
- Agents propose times only (a cron line or a one-off), not triggers and not "Then…" steps:
  those stay the human's to set up (question for the owner in the report).
- Each agent-made schedule carries an estimate: runs in the coming week after quiet hours and
  the caps, what pays (Claude subscription with its expiry, OpenRouter with its caps, Codex), and
  the agent's unattended cap with its last 7 days. guess: no dollar figure per run for OpenRouter
  (the spend ledger has today's per-agent estimate only).
- The audit is the MCP log (every call, refused ones too) plus `made_by` on the schedule.
- Files: polled every 15 s, no inotify (it works the same on macOS and Linux and costs one
  listing); a ctime cursor kept with the schedule (catches files moved in, which keep their
  modification time, and changes while Muster was off); at most 5000 entries and 3 levels;
  hidden and partial files skipped; links never followed. "Ignoring the agent's own writes": a
  change seen while its agent, or any scheduled or triggered work, is working, or within 30 s
  after, counts as an agent's own (files have no author). Watched folders are a settings list,
  empty by default (the owner's opt-in rule): no file trigger can be made until a folder is
  chosen, and removing one stops its triggers at once.
- Phone push: not built here (feature/phone-push builds Web Push). Every schedule alert goes
  through `schedules::run::alert` (an in-app notice plus `notify::alert`), the one place a push
  sink needs to hook in.
- No store migration: schedule records and the settings are JSON.
The "Today" section below describes the code before Phase 1.

## Today (before Phase 1)

- **No triggers.** Nothing in the daemon fires on time or on events. Schedules and webhooks are listed as deferred in PLAN.md §10.
- **The policy exists.** There are three origins: human, schedule and webhook. Each agent has a list of the origins it accepts. Claude agents default to human only, Codex agents to all three. The New/Edit agent dialog shows an amber warning when you tick Schedule or Webhook on a Claude agent. Chains stop after 4 hops, and each agent takes at most 200 messages per rolling 24 hours. Both limits count every origin, the human's included.
- **The plumbing is there.** The daemon's internal "ask an agent" call already takes an origin, delivers through the same queue as everything else, and waits for the reply. Each message is stored with its text, origin, hop, reply and any refusal reason, so the agents panel's message log is already a history of runs.
- **An outcome is already detected.** A finished ask ends in one of these: replied; blocked (stopped on an approval prompt, a question or an error); no reply; agent ended; timed out.
- **Alerts are thin.** Desktop notifications fire only when a session needs you, and there are in-app notices. Phone push is planned (Android phase 2) but not built.
- **Gaps that matter here:**
  - The daemon listens only on this machine (plus the phone over Tailscale), so nothing on the internet can call it.
  - (Built since: Codex agents now resume their conversation after a restart, like Claude agents.)
  - The OpenRouter branch (under review) knows each model's price but doesn't track spend.

## Proposed features (ranked by value per effort)

**1. Schedules: recurring and one-off (M).** *Example:* "Every weekday at 9:00, ask the PR reviewer agent to go through open PRs and post a summary note." Or once: "at 15:30 tomorrow, ask worker to check whether the deploy finished."
- *How it looks:* each agent row in the Agents panel gets a "Schedules" section showing the next run ("next: Mon 09:00"). A New schedule dialog has: agent, prompt, and when.
- *When:* presets (every hour, every day at…, weekdays at…, weekly on…), "Once at…", or a custom cron line with a plain-English reading under it ("At 09:00, Monday to Friday").
- *Time zone:* your local zone by default, stored by name, so daylight-saving changes keep 9:00 at 9:00.
- *Prompt placeholders:* `{date}`, `{last_run}` (when it last ran) and `{last_reply}`. So "summarise PRs opened since {last_run}" works.
- *Commands:* `muster schedule add/ls/rm/pause/resume/run`.
- *Mechanics:* each run is an ordinary agent message with origin "schedule" at hop 0, so every existing guard applies.
- *Depends on:* nothing (one store migration).

**2. Run history and outcome (S).** *Example:* you open the 9:00 schedule and see the last 20 runs: "Mon 09:00 replied · 4m", "Tue 09:00 blocked: waiting for approval", "Wed 09:00 skipped: agent busy".
- *How it looks:* a list under the schedule. Clicking a run opens the message/reply overlay the panel's log already has. Failures are red, skips grey.
- *Outcomes:* replied, blocked, no reply, ended, timed out, refused (and why: daily cap, origin, budget), skipped (and why: paused, busy, quiet hours, missed).
- Mostly reuses the stored messages; it adds a small runs table for skips and refusals, which never become messages.

**3. Pause, resume, skip next, run now (S).** *Example:* on holiday, pause everything with one switch. Or skip only tomorrow's run because you know the build is broken.
- *How it looks:* buttons in the schedule's menu, a global "Pause all schedules" toggle in the panel header, and a paused badge on the schedule's row.

**4. Missed runs after downtime (S).** *Example:* the laptop slept from Friday night to Monday. The daily report runs once on wake instead of three times, and the hourly check just waits for its next hour.
- *How it looks:* a per-schedule choice, "If missed: run once when Muster is back · skip". Catch-up runs on start-up are spread out, one at a time per agent, never as a burst.
- *Depends on:* 1.

**5. When the agent is busy (S).** *Example:* the 9:00 run fires while you're mid-conversation with that agent, or yesterday's run is still going.
- *How it looks:* a per-schedule choice, "If busy: wait in line · skip this run · start fresh".
  - *Wait in line* is today's queue: the run goes in after the current turn, and is given up after an hour.
  - *Skip* records "skipped: busy".
  - *Start fresh* gives the run a new conversation. It is only allowed when the agent isn't in a turn you started, so it never wipes your own conversation.
- A separate "Conversation: continue · fresh each run" choice stops a weeks-long daily job from growing its context forever.
- *Depends on:* 1.

**6. Alerts and where the reply goes (S).** *Example:* when the nightly check comes back blocked or errored, you get a desktop notification "worker: nightly check blocked (approval needed)". Its reply is also appended to a "Nightly checks" note under a date heading.
- *How it looks:* per schedule, "Notify me: on failure (default) · always · never", and "Also put the reply in: a note · nothing".
- *Depends on:* 1, 2. Phone push comes later, with Android phase 2.

**7. Quiet hours and caps (S for counts, M for spend).** *Example:* nothing scheduled runs between 23:00 and 07:00. The worker takes at most 24 unattended runs a day. OpenRouter agents together stop at $5 a day, and you get one alert when that happens.
- *How it looks:* a Scheduling section in settings with quiet hours, a daily run cap per schedule, and a lower daily cap for unattended (schedule plus trigger) messages per agent than today's 200.
- *Spend cap:* a daily dollar cap for OpenRouter agents, overall and per agent. Spend is read from OpenRouter's usage for the key before and after each run (guess: its key endpoint reports daily usage). At the cap, further runs are skipped with "refused: budget".
- A run already in progress finishes; the cap stops the next one.
- *Depends on:* 1; the spend part needs the OpenRouter branch merged.

**8. Chaining: when X finishes, run Y (M).** *Example:* when the nightly "run the test suite" schedule on worker replies, ask the reviewer agent: "Here are last night's failures: {reply}. File a note with likely causes." There is also a failure branch: "if it's blocked, notify me instead".
- *How it looks:* a "Then…" step at the bottom of the schedule dialog: on success or on failure, ask agent Y with a prompt that can quote the reply.
- *Limits:* each step is one hop further down the same chain, so the existing 4-hop limit bounds chains and stops loops.
- *Depends on:* 1, 2.

**9. Triggers beyond time (M each).** All of them use the existing "webhook" origin, shown in the UI as "Triggered", so the existing accept/refuse rules apply unchanged.
- *A note changes:* "when anyone edits the 'Ideas' note, wait until it's been quiet for 2 minutes, then ask the researcher to expand new items". Edits made by the target agent itself don't count (no loops).
- *Git events, local:* "when master moves in ~/projects/muster, ask worker to update the changelog note". Muster watches the repo itself; no hook install is needed.
- *GitHub events, polled:* "when a PR is opened or CI fails on owner/repo, ask the PR reviewer". Muster checks through `gh` every few minutes, because nothing on the internet can reach the daemon (see question 6).
- *A local trigger URL and `muster trigger <name>`:* for your own scripts, git hooks or the phone. It only listens on this machine and needs a secret per trigger. Any data sent with it is size-capped and handed to the agent marked as untrusted text.
- *Another agent replies:* covered by chaining.
- *How it looks:* the schedule dialog's "When" becomes "At a time · When something happens", with a picker for the event.
- *Depends on:* 1, 2. GitHub polling reuses the existing safe `gh` runner from PR review.

**10. Agents propose schedules for themselves (M).** *Example:* the reviewer says "I'll check CI again in 20 minutes" and actually does. Or an agent suggests "summarise the Ideas note every Friday" and you approve it with one click.
- *How it looks:* MCP tools (list, propose, pause, delete schedules) and the same `muster schedule` commands from inside a session. Proposed schedules appear in the panel as "Proposed by reviewer · Approve / Dismiss".
- *Guard rails:*
  - An agent can only target itself or agents that accept scheduled work.
  - It can't create schedules while handling scheduled or triggered work, so a schedule can't breed more schedules.
  - Each agent has at most 5 active schedules, at most one run every 15 minutes, and 7-day expiry unless you renew.
  - Every create shows in the audit log.
- *Depends on:* 1–3.

**11. File-change trigger (M, low value).** *Example:* when anything in `~/inbox/*.pdf` appears, ask worker to file it. It needs care: debouncing, ignoring the agent's own writes, and limits on watching big trees. Build it last, or only if you ask for it.

## Constraints: unattended use and Anthropic's terms

PLAN.md §3 notes that Claude subscription limits assume "ordinary, individual usage", and §10 sets the rule for Claude agents' schedules: follow Claude Code's own bounds. Every feature above follows these rules:

- **Unattended work defaults to agents you pay for directly.** The New schedule dialog lists Codex and OpenRouter agents first and preselects one. A Claude subscription agent only appears if it already accepts "schedule" (or "triggered"). Choosing it shows the existing amber warning.
- **Claude Code's limits for Claude subscription agents:**
  - At least a 1-minute interval (Muster's preset floor is 15 minutes for everyone anyway).
  - Recurring schedules expire after 7 days unless you click Renew. You get a notice the day before.
  - Quiet hours are on by default.
  - A Claude agent on OpenRouter is pay per use, so these subscription rules don't apply to it; the spend cap does.
- **Nothing tries to maximise subscription use.** There's no "run as often as quota allows", no waiting for usage resets, no parallel copies of an agent to push more work through, and no automatic switching between providers. The "quota-aware scheduler" idea in PLAN.md §8 stays parked.
- **Caps count every unattended run.** Count caps apply to subscription agents and dollar caps to OpenRouter. Every scheduled or triggered run is labelled as such in the history and the audit log.
- **Nothing from outside reaches the daemon.** Triggers never listen beyond this machine (§10: "webhook triggers are never exposed to anyone but the user"). GitHub is polled with your own `gh` login, and Muster never sees or stores its token.
- **No unattended approvals.** A run that stops on an approval prompt is recorded as "blocked" and you're notified. Nothing approves it for you. Agents still have no approve tool.

## Suggested phases

**Phase 1: Schedules you can trust (features 1–6, plus quiet hours and count caps from 7). Built.** Recurring and one-off schedules, run history, pause/skip/run now, missed-run and busy rules, failure alerts, replies into notes, `muster schedule`, and the Claude rules (opt-in, warning, 7-day expiry). This is useful on its own: the daily PR summary and nightly checks work end to end. Size: M to L.

**Phase 2: Triggers and money (7's spend cap, 8, 9). Built.** Chaining, the note-changed trigger (opt-in per note), the local git watcher, GitHub polling, the local trigger URL with `muster trigger`, and OpenRouter spend caps. Size: L, splittable per trigger.

**Phase 3: Agents schedule for themselves (10, and 11 if wanted). Built** (phone push alerts excepted: they wait for Android Web Push). The MCP tools with the approval flow and guard rails, the file-change trigger, and phone push alerts once Android Web Push lands. Size: M.

## Decisions (owner, 2026-10-20)

The owner took the recommended answer on all six questions below: missed runs (c), busy agent: skip if the same schedule's run is still going, else wait in line up to an hour; agents propose schedules for approval, except one-off self reminders up to 24 h ahead (b); Claude subscription agents opt in with 7-day expiry and quiet hours on (a); spend cap overall $5/day plus optional per-agent caps (c); GitHub events by polling only (a).

For Phase 2 (2026-10-21) the owner added: note-change triggers must be opt-in, not automatic, so
they can't waste usage.

## Open questions for the owner (answered above)

1. **Missed runs: what should happen by default when Muster was off at a run's time?** If your laptop sleeps over a weekend, a daily report and an hourly check each missed several runs. (a) Run each once when Muster is back. (b) Skip them all and wait for the next time. (c) Daily and weekly schedules run once on return; hourly-or-faster ones skip. *Recommend (c):* a missed daily report is worth getting late, and a missed hourly check is about to run again anyway.

2. **Busy agent: what should a run do when its agent is already working?** For example, the 9:00 run fires while you're in a conversation with that agent. (a) Wait in line behind the current turn. (b) Skip this run. (c) Start a fresh conversation for the run. *Recommend:* skip if the same schedule's previous run is still going, otherwise wait in line for up to an hour. It never interrupts you, and a slow job can't pile up copies of itself.

3. **Can agents create schedules?** An agent might want "check CI again in 20 minutes" or "summarise this note every Friday". (a) No: only you create schedules. (b) Agents propose and you approve each one with a click in the Agents panel. (c) Agents create them freely within caps. *Recommend (b),* with one exception: an agent may set a one-off reminder for itself up to 24 hours ahead without approval. Follow-ups are the common case, and anything recurring is where runaway use would come from.

4. **How strict should we be with Claude subscription agents on schedules?** Today they only take scheduled work if you tick Schedule, and you see a warning. (a) Keep the opt-in, and add 7-day expiry with a Renew button plus quiet hours on by default. This matches Claude Code's own /loop limits. (b) Don't allow schedules on Claude subscription agents at all; use Codex or OpenRouter. (c) Opt-in and warning only, no expiry. *Recommend (a):* it's the rule PLAN.md already set, it mirrors what Anthropic itself ships, and it still lets you run the occasional Claude job.

5. **Spend cap for pay-per-use agents: what shape and what default?** OpenRouter agents bill your key per token, so a looping schedule could cost real money overnight. (a) One overall daily cap. (b) Caps per agent only. (c) Both: an overall cap that is always on, plus optional per-agent caps. Also: what should the overall default be (for example $5 a day)? *Recommend (c) with a $5/day overall default:* one number protects you even if you never configure anything, and per-agent caps are there for the one agent you trust more.

6. **GitHub events: poll, or receive GitHub's webhooks?** For "when a PR opens, ask the reviewer", Muster can either check GitHub every few minutes with your `gh` login, or receive GitHub's push messages, which needs a public address (for example Tailscale Funnel) pointing at the daemon. (a) Poll only, every 2–5 minutes. (b) Real webhooks through a tunnel you set up. (c) Poll now, webhooks later as an opt-in. *Recommend (a):* a minutes-long delay is fine for reviews, and it keeps the rule that nothing outside your machine can reach Muster.

## Where today's pieces live

| What | Where |
|---|---|
| The three origins and per-kind defaults | src/agents.rs:44, src/agents.rs:72 |
| Agent record (accepts, autostart) | src/agents.rs:96 |
| Internal ask, takes an origin | src/agents.rs:1547 |
| Run outcome (replied/blocked/no reply/ended/waiting) | src/agents.rs:656, src/agents.rs:670 |
| Autostart at daemon start | src/agents.rs:1404, src/main.rs:1093 |
| Background worker / PR reviewer templates | src/agents.rs:1871, src/agents.rs:1884 |
| Hop limit 4, daily cap 200 | src/mcp/guard.rs:110, src/mcp/guard.rs:112 |
| Origin refusal text; Claude non-agent rule | src/mcp/guard.rs:121, src/mcp/guard.rs:144 |
| Stored message history (text, reply, refused) | src/agents.rs:123, src/store.rs:266 |
| Amber warning for Claude + Schedule | docs/UI.md:400 |
| Agents panel and message log | docs/UI.md:370 |
| `agent.ask` has no origin parameter (by design) | docs/PROTOCOL.md:176 |
| Desktop notifications (needs-human only) | src/notify.rs:1 |
| In-app notices | src/hub.rs:798 |
| Loopback-only server | src/server.rs:1 |
| Safe `gh` runner (for GitHub polling) | src/pr.rs:7 |
| Notes MCP tools (for note triggers and reply-to-note) | docs/MCP.md:677 |
| Unattended-use constraint | PLAN.md:39 |
| Deferred: schedules/webhooks rules (Codex resume is built: PLAN.md "Continuity") | PLAN.md:212 |
| Parked quota-aware scheduler | PLAN.md:130 |
| OpenRouter model pricing (branch, no spend tracking) | feature/openrouter src/openrouter.rs:375 |

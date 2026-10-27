//! Agents schedule for themselves (docs/SCHEDULING_PLAN.md feature 10, Phase 3), through the MCP
//! tools `list_schedules`, `propose_schedule`, `set_reminder`, `pause_schedule` and
//! `delete_schedule` (src/mcp/tools.rs; `muster schedule propose/remind/pause/rm` inside an agent's
//! session run the same tools).
//!
//! The owner's rule (docs/SCHEDULING_PLAN.md "Decisions", question 3, answer b): an agent
//! proposes, and the human approves each proposal with a click in the Agents panel. The one
//! exception is a one-off reminder for the agent itself, at most 24 hours ahead, which runs
//! without approval. Recurring schedules are where runaway use comes from, so:
//!
//! - A proposal is stored with `made_by` and no time to run: the scheduler never sees it as due,
//!   and approving it (`schedules::approve`, the human's RPC only) checks it again as if made now.
//!   No tool approves, resumes or edits a schedule: an agent can pause or delete its own, never
//!   widen one (to change it, it proposes a new one).
//! - Only a named agent proposes, and only while it is working for the human: a session handling
//!   scheduled or triggered work (any chain a schedule or trigger started) is refused, so a
//!   reminder can't set the next reminder and a schedule can't breed more schedules.
//! - A reminder is for the caller itself, a one-off 5 minutes to 24 hours ahead, at most
//!   `REMINDERS_WAITING_MAX` waiting at once, 15 minutes apart, `REMINDERS_PER_DAY` a day.
//!   Proposals: at most `PROPOSALS_WAITING_MAX` waiting, `PROPOSALS_PER_DAY` a day; an agent has
//!   at most `AGENT_MADE_MAX` live schedules it made. Agents propose times (a cron line or a
//!   one-off), not triggers, and no "Then…" steps: those stay the human's.
//! - Once made (a reminder) or approved (a proposal), it is an ordinary schedule: its agent must
//!   take scheduled work (a Claude subscription agent only if the human opted it in, with the
//!   7-day expiry and quiet hours on by default), and the caps, spend caps, busy, missed-run
//!   and quiet-hour rules all apply when it runs.
//!
//! Limit (as for every agent rule, docs/PROTOCOL.md "Limits"): an agent that calls the client API
//! with the client token directly, not through MCP or `muster`, is the human as far as the daemon
//! can tell. What it makes or widens that way is still held for the owner's click in Muster's
//! window (`super::Held`, `rpc::from_window`; docs/SCHEDULING_PLAN.md "Approval and its limits").

use std::collections::HashMap;

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::plan::{self, DAY_MS, MINUTE_MS};
use super::{Conversation, MadeBy, MadeKind, NoteChoice, Schedule, ScheduleInput, When, WhenInput, cron};
use crate::agents::{Agent, Origin};
use crate::hub::Hub;
use crate::model::*;

/// A reminder runs at least this far ahead...
pub const REMINDER_MIN_AHEAD_MS: i64 = 5 * MINUTE_MS;
/// ...and at most this far.
pub const REMINDER_MAX_AHEAD_MS: i64 = DAY_MS;
/// Reminders of one agent still to run, at most.
pub const REMINDERS_WAITING_MAX: usize = 3;
/// Reminders one agent sets per rolling 24 hours (deleted ones count).
pub const REMINDERS_PER_DAY: usize = 10;
/// Proposals of one agent waiting for approval, at most.
pub const PROPOSALS_WAITING_MAX: usize = 3;
/// Proposals one agent makes per rolling 24 hours (deleted or dismissed ones count).
pub const PROPOSALS_PER_DAY: usize = 10;
/// Schedules one agent made that are still live (waiting, active or paused), at most.
pub const AGENT_MADE_MAX: usize = 5;
/// A proposal nobody approved is removed after this long.
pub const PROPOSAL_KEEP_MS: i64 = 7 * DAY_MS;
/// A reminder that ran is removed this long after it ran (its message stays in the agent's log).
pub const DONE_REMINDER_KEEP_MS: i64 = DAY_MS;
pub const REASON_MAX: usize = 1000;

/// What each agent made lately (scheduler clock), for the per-day limits: kept in memory, so a
/// delete doesn't give the allowance back. (After a restart it counts at least the schedules
/// still there that were made in the last day: `made_today`.)
#[derive(Default)]
pub struct MadeLog {
    log: Mutex<HashMap<i64, Vec<(MadeKind, i64)>>>,
    /// Held from the first limit check to the record of what was made, so two calls at once
    /// (the MCP tool and `muster schedule remind` together, say) can't both pass a limit that
    /// only one of them fits in. One lock for every agent: making a schedule is rare and quick.
    making: Mutex<()>,
}

impl MadeLog {
    fn count(&self, agent: i64, kind: MadeKind, since: i64) -> usize {
        self.log.lock().get(&agent).map_or(0, |v| v.iter().filter(|(k, t)| *k == kind && *t > since).count())
    }
    fn add(&self, agent: i64, kind: MadeKind, at: i64) {
        let mut g = self.log.lock();
        let v = g.entry(agent).or_default();
        v.retain(|(_, t)| *t > at - DAY_MS);
        v.push((kind, at));
    }
}

/// How many `kind` schedules `agent` made in the 24 hours before `now`: what the log recorded,
/// or, if more (the log starts empty after a restart), those still there.
fn made_today(hub: &Hub, agent: i64, kind: MadeKind, now: i64) -> usize {
    let logged = hub.schedules.made.count(agent, kind, now - DAY_MS);
    let kept = hub
        .schedules
        .list()
        .iter()
        .filter(|s| s.made_by.as_ref().is_some_and(|m| m.agent_id == agent && m.kind == kind && m.at > now - DAY_MS))
        .count();
    logged.max(kept)
}

/// The calling agent: a named agent's own session.
fn me(hub: &Hub, caller: SessionId) -> anyhow::Result<Agent> {
    hub.agents().by_session(caller).context(
        "only named agents (the ones in Muster's Agents panel) can have schedules: this session isn't one",
    )
}

/// Refused while handling scheduled or triggered work: a schedule can't make more schedules.
fn attended(hub: &Hub, caller: SessionId) -> anyhow::Result<()> {
    if let Some(p) = hub.agents().prov(caller)
        && p.origin != Origin::Human
    {
        let what = if p.origin == Origin::Webhook { "triggered" } else { "scheduled" };
        anyhow::bail!(
            "you're handling {what} work, and nothing started by a schedule or trigger may make schedules \
             (so one can't set up the next). Once the human writes to you again you can propose one, or \
             say in your reply what you'd like scheduled."
        );
    }
    Ok(())
}

/// Schedules `agent` made that are still live: waiting for approval, to run, paused, or with a
/// run still to happen (a one-off whose run was claimed keeps no next run, but may still wait
/// behind a turn of the human's). Not done or expired. Each with the time it runs at, for a
/// one-off: its next run, or the slot of its run still pending.
/// The schedules and the pending runs are read together, under the scheduler's claim lock
/// (`Schedules::with_pending`): a reminder whose run is being claimed can't slip between them.
fn live_made(hub: &Hub, agent: i64) -> anyhow::Result<Vec<(Schedule, Option<i64>)>> {
    let now = hub.schedules.now();
    let (list, pending) = hub.schedules.with_pending().context("reading the schedules")?;
    let pending: HashMap<i64, i64> = pending.into_iter().map(|r| (r.schedule_id, r.slot)).collect();
    Ok(list
        .into_iter()
        .filter(|s| s.made_by.as_ref().is_some_and(|m| m.agent_id == agent))
        .filter_map(|s| {
            let runs_at = s.next_run.or_else(|| pending.get(&s.id).copied());
            if s.waiting_approval() {
                return Some((s, None));
            }
            let a = hub.agents().by_id(s.agent_id)?;
            let expired = plan::expires_at(&s, &super::facts_for(hub, &a, &s)).is_some_and(|e| now >= e);
            (!expired && (runs_at.is_some() || s.when.is_trigger())).then_some((s, runs_at))
        })
        .collect())
}

/// When, as an agent gives it: exactly one of a cron line, a one-off time (`YYYY-MM-DDTHH:MM`
/// in `tz`, or unix ms) or minutes from now.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum At {
    Ms(i64),
    Local(String),
}

fn when_of(cron: Option<String>, at: Option<At>, in_minutes: Option<i64>, now: i64) -> anyhow::Result<WhenInput> {
    let given = cron.is_some() as u8 + at.is_some() as u8 + in_minutes.is_some() as u8;
    ensure!(given == 1, "give exactly one of cron (e.g. \"0 16 * * 5\"), at (\"YYYY-MM-DDTHH:MM\") or in_minutes");
    Ok(match (cron, at, in_minutes) {
        (Some(c), _, _) => WhenInput::Cron { cron: c },
        (_, Some(At::Ms(ms)), _) => WhenInput::Once { at: Some(ms), local: None },
        (_, Some(At::Local(l)), _) => WhenInput::Once { at: None, local: Some(l) },
        (_, _, Some(m)) => {
            ensure!((1..=366 * 24 * 60).contains(&m), "in_minutes is a whole number of minutes from 1");
            WhenInput::Once { at: Some(now + m * MINUTE_MS), local: None }
        }
        _ => unreachable!(),
    })
}

fn clean_reason(r: &str) -> anyhow::Result<String> {
    let r = crate::mcp::tools::clean_text(r).trim().to_string();
    ensure!(!r.is_empty(), "say why (reason): the human reads it before approving");
    ensure!(r.len() <= REASON_MAX, "the reason is at most {REASON_MAX} bytes");
    Ok(r)
}

fn local(s: &Schedule, ms: i64) -> String {
    let tz = cron::zone(&s.tz).unwrap_or_else(|_| jiff::tz::TimeZone::system());
    cron::format(&tz, ms, "%a %-d %b %Y %H:%M %Z")
}

/// A schedule as the agent tools show it.
fn brief(hub: &Hub, s: &Schedule) -> Value {
    let full = super::schedule_json(hub, s);
    json!({
        "schedule_id": s.id,
        "agent": full["agent"],
        "name": s.name,
        "prompt": s.prompt,
        "when": full["reading"],
        "status": full["status"],
        "next_run": s.next_run.map(|t| local(s, t)),
        "made_by": s.made_by.as_ref().map(|m| json!({"agent": m.agent, "kind": m.kind, "approved": m.approved_at.is_some()})),
        "last_run": full["last_run"].get("outcome"),
    })
}

/// The MCP schedule tools, run as session `caller` (src/mcp/tools.rs checks the rate limit).
pub fn tool(hub: &Hub, caller: SessionId, name: &str, args: Value) -> anyhow::Result<Value> {
    let me = me(hub, caller)?;
    let now = hub.schedules.now();
    match name {
        "list_schedules" => {
            ensure!(args.as_object().is_none_or(|o| o.is_empty()), "list_schedules takes no arguments");
            let list: Vec<Value> = hub
                .schedules
                .list()
                .iter()
                .filter(|s| s.agent_id == me.id || s.made_by.as_ref().is_some_and(|m| m.agent_id == me.id))
                .map(|s| brief(hub, s))
                .collect();
            Ok(json!({
                "schedules": list,
                "takes_scheduled_work": me.accepts_origins.contains(&Origin::Schedule),
                "limits": {
                    "reminder": "a one-off for yourself, 5 minutes to 24 hours ahead, no approval",
                    "proposals": "the human approves each one in the Agents panel",
                    "reminders_waiting_max": REMINDERS_WAITING_MAX,
                    "proposals_waiting_max": PROPOSALS_WAITING_MAX,
                    "live_schedules_you_made_max": AGENT_MADE_MAX,
                },
            }))
        }
        "set_reminder" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                prompt: String,
                #[serde(default)]
                in_minutes: Option<i64>,
                #[serde(default)]
                at: Option<At>,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                tz: Option<String>,
            }
            let p: P = serde_json::from_value(args)?;
            attended(hub, caller)?;
            // Its own agent must take scheduled work at all (the human's opt-in), said first.
            super::check_agent(hub, &me, Origin::Schedule).map_err(reword)?;
            let when = when_of(None, p.at, p.in_minutes, now)?;
            let tz = p.tz.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(cron::local_zone_name);
            let When::Once { at } = when.resolve(&tz)? else { unreachable!() };
            ensure!(
                at - now >= REMINDER_MIN_AHEAD_MS && at - now <= REMINDER_MAX_AHEAD_MS,
                "a reminder is a one-off for yourself 5 minutes to 24 hours ahead; for anything later or recurring, use propose_schedule (the human approves it)"
            );
            // From the first check to the record of it: one call at a time.
            let _making = hub.schedules.made.making.lock();
            let mine = live_made(hub, me.id)?;
            let waiting: Vec<i64> = mine
                .iter()
                .filter(|(s, _)| s.made_by.as_ref().is_some_and(|m| m.kind == MadeKind::Reminder))
                .filter_map(|(_, at)| *at)
                .collect();
            ensure!(waiting.len() < REMINDERS_WAITING_MAX, "you already have {} reminders waiting (at most {REMINDERS_WAITING_MAX}): delete one first", waiting.len());
            ensure!(
                waiting.iter().all(|t| (t - at).abs() >= plan::MIN_GAP_MS),
                "another of your reminders runs within 15 minutes of that time: reminders are at least 15 minutes apart"
            );
            ensure!(mine.len() < AGENT_MADE_MAX, "you already have {AGENT_MADE_MAX} schedules you made: delete one first");
            let today = made_today(hub, me.id, MadeKind::Reminder, now);
            ensure!(today < REMINDERS_PER_DAY, "you set {today} reminders in the last 24 hours (at most {REMINDERS_PER_DAY})");
            // Quiet hours: on by default for a Claude subscription agent, and not for the agent to
            // turn off. A reminder that would only be skipped is refused now.
            let f = super::facts(hub, &me);
            let settings = hub.schedules.settings();
            if f.subscription_claude && super::quiet_of(&settings).contains(at) {
                anyhow::bail!(
                    "that time is in the quiet hours ({}–{}), when your scheduled work is skipped: pick a time outside them",
                    settings.quiet_start,
                    settings.quiet_end
                );
            }
            let name = p.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
                let first = p.prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                crate::agents::truncate_bytes(&format!("Reminder: {first}"), 60)
            });
            let req = ScheduleInput {
                agent: Some(me.name.clone()),
                name: Some(name),
                prompt: Some(p.prompt),
                when: Some(WhenInput::Once { at: Some(at), local: None }),
                tz: Some(tz),
                conversation: Some(Conversation::Continue),
                ..Default::default()
            };
            let made = MadeBy { agent_id: me.id, agent: me.name.clone(), session_id: Some(caller), kind: MadeKind::Reminder, reason: String::new(), at: now, approved_at: None };
            let s = super::create_as(hub, req, Some(made), None).map_err(reword)?;
            hub.schedules.made.add(me.id, MadeKind::Reminder, now);
            Ok(json!({
                "schedule_id": s.id,
                "status": "active",
                "runs_at": local(&s, at),
                "note": "Muster sends you this prompt then, marked as scheduled (no approval needed for a one-off reminder for yourself). \
                         While you handle it you can't set another reminder. delete_schedule cancels it.",
            }))
        }
        "propose_schedule" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                #[serde(default)]
                agent: Option<String>,
                prompt: String,
                reason: String,
                #[serde(default)]
                cron: Option<String>,
                #[serde(default)]
                at: Option<At>,
                #[serde(default)]
                in_minutes: Option<i64>,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                tz: Option<String>,
                #[serde(default)]
                conversation: Option<Conversation>,
                #[serde(default)]
                quiet_hours: Option<bool>,
                #[serde(default)]
                daily_cap: Option<u32>,
                #[serde(default)]
                note_id: Option<NoteId>,
            }
            let p: P = serde_json::from_value(args)?;
            attended(hub, caller)?;
            let reason = clean_reason(&p.reason)?;
            let when = when_of(p.cron, p.at, p.in_minutes, now)?;
            let target = match p.agent.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                Some(n) => hub.agents().get(n).ok_or_else(|| crate::agents::no_agent(hub, n))?,
                None => me.clone(),
            };
            let _making = hub.schedules.made.making.lock();
            let mine = live_made(hub, me.id)?;
            let waiting = mine.iter().filter(|(s, _)| s.made_by.as_ref().is_some_and(MadeBy::waiting)).count();
            ensure!(waiting < PROPOSALS_WAITING_MAX, "{waiting} of your proposals are already waiting for the human (at most {PROPOSALS_WAITING_MAX})");
            ensure!(mine.len() < AGENT_MADE_MAX, "you already have {AGENT_MADE_MAX} schedules you made: delete one first");
            let today = made_today(hub, me.id, MadeKind::Proposal, now);
            ensure!(today < PROPOSALS_PER_DAY, "you made {today} proposals in the last 24 hours (at most {PROPOSALS_PER_DAY})");
            if let Some(n) = p.note_id {
                crate::notes::get(hub, n)?;
            }
            let req = ScheduleInput {
                agent: Some(target.name.clone()),
                name: p.name,
                prompt: Some(p.prompt),
                when: Some(when),
                tz: p.tz,
                conversation: p.conversation,
                quiet_hours: p.quiet_hours,
                daily_cap: p.daily_cap.map(Some),
                note: p.note_id.map(|id| Some(NoteChoice::Id { id })),
                ..Default::default()
            };
            let made = MadeBy { agent_id: me.id, agent: me.name.clone(), session_id: Some(caller), kind: MadeKind::Proposal, reason, at: now, approved_at: None };
            let s = super::create_as(hub, req, Some(made), None).map_err(reword)?;
            hub.schedules.made.add(me.id, MadeKind::Proposal, now);
            drop(_making);
            let j = super::schedule_json(hub, &s);
            let reading = j["reading"].as_str().unwrap_or("").to_string();
            let on = if target.id == me.id { String::new() } else { format!(" on {}", target.name) };
            super::run::alert(
                hub,
                "info",
                &format!("{} proposes a schedule: \"{}\"", me.name, s.name),
                &format!("{reading}{on}. Approve or dismiss it in the Agents panel."),
            );
            Ok(json!({
                "schedule_id": s.id,
                "status": "proposed",
                "agent": target.name,
                "when": reading,
                "estimate": j["estimate"]["text"],
                "note": "It doesn't run until the human approves it in the Agents panel (they may edit it first). \
                         list_schedules shows whether it was approved; dismissed proposals disappear.",
            }))
        }
        "pause_schedule" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                schedule_id: i64,
            }
            let p: P = serde_json::from_value(args)?;
            let s = hub.schedules.get(p.schedule_id).with_context(|| format!("no schedule {} (see list_schedules)", p.schedule_id))?;
            ensure!(
                s.agent_id == me.id || s.made_by.as_ref().is_some_and(|m| m.agent_id == me.id),
                "schedule {} neither runs on you nor was made by you: only the human can pause it",
                s.id
            );
            let j = super::set_paused(hub, s.id, true, None)?;
            Ok(json!({"schedule_id": s.id, "status": j["status"], "note": "Only the human can resume it."}))
        }
        "delete_schedule" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                schedule_id: i64,
            }
            let p: P = serde_json::from_value(args)?;
            let s = hub.schedules.get(p.schedule_id).with_context(|| format!("no schedule {} (see list_schedules)", p.schedule_id))?;
            ensure!(
                s.made_by.as_ref().is_some_and(|m| m.agent_id == me.id),
                "you didn't make schedule {}: only the human can delete it (pause_schedule pauses one that runs on you)",
                s.id
            );
            super::remove(hub, s.id)?;
            Ok(json!({"deleted": s.id}))
        }
        _ => anyhow::bail!("unknown schedule tool"),
    }
}

/// The human-facing errors of `create`, said to an agent.
fn reword(e: anyhow::Error) -> anyhow::Error {
    let t = format!("{e:#}");
    if t.contains("doesn't take scheduled work") {
        return anyhow::anyhow!(
            "{t} Only the human can change that: say in your reply what you'd like scheduled, and why."
        );
    }
    e
}

// ----- the estimate shown with a proposal -----

/// What a schedule would use: runs in the coming week (after quiet hours and the caps), what
/// pays for them, and the caps that bound them. Shown with every schedule an agent made.
pub fn estimate(hub: &Hub, agent: &Agent, s: &Schedule, now: i64) -> Value {
    let settings = hub.schedules.settings();
    let f = super::facts_for(hub, agent, s);
    let quiet = super::quiet_of(&settings);
    let tz = cron::zone(&s.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let per_day_cap = s.daily_cap.unwrap_or(u32::MAX).min(settings.unattended_cap);
    let (week, busiest) = match &s.when {
        When::Once { at } => (Some(u32::from(*at > now || s.waiting_approval())), Some(1)),
        When::Cron { cron: line } => match cron::Cron::parse(line) {
            Ok(c) => {
                let mut days: std::collections::BTreeMap<String, u32> = Default::default();
                let mut t = now;
                for _ in 0..(7 * 24 * 4 + 8) {
                    let Some(n) = c.next_after(&tz, t) else { break };
                    if n > now + 7 * DAY_MS {
                        break;
                    }
                    t = n;
                    if s.quiet_hours && quiet.contains(n) {
                        continue;
                    }
                    *days.entry(cron::format(&tz, n, "%Y-%m-%d")).or_default() += 1;
                }
                let capped: Vec<u32> = days.values().map(|n| (*n).min(per_day_cap)).collect();
                (Some(capped.iter().sum()), capped.iter().max().copied().or(Some(0)))
            }
            Err(_) => (None, None),
        },
        // A trigger runs when its event comes: at most its daily cap.
        _ => (None, s.daily_cap.map(|c| c.min(settings.unattended_cap))),
    };
    let billing = if f.subscription_claude {
        "claude_subscription"
    } else if crate::spend::bills_openrouter(hub, agent) {
        "openrouter"
    } else if agent.kind == SessionKind::Codex {
        "codex"
    } else {
        "other"
    };
    let recent = hub.store().count_unattended_since(agent.id, now_ms() - 7 * DAY_MS).unwrap_or(0);
    // Its times are in its own zone: said whenever that isn't this computer's, with the time
    // here for a one-off (an owner in London reading "08:30" for Los Angeles).
    let here = cron::local_zone_name();
    let other_zone = !s.when.is_trigger() && s.tz.trim() != here;
    // "America/Los Angeles", as the panel writes it.
    let zone = s.tz.trim().replace('_', " ");
    let mut lines = Vec::new();
    lines.push(match (&s.when, week, busiest) {
        (When::Once { at }, _, _) if other_zone => format!(
            "One run, {} {} time ({} here).",
            cron::format(&tz, *at, "%a %-d %b at %H:%M"),
            zone,
            cron::format(&jiff::tz::TimeZone::system(), *at, "%a %H:%M")
        ),
        (When::Once { at }, _, _) => format!("One run, {}.", cron::format(&tz, *at, "%a %-d %b at %H:%M")),
        (_, Some(0), _) => "No run in the coming week (quiet hours or its timing).".to_string(),
        (_, Some(w), Some(b)) => format!(
            "About {w} run{} in the coming week (at most {b} a day){}.",
            if w == 1 { "" } else { "s" },
            if other_zone { format!(", at {zone} times") } else { String::new() }
        ),
        (_, None, Some(b)) => format!("Runs when its event comes, at most {b} a day."),
        _ => "Runs when its event comes.".to_string(),
    });
    lines.push(match billing {
        "claude_subscription" => format!(
            "Uses your Claude subscription ({} is opted in to scheduled work){}; quiet hours {}.",
            agent.name,
            if s.when.recurs() { ": stops 7 days after approval unless you renew it" } else { "" },
            if s.quiet_hours { "on" } else { "off" }
        ),
        "openrouter" => {
            let own = settings.agent_spend_caps.get(&agent.id).map(|c| format!("; {} at {}", agent.name, crate::spend::money(*c as f64 / 100.0))).unwrap_or_default();
            format!(
                "Pay per use on OpenRouter: each run costs what its tokens cost. All OpenRouter agents together stop at {} a day{own}.",
                crate::spend::money(settings.spend_cap_cents as f64 / 100.0)
            )
        }
        "codex" => "Uses this computer's Codex login (your OpenAI plan).".to_string(),
        _ => "Uses the agent's own login.".to_string(),
    });
    lines.push(format!(
        "{} takes at most {} scheduled or triggered messages a day in all ({recent} in the last 7 days).",
        agent.name, settings.unattended_cap
    ));
    json!({
        "runs_per_week": week,
        "max_per_day": busiest,
        "billing": billing,
        "recent_unattended_7d": recent,
        // How often, what pays for it, the caps that bound it: one sentence each.
        "lines": lines,
        "text": lines.join(" "),
    })
}

/// The scheduler's tidying (`run::tick`): proposals nobody approved within 7 days go, and so do
/// schedules made outside Muster's window that nobody approved within 7 days (`Held`), and
/// reminders a day after they ran.
pub fn tidy(hub: &Hub, now: i64) {
    for s in hub.schedules.list() {
        if s.made_by.is_none()
            && let Some(h) = &s.held
            && h.what.contains(&super::HeldWhat::Made)
            && now - h.at >= super::HELD_KEEP_MS
        {
            hub.notice("info", format!("The schedule \"{}\", made outside Muster's window, waited 7 days without approval and was removed.", s.name));
            if let Err(e) = super::remove(hub, s.id) {
                tracing::warn!(schedule = s.id, "tidying a held schedule: {e:#}");
            }
            continue;
        }
        let Some(m) = &s.made_by else { continue };
        let stale = if m.waiting() {
            now - m.at >= PROPOSAL_KEEP_MS
        } else {
            m.kind == MadeKind::Reminder
                && s.next_run.is_none()
                && s.last_slot.is_some_and(|t| now - t >= DONE_REMINDER_KEEP_MS)
                && !hub.store().pending_runs().unwrap_or_default().iter().any(|r| r.schedule_id == s.id)
        };
        if !stale {
            continue;
        }
        if m.waiting() {
            hub.notice("info", format!("{}'s proposal \"{}\" waited 7 days without approval and was removed.", m.agent, s.name));
        }
        if let Err(e) = super::remove(hub, s.id) {
            tracing::warn!(schedule = s.id, "tidying an agent's schedule: {e:#}");
        }
    }
}

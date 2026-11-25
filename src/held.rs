//! Held queues: prompts that have waited too long to be typed, shown to the human once.
//!
//! `inject::watch_held` looks at every session with prompts waiting, every few seconds, and
//! feeds what it sees to [`step`]. A queue is held when its oldest prompt has not moved and
//! - for `HELD_AFTER_MS` nothing but the agent's own turn was in the way (a draft in the input
//!   box, a dialog, a session that isn't running, no reason at all), or
//! - the agent has worked for `STUCK_TURN_AFTER_MS` without a pause, that prompt still waiting.
//!
//! A held queue sets `SessionInfo::queue_held` (the sidebar's Needs you box lists it) and gets
//! one in-app notice and one desktop notification. It clears by itself when the queue moves.
//! Only who sent the prompts is shown, never their text: the sender in the
//! `[muster msg <id> from <sender> …]` header of a prompt Muster recorded as message `<id>`.

use serde::{Deserialize, Serialize};

/// Prompts that waited this long for anything but the agent's own turn are held.
pub const HELD_AFTER_MS: i64 = 15 * 60_000;
/// An agent in a turn this long, its oldest prompt still waiting, counts as held (a stuck turn).
pub const STUCK_TURN_AFTER_MS: i64 = 60 * 60_000;
/// How many senders a held queue names.
const MAX_SENDERS: usize = 6;

/// On `SessionInfo`: this session's prompts are held. Runtime only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueHeld {
    /// Unix ms since when the oldest prompt has waited at the head of the queue.
    pub since: i64,
    /// Why, as `session.queue`'s `waiting` says it (or that the turn may be stuck).
    pub reason: String,
    /// Who sent the waiting prompts, in queue order, each once ("you" for the human's own).
    pub senders: Vec<String>,
    /// How many prompts wait.
    pub prompts: u32,
}

/// What the watcher sees of one session's queue.
#[derive(Debug, Clone)]
pub struct Seen {
    /// Identifies the oldest prompt: a change means the queue moved.
    pub head: u64,
    /// The only thing in the way is the agent's own turn.
    pub busy: bool,
    /// Why the prompts wait, in Muster's words.
    pub reason: String,
    pub senders: Vec<String>,
    pub prompts: u32,
}

/// One session's wait, kept between looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    head: u64,
    /// Since when `head` has been the oldest prompt.
    since: i64,
    /// Since when something other than the agent's turn has been in the way (while it is).
    calm_since: i64,
    /// Since when the agent has worked without a pause (while it does).
    busy_since: i64,
    /// The human was told of this wait (cleared when the agent works again) / of this stuck
    /// turn (cleared when it stops working).
    told_wait: bool,
    told_stuck: bool,
}

/// One look at a session's queue: what to show for it (None: not held) and whether to tell the
/// human now. Each hold is told once: a wait until the agent works again, a stuck turn until it
/// stops working, both until the oldest prompt is typed.
pub fn step(track: &mut Option<Track>, seen: &Seen, now: i64) -> (Option<QueueHeld>, bool) {
    let t = match track {
        Some(t) if t.head == seen.head => t,
        _ => track.insert(Track { head: seen.head, since: now, calm_since: now, busy_since: now, told_wait: false, told_stuck: false }),
    };
    if seen.busy {
        t.calm_since = now;
        t.told_wait = false;
    } else {
        t.busy_since = now;
        t.told_stuck = false;
    }
    let stuck = seen.busy && now - t.busy_since >= STUCK_TURN_AFTER_MS;
    let waited = !seen.busy && now - t.calm_since >= HELD_AFTER_MS;
    if !stuck && !waited { return (None, false) }
    let tell = if stuck { !std::mem::replace(&mut t.told_stuck, true) } else { !std::mem::replace(&mut t.told_wait, true) };
    let reason = if stuck {
        format!("the agent has worked for over {} minutes without typing them and may be stuck: check its terminal", STUCK_TURN_AFTER_MS / 60_000)
    } else {
        seen.reason.clone()
    };
    (Some(QueueHeld { since: t.since, reason, senders: seen.senders.clone(), prompts: seen.prompts }), tell)
}

/// The sender named in the `[muster msg <id> from <sender> …]` header (agents.rs `envelope`) of
/// the prompt Muster recorded as message `msg_id`. Reads that header only: nothing of the
/// message itself, and nothing of a prompt that merely starts like one.
pub fn sender_of(prompt: &str, msg_id: i64) -> Option<&str> {
    let rest = prompt.strip_prefix("[muster msg ")?;
    let (id, rest) = rest.split_once(' ')?;
    if id != msg_id.to_string() { return None }
    let rest = rest.strip_prefix("from ")?;
    let name = &rest[..rest.find([' ', ']', '\n'])?];
    (!name.is_empty() && name.len() <= 64).then_some(name)
}

/// The senders of waiting prompts, in order, each once, at most `MAX_SENDERS`. `prompts` gives
/// each prompt's text, the id of the message Muster recorded it as (if any) and whether it is the
/// human's own (then it is "you").
pub fn senders<'a>(prompts: impl Iterator<Item = (&'a str, Option<i64>, bool)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (text, msg_id, human) in prompts {
        let name = match msg_id.and_then(|id| sender_of(text, id)) {
            _ if human => "you",
            Some("human") => "you",
            Some(name) => name,
            None => continue,
        };
        if !out.iter().any(|n| n == name) && out.len() < MAX_SENDERS { out.push(name.to_string()); }
    }
    out
}

/// "42 min", "1 h 5 min", "2 h": how long a queue has been held.
pub fn waited(ms: i64) -> String {
    let min = (ms / 60_000).max(0);
    match (min / 60, min % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// The notice for a queue that just became held: a short headline and the rest.
/// `who` is the agent's (or session's) name.
pub fn notice(who: &str, held: &QueueHeld, now: i64) -> (String, String) {
    let n = held.prompts;
    let what = if n == 1 { "1 message has".to_string() } else { format!("{n} messages have") };
    let summary = format!("{who}: {what} waited {}", waited(now - held.since));
    let mut body = held.reason.clone();
    if let Some(first) = body.get(..1) { body = first.to_uppercase() + &body[1..]; }
    if !held.senders.is_empty() { body.push_str(&format!(". From {}", held.senders.join(", "))); }
    body.push('.');
    (summary, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn seen(head: u64, busy: bool, reason: &str) -> Seen {
        Seen { head, busy, reason: reason.into(), senders: vec!["lead".into()], prompts: 2 }
    }

    /// 24 Nov: three jobs for one agent sat 40+ minutes behind "unsent text is in the input
    /// box" and nothing told the human. After 15 minutes it is held, and told once.
    #[test]
    fn a_wait_that_is_not_the_agents_turn_is_held_after_15_minutes_and_told_once() {
        let draft = seen(1, false, "unsent text is in the input box: send it or clear it");
        let mut t = None;
        assert_eq!(step(&mut t, &draft, 0), (None, false));
        assert_eq!(step(&mut t, &draft, 14 * MIN + 59_000), (None, false));
        let (held, tell) = step(&mut t, &draft, 15 * MIN);
        assert!(tell);
        assert_eq!(held, Some(QueueHeld { since: 0, reason: draft.reason.clone(), senders: vec!["lead".into()], prompts: 2 }));
        // Still held on every later look, and never told again while that prompt waits.
        for at in [15 * MIN + 10_000, 40 * MIN, 3 * 60 * MIN] {
            let (held, tell) = step(&mut t, &draft, at);
            assert!(held.is_some() && !tell, "{at}");
        }
        // Its reason follows what is in the way now, without a second notice.
        let (held, tell) = step(&mut t, &seen(1, false, "a dialog is open in the terminal"), 3 * 60 * MIN + 10_000);
        assert_eq!((held.unwrap().reason.as_str(), tell), ("a dialog is open in the terminal", false));
    }

    #[test]
    fn it_clears_when_the_queue_moves_and_the_next_hold_is_told_again() {
        let mut t = None;
        step(&mut t, &seen(1, false, "a dialog is open in the terminal"), 0);
        assert!(step(&mut t, &seen(1, false, "a dialog is open in the terminal"), 20 * MIN).0.is_some());
        // The oldest prompt was typed: the next one starts its own wait.
        assert_eq!(step(&mut t, &seen(2, false, "a dialog is open in the terminal"), 21 * MIN), (None, false));
        assert_eq!(step(&mut t, &seen(2, false, "a dialog is open in the terminal"), 35 * MIN), (None, false));
        let (held, tell) = step(&mut t, &seen(2, false, "a dialog is open in the terminal"), 36 * MIN);
        assert_eq!((held.unwrap().since, tell), (21 * MIN, true));
    }

    /// A busy agent is not a hold: its prompts are typed when its turn ends. Only a turn of an
    /// hour, the same prompt still waiting, counts (a stuck turn), with that reason.
    #[test]
    fn a_busy_agent_is_held_only_after_an_hour() {
        let busy = seen(1, true, "it is typed once the agent is idle (now working)");
        let mut t = None;
        for at in [0, 15 * MIN, 40 * MIN, 59 * MIN] {
            assert_eq!(step(&mut t, &busy, at), (None, false), "{at}");
        }
        let (held, tell) = step(&mut t, &busy, 60 * MIN);
        let held = held.unwrap();
        assert!(tell && held.reason.contains("over 60 minutes without typing them and may be stuck"), "{}", held.reason);
        assert_eq!(held.since, 0);
        assert!(!step(&mut t, &busy, 61 * MIN).1);
    }

    /// The hour is an hour of work without a pause: a prompt held 65 minutes behind a draft is
    /// not behind a stuck turn the moment the draft is sent.
    #[test]
    fn a_turn_that_just_started_is_not_stuck_however_long_the_prompt_waited() {
        let busy = seen(1, true, "it is typed once the agent is idle (now working)");
        let draft = seen(1, false, "unsent text is in the input box: send it or clear it");
        let mut t = None;
        step(&mut t, &draft, 0);
        assert!(step(&mut t, &draft, 65 * MIN).1);
        assert_eq!(step(&mut t, &busy, 65 * MIN + 10_000), (None, false));
        assert_eq!(step(&mut t, &busy, 124 * MIN), (None, false));
        // A second wait in the way of the same prompt is a new hold, told again.
        let dialog = seen(1, false, "a permission prompt is open in the terminal");
        assert_eq!(step(&mut t, &dialog, 125 * MIN), (None, false));
        let (held, tell) = step(&mut t, &dialog, 140 * MIN);
        assert_eq!((held.unwrap().since, tell), (0, true));
        assert!(!step(&mut t, &dialog, 141 * MIN).1);
    }

    /// The 15 minutes count from when the turn stopped being the reason: a prompt queued behind
    /// a 50-minute turn and then a draft is held 15 minutes after the turn ended.
    #[test]
    fn time_behind_a_turn_does_not_count_toward_the_15_minutes() {
        let busy = seen(1, true, "it is typed once the agent is idle (now working)");
        let draft = seen(1, false, "unsent text is in the input box: send it or clear it");
        let mut t = None;
        step(&mut t, &busy, 0);
        step(&mut t, &busy, 50 * MIN);
        assert_eq!(step(&mut t, &draft, 50 * MIN + 10_000), (None, false));
        assert_eq!(step(&mut t, &draft, 64 * MIN), (None, false));
        let (held, tell) = step(&mut t, &draft, 65 * MIN);
        // It says how long the prompt has waited in all.
        assert_eq!((held.unwrap().since, tell), (0, true));
    }

    #[test]
    fn senders_come_from_the_headers_only() {
        assert_eq!(sender_of("[muster msg 412 from lead origin=human hop=1]\n  the secret plan", 412), Some("lead"));
        assert_eq!(sender_of("[muster msg 7 from schedule:nightly origin=schedule hop=0]\nbody", 7), Some("schedule:nightly"));
        for not_a_header in ["", "from lead", "[muster msg x from lead origin=human hop=0]", "[muster msg 7 from ",
            "[muster msg 7 to lead]", " [muster msg 7 from lead origin=human hop=0]", "please read [muster msg 7 from lead x]",
            // Another message's header, pasted at the start of this one.
            "[muster msg 8 from lead origin=human hop=0]"] {
            assert_eq!(sender_of(not_a_header, 7), None, "{not_a_header:?}");
        }
        let list = senders([
            ("[muster msg 1 from lead origin=human hop=1]\n  job one: the secret plan", Some(1), false),
            ("[muster msg 2 from manager origin=human hop=1]\n  job two", Some(2), false),
            ("[muster msg 3 from lead origin=human hop=1]\n  job three", Some(3), false),
            ("[muster msg 4 from human origin=human hop=0]\n  mine", Some(4), false),
            ("answer Q3: a", None, true),
            ("a spawn prompt with no header", None, false),
            // A prompt that only looks like a message (a pasted log line) names nobody.
            ("[muster msg 7 from private-token and more", None, false),
            ("[muster msg 7 from private-token and more", Some(9), false),
            ("[muster msg 7 from private-token and more", None, true),
        ].into_iter());
        assert_eq!(list, ["lead", "manager", "you"]);
        assert_eq!(senders((0..20).map(|_| ("x", None, false))).len(), 0);
    }

    #[test]
    fn the_notice_names_the_agent_how_long_why_and_who_sent_them() {
        let held = QueueHeld { since: 0, reason: "unsent text is in the input box: send it or clear it".into(),
            senders: vec!["lead".into(), "manager".into()], prompts: 3 };
        assert_eq!(notice("writer", &held, 15 * MIN), (
            "writer: 3 messages have waited 15 min".to_string(),
            "Unsent text is in the input box: send it or clear it. From lead, manager.".to_string(),
        ));
        let one = QueueHeld { senders: vec![], prompts: 1, ..held };
        assert_eq!(notice("writer", &one, 75 * MIN).0, "writer: 1 message has waited 1 h 15 min");
        assert_eq!(notice("writer", &one, 75 * MIN).1, "Unsent text is in the input box: send it or clear it.");
        assert_eq!((waited(120 * MIN), waited(59_000)), ("2 h".to_string(), "0 min".to_string()));
    }
}

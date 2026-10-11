//! Guardrails for agent-driven orchestration. Pure logic, no hub access, so it is unit-testable.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::bots::Origin;
use crate::model::{AgentState, ChannelId, SessionId, SessionInfo, SessionKind};

/// Env var (on the daemon) that lets agents run shell commands through MCP.
pub const ENV_ALLOW_SHELL: &str = "MUSTER_MCP_ALLOW_SHELL";
/// Env var (on the daemon) that lets agents reach channels/sessions in other workspaces.
pub const ENV_CROSS_WORKSPACE: &str = "MUSTER_MCP_CROSS_WORKSPACE";

/// Tunable limits. `Default` holds the values documented in docs/MCP.md.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Max generations below a human-started session (human-started = depth 0).
    pub max_depth: u32,
    /// Max live sessions whose parent is the caller.
    pub max_children_per_caller: usize,
    /// Max live MCP-spawned sessions (parent_id set) daemon-wide.
    pub max_mcp_live_total: usize,
    /// send_message calls allowed per caller per `send_window_ms`.
    pub send_per_window: usize,
    pub send_window_ms: i64,
    /// post_to_channel calls allowed per caller per `send_window_ms`.
    pub post_per_window: usize,
    pub wait_default_s: u64,
    pub wait_max_s: u64,
    /// wait_for_state: how long after a send an unchanged Idle state still counts as stale.
    pub settle_grace_ms: i64,
    pub recent_default_lines: usize,
    pub recent_max_lines: usize,
    pub diff_default_bytes: usize,
    pub diff_max_bytes: usize,
    pub max_message_chars: usize,
    pub max_post_chars: usize,
    /// spawn_agent calls allowed per caller per `spawn_window_ms`.
    pub spawn_per_window: usize,
    pub spawn_window_ms: i64,
    /// Max non-archived channels created through spawn_agent's `new_channel`, daemon-wide.
    pub max_mcp_channels: usize,
    /// Allow shell spawns with a prompt and send_message to shell sessions
    /// (`MUSTER_MCP_ALLOW_SHELL=1`). Off by default: that is arbitrary command execution.
    pub allow_shell: bool,
    /// Allow targets outside the caller's workspace (`MUSTER_MCP_CROSS_WORKSPACE=1`).
    pub cross_workspace: bool,
}

impl Limits {
    /// Defaults plus the human's opt-ins from the daemon's environment.
    pub fn from_env() -> Self {
        let on = |k: &str| {
            std::env::var(k).is_ok_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
        };
        Self {
            allow_shell: on(ENV_ALLOW_SHELL),
            cross_workspace: on(ENV_CROSS_WORKSPACE),
            ..Self::default()
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_children_per_caller: 8,
            max_mcp_live_total: 24,
            send_per_window: 20,
            send_window_ms: 60_000,
            post_per_window: 30,
            wait_default_s: 300,
            wait_max_s: 1800,
            settle_grace_ms: 15_000,
            recent_default_lines: 100,
            recent_max_lines: 2000,
            diff_default_bytes: 60_000,
            diff_max_bytes: 400_000,
            max_message_chars: 100_000,
            max_post_chars: 20_000,
            spawn_per_window: 10,
            spawn_window_ms: 10 * 60_000,
            max_mcp_channels: 10,
            allow_shell: false,
            cross_workspace: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// FUTURE (human-enabled approvals): PLAN.md forbids agents from answering permission prompts
// "unless the human enables it". There is deliberately NO approve tool today. To add one:
//   1. Replace this constant with a per-channel/per-workspace human setting read from the hub.
//   2. Add an `approve_prompt(session_id, decision)` tool in tools.rs that is only listed and
//      only callable when the setting is on, calls `inject::approve`, and never lets a session
//      approve its own prompt.
//   3. Keep logging it as McpCall plus an Approval event with `from: "session:<id>"`.
// ---------------------------------------------------------------------------------------------
pub const AGENTS_MAY_APPROVE: bool = false;

// ----- bots (PLAN.md §10) -----

/// Max hop of a bot message: a message from the human or a trigger is hop 0, a message a bot
/// sends while handling a hop-N message is hop N+1. Deeper chains are refused.
pub const MAX_BOT_HOPS: u32 = 4;
/// Messages muster delivers to one bot per rolling 24 hours (any origin, the human's included).
pub const BOT_DAILY_PROMPT_CAP: usize = 200;
pub const BOT_CAP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// May a message with `origin` at `hop` be delivered to bot `bot` (which accepts `accepts`
/// and was sent `recent` messages in the last 24 hours)?
pub fn check_bot_delivery(bot: &str, accepts: &[Origin], origin: Origin, hop: u32, recent: usize) -> Result<(), String> {
    if !accepts.contains(&origin) {
        let list: Vec<&str> = accepts.iter().map(|o| o.as_str()).collect();
        return Err(format!(
            "bot {bot} accepts only {} messages, and this one is origin={} (the chain it belongs to was started by a {}, not the human). \
             Claude bots take human-started work only by default; route scheduled or triggered work to a Codex bot, or ask the human \
             to run it (or to recreate the bot with --accepts).",
            if list.is_empty() { "no".to_string() } else { list.join("/") },
            origin.as_str(),
            origin.as_str()
        ));
    }
    if hop > MAX_BOT_HOPS {
        return Err(format!(
            "bot message chain too deep: this would be hop {hop} and the maximum is {MAX_BOT_HOPS}. Answer with what you have, or ask the human."
        ));
    }
    if recent >= BOT_DAILY_PROMPT_CAP {
        return Err(format!(
            "bot {bot} already got {recent} messages in the last 24 hours (cap {BOT_DAILY_PROMPT_CAP}). Try again later or ask the human."
        ));
    }
    Ok(())
}

/// Claude agents that aren't bots (no accepts list of their own) take human-rooted work only,
/// like a Claude bot by default: no messages or spawns from a scheduled or triggered chain.
pub fn check_claude_origin(kind: SessionKind, origin: Origin) -> Result<(), String> {
    if kind == SessionKind::Claude && origin != Origin::Human {
        return Err(format!(
            "you are handling an origin={} message, and Claude agents only take work the human started. Use a Codex agent or bot \
             for it, or ask the human.",
            origin.as_str()
        ));
    }
    Ok(())
}

/// Number of ancestors of `id` (0 for a human-started session). Cycle-safe.
///
/// `recorded` holds depths remembered at spawn time, so removing an ancestor from the session
/// list can't shorten a lineage: the result is the max of the walked chain and
/// `steps + recorded[node]` for every node on it.
pub fn spawn_depth(
    sessions: &[SessionInfo],
    recorded: &HashMap<SessionId, u32>,
    id: SessionId,
) -> u32 {
    let parents: HashMap<SessionId, Option<SessionId>> =
        sessions.iter().map(|s| (s.id, s.parent_id)).collect();
    let mut seen = HashSet::from([id]);
    let mut steps = 0;
    let mut best = recorded.get(&id).copied().unwrap_or(0);
    let mut cur = parents.get(&id).copied().flatten();
    while let Some(p) = cur {
        if !seen.insert(p) {
            break;
        }
        steps += 1;
        best = best.max(steps + recorded.get(&p).copied().unwrap_or(0));
        cur = parents.get(&p).copied().flatten();
    }
    best.max(steps)
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SpawnBudget {
    pub caller_depth: u32,
    pub max_depth: u32,
    pub live_children: usize,
    pub max_children: usize,
    pub mcp_live_total: usize,
    pub max_mcp_live_total: usize,
}

pub fn spawn_budget(
    sessions: &[SessionInfo],
    live: &HashSet<SessionId>,
    recorded: &HashMap<SessionId, u32>,
    caller: SessionId,
    limits: &Limits,
) -> SpawnBudget {
    let is_live = |s: &&SessionInfo| live.contains(&s.id);
    SpawnBudget {
        caller_depth: spawn_depth(sessions, recorded, caller),
        max_depth: limits.max_depth,
        live_children: sessions
            .iter()
            .filter(is_live)
            .filter(|s| s.parent_id == Some(caller))
            .count(),
        max_children: limits.max_children_per_caller,
        mcp_live_total: sessions
            .iter()
            .filter(is_live)
            .filter(|s| s.parent_id.is_some())
            .count(),
        max_mcp_live_total: limits.max_mcp_live_total,
    }
}

/// Ok if `caller` may spawn one more session.
pub fn check_spawn(b: &SpawnBudget) -> Result<(), String> {
    if b.caller_depth + 1 > b.max_depth {
        return Err(format!(
            "spawn depth limit reached: you are {} level(s) below a human-started session and the \
             maximum is {}. Do the work yourself or ask the human.",
            b.caller_depth, b.max_depth
        ));
    }
    if b.live_children >= b.max_children {
        return Err(format!(
            "you already have {} live child sessions (max {}). Wait for one to finish or ask the \
             human to close some.",
            b.live_children, b.max_children
        ));
    }
    if b.mcp_live_total >= b.max_mcp_live_total {
        return Err(format!(
            "daemon-wide limit of {} live agent-spawned sessions reached.",
            b.max_mcp_live_total
        ));
    }
    Ok(())
}

/// Sliding-window rate limiter keyed by caller.
#[derive(Debug, Default)]
pub struct RateLimiter {
    hits: HashMap<SessionId, VecDeque<i64>>,
}

impl RateLimiter {
    /// Records a hit and returns Ok, or returns Err(seconds until a slot frees up).
    pub fn check_and_record(
        &mut self,
        caller: SessionId,
        now: i64,
        max: usize,
        window_ms: i64,
    ) -> Result<(), u64> {
        let q = self.hits.entry(caller).or_default();
        while q.front().is_some_and(|&t| now - t >= window_ms) {
            q.pop_front();
        }
        if q.len() >= max {
            let oldest = *q.front().unwrap_or(&now);
            let wait_ms = (oldest + window_ms - now).max(0);
            return Err((wait_ms as u64).div_ceil(1000).max(1));
        }
        q.push_back(now);
        Ok(())
    }

    /// Give back the most recent hit (the action it paid for failed).
    pub fn refund(&mut self, caller: SessionId) {
        if let Some(q) = self.hits.get_mut(&caller) {
            q.pop_back();
        }
    }

    pub fn forget(&mut self, caller: SessionId) {
        self.hits.remove(&caller);
    }
}

/// Mutable guard state shared by all MCP requests.
#[derive(Debug, Default)]
pub struct GuardState {
    pub sends: RateLimiter,
    pub posts: RateLimiter,
    pub spawns: RateLimiter,
    /// Channels created through spawn_agent's `new_channel` (in memory: after a daemon restart
    /// earlier ones no longer count toward `max_mcp_channels`).
    pub mcp_channels: HashSet<ChannelId>,
    /// Target session -> unix ms of the last MCP send_message to it (for wait_for_state).
    pub last_send: HashMap<SessionId, i64>,
    /// Session -> unix ms when its injection queue last shrank (a queued message was delivered).
    pub last_delivery: HashMap<SessionId, i64>,
    /// Last seen queue length per session (to detect deliveries).
    pub prev_queued: HashMap<SessionId, u32>,
    /// Depth of each MCP-spawned session, recorded at spawn time.
    pub spawned_depth: HashMap<SessionId, u32>,
}

impl GuardState {
    /// Feed every session update through here; records deliveries out of the queue.
    pub fn observe_session(&mut self, info: &SessionInfo, now: i64) {
        let prev = self.prev_queued.insert(info.id, info.queued).unwrap_or(0);
        if info.queued < prev {
            self.last_delivery.insert(info.id, now);
        }
    }

    /// Note that a send left `queued` messages waiting (so the later drain is detected even
    /// if the update announcing the increase was missed).
    pub fn note_queued(&mut self, id: SessionId, queued: u32) {
        let e = self.prev_queued.entry(id).or_insert(0);
        *e = (*e).max(queued);
    }

    pub fn forget(&mut self, id: SessionId) {
        self.sends.forget(id);
        self.posts.forget(id);
        self.spawns.forget(id);
        self.last_send.remove(&id);
        self.last_delivery.remove(&id);
        self.prev_queued.remove(&id);
    }
}

/// One word summarizing a channel's sessions, most urgent first.
pub fn aggregate_state<I: IntoIterator<Item = AgentState>>(states: I) -> &'static str {
    const ORDER: [AgentState; 8] = [
        AgentState::NeedsApproval,
        AgentState::AskingQuestion,
        AgentState::Error,
        AgentState::Stuck,
        AgentState::Working,
        AgentState::Starting,
        AgentState::Idle,
        AgentState::Exited,
    ];
    let set: HashSet<AgentState> = states.into_iter().collect();
    ORDER
        .iter()
        .find(|s| set.contains(s))
        .map(|s| s.as_str())
        .unwrap_or("empty")
}

#[cfg(test)]
pub(crate) fn test_session(id: SessionId, parent: Option<SessionId>) -> SessionInfo {
    SessionInfo {
        id,
        channel_id: 1,
        kind: crate::model::SessionKind::Shell,
        name: format!("s{id}"),
        cwd: "/tmp".into(),
        state: AgentState::Idle,
        state_detail: None,
        state_since: 0,
        title: None,
        parent_id: parent,
        pid: None,
        exit_code: None,
        cols: 80,
        rows: 24,
        queued: 0,
        subagents: 0,
        size_owner: None,
        state_seq: 0,
        created_at: 0,
        window_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_walks_parents_and_survives_cycles() {
        let s = vec![
            test_session(1, None),
            test_session(2, Some(1)),
            test_session(3, Some(2)),
            test_session(4, Some(3)),
        ];
        let none = HashMap::new();
        assert_eq!(spawn_depth(&s, &none, 1), 0);
        assert_eq!(spawn_depth(&s, &none, 3), 2);
        assert_eq!(spawn_depth(&s, &none, 4), 3);
        // Unknown parent (removed) stops the walk...
        assert_eq!(spawn_depth(&[test_session(9, Some(77))], &none, 9), 1);
        // ...but recorded depths keep the lineage: 77 was depth 2, so 9 is depth 3.
        let rec = HashMap::from([(77, 2)]);
        assert_eq!(spawn_depth(&[test_session(9, Some(77))], &rec, 9), 3);
        let rec = HashMap::from([(9, 3)]);
        assert_eq!(spawn_depth(&[test_session(9, None)], &rec, 9), 3);
        let cyc = vec![test_session(1, Some(2)), test_session(2, Some(1))];
        assert_eq!(spawn_depth(&cyc, &none, 1), 1);
    }

    #[test]
    fn spawn_limits() {
        let l = Limits::default();
        let s = vec![
            test_session(1, None),
            test_session(2, Some(1)),
            test_session(3, Some(2)),
        ];
        let live: HashSet<_> = [1, 2, 3].into();
        // depth 2 caller may create depth 3.
        assert!(check_spawn(&spawn_budget(&s, &live, &HashMap::new(), 3, &l)).is_ok());
        let mut s4 = s.clone();
        s4.push(test_session(4, Some(3)));
        let live4: HashSet<_> = [1, 2, 3, 4].into();
        let err = check_spawn(&spawn_budget(&s4, &live4, &HashMap::new(), 4, &l)).unwrap_err();
        assert!(err.contains("depth"), "{err}");

        // 8 live children blocks; exited children don't count.
        let mut kids = vec![test_session(1, None)];
        kids.extend((10..18).map(|i| test_session(i, Some(1))));
        let mut live: HashSet<_> = kids.iter().map(|k| k.id).collect();
        assert!(
            check_spawn(&spawn_budget(&kids, &live, &HashMap::new(), 1, &l))
                .unwrap_err()
                .contains("child")
        );
        live.remove(&10);
        assert!(check_spawn(&spawn_budget(&kids, &live, &HashMap::new(), 1, &l)).is_ok());

        // Global cap.
        let mut many = vec![test_session(1, None)];
        for p in 0..6u32 {
            many.push(test_session(100 + p, None));
            many.extend((0..4).map(|i| test_session(200 + p * 10 + i, Some(100 + p))));
        }
        let live: HashSet<_> = many.iter().map(|k| k.id).collect();
        let b = spawn_budget(&many, &live, &HashMap::new(), 1, &l);
        assert_eq!(b.mcp_live_total, 24);
        assert!(check_spawn(&b).unwrap_err().contains("daemon-wide"));
    }

    #[test]
    fn rate_limit_window() {
        let mut r = RateLimiter::default();
        for i in 0..20 {
            assert!(r.check_and_record(1, 1000 + i, 20, 60_000).is_ok());
        }
        let wait = r.check_and_record(1, 2000, 20, 60_000).unwrap_err();
        assert_eq!(wait, 59);
        // Other callers are independent.
        assert!(r.check_and_record(2, 2000, 20, 60_000).is_ok());
        // A refund returns the slot.
        r.refund(2);
        assert!(r.check_and_record(2, 2000, 1, 60_000).is_ok());
        // After the window slides past the first hit, one slot frees up.
        assert!(r.check_and_record(1, 61_000, 20, 60_000).is_ok());
        assert!(r.check_and_record(1, 61_000, 20, 60_000).is_err());
    }

    #[test]
    fn aggregate_prefers_urgent() {
        use AgentState::*;
        assert_eq!(
            aggregate_state([Idle, Working, NeedsApproval]),
            "needs_approval"
        );
        assert_eq!(aggregate_state([Idle, Exited]), "idle");
        assert_eq!(aggregate_state([]), "empty");
    }

    #[test]
    fn delivery_tracking() {
        let mut g = GuardState::default();
        let mut s = test_session(5, None);
        s.queued = 2;
        g.observe_session(&s, 100);
        assert!(g.last_delivery.get(&5).is_none());
        s.queued = 1;
        g.observe_session(&s, 200);
        assert_eq!(g.last_delivery.get(&5), Some(&200));
        // note_queued catches a drain whose increase we never observed.
        g.note_queued(6, 1);
        let mut t = test_session(6, None);
        t.queued = 0;
        g.observe_session(&t, 300);
        assert_eq!(g.last_delivery.get(&6), Some(&300));
    }

    #[test]
    fn bot_origin_rule() {
        use Origin::*;
        let claude = [Human];
        let codex = [Human, Schedule, Webhook];
        assert!(check_bot_delivery("a", &claude, Human, 0, 0).is_ok());
        let e = check_bot_delivery("a", &claude, Schedule, 1, 0).unwrap_err();
        assert!(e.contains("accepts only human") && e.contains("origin=schedule"), "{e}");
        assert!(check_bot_delivery("a", &claude, Webhook, 0, 0).is_err());
        assert!(check_bot_delivery("b", &codex, Schedule, 0, 0).is_ok());
        assert!(check_bot_delivery("b", &[], Human, 0, 0).unwrap_err().contains("accepts only no"));
        // Non-bot Claude agents: human-rooted work only; Codex and shells aren't limited.
        assert!(check_claude_origin(SessionKind::Claude, Human).is_ok());
        assert!(check_claude_origin(SessionKind::Claude, Webhook).is_err());
        assert!(check_claude_origin(SessionKind::Codex, Schedule).is_ok());
    }

    #[test]
    fn bot_hop_limit_and_daily_cap() {
        let all = [Origin::Human];
        assert!(check_bot_delivery("a", &all, Origin::Human, MAX_BOT_HOPS, 0).is_ok());
        let e = check_bot_delivery("a", &all, Origin::Human, MAX_BOT_HOPS + 1, 0).unwrap_err();
        assert!(e.contains("too deep"), "{e}");
        assert!(check_bot_delivery("a", &all, Origin::Human, 0, BOT_DAILY_PROMPT_CAP - 1).is_ok());
        let e = check_bot_delivery("a", &all, Origin::Human, 0, BOT_DAILY_PROMPT_CAP).unwrap_err();
        assert!(e.contains("cap 200"), "{e}");
    }

    #[test]
    fn approvals_disabled() {
        assert!(!AGENTS_MAY_APPROVE);
    }
}

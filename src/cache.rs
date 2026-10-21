//! Cold prompt cache: when an agent has sat idle longer than its provider keeps the
//! conversation cached, its next message re-reads the whole conversation at full price.
//!
//! The daemon tracks, per agent session, when it last talked to its model
//! (`SessionInfo.last_turn_at`: the last time it was seen entering or leaving `Working`, or a
//! turn-end hook), and publishes `cache_cold_at` = that + the provider's cache lifetime from the
//! shared preferences (`UiPrefs.cache_ttl`). Shells never get one. Every function takes `now`,
//! so tests choose the clock.
//!
//! Why this clock: Anthropic and OpenAI (for its default 30-minute cache) refresh the cache on
//! every request that reads it, and Claude Code itself treats the cache as warm "for one cache
//! TTL after Claude Code last sent a request in this conversation or Claude last responded"
//! (code.claude.com/docs/en/prompt-caching). The end of the last turn is the newest request the
//! daemon can see.

use serde::{Deserialize, Serialize};

use crate::model::{AgentState, SessionInfo, SessionKind};
use crate::openrouter::Provider;

/// Longest cache lifetime one can set, in minutes (a day).
pub const TTL_MAX_MIN: u32 = 24 * 60;

/// How long each CLI/provider pair keeps a conversation cached, in minutes.
///
/// Defaults (researched 2026-10, see docs/PROTOCOL.md "Cold prompt cache"):
/// - Claude Code on a Claude subscription: 1 hour for the main conversation (5 minutes once
///   the plan is on usage credits).
/// - Claude Code on OpenRouter (an Anthropic API endpoint): 5 minutes unless the CLI is told
///   to ask for the 1-hour cache.
/// - Codex on a ChatGPT plan: not documented; 30 minutes is OpenAI's API default for current
///   models (guess).
/// - Codex on OpenRouter: 10 minutes, OpenRouter's sticky routing ends after 10 idle minutes and
///   the next request may land on another provider's cache (guess).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheTtl {
    pub claude: u32,
    pub claude_openrouter: u32,
    pub codex: u32,
    pub codex_openrouter: u32,
}

impl Default for CacheTtl {
    fn default() -> Self {
        Self { claude: 60, claude_openrouter: 5, codex: 30, codex_openrouter: 10 }
    }
}

/// `prefs.set {cache_ttl}`: only the fields given change.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheTtlChange {
    pub claude: Option<u32>,
    pub claude_openrouter: Option<u32>,
    pub codex: Option<u32>,
    pub codex_openrouter: Option<u32>,
}

impl CacheTtl {
    /// Apply a change; each value must be 1..=TTL_MAX_MIN minutes.
    pub fn apply(&mut self, change: CacheTtlChange) -> anyhow::Result<()> {
        let mut next = *self;
        for (name, v, slot) in [
            ("claude", change.claude, &mut next.claude),
            ("claude_openrouter", change.claude_openrouter, &mut next.claude_openrouter),
            ("codex", change.codex, &mut next.codex),
            ("codex_openrouter", change.codex_openrouter, &mut next.codex_openrouter),
        ] {
            if let Some(v) = v {
                anyhow::ensure!((1..=TTL_MAX_MIN).contains(&v), "cache_ttl.{name}: 1 to {TTL_MAX_MIN} minutes");
                *slot = v;
            }
        }
        *self = next;
        Ok(())
    }

    /// Minutes for a session of `kind` billed to `provider`; None for shells.
    pub fn minutes(&self, kind: SessionKind, provider: Provider) -> Option<u32> {
        match (kind, provider) {
            (SessionKind::Claude, Provider::Subscription) => Some(self.claude),
            (SessionKind::Claude, Provider::OpenRouter) => Some(self.claude_openrouter),
            (SessionKind::Codex, Provider::Subscription) => Some(self.codex),
            (SessionKind::Codex, Provider::OpenRouter) => Some(self.codex_openrouter),
            (SessionKind::Shell, _) => None,
        }
    }
}

/// What the cold computation needs from the shared preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheRules {
    pub enabled: bool,
    pub ttl: CacheTtl,
}

impl CacheRules {
    pub fn from_prefs(p: &crate::model::UiPrefs) -> Self {
        Self { enabled: p.cache_warn, ttl: p.cache_ttl }
    }

    /// When `info`'s cache is likely gone: its last model request plus its provider's lifetime.
    /// None for shells, while warnings are off, and before its conversation's first turn.
    pub fn cold_at(&self, info: &SessionInfo) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        let minutes = self.ttl.minutes(info.kind, info.provider)?;
        info.last_turn_at.map(|t| t.saturating_add(i64::from(minutes) * 60_000))
    }

    /// Set `info.cache_cold_at` from its `last_turn_at`. True when it changed.
    pub fn refresh(&self, info: &mut SessionInfo) -> bool {
        let next = self.cold_at(info);
        let changed = info.cache_cold_at != next;
        info.cache_cold_at = next;
        changed
    }
}

/// The session's next message would re-read its conversation uncached: past its `cache_cold_at`
/// and waiting (not working, starting or exited).
pub fn is_cold(info: &SessionInfo, now: i64) -> bool {
    waiting(info.state) && info.cache_cold_at.is_some_and(|t| now >= t)
}

/// States in which the agent is not talking to its model and the cache can run out.
fn waiting(state: AgentState) -> bool {
    !matches!(state, AgentState::Working | AgentState::Starting | AgentState::Exited)
}

/// A state change `from` -> `to` at `now`: entering or leaving `Working` means the agent just
/// talked to its model. Shells have no model. True when `last_turn_at` changed.
pub fn note_state(info: &mut SessionInfo, from: AgentState, to: AgentState, now: i64) -> bool {
    if info.kind == SessionKind::Shell || from == to || (from != AgentState::Working && to != AgentState::Working) {
        return false;
    }
    info.last_turn_at = Some(now);
    true
}

/// A hook at `now`. A turn's end (Claude `Stop`/`StopFailure`, Codex `notify` turn complete)
/// marks a model request even when the screen already said Idle; Claude's `SessionStart` from
/// `/clear` starts an empty conversation, with nothing cached to lose. True when it changed.
pub fn note_hook(info: &mut SessionInfo, event: &str, payload: &serde_json::Value, now: i64) -> bool {
    if info.kind == SessionKind::Shell {
        return false;
    }
    let before = info.last_turn_at;
    match event {
        "Stop" | "StopFailure" => info.last_turn_at = Some(now),
        "notify" if payload.get("type").and_then(|v| v.as_str()) == Some("agent-turn-complete") => {
            info.last_turn_at = Some(now)
        }
        "SessionStart" if payload.get("source").and_then(|v| v.as_str()) == Some("clear") => info.last_turn_at = None,
        _ => {}
    }
    info.last_turn_at != before
}

/// A new run of a `kind` session started with `args` carries its conversation on (Claude's
/// `--resume`/`--continue`), so the cache clock carries on too. Any other start is a new,
/// empty conversation.
pub fn continues_conversation(kind: SessionKind, args: &[String]) -> bool {
    kind == SessionKind::Claude
        && args.iter().any(|a| {
            matches!(a.as_str(), "--resume" | "-r" | "--continue" | "-c") || a.starts_with("--resume=")
        })
}

/// "1 h 20 m", "45 m", "2 h": an idle time for people.
pub fn idle_text(ms: i64) -> String {
    let min = (ms.max(0) / 60_000) as u64;
    match (min / 60, min % 60) {
        (0, m) => format!("{m} m"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sess(kind: SessionKind, provider: Provider) -> SessionInfo {
        let mut s: SessionInfo = serde_json::from_value(json!({
            "id": 1, "channel_id": 1, "kind": kind, "name": "a", "cwd": "/", "state": "idle",
            "state_detail": null, "state_since": 0, "title": null, "parent_id": null, "pid": null,
            "exit_code": null, "cols": 80, "rows": 24, "queued": 0, "created_at": 0,
        }))
        .unwrap();
        s.provider = provider;
        s
    }

    const MIN: i64 = 60_000;

    #[test]
    fn ttl_per_provider() {
        let rules = CacheRules { enabled: true, ttl: CacheTtl::default() };
        for (kind, provider, minutes) in [
            (SessionKind::Claude, Provider::Subscription, 60),
            (SessionKind::Claude, Provider::OpenRouter, 5),
            (SessionKind::Codex, Provider::Subscription, 30),
            (SessionKind::Codex, Provider::OpenRouter, 10),
        ] {
            let mut s = sess(kind, provider);
            s.last_turn_at = Some(1_000);
            assert!(rules.refresh(&mut s));
            assert_eq!(s.cache_cold_at, Some(1_000 + minutes * MIN), "{kind:?} {provider:?}");
            assert!(!is_cold(&s, 1_000 + minutes * MIN - 1));
            assert!(is_cold(&s, 1_000 + minutes * MIN));
        }
    }

    #[test]
    fn shells_and_fresh_conversations_never_go_cold() {
        let rules = CacheRules { enabled: true, ttl: CacheTtl::default() };
        let mut shell = sess(SessionKind::Shell, Provider::Subscription);
        assert!(!note_state(&mut shell, AgentState::Working, AgentState::Idle, 5));
        assert!(!note_hook(&mut shell, "Stop", &json!({}), 5));
        shell.last_turn_at = Some(0);
        rules.refresh(&mut shell);
        assert_eq!(shell.cache_cold_at, None);
        assert!(!is_cold(&shell, i64::MAX));
        // Never prompted: nothing cached to lose.
        let mut fresh = sess(SessionKind::Claude, Provider::Subscription);
        rules.refresh(&mut fresh);
        assert!(!is_cold(&fresh, i64::MAX));
    }

    #[test]
    fn turn_end_resets_the_clock() {
        let rules = CacheRules { enabled: true, ttl: CacheTtl::default() };
        let mut s = sess(SessionKind::Claude, Provider::Subscription);
        assert!(note_state(&mut s, AgentState::Idle, AgentState::Working, 0));
        assert!(note_state(&mut s, AgentState::Working, AgentState::Idle, 10 * MIN));
        rules.refresh(&mut s);
        assert!(is_cold(&s, 70 * MIN));
        // Idle <-> approval doesn't touch the model.
        assert!(!note_state(&mut s, AgentState::Idle, AgentState::NeedsApproval, 80 * MIN));
        assert_eq!(s.last_turn_at, Some(10 * MIN));
        // A Stop hook while the screen already said Idle still ends a turn.
        assert!(note_hook(&mut s, "Stop", &json!({}), 90 * MIN));
        rules.refresh(&mut s);
        assert!(!is_cold(&s, 100 * MIN));
        assert!(is_cold(&s, 150 * MIN));
        // Working never counts as cold, even long past the time.
        s.state = AgentState::Working;
        assert!(!is_cold(&s, 1_000 * MIN));
        // Codex's turn-complete notify; title generation's other notify kinds don't count.
        let mut c = sess(SessionKind::Codex, Provider::Subscription);
        assert!(!note_hook(&mut c, "notify", &json!({"type": "other"}), 1));
        assert!(note_hook(&mut c, "notify", &json!({"type": "agent-turn-complete"}), 2));
        assert_eq!(c.last_turn_at, Some(2));
        // /clear empties the conversation.
        assert!(note_hook(&mut s, "SessionStart", &json!({"source": "clear"}), 200 * MIN));
        rules.refresh(&mut s);
        assert_eq!(s.cache_cold_at, None);
        assert!(!note_hook(&mut s, "SessionStart", &json!({"source": "resume"}), 201 * MIN));
    }

    #[test]
    fn switched_off_and_changed() {
        let mut ttl = CacheTtl::default();
        ttl.apply(CacheTtlChange { codex: Some(12 * 60), ..Default::default() }).unwrap();
        assert_eq!(ttl.codex, 720);
        assert_eq!(ttl.claude, 60);
        assert!(ttl.apply(CacheTtlChange { claude: Some(0), codex: Some(5), ..Default::default() }).is_err());
        assert_eq!(ttl.codex, 720, "a refused change changes nothing");
        assert!(ttl.apply(CacheTtlChange { claude: Some(TTL_MAX_MIN + 1), ..Default::default() }).is_err());
        let mut s = sess(SessionKind::Codex, Provider::Subscription);
        s.last_turn_at = Some(0);
        let on = CacheRules { enabled: true, ttl };
        on.refresh(&mut s);
        assert_eq!(s.cache_cold_at, Some(720 * MIN));
        let off = CacheRules { enabled: false, ttl };
        assert!(off.refresh(&mut s));
        assert!(!is_cold(&s, i64::MAX));
    }

    #[test]
    fn resumed_runs_keep_the_clock() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(continues_conversation(SessionKind::Claude, &a(&["--model", "opus", "--resume", "x"])));
        assert!(continues_conversation(SessionKind::Claude, &a(&["-c"])));
        assert!(continues_conversation(SessionKind::Claude, &a(&["--resume=abc"])));
        assert!(!continues_conversation(SessionKind::Claude, &a(&["--model", "opus"])));
        // Codex's -c is a config override, and Codex runs never resume in Muster.
        assert!(!continues_conversation(SessionKind::Codex, &a(&["-c", "model=x"])));
    }

    #[test]
    fn idle_texts() {
        assert_eq!(idle_text(45 * MIN + 30_000), "45 m");
        assert_eq!(idle_text(120 * MIN), "2 h");
        assert_eq!(idle_text(80 * MIN), "1 h 20 m");
    }
}

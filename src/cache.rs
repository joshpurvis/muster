//! Cold prompt cache: when an agent has sat idle longer than its provider keeps the
//! conversation cached, its next message re-reads the whole conversation at full price.
//!
//! The daemon tracks, per agent session, when it last talked to its model
//! (`SessionInfo.last_turn_at`: the last time it was seen entering or leaving `Working`, or a
//! turn-end hook; estimated at startup for sessions saved before it existed), and publishes `cache_cold_at` = that + the provider's cache lifetime from the
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

    /// Minutes for a session of `kind` billed to `provider`, whose launch says `basis`; None
    /// for shells and for a Claude launched with prompt caching off.
    pub fn minutes(&self, kind: SessionKind, provider: Provider, basis: CacheBasis) -> Option<u32> {
        match (kind, provider) {
            (SessionKind::Claude, _) if basis.env.is_some() => basis.env.flatten(),
            (SessionKind::Claude, Provider::OpenRouter) => Some(self.claude_openrouter),
            // Claude Code on an API key (or a cloud provider) gets the API's 5-minute default.
            (SessionKind::Claude, Provider::Subscription) if basis.api_auth => Some(self.claude_openrouter),
            (SessionKind::Claude, Provider::Subscription) => Some(self.claude),
            (SessionKind::Codex, Provider::Subscription) => Some(self.codex),
            (SessionKind::Codex, Provider::OpenRouter) => Some(self.codex_openrouter),
            (SessionKind::Shell, _) => None,
        }
    }
}

/// What a run's own launch environment says about its prompt cache (Claude Code only; read at
/// each start from the launch, and again from the saved launch after a backend restart).
/// Runtime only. Credentials are only tested for being set, never read or kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheBasis {
    /// Claude Code authenticates with an API key or a cloud provider, not the subscription.
    pub api_auth: bool,
    /// A lifetime the launch sets explicitly: `Some(Some(minutes))`, or `Some(None)` with
    /// prompt caching off.
    pub env: Option<Option<u32>>,
}

/// Variables whose presence means Claude Code bills an API key or a cloud account
/// (code.claude.com/docs/en/prompt-caching "Which TTL each request gets").
const API_AUTH_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// `get(name)`: the launch environment's value of `name`. Only the cache switches' values are
/// looked at; credentials only for being non-empty.
pub fn basis_from_env<'a>(kind: SessionKind, get: impl Fn(&str) -> Option<&'a str>) -> CacheBasis {
    if kind != SessionKind::Claude {
        return CacheBasis::default();
    }
    let on = |name: &str| get(name).is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"));
    let present = |name: &str| get(name).is_some_and(|v| !v.is_empty());
    let ttl = get("CLAUDE_CODE_PROMPT_CACHE_TTL").and_then(|v| match v.trim() {
        "5m" => Some(5),
        "1h" => Some(60),
        _ => None,
    });
    // The order Claude Code applies them in: off, then forced 5 min, then the setting, then 1 h.
    let env = if on("DISABLE_PROMPT_CACHING") {
        Some(None)
    } else if on("FORCE_PROMPT_CACHING_5M") {
        Some(Some(5))
    } else if let Some(m) = ttl {
        Some(Some(m))
    } else if on("ENABLE_PROMPT_CACHING_1H") {
        Some(Some(60))
    } else {
        None
    };
    CacheBasis { api_auth: API_AUTH_VARS.iter().any(|v| present(v)), env }
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
        let minutes = self.ttl.minutes(info.kind, info.provider, info.cache_basis)?;
        info.last_turn_at.map(|t| t.saturating_add(i64::from(minutes) * 60_000))
    }

    /// Set `info.cache_cold_at` (and `cache_ttl_min`) from its `last_turn_at`. True when
    /// either changed.
    pub fn refresh(&self, info: &mut SessionInfo) -> bool {
        let next = self.cold_at(info);
        let ttl = if self.enabled { self.ttl.minutes(info.kind, info.provider, info.cache_basis) } else { None };
        let changed = info.cache_cold_at != next || info.cache_ttl_min != ttl;
        info.cache_cold_at = next;
        info.cache_ttl_min = ttl;
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

/// A screen-detected state change `from` -> `to` at `now`, for a session whose CLI sends no
/// hooks: entering or leaving `Working` is the only sign it talked to its model. The hub calls it
/// only for genuine screen transitions (never timers, recovery or process exits), and not once
/// hooks report (they say exactly when a request succeeded). True when `last_turn_at` changed.
pub fn note_screen(info: &mut SessionInfo, from: AgentState, to: AgentState, now: i64) -> bool {
    if info.kind == SessionKind::Shell || from == to || (from != AgentState::Working && to != AgentState::Working) {
        return false;
    }
    advance(info, now)
}

/// A hook captured at `at`: the model just answered (Claude or Codex `PreToolUse` or
/// `PermissionRequest`, a turn's `Stop`, Codex's turn-complete `notify`), which refreshed the
/// cache. A failed turn (`StopFailure`: a rate limit, an auth error) may never have reached it,
/// so it leaves the clock alone, as do a subagent's hooks (its own cache) and Codex's
/// title-generation turns. Claude's `SessionStart` from `/clear` starts an empty conversation,
/// with nothing cached to lose. True when it changed.
pub fn note_hook(info: &mut SessionInfo, event: &str, payload: &serde_json::Value, at: i64) -> bool {
    if info.kind == SessionKind::Shell {
        return false;
    }
    let subagent = payload.get("agent_id").and_then(|v| v.as_str()).is_some_and(|a| !a.is_empty());
    match event {
        "PreToolUse" | "PermissionRequest" | "Stop" if !subagent => advance(info, at),
        "notify"
            if payload.get("type").and_then(|v| v.as_str()) == Some("agent-turn-complete")
                && !crate::adapters::codex::is_title_generation(payload) =>
        {
            advance(info, at)
        }
        "SessionStart" if payload.get("source").and_then(|v| v.as_str()) == Some("clear") => {
            info.last_turn_at.take().is_some()
        }
        _ => false,
    }
}

/// Move the clock to `at`, never back (a callback replayed late, after a newer one).
fn advance(info: &mut SessionInfo, at: i64) -> bool {
    let next = info.last_turn_at.map_or(at, |t| t.max(at));
    let changed = info.last_turn_at != Some(next);
    info.last_turn_at = Some(next);
    changed
}

/// A session saved before the clock existed (store migration v20) has no `last_turn_at`, so it
/// would never warn. Estimate it as when the session came to rest (`state_since`): a resting
/// agent's last request ended about then. Only for an agent resting now (not working, starting
/// or exited) that has had a turn, which the task title its CLI sets shows (Claude Code and Codex
/// name the conversation after its first prompt; a fresh session at its first prompt has none).
/// Codex's default title is the project folder's name even before the first prompt, so a title
/// that only names the session's folder doesn't count.
/// Read from the saved row at startup, before recovery rewrites a dialog's `state_since`.
pub fn estimated_turn(info: &SessionInfo) -> Option<i64> {
    let folder = |dir: &str| std::path::Path::new(dir).file_name().map(|n| n.to_string_lossy().to_lowercase());
    let names_folder = |t: &str| {
        let t = t.trim().to_lowercase();
        [Some(info.cwd.as_str()), info.live_cwd.as_deref()].into_iter().flatten().any(|d| folder(d).as_deref() == Some(t.as_str()))
    };
    let had_turn = info.title.as_deref().and_then(|t| crate::hub::clean_title(t, info.kind))
        .is_some_and(|t| !names_folder(&t));
    (info.last_turn_at.is_none() && info.kind != SessionKind::Shell && waiting(info.state) && had_turn
        && info.state_since > 0)
        .then_some(info.state_since)
}

/// A new run of a `kind` session started with `args` carries its conversation on (Claude's
/// `--resume`/`--continue`, in every spelling the argument check accepts; Codex's Muster-only
/// `--resume`, launched as `codex resume`), so the cache clock carries on too. Any other start
/// is a new, empty conversation.
pub fn continues_conversation(kind: SessionKind, args: &[String]) -> bool {
    kind != SessionKind::Shell
        && crate::adapters::parse_extra_args(kind, args)
            .is_ok_and(|flags| flags.iter().any(|(long, _)| matches!(*long, "--resume" | "--continue")))
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
        assert!(!note_screen(&mut shell, AgentState::Working, AgentState::Idle, 5));
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
        // A hookless CLI: the screen's Working in and out.
        assert!(note_screen(&mut s, AgentState::Idle, AgentState::Working, 0));
        assert!(note_screen(&mut s, AgentState::Working, AgentState::Idle, 10 * MIN));
        rules.refresh(&mut s);
        assert!(is_cold(&s, 70 * MIN));
        // Idle <-> approval doesn't touch the model.
        assert!(!note_screen(&mut s, AgentState::Idle, AgentState::NeedsApproval, 80 * MIN));
        assert_eq!(s.last_turn_at, Some(10 * MIN));
        // A Stop hook while the screen already said Idle still ends a turn.
        assert!(note_hook(&mut s, "Stop", &json!({}), 90 * MIN));
        rules.refresh(&mut s);
        assert!(!is_cold(&s, 100 * MIN));
        assert!(is_cold(&s, 150 * MIN));
        // The model asking for a tool is a request too.
        assert!(note_hook(&mut s, "PreToolUse", &json!({"tool_name": "Bash"}), 95 * MIN));
        assert!(note_hook(&mut s, "PermissionRequest", &json!({"tool_name": "Bash"}), 96 * MIN));
        // Never back: a callback replayed late after a newer one.
        assert!(!note_hook(&mut s, "Stop", &json!({}), 91 * MIN));
        assert_eq!(s.last_turn_at, Some(96 * MIN));
        // Working never counts as cold, even long past the time.
        s.state = AgentState::Working;
        assert!(!is_cold(&s, 1_000 * MIN));
        s.state = AgentState::Idle;
        // /clear empties the conversation.
        assert!(note_hook(&mut s, "SessionStart", &json!({"source": "clear"}), 200 * MIN));
        rules.refresh(&mut s);
        assert_eq!(s.cache_cold_at, None);
        assert!(!note_hook(&mut s, "SessionStart", &json!({"source": "resume"}), 201 * MIN));
    }

    #[test]
    fn only_requests_that_reached_the_model_count() {
        let mut s = sess(SessionKind::Claude, Provider::Subscription);
        s.last_turn_at = Some(MIN);
        // A failed turn (rate limit, auth) may never have reached the model.
        assert!(!note_hook(&mut s, "StopFailure", &json!({"error_type": "rate_limit"}), 5 * MIN));
        // A turn starting, a tool finishing, an idle reminder: no request answered yet.
        for ev in ["UserPromptSubmit", "PostToolUse", "Notification", "PreCompact"] {
            assert!(!note_hook(&mut s, ev, &json!({"prompt": "hi"}), 6 * MIN), "{ev}");
        }
        // A subagent's request warms its own cache, not the conversation's.
        assert!(!note_hook(&mut s, "PreToolUse", &json!({"agent_id": "sub-1", "tool_name": "Read"}), 7 * MIN));
        assert!(!note_hook(&mut s, "Stop", &json!({"agent_id": "sub-1"}), 7 * MIN));
        assert_eq!(s.last_turn_at, Some(MIN));
        // Codex: a turn-complete notify counts, its title-generation turn and other kinds don't.
        let mut c = sess(SessionKind::Codex, Provider::Subscription);
        assert!(!note_hook(&mut c, "notify", &json!({"type": "other"}), 1));
        let title = json!({"type": "agent-turn-complete",
            "input-messages": ["Generate a concise, single-line task title for this conversation"]});
        assert!(!note_hook(&mut c, "notify", &title, 1));
        assert!(note_hook(&mut c, "notify", &json!({"type": "agent-turn-complete", "input-messages": ["fix it"]}), 2));
        assert_eq!(c.last_turn_at, Some(2));
    }

    #[test]
    fn the_launch_environment_decides_claudes_lifetime() {
        let ttl = CacheTtl::default();
        let basis = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            basis_from_env(SessionKind::Claude, |k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()))
        };
        let sub = |b| ttl.minutes(SessionKind::Claude, Provider::Subscription, b);
        assert_eq!(sub(basis(&[])), Some(60), "the subscription's 1 h");
        // An API key or a cloud provider bills the API: 5 minutes. Empty means unset.
        assert_eq!(sub(basis(&[("ANTHROPIC_API_KEY", "x")])), Some(5));
        assert_eq!(sub(basis(&[("CLAUDE_CODE_USE_BEDROCK", "1")])), Some(5));
        assert_eq!(sub(basis(&[("ANTHROPIC_API_KEY", "")])), Some(60));
        // Explicit switches win, in Claude Code's order.
        assert_eq!(sub(basis(&[("ANTHROPIC_API_KEY", "x"), ("ENABLE_PROMPT_CACHING_1H", "1")])), Some(60));
        assert_eq!(sub(basis(&[("CLAUDE_CODE_PROMPT_CACHE_TTL", "5m")])), Some(5));
        assert_eq!(sub(basis(&[("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h"), ("FORCE_PROMPT_CACHING_5M", "1")])), Some(5));
        assert_eq!(sub(basis(&[("ENABLE_PROMPT_CACHING_1H", "0")])), Some(60));
        // Caching off: nothing to lose, no warning.
        assert_eq!(sub(basis(&[("DISABLE_PROMPT_CACHING", "1")])), None);
        assert_eq!(
            ttl.minutes(SessionKind::Claude, Provider::OpenRouter, basis(&[("ENABLE_PROMPT_CACHING_1H", "true")])),
            Some(60)
        );
        // Codex reads none of these.
        assert_eq!(basis_from_env(SessionKind::Codex, |_| Some("1")), CacheBasis::default());
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
        assert!(continues_conversation(SessionKind::Claude, &a(&["-rabc"])));
        assert!(continues_conversation(SessionKind::Claude, &a(&["-r", "abc"])));
        assert!(continues_conversation(SessionKind::Claude, &a(&["--continue"])));
        assert!(continues_conversation(SessionKind::Claude, &a(&["--model", "opus", "-r"])));
        assert!(!continues_conversation(SessionKind::Claude, &a(&["--model", "opus"])));
        // Codex's -c is a config override; Muster's --resume reopens a thread (`codex resume`).
        assert!(!continues_conversation(SessionKind::Codex, &a(&["-c", "model=x"])));
        assert!(continues_conversation(SessionKind::Codex, &a(&["-c", "model=x", "--resume", "t-1"])));
        assert!(!continues_conversation(SessionKind::Shell, &a(&["--resume", "t-1"])));
    }

    #[test]
    fn sessions_from_before_the_clock_get_an_estimate() {
        let rules = CacheRules { enabled: true, ttl: CacheTtl::default() };
        let mut s = sess(SessionKind::Claude, Provider::Subscription);
        s.title = Some("✳ Fix the login bug".into());
        s.state_since = 10 * MIN;
        s.last_turn_at = estimated_turn(&s);
        rules.refresh(&mut s);
        assert!(is_cold(&s, 3 * 60 * MIN), "idle for hours: cold");
        assert!(!is_cold(&s, 30 * MIN));
        for state in [AgentState::NeedsApproval, AgentState::AskingQuestion, AgentState::Error, AgentState::Stuck] {
            s.state = state;
            s.last_turn_at = None;
            assert_eq!(estimated_turn(&s), Some(10 * MIN), "{state:?}");
        }
        // A clock it already has wins.
        s.last_turn_at = Some(20 * MIN);
        assert_eq!(estimated_turn(&s), None);
        s.last_turn_at = None;
        // Talking to its model, or gone: no estimate.
        for state in [AgentState::Working, AgentState::Starting, AgentState::Exited] {
            s.state = state;
            assert_eq!(estimated_turn(&s), None, "{state:?}");
        }
        s.state = AgentState::Idle;
        // A fresh session at its first prompt: no task title yet, nothing cached.
        for title in [None, Some("✳ Claude Code"), Some("codex")] {
            s.title = title.map(str::to_string);
            assert_eq!(estimated_turn(&s), None, "{title:?}");
        }
        // Codex names a fresh session after its folder before any prompt.
        let mut codex = sess(SessionKind::Codex, Provider::Subscription);
        codex.cwd = "/home/me/projects/muster".into();
        codex.state_since = 10 * MIN;
        codex.title = Some("muster".into());
        assert_eq!(estimated_turn(&codex), None, "folder-name title");
        codex.title = Some("Fix the login bug".into());
        assert_eq!(estimated_turn(&codex), Some(10 * MIN));
        // Shells never.
        let mut shell = sess(SessionKind::Shell, Provider::Subscription);
        shell.title = Some("make test".into());
        shell.state_since = 10 * MIN;
        assert_eq!(estimated_turn(&shell), None);
    }

    #[test]
    fn idle_texts() {
        assert_eq!(idle_text(45 * MIN + 30_000), "45 m");
        assert_eq!(idle_text(120 * MIN), "2 h");
        assert_eq!(idle_text(80 * MIN), "1 h 20 m");
    }
}

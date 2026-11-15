//! Self handoff: a named agent the human promoted asks Muster to start it fresh, from a handoff
//! file it wrote (docs/MCP.md "Self handoff").
//!
//! Long-running agents grow large conversations, and only the human could clear one (`/clear`,
//! then a "resume" line). The owner's decision (15 Nov 2026): agents can be promoted to do it
//! themselves; only the ones the human picks.
//!
//! - The right (`Agent::self_handoff`) is off by default. Only the human turns it on: a click in
//!   Edit agent in Muster's window, or `muster agent edit NAME --self-handoff on` from a terminal
//!   outside every Muster session (`check_grant`, `rpc::owner_terminal`). No MCP tool changes it,
//!   and a call made from inside a session (an agent's, a shell tab's) is refused. Anyone may
//!   turn it off.
//! - A promoted Claude agent asks with the MCP tool `start_fresh` (`muster start-fresh FILE` calls
//!   it with the session's own token): only for itself, with a handoff file that exists, is a
//!   regular file and lies inside its home folder or its session's working directory once
//!   symlinks are resolved (`resolve_file`). One request waits at a time per agent; one fresh
//!   start per `MIN_GAP_MS` (30 minutes, this constant).
//! - Muster waits for the turn that asked to end (idle, nothing queued, no draft of the human's in
//!   its input box, for `SETTLE`), types `/clear` (Claude Code's own new conversation: its
//!   process and background tasks keep running), waits for Claude's SessionStart hook with source
//!   `clear` (a `/clear` that never got typed in that time is taken back), then types one prompt
//!   naming the file, saying the agent asked for it and when (`first_prompt`). Both carry the
//!   asking turn's origin and hop, so a handoff asked while handling scheduled or triggered work
//!   stays that work and never becomes the human's.
//! - Every request, refusal, cancellation, failure and finished handoff is a `system` event in the
//!   agent's tab (`channel.timeline`, data `self_handoff`); a finished or failed one is a notice too.
//! - Codex agents: not supported yet (granting and asking are refused).
//!
//! Known gaps: a request waits in this backend's memory, so a backend restart (`muster reload`)
//! forgets it (the agent can ask again). A restart between `/clear` and the prompt leaves the
//! agent in its new, empty conversation without the prompt (its timeline shows the `/clear`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::agents::{Agent, Origin, Prov, Source};
use crate::hub::{Hub, ServerEvent};
use crate::model::*;

/// The MCP tool's name (`muster start-fresh` calls it).
pub const TOOL: &str = "start_fresh";
/// The rate limit: one fresh start per agent per this long (from the `/clear` Muster typed).
pub const MIN_GAP_MS: i64 = 30 * 60_000;
/// How long the agent must stay idle, with nothing queued, before `/clear` is typed.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a request waits for the agent to come to rest before it is given up.
const WAIT_IDLE_MAX: Duration = Duration::from_secs(2 * 3600);
/// How long Muster waits for Claude to say it started the new conversation after `/clear`.
const CLEAR_WAIT: Duration = Duration::from_secs(5 * 60);
/// How often the wait for idle looks again.
const POLL: Duration = Duration::from_millis(250);
/// `from` on the prompts Muster types for a handoff (the timeline's `prompt` events).
const FROM: &str = "self-handoff";
/// Claude Code's command for a new conversation in the same process.
const CLEAR: &str = "/clear";

/// `MIN_GAP_MS`; a debug build can shorten it for the smoke test.
fn min_gap_ms() -> i64 {
    if cfg!(debug_assertions)
        && let Some(v) = std::env::var("MUSTER_TEST_SELF_HANDOFF_GAP_MS").ok().and_then(|v| v.parse().ok())
    {
        return v;
    }
    MIN_GAP_MS
}

/// Requests waiting for their agent to come to rest: agent id -> when it asked (Unix ms).
#[derive(Default)]
pub struct State {
    pending: Mutex<HashMap<i64, i64>>,
}

/// Removes an agent's waiting request when its handoff ends, however it ends.
struct PendingGuard {
    hub: Arc<Hub>,
    agent: i64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.hub.agents().self_handoff.pending.lock().remove(&self.agent);
    }
}

/// How the human grants the right, for refusals.
pub fn grant_how(name: &str) -> String {
    format!(
        "the human turns it on in Edit agent (More options, \"Can start itself fresh\") in Muster's window, or with \
         `muster agent edit {name} --self-handoff on` in a terminal outside Muster"
    )
}

/// May this `agent.update` turn the right on for `a`? `by_owner`: the call is the human's own
/// (a click in Muster's window, or the CLI from a terminal outside every session).
pub fn check_grant(a: &Agent, by_owner: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        a.kind == SessionKind::Claude,
        "self handoff isn't supported for Codex agents yet: only a Claude agent can be given it"
    );
    anyhow::ensure!(
        by_owner,
        "only the human can let {} start itself fresh: {}. This call came from inside a Muster session (an agent, or a \
         shell tab), from a script, or from a program Muster can't identify, so it may be an agent's",
        a.name,
        grant_how(&a.name)
    );
    Ok(())
}

/// The section a promoted agent's CLAUDE.md gets (`agents::persona_text_for`).
pub fn persona_section() -> String {
    format!(
        "\n## Starting fresh\n\n\
         The human lets you start a new conversation yourself. Use it between tasks when this conversation has \
         grown long (for example past about 200k tokens), never in the middle of a task:\n\n\
         1. Write everything you need to carry on into a handoff file in this folder (for example \
         `memory/handoff.md`): open threads, decisions and their reasons, what to do next, and anything a \
         message you received still waits for.\n\
         2. Call the muster MCP tool `{TOOL}` with its path (or run `muster start-fresh <path>`), then end your turn \
         without starting anything else.\n\n\
         Once you are idle, Muster clears your conversation (`/clear`: your process and background tasks keep \
         running) and sends you one prompt that names the file and says you asked for it. At most once every {} \
         minutes; the file must be in this folder or your working directory.\n",
        MIN_GAP_MS / 60_000
    )
}

/// The handoff file named by `given` (relative paths are read from `cwd`), when it exists, is a
/// regular file and lies inside `home` or `cwd`, symlinks resolved. Its real path.
pub fn resolve_file(given: &str, home: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let given = given.trim();
    if given.is_empty() {
        return Err("give the path of your handoff file (file)".into());
    }
    let path = Path::new(given);
    let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    let real = std::fs::canonicalize(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{} doesn't exist: write your handoff file first", path.display()),
        _ => format!("can't read {}: {e}", path.display()),
    })?;
    let meta = std::fs::metadata(&real).map_err(|e| format!("can't read {}: {e}", real.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let inside = [home, cwd].iter().filter_map(|d| std::fs::canonicalize(d).ok()).any(|d| real.starts_with(d));
    if !inside {
        return Err(format!(
            "{} is outside your home folder ({}) and your working directory ({}): put the handoff file in one of them",
            real.display(),
            home.display(),
            cwd.display()
        ));
    }
    Ok(real)
}

/// May `agent`, running in `session`, ask for a fresh start from `file` now? Its handoff file's
/// real path, or why not. `pending`: it already asked and that one hasn't happened yet.
pub fn check_request(agent: Option<&Agent>, session: &SessionInfo, file: &str, now: i64, pending: bool)
    -> Result<PathBuf, String> {
    let Some(a) = agent else {
        return Err("only named agents (the ones in Muster's Agents panel) can start themselves fresh, and only when \
            the human allows it: this session isn't one. Ask the human to /clear it instead"
            .into());
    };
    if !a.self_handoff {
        return Err(format!(
            "{} isn't allowed to start itself fresh: {}. Until then, ask the human to clear your conversation, or carry on",
            a.name,
            grant_how(&a.name)
        ));
    }
    if a.kind != SessionKind::Claude {
        return Err("self handoff isn't supported for Codex agents yet".into());
    }
    if session.state == AgentState::Exited {
        return Err("your session isn't running".into());
    }
    if pending {
        return Err("you already asked for a fresh start: it happens once this turn ends and you are idle. End your turn".into());
    }
    if let Some(at) = a.self_handoff_at
        && now - at < min_gap_ms()
    {
        return Err(format!(
            "you started fresh at {}; the next fresh start is allowed from {} (at most one every {} minutes). Carry on \
             in this conversation until then",
            crate::agents::local_time(at),
            crate::agents::local_time(at + min_gap_ms()),
            min_gap_ms() / 60_000
        ));
    }
    resolve_file(file, Path::new(&a.home_dir), Path::new(&session.cwd))
}

/// The one prompt the new conversation gets: the agent asked for it itself, when, on what
/// work, and where its handoff file is.
pub fn first_prompt(name: &str, file: &Path, asked_at: i64, prov: Option<Prov>) -> String {
    let (origin, hop) = prov.map_or((Origin::Human, 0), |p| (p.origin, p.hop));
    let mut t = format!(
        "[muster self-handoff origin={} hop={hop}] You ({name}) asked Muster for this new conversation yourself at {}, \
         with the tool {TOOL}; the human did not type this. Your previous conversation was cleared. Read your handoff \
         file {} and your memory/ notes, then carry on from there.",
        origin.as_str(),
        crate::agents::local_time(asked_at),
        file.display()
    );
    if origin != Origin::Human {
        let work = if origin == Origin::Webhook { "triggered" } else { "scheduled" };
        t.push_str(&format!(
            " You were handling {work} work when you asked, so this conversation continues that work: it is not a new \
             request from the human."
        ));
    }
    t
}

/// Write one step to the agent's tab timeline (`system` event) and the log.
fn record(hub: &Hub, channel: Option<ChannelId>, session: Option<SessionId>, level: &str, stage: &str, text: &str, data: Value) {
    let mut d = json!({ "stage": stage });
    if let (Some(o), Value::Object(extra)) = (d.as_object_mut(), data) {
        o.extend(extra);
    }
    tracing::info!(session, stage, "self handoff: {text}");
    hub.log_event(channel, session, EventKind::System, json!({ "text": text, "level": level, "self_handoff": d }));
}

/// `start_fresh` from session `caller`: check it, and on success start the handoff in the
/// background (it waits for the turn that asked to end). What the tool answers.
pub fn request(hub: &Arc<Hub>, caller: SessionId, file: &str) -> anyhow::Result<Value> {
    let session = hub.session(caller).ok_or_else(|| anyhow::anyhow!("calling session no longer exists"))?;
    let agent = hub.agents().by_session(caller);
    let now = now_ms();
    let checked = {
        let mut pending = hub.agents().self_handoff.pending.lock();
        let waiting = agent.as_ref().is_some_and(|a| pending.contains_key(&a.id));
        let r = check_request(agent.as_ref(), &session, file, now, waiting);
        if let (Ok(_), Some(a)) = (&r, &agent) {
            pending.insert(a.id, now);
        }
        r
    };
    let name = agent.as_ref().map_or_else(|| format!("session:{caller}"), |a| a.name.clone());
    let path = match checked {
        Ok(p) => p,
        Err(why) => {
            // Kept within a budget, so a looping caller can't flood the timeline.
            if hub.agents().refusal_budget(&format!("self-handoff:{caller}"), now).is_some() {
                record(hub, Some(session.channel_id), Some(caller), "warn", "refused",
                    &format!("{name} asked to start fresh; refused: {why}"),
                    json!({ "agent": name, "file": file.chars().take(500).collect::<String>(), "why": why }));
            }
            anyhow::bail!("{why}");
        }
    };
    let a = agent.expect("checked: a named agent");
    let prov = hub.agents().prov(caller);
    let (origin, hop) = prov.map_or((Origin::Human, 0), |p| (p.origin, p.hop));
    record(hub, Some(session.channel_id), Some(caller), "info", "requested",
        &format!("{} asked to start fresh from {} (while handling {} work); waiting for its turn to end",
            a.name, path.display(), origin.as_str()),
        json!({ "agent": a.name, "file": path, "origin": origin, "hop": hop, "asked_at": now }));
    let ask = Ask {
        agent: a.id,
        name: a.name.clone(),
        session: caller,
        channel: session.channel_id,
        generation: hub.pty_generation(caller),
        file: path.clone(),
        source: Source::Chain(Prov { origin, hop, msg_id: None }),
        prov,
        asked_at: now,
    };
    let guard = PendingGuard { hub: hub.clone(), agent: a.id };
    let hub2 = hub.clone();
    tokio::spawn(async move {
        let outcome = drive(&hub2, &ask).await;
        // Done waiting before it is recorded: whoever reads the record may ask again.
        drop(guard);
        finish(&hub2, &ask, outcome);
    });
    Ok(json!({
        "status": "waiting",
        "file": path,
        "note": format!(
            "Muster will clear your conversation once this turn ends and you are idle, then send you one prompt \
             pointing at {}. End your turn now and don't start anything else.",
            path.display()
        ),
    }))
}

/// A request that passed its checks.
struct Ask {
    agent: i64,
    name: String,
    session: SessionId,
    channel: ChannelId,
    /// The run that asked: a restart since cancels it.
    generation: Option<u64>,
    file: PathBuf,
    /// What the prompts carry: the asking turn's origin and hop.
    source: Source,
    prov: Option<Prov>,
    asked_at: i64,
}

/// Why a handoff stopped before its prompt was typed.
#[derive(Debug, PartialEq)]
enum Stop {
    /// Nothing was typed (the agent's conversation is as it was).
    Cancelled(String),
    /// Something went wrong on the way.
    Failed(String),
}

/// What the wait for idle sees now.
#[derive(Debug, PartialEq)]
enum Rest {
    Busy,
    Idle,
    Cancel(String),
}

/// What else the wait for idle looks at.
#[derive(Debug, Clone, Copy)]
struct Now {
    /// Its process runs in the run it asked from.
    live: bool,
    /// No prompt waits, is being typed, or is paused for review (`inject::queue_idle`).
    queue_idle: bool,
    /// Text sits in its input box: the human is writing to it (`inject::has_draft`).
    draft: bool,
}

/// Has the agent come to rest in the run that asked (idle, nothing queued, no draft of the
/// human's in its input box), or is the request moot?
fn rest(agent: Option<&Agent>, session: Option<&SessionInfo>, sid: SessionId, now: Now) -> Rest {
    let Some(a) = agent else { return Rest::Cancel("the agent was removed".into()) };
    if !a.self_handoff {
        return Rest::Cancel("the human took back its right to start itself fresh".into());
    }
    if a.session_id != Some(sid) {
        return Rest::Cancel("it moved to another session".into());
    }
    if a.asleep_at.is_some() {
        return Rest::Cancel("it fell asleep".into());
    }
    match session {
        Some(s) if now.live && s.state != AgentState::Exited => {
            if crate::agents::session_idle(s) && now.queue_idle && !now.draft { Rest::Idle } else { Rest::Busy }
        }
        _ => Rest::Cancel("its run ended or restarted".into()),
    }
}

fn rest_now(hub: &Hub, ask: &Ask) -> Rest {
    if hub.is_shutting_down() {
        return Rest::Cancel("Muster's backend is stopping".into());
    }
    let now = Now {
        live: hub.live_session_ids().contains(&ask.session) && hub.pty_generation(ask.session) == ask.generation,
        queue_idle: crate::inject::queue_idle(hub, ask.session),
        draft: crate::inject::has_draft(hub, ask.session),
    };
    rest(hub.agents().by_id(ask.agent).as_ref(), hub.session(ask.session).as_ref(), ask.session, now)
}

/// Is `e` Claude's SessionStart hook for `sid` saying a `/clear` started a new conversation?
fn cleared_event(e: &Event, sid: SessionId) -> bool {
    e.session_id == Some(sid)
        && e.kind == EventKind::Hook
        && e.data["event"] == "SessionStart"
        && e.data["summary"]["source"] == "clear"
}

async fn drive(hub: &Arc<Hub>, ask: &Ask) -> Result<(), Stop> {
    // 1. The turn that asked ends: idle, nothing queued, for SETTLE.
    let deadline = tokio::time::Instant::now() + WAIT_IDLE_MAX;
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        let now = tokio::time::Instant::now();
        match rest_now(hub, ask) {
            Rest::Cancel(why) => return Err(Stop::Cancelled(why)),
            Rest::Busy => idle_since = None,
            Rest::Idle => {
                if now.duration_since(*idle_since.get_or_insert(now)) >= SETTLE {
                    break;
                }
            }
        }
        if now >= deadline {
            return Err(Stop::Cancelled(format!("it didn't come to rest within {} hours", WAIT_IDLE_MAX.as_secs() / 3600)));
        }
        tokio::time::sleep(POLL).await;
    }
    // 2. Checked again now: the file, and the spend caps (an unattended turn on OpenRouter).
    let home = hub.agents().by_id(ask.agent).map(|a| a.home_dir).unwrap_or_default();
    let cwd = hub.session(ask.session).map(|s| s.cwd).unwrap_or_default();
    if let Err(why) = resolve_file(&ask.file.to_string_lossy(), Path::new(&home), Path::new(&cwd)) {
        return Err(Stop::Cancelled(format!("its handoff file is no longer usable: {why}")));
    }
    if let Some(why) = crate::spend::submit_gate(hub, ask.session, &ask.source) {
        return Err(Stop::Cancelled(why));
    }
    // 3. /clear, counted for the rate limit before it is typed (a crash after it still counts).
    let mut rx = hub.subscribe();
    let typed_at = now_ms();
    hub.agents().set_self_handoff_at(hub.store(), ask.agent, typed_at)
        .map_err(|e| Stop::Failed(format!("recording the fresh start: {e:#}")))?;
    if let Some(a) = hub.agents().by_id(ask.agent) {
        crate::agents::publish(hub, &a);
    }
    crate::inject::send(hub, ask.session, CLEAR.into(), crate::inject::SendMode::Queue, FROM.into(), ask.source)
        .await
        .map_err(|e| Stop::Failed(format!("typing /clear: {e:#}")))?;
    // 4. Claude starts the new conversation (its SessionStart hook, source `clear`).
    let deadline = tokio::time::Instant::now() + CLEAR_WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            // Still waiting in the queue (behind a draft, a dialog): never typed later, out of turn.
            if crate::inject::withdraw(hub, ask.session, CLEAR).unwrap_or(false) {
                return Err(Stop::Cancelled(format!(
                    "/clear couldn't be typed within {} minutes (something was in the way, such as text in its input \
                     box), so it was taken back unsent",
                    CLEAR_WAIT.as_secs() / 60
                )));
            }
            return Err(Stop::Failed(format!(
                "Claude didn't start a new conversation within {} minutes of /clear (no SessionStart hook with source \
                 clear): its conversation may be unchanged",
                CLEAR_WAIT.as_secs() / 60
            )));
        }
        match tokio::time::timeout(left.min(Duration::from_secs(1)), rx.recv()).await {
            Ok(Ok(ServerEvent::Event(e))) if cleared_event(&e, ask.session) => break,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                // Fell behind: the event log has it if it happened.
                let seen = hub.timeline(ask.channel, None, 200).iter()
                    .any(|e| e.ts >= typed_at && cleared_event(e, ask.session));
                if seen {
                    break;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(Stop::Failed("Muster's backend is stopping".into()));
            }
            _ => {}
        }
        let live = hub.live_session_ids().contains(&ask.session) && hub.pty_generation(ask.session) == ask.generation;
        if !live {
            return Err(Stop::Failed("its run ended or restarted right after /clear".into()));
        }
    }
    // 5. The one prompt of the new conversation.
    let prompt = first_prompt(&ask.name, &ask.file, ask.asked_at, ask.prov);
    crate::inject::send(hub, ask.session, prompt, crate::inject::SendMode::Queue, FROM.into(), ask.source)
        .await
        .map_err(|e| Stop::Failed(format!(
            "its conversation was cleared, but the prompt naming {} couldn't be typed ({e:#}): tell it to read that file",
            ask.file.display()
        )))?;
    Ok(())
}

/// Record how a handoff ended, in the tab's timeline and as a notice.
fn finish(hub: &Hub, ask: &Ask, outcome: Result<(), Stop>) {
    let data = json!({ "agent": ask.name, "file": ask.file, "asked_at": ask.asked_at });
    let (level, stage, text) = match outcome {
        Ok(()) => ("info", "done", format!(
            "{} started fresh: its conversation was cleared and it was pointed at {}", ask.name, ask.file.display())),
        Err(Stop::Cancelled(why)) => ("info", "cancelled", format!("{}'s fresh start didn't happen: {why}", ask.name)),
        Err(Stop::Failed(why)) => ("warn", "failed", format!("{}'s fresh start failed: {why}", ask.name)),
    };
    record(hub, Some(ask.channel), Some(ask.session), level, stage, &text, data);
    hub.notice(level, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(home: &Path) -> Agent {
        Agent {
            account_profile_id: None,
            provider: crate::openrouter::Provider::Subscription,
            id: 1,
            name: "otto".into(),
            kind: SessionKind::Claude,
            model: None,
            effort: None,
            persona: String::new(),
            home_dir: home.to_string_lossy().into_owned(),
            working_location: Default::default(),
            reset_on_next_start: false,
            handoff: None,
            permissions: Permissions::default(),
            accepts_origins: vec![Origin::Human],
            channel_id: Some(1),
            session_id: Some(2),
            resume_id: Some("conv-1".into()),
            created_at: 0,
            autostart: false,
            sleep_after_min: None,
            asleep_at: None,
            self_handoff: true,
            self_handoff_at: None,
        }
    }

    fn session(cwd: &Path, state: AgentState) -> SessionInfo {
        let mut s: SessionInfo = serde_json::from_value(json!({
            "id": 2, "channel_id": 1, "kind": "claude", "name": "otto", "cwd": cwd, "state": "idle",
            "state_detail": null, "state_since": 0, "title": null, "parent_id": null, "pid": 7,
            "exit_code": null, "cols": 80, "rows": 24, "queued": 0, "created_at": 0,
        }))
        .unwrap();
        s.state = state;
        s
    }

    /// A home folder with memory/handoff.md, and a separate working directory.
    fn dirs() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("muster-self-handoff-{}", crate::paths::random_token()));
        let (home, work) = (root.join("home"), root.join("repo"));
        std::fs::create_dir_all(home.join("memory")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(home.join("memory/handoff.md"), "next: ship it\n").unwrap();
        (root, home, work)
    }

    #[test]
    fn only_a_promoted_claude_agent_asks_and_only_for_a_file_of_its_own() {
        let (root, home, work) = dirs();
        let a = agent(&home);
        let s = session(&home, AgentState::Working);
        let ok = |a: &Agent, s: &SessionInfo, f: &str| check_request(Some(a), s, f, 10 * MIN_GAP_MS, false);
        let real = std::fs::canonicalize(home.join("memory/handoff.md")).unwrap();
        // Absolute, or relative to its working directory.
        assert_eq!(ok(&a, &s, &real.to_string_lossy()).unwrap(), real);
        assert_eq!(ok(&a, &s, "memory/handoff.md").unwrap(), real);
        // In its working directory (a repository) too.
        std::fs::write(work.join("HANDOFF.md"), "x").unwrap();
        assert!(ok(&a, &session(&work, AgentState::Working), "HANDOFF.md").is_ok());

        // Not a named agent; not promoted (the error says how the human grants it); Codex.
        assert!(check_request(None, &s, "memory/handoff.md", 0, false).unwrap_err().contains("only named agents"));
        let no = ok(&Agent { self_handoff: false, ..a.clone() }, &s, "memory/handoff.md").unwrap_err();
        assert!(no.contains("isn't allowed") && no.contains("--self-handoff on") && no.contains("Edit agent"), "{no}");
        let codex = ok(&Agent { kind: SessionKind::Codex, ..a.clone() }, &s, "memory/handoff.md").unwrap_err();
        assert!(codex.contains("isn't supported for Codex agents yet"), "{codex}");
        assert!(ok(&a, &session(&home, AgentState::Exited), "memory/handoff.md").is_err());

        // The file: missing, empty, a directory, outside home and cwd, a symlink out of home.
        assert!(ok(&a, &s, "memory/nope.md").unwrap_err().contains("doesn't exist"));
        assert!(ok(&a, &s, "  ").unwrap_err().contains("give the path"));
        assert!(ok(&a, &s, "memory").unwrap_err().contains("not a regular file"));
        std::fs::write(root.join("elsewhere.md"), "x").unwrap();
        assert!(ok(&a, &s, &root.join("elsewhere.md").to_string_lossy()).unwrap_err().contains("outside your home"));
        assert!(ok(&a, &s, "../elsewhere.md").unwrap_err().contains("outside your home"));
        std::os::unix::fs::symlink(root.join("elsewhere.md"), home.join("link.md")).unwrap();
        assert!(ok(&a, &s, "link.md").unwrap_err().contains("outside your home"), "a link is judged by its target");
        assert!(ok(&a, &s, "/etc/passwd").unwrap_err().contains("outside your home"));
        // The working directory of a session in a repository doesn't let it reach its home's parent.
        assert!(ok(&a, &session(&work, AgentState::Working), "../elsewhere.md").is_err());

        // One waiting at a time; one fresh start per MIN_GAP_MS.
        assert!(check_request(Some(&a), &s, "memory/handoff.md", 0, true).unwrap_err().contains("already asked"));
        let recent = Agent { self_handoff_at: Some(1_000), ..a.clone() };
        let soon = check_request(Some(&recent), &s, "memory/handoff.md", 1_000 + MIN_GAP_MS - 1, false).unwrap_err();
        assert!(soon.contains("at most one every 30 minutes"), "{soon}");
        assert!(check_request(Some(&recent), &s, "memory/handoff.md", 1_000 + MIN_GAP_MS, false).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn granting_it_takes_the_human() {
        let (root, home, _) = dirs();
        let a = Agent { self_handoff: false, ..agent(&home) };
        assert!(check_grant(&a, true).is_ok());
        let e = check_grant(&a, false).unwrap_err().to_string();
        assert!(e.contains("only the human") && e.contains("inside a Muster session"), "{e}");
        let e = check_grant(&Agent { kind: SessionKind::Codex, ..a }, true).unwrap_err().to_string();
        assert!(e.contains("Codex"), "{e}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_first_prompt_says_who_asked_when_and_on_what_work() {
        let f = Path::new("/home/a/memory/handoff.md");
        let t = first_prompt("otto", f, 0, None);
        assert!(t.starts_with("[muster self-handoff origin=human hop=0] You (otto) asked Muster"), "{t}");
        assert!(t.contains("the human did not type this") && t.contains("/home/a/memory/handoff.md"), "{t}");
        assert!(!t.contains("not a new request"));
        // Never an agent message's envelope: no reply goes anywhere.
        assert_eq!(crate::agents::parse_envelope(&t), None);
        let s = first_prompt("otto", f, 0, Some(Prov { origin: Origin::Schedule, hop: 2, msg_id: Some(9) }));
        assert!(s.starts_with("[muster self-handoff origin=schedule hop=2]") && s.contains("handling scheduled work"), "{s}");
        assert!(s.contains("not a new request from the human"));
        let w = first_prompt("otto", f, 0, Some(Prov { origin: Origin::Webhook, hop: 1, msg_id: None }));
        assert!(w.contains("handling triggered work"), "{w}");
    }

    #[test]
    fn it_waits_for_rest_in_the_run_that_asked() {
        let (root, home, _) = dirs();
        let a = agent(&home);
        let idle = session(&home, AgentState::Idle);
        let ok = Now { live: true, queue_idle: true, draft: false };
        assert_eq!(rest(Some(&a), Some(&idle), 2, ok), Rest::Idle);
        for state in [AgentState::Working, AgentState::NeedsApproval, AgentState::AskingQuestion, AgentState::Starting] {
            assert_eq!(rest(Some(&a), Some(&session(&home, state)), 2, ok), Rest::Busy, "{state:?}");
        }
        let mut queued = idle.clone();
        queued.queued = 1;
        assert_eq!(rest(Some(&a), Some(&queued), 2, ok), Rest::Busy, "a prompt waiting goes first");
        assert_eq!(rest(Some(&a), Some(&idle), 2, Now { queue_idle: false, ..ok }), Rest::Busy, "one being typed too");
        assert_eq!(rest(Some(&a), Some(&idle), 2, Now { draft: true, ..ok }), Rest::Busy, "the human is writing to it");
        let cancelled = |r: Rest| matches!(r, Rest::Cancel(_));
        assert!(cancelled(rest(None, Some(&idle), 2, ok)));
        assert!(cancelled(rest(Some(&Agent { self_handoff: false, ..a.clone() }), Some(&idle), 2, ok)), "taken back");
        assert!(cancelled(rest(Some(&Agent { session_id: Some(3), ..a.clone() }), Some(&idle), 2, ok)));
        assert!(cancelled(rest(Some(&Agent { asleep_at: Some(1), ..a.clone() }), Some(&idle), 2, ok)));
        assert!(cancelled(rest(Some(&a), Some(&idle), 2, Now { live: false, ..ok })), "restarted or stopped");
        assert!(cancelled(rest(Some(&a), Some(&session(&home, AgentState::Exited)), 2, ok)));
        assert!(cancelled(rest(Some(&a), None, 2, ok)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_claudes_clear_hook_for_this_session_counts() {
        let ev = |sid: SessionId, kind: EventKind, data: Value| Event { id: 1, channel_id: Some(1), session_id: Some(sid), kind, data, ts: 0 };
        let clear = json!({"session_id": 2, "event": "SessionStart", "summary": {"source": "clear"}});
        assert!(cleared_event(&ev(2, EventKind::Hook, clear.clone()), 2));
        assert!(!cleared_event(&ev(3, EventKind::Hook, clear.clone()), 2));
        assert!(!cleared_event(&ev(2, EventKind::System, clear), 2));
        assert!(!cleared_event(&ev(2, EventKind::Hook, json!({"event": "SessionStart", "summary": {"source": "startup"}})), 2));
    }

    #[test]
    fn a_promoted_agents_persona_tells_it_how() {
        let (root, home, _) = dirs();
        let t = crate::agents::persona_text_for(&agent(&home));
        assert!(t.contains("## Starting fresh") && t.contains("`start_fresh`") && t.contains("muster start-fresh"), "{t}");
        assert!(!crate::agents::persona_text_for(&Agent { self_handoff: false, ..agent(&home) }).contains("Starting fresh"));
        assert!(!crate::agents::persona_text_for(&Agent { kind: SessionKind::Codex, ..agent(&home) }).contains("Starting fresh"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

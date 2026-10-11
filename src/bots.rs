//! Bots (PLAN.md §10): persistent named agent sessions other bots (or the human) can ask
//! things and get replies from.
//!
//! A bot is a stored record (`bots` table) plus at most one muster session at a time: a claude
//! or codex agent whose cwd is the bot's home dir (`<data>/bots/<name>/`, with a CLAUDE.md or
//! AGENTS.md written from its persona and a `memory/` dir), in a tab named after the bot in the
//! "bots" workspace (`<data>/bots`). Claude bots resume their conversation (`--resume <id>`,
//! the id captured from the SessionStart hook).
//!
//! Messages: everything muster delivers to a bot gets a one-line envelope
//! (`[muster msg <id> from <sender> origin=<origin> hop=<n>]`) and a `bot_messages` row. The
//! origin is where the chain started (`human`, `schedule`, `webhook`) and propagates: a message
//! a session sends while handling message M inherits M's origin at hop+1 (the "current inbound
//! message", per session, also walked up `parent_id`, so a helper a bot spawned inherits it). A
//! prompt with no envelope (the human typing, `muster send`, `muster ask` outside a session)
//! is origin=human, hop=0. Delivery is refused when the bot doesn't accept the origin, the hop
//! is over `guard::MAX_BOT_HOPS`, or the bot's daily cap is used up (`guard::check_bot_delivery`).
//!
//! The registry keeps bots in memory and writes through to the store (like `devices`). Current
//! inbound messages and last replies are runtime only; the messages and their replies persist.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::{CreateChannel, Hub, SpawnSession};
use crate::mcp::guard;
use crate::model::*;
use crate::paths;
use crate::store::Store;

/// Where a message chain started. Claude bots accept `human` only by default (PLAN.md §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Schedule,
    Webhook,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Human => "human",
            Origin::Schedule => "schedule",
            Origin::Webhook => "webhook",
        }
    }
    pub fn parse(s: &str) -> Option<Origin> {
        match s.trim().to_ascii_lowercase().as_str() {
            "human" => Some(Origin::Human),
            "schedule" => Some(Origin::Schedule),
            "webhook" => Some(Origin::Webhook),
            _ => None,
        }
    }
}

/// Default accepted origins: Claude bots take human-started work only (Anthropic's consumer
/// terms are unclear about unattended use); Codex bots also take scheduled and webhook work.
pub fn default_accepts(kind: SessionKind) -> Vec<Origin> {
    match kind {
        SessionKind::Claude => vec![Origin::Human],
        _ => vec![Origin::Human, Origin::Schedule, Origin::Webhook],
    }
}

/// `human,schedule` -> origins (deduplicated, in order).
pub fn parse_accepts(s: &str) -> anyhow::Result<Vec<Origin>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let o = Origin::parse(part)
            .ok_or_else(|| anyhow::anyhow!("unknown origin `{part}` (expected human, schedule or webhook)"))?;
        if !out.contains(&o) {
            out.push(o);
        }
    }
    if out.is_empty() {
        anyhow::bail!("accepts needs at least one origin (human, schedule, webhook)");
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct Bot {
    pub id: i64,
    pub name: String,
    pub kind: SessionKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub persona: String,
    pub home_dir: String,
    /// Permission profile of the bot's tab when muster creates it (the human may change the
    /// tab's profile later, like any tab's).
    pub permissions: Permissions,
    pub accepts_origins: Vec<Origin>,
    /// The bot's current muster session (live or exited).
    pub session_id: Option<SessionId>,
    /// The agent's own conversation id, passed back as `--resume` (Claude).
    pub resume_id: Option<String>,
    pub created_at: i64,
}

/// One message muster delivered to a bot.
#[derive(Debug, Clone, Serialize)]
pub struct BotMessage {
    pub id: i64,
    /// Sender label as shown in the envelope: a bot name, `session:<id>`, `human`, `schedule`.
    pub from: String,
    pub from_session: Option<SessionId>,
    pub to_bot: i64,
    pub to_session: Option<SessionId>,
    pub origin: Origin,
    pub hop: u32,
    /// The message the sender was handling when it sent this one.
    pub reply_to: Option<i64>,
    pub created_at: i64,
    /// The bot's final message for the turn that handled it.
    pub reply: Option<String>,
}

/// The message a session is handling right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Inbound {
    pub msg_id: i64,
    pub origin: Origin,
    pub hop: u32,
}

/// Origin, hop and reply_to of a message about to be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub origin: Origin,
    pub hop: u32,
    pub reply_to: Option<i64>,
}

pub const NAME_MAX: usize = 40;

/// Bot names: a letter or digit, then letters, digits, `-` or `_` (they name a directory and a
/// tab, and appear in envelopes).
pub fn check_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= NAME_MAX
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!(
            "bad bot name `{name}`: use 1-{NAME_MAX} letters, digits, `-` or `_`, starting with a letter or digit"
        );
    }
    Ok(())
}

pub fn home_dir_for(name: &str) -> PathBuf {
    paths::bots_dir().join(name.to_ascii_lowercase())
}

/// The instructions file the agent CLI reads from its cwd.
pub fn persona_file(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Claude => "CLAUDE.md",
        _ => "AGENTS.md",
    }
}

pub fn persona_text(name: &str, kind: SessionKind, persona: &str) -> String {
    let persona = persona.trim();
    format!(
        "# {name}\n\n\
         You are {name}, a persistent muster bot ({kind}). This folder is your home: muster starts you \
         here{resume}.\n\n\
         ## Persona\n\n{persona}\n\n\
         ## Memory\n\n\
         Keep durable notes (facts, decisions, open threads) in `memory/` in this folder, one topic per \
         file, and read them when a conversation starts. Conversations get compacted or restarted; \
         `memory/` is what survives.\n\n\
         ## Messages\n\n\
         A prompt that starts with `[muster msg <id> from <sender> origin=<origin> hop=<n>]` came from \
         another bot or session through muster, not from the human typing. Answer it in your reply: \
         your final message for that turn goes back to the sender. To ask other bots, use the muster \
         MCP tools `list_bots` and `ask_bot`.\n",
        kind = kind.as_str(),
        resume = if kind == SessionKind::Claude { " and resumes your conversation when you restart" } else { "" },
        persona = if persona.is_empty() { "(none given)" } else { persona },
    )
}

/// The one-line header put before every message muster delivers to a bot.
pub fn envelope(msg_id: i64, from: &str, origin: Origin, hop: u32) -> String {
    // The sender label is one token: never a `]` or whitespace that could fake the header's end.
    let from: String = from
        .chars()
        .map(|c| if c.is_whitespace() || c == ']' || c == '[' || c.is_control() { '_' } else { c })
        .collect();
    format!("[muster msg {msg_id} from {from} origin={} hop={hop}]", origin.as_str())
}

/// The message id of an envelope on the first line of `text`.
pub fn parse_envelope(text: &str) -> Option<i64> {
    let first = text.trim_start().lines().next()?;
    let rest = first.strip_prefix("[muster msg ")?;
    let (id, tail) = rest.split_once(' ')?;
    if !tail.starts_with("from ") || !first.trim_end().ends_with(']') {
        return None;
    }
    id.parse().ok()
}

/// The inbound message whose origin a message from `caller` inherits: the caller's own, else
/// its nearest ancestor's (`parent_id`, cycle-safe). None: human-rooted.
pub fn effective_inbound(
    sessions: &[SessionInfo],
    inbound: &HashMap<SessionId, Inbound>,
    caller: SessionId,
) -> Option<Inbound> {
    let parents: HashMap<SessionId, Option<SessionId>> = sessions.iter().map(|s| (s.id, s.parent_id)).collect();
    let mut seen = HashSet::new();
    let mut cur = Some(caller);
    while let Some(id) = cur {
        if !seen.insert(id) {
            return None;
        }
        if let Some(m) = inbound.get(&id) {
            return Some(*m);
        }
        cur = parents.get(&id).copied().flatten();
    }
    None
}

/// Decide a message's origin and hop, and check the bot's policy. `forced`: an origin given by
/// the daemon itself (schedules and webhooks later; tests now), which starts a new chain.
pub fn route(bot: &Bot, inbound: Option<Inbound>, forced: Option<Origin>, recent: usize) -> Result<Route, String> {
    let r = match (forced, inbound) {
        (Some(origin), _) => Route { origin, hop: 0, reply_to: None },
        (None, Some(m)) => Route {
            origin: m.origin,
            hop: m.hop.saturating_add(1),
            reply_to: Some(m.msg_id),
        },
        (None, None) => Route { origin: Origin::Human, hop: 0, reply_to: None },
    };
    guard::check_bot_delivery(&bot.name, &bot.accepts_origins, r.origin, r.hop, recent)?;
    Ok(r)
}

/// While a message waits in the queue, the session is still handling an earlier one; until a
/// hook says which prompt it submitted, assume the stricter of the two (a non-human origin
/// wins, the higher hop wins), so a queued scheduled message can't run as human-rooted.
pub fn stricter(cur: Option<Inbound>, new: Inbound) -> Inbound {
    let Some(cur) = cur else { return new };
    Inbound {
        msg_id: new.msg_id,
        origin: if new.origin == Origin::Human { cur.origin } else { new.origin },
        hop: cur.hop.max(new.hop),
    }
}

/// `--model`/`--effort` (claude) or `-m`/`-c model_reasoning_effort=` (codex), plus
/// `--resume <id>` for a Claude bot with a conversation to resume. Checked against the
/// agent-argument allowlist.
pub fn agent_args(
    kind: SessionKind,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let effort = effort.map(str::trim).filter(|e| !e.is_empty());
    match kind {
        SessionKind::Claude => {
            if let Some(m) = model {
                out.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = effort {
                out.extend(["--effort".to_string(), e.to_string()]);
            }
            if let Some(r) = resume.filter(|r| valid_resume_id(r)) {
                out.extend(["--resume".to_string(), r.to_string()]);
            }
        }
        SessionKind::Codex => {
            if let Some(m) = model {
                out.extend(["-m".to_string(), m.to_string()]);
            }
            if let Some(e) = effort {
                out.extend(["-c".to_string(), format!("model_reasoning_effort={e}")]);
            }
            // TODO(codex resume): Codex resumes with a subcommand (`codex resume <id>`), which
            // the adapter doesn't build and the argument allowlist refuses; the id is captured
            // (`resume_id_from_hook`) but not passed until that is verified against the CLI.
        }
        SessionKind::Shell => anyhow::bail!("bots are claude or codex agents"),
    }
    crate::agents::check_extra_args(kind, &out)?;
    Ok(out)
}

/// A conversation id we pass back on a command line: a uuid-like token, never a flag.
pub fn valid_resume_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The agent's own conversation id from a hook payload: Claude's (and Codex's) SessionStart
/// `session_id`, Codex notify's `thread-id`.
pub fn resume_id_from_hook(kind: SessionKind, event: &str, payload: &Value) -> Option<String> {
    let key = match (kind, event) {
        (SessionKind::Claude | SessionKind::Codex, "SessionStart") => "session_id",
        (SessionKind::Codex, "notify") => "thread-id",
        _ => return None,
    };
    let id = payload.get(key)?.as_str()?.trim();
    valid_resume_id(id).then(|| id.to_string())
}

/// The agent's final message for a turn: Claude's Stop `last_assistant_message`, Codex's notify
/// `last-assistant-message` (agent-turn-complete, not its title request) or Stop hook.
pub fn reply_from_hook(kind: SessionKind, event: &str, payload: &Value) -> Option<String> {
    let text = match (kind, event) {
        (SessionKind::Claude | SessionKind::Codex, "Stop") => payload.get("last_assistant_message"),
        (SessionKind::Codex, "notify") => {
            if payload.get("type").and_then(|v| v.as_str()) != Some("agent-turn-complete")
                || crate::agents::codex::is_title_generation(payload)
            {
                return None;
            }
            payload.get("last-assistant-message")
        }
        _ => None,
    }?;
    let t = text.as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[derive(Default)]
struct RegInner {
    bots: BTreeMap<i64, Bot>,
    /// Session -> the message it is handling (from the UserPromptSubmit hook, else delivery).
    inbound: HashMap<SessionId, Inbound>,
    /// Bot session -> (unix ms, its last final message).
    replies: HashMap<SessionId, (i64, String)>,
}

pub struct Registry {
    inner: Mutex<RegInner>,
    /// Serializes bot starts, so two asks can't start one bot twice.
    start_lock: tokio::sync::Mutex<()>,
}

impl Registry {
    pub fn load(store: &Store) -> anyhow::Result<Self> {
        let mut inner = RegInner::default();
        for b in store.load_bots()? {
            inner.bots.insert(b.id, b);
        }
        Ok(Registry {
            inner: Mutex::new(inner),
            start_lock: Default::default(),
        })
    }

    pub fn list(&self) -> Vec<Bot> {
        self.inner.lock().bots.values().cloned().collect()
    }

    /// By name, case-insensitively.
    pub fn get(&self, name: &str) -> Option<Bot> {
        self.inner
            .lock()
            .bots
            .values()
            .find(|b| b.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
    }

    pub fn by_session(&self, id: SessionId) -> Option<Bot> {
        self.inner.lock().bots.values().find(|b| b.session_id == Some(id)).cloned()
    }

    fn insert(&self, store: &Store, mut b: Bot) -> anyhow::Result<Bot> {
        let mut g = self.inner.lock();
        if g.bots.values().any(|x| x.name.eq_ignore_ascii_case(&b.name)) {
            anyhow::bail!("a bot named {} already exists", b.name);
        }
        b.id = store.insert_bot(&b)?;
        g.bots.insert(b.id, b.clone());
        Ok(b)
    }

    fn remove(&self, store: &Store, id: i64) -> anyhow::Result<()> {
        store.delete_bot(id)?;
        self.inner.lock().bots.remove(&id);
        Ok(())
    }

    fn set_session(&self, store: &Store, id: i64, session: Option<SessionId>) -> anyhow::Result<()> {
        let mut g = self.inner.lock();
        store.set_bot_session(id, session)?;
        if let Some(b) = g.bots.get_mut(&id) {
            b.session_id = session;
        }
        Ok(())
    }

    fn set_resume(&self, store: &Store, id: i64, resume: Option<&str>) -> anyhow::Result<()> {
        let mut g = self.inner.lock();
        if g.bots.get(&id).is_some_and(|b| b.resume_id.as_deref() == resume) {
            return Ok(());
        }
        store.set_bot_resume(id, resume)?;
        if let Some(b) = g.bots.get_mut(&id) {
            b.resume_id = resume.map(str::to_string);
        }
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn inbound(&self, id: SessionId) -> Option<Inbound> {
        self.inner.lock().inbound.get(&id).copied()
    }

    pub fn effective_inbound(&self, sessions: &[SessionInfo], caller: SessionId) -> Option<Inbound> {
        effective_inbound(sessions, &self.inner.lock().inbound, caller)
    }

    /// A message was handed to `id`'s injection queue: it is what the session handles now if
    /// it was typed at once, else assume the stricter of it and the current one.
    fn note_delivery(&self, id: SessionId, m: Inbound, delivered: bool) {
        let mut g = self.inner.lock();
        let next = if delivered { m } else { stricter(g.inbound.get(&id).copied(), m) };
        g.inbound.insert(id, next);
    }

    /// The last final message of `id` recorded at or after `after_ms`.
    pub fn last_reply(&self, id: SessionId, after_ms: i64) -> Option<String> {
        let g = self.inner.lock();
        g.replies.get(&id).filter(|(t, _)| *t >= after_ms).map(|(_, r)| r.clone())
    }

    pub fn forget_session(&self, id: SessionId) {
        let mut g = self.inner.lock();
        g.inbound.remove(&id);
        g.replies.remove(&id);
    }

    /// Every hook callback passes through here (after the adapter's state update):
    /// - SessionStart / notify: the agent's conversation id -> the bot's `resume_id`;
    /// - UserPromptSubmit: the prompt's envelope (or none: the human typed it) -> the
    ///   session's current inbound message;
    /// - Stop / notify: the turn's final message -> the session's last reply and the inbound
    ///   message's stored reply.
    pub fn on_hook(&self, store: &Store, info: &SessionInfo, event: &str, payload: &Value) {
        if event == "UserPromptSubmit" {
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            match parse_envelope(prompt) {
                None => {
                    self.inner.lock().inbound.remove(&info.id);
                }
                Some(msg) => match store.bot_message(msg) {
                    // Only a real message addressed to this session counts; anything else
                    // (a pasted or forged header) keeps what delivery recorded.
                    Ok(Some(m)) if m.to_session == Some(info.id) => {
                        self.inner.lock().inbound.insert(
                            info.id,
                            Inbound {
                                msg_id: m.id,
                                origin: m.origin,
                                hop: m.hop,
                            },
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(session = info.id, "reading bot message {msg}: {e}"),
                },
            }
        }
        let Some(bot) = self.by_session(info.id) else { return };
        if let Some(rid) = resume_id_from_hook(info.kind, event, payload)
            && let Err(e) = self.set_resume(store, bot.id, Some(&rid))
        {
            tracing::warn!(bot = bot.name, "saving resume id: {e}");
        }
        if let Some(reply) = reply_from_hook(info.kind, event, payload) {
            let now = now_ms();
            let inbound = {
                let mut g = self.inner.lock();
                g.replies.insert(info.id, (now, reply.clone()));
                g.inbound.get(&info.id).copied()
            };
            if let Some(m) = inbound
                && let Err(e) = store.set_bot_message_reply(m.msg_id, &reply, now)
            {
                tracing::warn!(bot = bot.name, "saving reply: {e}");
            }
        }
    }
}

// ----- operations (RPC and MCP) -----

/// `bot.create` / `muster bot new`.
#[derive(Debug, Clone, Deserialize)]
pub struct NewBot {
    pub name: String,
    #[serde(default)]
    pub kind: Option<SessionKind>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    /// Default: `default_accepts(kind)`.
    #[serde(default)]
    pub accepts: Option<Vec<Origin>>,
}

pub fn create(hub: &Hub, req: NewBot) -> anyhow::Result<Bot> {
    let name = req.name.trim().to_string();
    check_name(&name)?;
    let kind = req.kind.unwrap_or(SessionKind::Claude);
    let clean = |o: Option<String>| o.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (model, effort) = (clean(req.model), clean(req.effort));
    agent_args(kind, model.as_deref(), effort.as_deref(), None)?;
    let accepts = match req.accepts {
        Some(a) if !a.is_empty() => {
            let mut out: Vec<Origin> = Vec::new();
            for o in a {
                if !out.contains(&o) {
                    out.push(o);
                }
            }
            out
        }
        Some(_) => anyhow::bail!("accepts needs at least one origin (human, schedule, webhook)"),
        None => default_accepts(kind),
    };
    if hub.bots().get(&name).is_some() {
        anyhow::bail!("a bot named {name} already exists");
    }
    let persona = req.persona.unwrap_or_default();
    let home = home_dir_for(&name);
    paths::ensure_private_dir(&home)?;
    paths::ensure_private_dir(&home.join("memory"))?;
    // Ours to write: regenerated from the persona (memory/ is never touched).
    std::fs::write(home.join(persona_file(kind)), persona_text(&name, kind, &persona))?;
    hub.bots().insert(
        hub.store(),
        Bot {
            id: 0,
            name,
            kind,
            model,
            effort,
            persona,
            home_dir: home.to_string_lossy().into_owned(),
            permissions: Permissions::default(),
            accepts_origins: accepts,
            session_id: None,
            resume_id: None,
            created_at: now_ms(),
        },
    )
}

/// `bot.remove`: removes the bot's session (if any) and its record. The home dir (persona,
/// memory) stays on disk.
pub fn remove(hub: &Hub, name: &str) -> anyhow::Result<Bot> {
    let bot = hub.bots().get(name).ok_or_else(|| no_bot(hub, name))?;
    if let Some(sid) = bot.session_id
        && hub.session(sid).is_some()
        && let Err(e) = hub.remove_session(sid)
    {
        tracing::warn!(bot = bot.name, "removing its session: {e:#}");
    }
    hub.bots().remove(hub.store(), bot.id)?;
    Ok(bot)
}

pub fn no_bot(hub: &Hub, name: &str) -> anyhow::Error {
    let names: Vec<String> = hub.bots().list().into_iter().map(|b| b.name).collect();
    anyhow::anyhow!(
        "no bot named {name} ({})",
        if names.is_empty() { "there are none: create one with `muster bot new`".to_string() } else { format!("bots: {}", names.join(", ")) }
    )
}

/// A bot plus its session.
pub struct BotSession {
    pub bot: Bot,
    pub session: SessionInfo,
    /// A process was started (or restarted) for this call.
    pub started: bool,
}

/// `bot.start`: the bot's live session, else restart its exited one, else a new session in a
/// tab named after the bot in the "bots" workspace. `fresh` forgets the conversation id first.
pub async fn start(hub: &Arc<Hub>, name: &str, fresh: bool) -> anyhow::Result<BotSession> {
    let _one = hub.bots().start_lock.lock().await;
    let mut bot = hub.bots().get(name).ok_or_else(|| no_bot(hub, name))?;
    if fresh {
        hub.bots().set_resume(hub.store(), bot.id, None)?;
        bot.resume_id = None;
    }
    let args = agent_args(bot.kind, bot.model.as_deref(), bot.effort.as_deref(), bot.resume_id.as_deref())?;
    let home = PathBuf::from(&bot.home_dir);
    paths::ensure_private_dir(&home.join("memory"))?;
    if !home.join(persona_file(bot.kind)).exists() {
        std::fs::write(home.join(persona_file(bot.kind)), persona_text(&bot.name, bot.kind, &bot.persona))?;
    }
    if let Some(sid) = bot.session_id
        && let Some(info) = hub.session(sid)
        && hub.channel(info.channel_id).is_some_and(|c| !c.archived)
    {
        if info.state != AgentState::Exited && hub.live_session_ids().contains(&sid) {
            return Ok(BotSession { bot, session: info, started: false });
        }
        hub.set_session_extra_args(sid, args)?;
        let session = hub.restart_session(sid).await?;
        return Ok(BotSession { bot, session, started: true });
    }
    let dir = paths::bots_dir();
    paths::ensure_private_dir(&dir)?;
    let ws = hub.add_workspace(dir.to_string_lossy().into_owned(), Some("bots".into())).await?;
    let existing = hub
        .channels()
        .into_iter()
        .find(|c| c.workspace_id == ws.id && c.name.eq_ignore_ascii_case(&bot.name));
    let channel = match existing {
        Some(c) => c,
        None => {
            hub.create_channel(CreateChannel {
                workspace_id: ws.id,
                name: Some(bot.name.clone()),
                worktree: false,
                base: None,
                agent: None,
                prompt: None,
                extra_args: Vec::new(),
                permissions: Some(bot.permissions),
                window_id: None,
                prefer_window: None,
                cols: crate::hub::default_cols(),
                rows: crate::hub::default_rows(),
            })
            .await?
        }
    };
    let session = hub
        .spawn_session(SpawnSession {
            channel_id: channel.id,
            kind: bot.kind,
            name: Some(bot.name.clone()),
            cwd: Some(bot.home_dir.clone()),
            prompt: None,
            extra_args: args,
            cols: crate::hub::default_cols(),
            rows: crate::hub::default_rows(),
            parent_id: None,
            window_id: None,
        })
        .await?;
    hub.bots().set_session(hub.store(), bot.id, Some(session.id))?;
    bot.session_id = Some(session.id);
    Ok(BotSession { bot, session, started: true })
}

/// A message to deliver to a bot (already routed and checked).
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub bot: String,
    pub text: String,
    /// Sender label for the envelope.
    pub from: String,
    pub from_session: Option<SessionId>,
    pub route: Route,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub msg_id: i64,
    pub session_id: SessionId,
    pub sent_at_ms: i64,
    pub delivered: bool,
    pub queued: u32,
    pub started: bool,
}

/// Start the bot if needed, record the message, and send it (envelope first) through the
/// injection queue.
pub async fn deliver(hub: &Arc<Hub>, out: Outgoing) -> anyhow::Result<Delivery> {
    let bs = start(hub, &out.bot, false).await?;
    let sid = bs.session.id;
    let now = now_ms();
    let msg_id = hub.store().insert_bot_message(&BotMessage {
        id: 0,
        from: out.from.clone(),
        from_session: out.from_session,
        to_bot: bs.bot.id,
        to_session: Some(sid),
        origin: out.route.origin,
        hop: out.route.hop,
        reply_to: out.route.reply_to,
        created_at: now,
        reply: None,
    })?;
    let text = format!("{}\n{}", envelope(msg_id, &out.from, out.route.origin, out.route.hop), out.text);
    let log_from = match out.from_session {
        Some(s) => format!("session:{s}"),
        None => out.route.origin.as_str().to_string(),
    };
    match crate::inject::send(hub, sid, text, crate::inject::SendMode::Queue, log_from).await {
        Ok(o) => {
            hub.bots().note_delivery(
                sid,
                Inbound {
                    msg_id,
                    origin: out.route.origin,
                    hop: out.route.hop,
                },
                o.delivered,
            );
            Ok(Delivery {
                msg_id,
                session_id: sid,
                sent_at_ms: now,
                delivered: o.delivered,
                queued: o.queued,
                started: bs.started,
            })
        }
        Err(e) => {
            let _ = hub.store().delete_bot_message(msg_id);
            Err(e)
        }
    }
}

/// The reply to message `msg_id` (stored when its turn ended), else the session's last reply
/// since `after_ms`.
pub fn reply_for(hub: &Hub, msg_id: i64, session: SessionId, after_ms: i64) -> Option<String> {
    hub.store()
        .bot_message(msg_id)
        .ok()
        .flatten()
        .and_then(|m| m.reply)
        .or_else(|| hub.bots().last_reply(session, after_ms))
}

/// `bot.ask` from the CLI: who is asking.
pub struct Asker {
    /// `MUSTER_SESSION_ID` of the terminal running `muster ask`, if any: its inbound message
    /// decides the origin, as for an MCP call.
    pub from_session: Option<SessionId>,
    /// Daemon-internal origin (tests now; schedules and webhooks later).
    pub forced: Option<Origin>,
}

/// `bot.ask`: route, deliver, optionally wait for the turn to end, and return the reply.
pub async fn ask(hub: &Arc<Hub>, name: &str, text: String, who: Asker, wait: Option<Duration>) -> anyhow::Result<Value> {
    let bot = hub.bots().get(name).ok_or_else(|| no_bot(hub, name))?;
    let text = crate::mcp::tools::clean_text(&text);
    if text.trim().is_empty() {
        anyhow::bail!("text must not be empty");
    }
    if who.forced.is_some() && who.from_session.is_some() {
        anyhow::bail!("pass from_session or origin, not both");
    }
    if let Some(s) = who.from_session {
        if hub.session(s).is_none() {
            anyhow::bail!("no such session: {s}");
        }
        if bot.session_id == Some(s) {
            anyhow::bail!("a bot can't ask itself");
        }
    }
    let inbound = who.from_session.and_then(|s| hub.bots().effective_inbound(&hub.sessions(), s));
    let recent = hub
        .store()
        .count_bot_messages_since(bot.id, now_ms() - guard::BOT_CAP_WINDOW_MS)?;
    let route = route(&bot, inbound, who.forced, recent).map_err(|e| anyhow::anyhow!(e))?;
    let from = match who.from_session {
        Some(s) => sender_label(hub.bots().by_session(s).as_ref(), s),
        None => route.origin.as_str().to_string(),
    };
    let d = deliver(
        hub,
        Outgoing {
            bot: bot.name.clone(),
            text,
            from,
            from_session: who.from_session,
            route,
        },
    )
    .await?;
    let mut out = json!({
        "bot": bot.name,
        "msg_id": d.msg_id,
        "session_id": d.session_id,
        "origin": route.origin,
        "hop": route.hop,
        "sent_at_ms": d.sent_at_ms,
        "started": d.started,
    });
    let Some(timeout) = wait else {
        out["queued"] = json!(d.queued);
        return Ok(out);
    };
    let (info, reached, timed_out) = wait_settled(hub, d.session_id, d.sent_at_ms, timeout).await?;
    let (reply, source) = match reply_for(hub, d.msg_id, d.session_id, d.sent_at_ms) {
        Some(r) => (Some(r), Some("stop_hook")),
        None if reached && info.state == AgentState::Idle => (
            hub.recent_output_text(d.session_id, 60).map(|t| crate::mcp::backend::last_lines(&t, 60)),
            Some("terminal_output"),
        ),
        None => (None, None),
    };
    out["reply"] = json!(reply);
    out["reply_source"] = json!(source);
    out["state"] = json!(info.state);
    out["detail"] = json!(info.state_detail);
    out["timed_out"] = json!(timed_out);
    Ok(out)
}

/// Envelope sender label: the bot's name for a bot session, else `session:<id>`.
pub fn sender_label(bot: Option<&Bot>, session: SessionId) -> String {
    match bot {
        Some(b) => b.name.clone(),
        None => format!("session:{session}"),
    }
}

/// Wait until `id` settles (the default wait states) after `after_ms`, with `session.wait`'s
/// staleness rules. Returns (info, reached, timed_out).
async fn wait_settled(
    hub: &Arc<Hub>,
    id: SessionId,
    after_ms: i64,
    timeout: Duration,
) -> anyhow::Result<(SessionInfo, bool, bool)> {
    use crate::mcp::wait::{DEFAULT_STATES, Verdict, WaitCtx, evaluate, next_change};
    let mut rx = hub.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let info = hub
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("session {id} was removed while waiting"))?;
        let ctx = WaitCtx {
            after_ms: Some(after_ms),
            last_send_ms: None,
            last_delivery_ms: hub.last_dequeue_at(id),
            settle_grace_ms: guard::Limits::default().settle_grace_ms,
            now_ms: now_ms(),
        };
        let verdict = evaluate(&info, &DEFAULT_STATES, &ctx);
        let now = tokio::time::Instant::now();
        if verdict != Verdict::Wait || now >= deadline {
            return Ok((info, verdict == Verdict::Reached, verdict == Verdict::Wait));
        }
        next_change(&mut rx, id, (deadline - now).min(Duration::from_secs(1))).await;
    }
}

/// A bot as listed (`bot.list`, MCP `list_bots`): the record plus its session's state.
pub fn bot_json(b: &Bot, session: Option<&SessionInfo>) -> Value {
    json!({
        "id": b.id,
        "name": b.name,
        "kind": b.kind,
        "model": b.model,
        "effort": b.effort,
        "accepts_origins": b.accepts_origins,
        "home_dir": b.home_dir,
        "session_id": b.session_id,
        "state": session.map(|s| s.state),
        "detail": session.and_then(|s| s.state_detail.clone()),
        "resumable": b.resume_id.is_some(),
        "created_at": b.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::guard::test_session;

    fn bot(kind: SessionKind) -> Bot {
        Bot {
            id: 1,
            name: "ada".into(),
            kind,
            model: None,
            effort: None,
            persona: String::new(),
            home_dir: "/x".into(),
            permissions: Permissions::default(),
            accepts_origins: default_accepts(kind),
            session_id: Some(9),
            resume_id: None,
            created_at: 0,
        }
    }

    #[test]
    fn envelope_round_trip() {
        let e = envelope(17, "scout", Origin::Schedule, 2);
        assert_eq!(e, "[muster msg 17 from scout origin=schedule hop=2]");
        assert_eq!(parse_envelope(&format!("{e}\nwhat's new?")), Some(17));
        // The sender label can't break the header.
        assert_eq!(
            envelope(3, "a b]c\n", Origin::Human, 0),
            "[muster msg 3 from a_b_c_ origin=human hop=0]"
        );
        assert_eq!(parse_envelope("hello"), None);
        assert_eq!(parse_envelope("[muster msg x from a origin=human hop=0]"), None);
        assert_eq!(parse_envelope("[muster msg 5 hi"), None);
        assert_eq!(parse_envelope("text\n[muster msg 5 from a origin=human hop=0]"), None);
    }

    #[test]
    fn origin_propagates_and_is_refused() {
        let claude = bot(SessionKind::Claude);
        let codex = bot(SessionKind::Codex);
        // Human (no inbound): hop 0, accepted by both.
        assert_eq!(
            route(&claude, None, None, 0),
            Ok(Route { origin: Origin::Human, hop: 0, reply_to: None })
        );
        // A bot handling a scheduled message inherits its origin, one hop further.
        let sched = Inbound { msg_id: 4, origin: Origin::Schedule, hop: 1 };
        assert_eq!(
            route(&codex, Some(sched), None, 0),
            Ok(Route { origin: Origin::Schedule, hop: 2, reply_to: Some(4) })
        );
        assert!(route(&claude, Some(sched), None, 0).unwrap_err().contains("accepts only human"));
        // A forced origin starts a new chain.
        assert!(route(&claude, None, Some(Origin::Schedule), 0).is_err());
        assert_eq!(route(&codex, Some(sched), Some(Origin::Webhook), 0).unwrap().hop, 0);
        // Hop limit and daily cap.
        let deep = Inbound { msg_id: 1, origin: Origin::Human, hop: guard::MAX_BOT_HOPS };
        assert!(route(&claude, Some(deep), None, 0).unwrap_err().contains("too deep"));
        assert!(route(&claude, None, None, guard::BOT_DAILY_PROMPT_CAP).is_err());
    }

    #[test]
    fn inbound_is_inherited_from_ancestors() {
        let sessions = vec![test_session(1, None), test_session(2, Some(1)), test_session(3, Some(2))];
        let m = Inbound { msg_id: 7, origin: Origin::Webhook, hop: 1 };
        let map = HashMap::from([(1, m)]);
        assert_eq!(effective_inbound(&sessions, &map, 3), Some(m));
        assert_eq!(effective_inbound(&sessions, &HashMap::new(), 3), None);
        // A nearer one wins.
        let own = Inbound { msg_id: 8, origin: Origin::Human, hop: 0 };
        let map = HashMap::from([(1, m), (2, own)]);
        assert_eq!(effective_inbound(&sessions, &map, 3), Some(own));
        let cyc = vec![test_session(1, Some(2)), test_session(2, Some(1))];
        assert_eq!(effective_inbound(&cyc, &HashMap::new(), 1), None);
    }

    #[test]
    fn queued_messages_assume_the_stricter_origin() {
        let human = Inbound { msg_id: 1, origin: Origin::Human, hop: 3 };
        let sched = Inbound { msg_id: 2, origin: Origin::Schedule, hop: 0 };
        assert_eq!(stricter(Some(human), sched), Inbound { msg_id: 2, origin: Origin::Schedule, hop: 3 });
        assert_eq!(stricter(Some(sched), human).origin, Origin::Schedule);
        assert_eq!(stricter(None, human), human);
    }

    #[test]
    fn resume_id_is_captured_from_session_start() {
        let p = json!({"session_id": "0b4f6c1e-2d3a-4f5b-9c8d-7e6f5a4b3c2d", "source": "startup"});
        assert_eq!(
            resume_id_from_hook(SessionKind::Claude, "SessionStart", &p).as_deref(),
            Some("0b4f6c1e-2d3a-4f5b-9c8d-7e6f5a4b3c2d")
        );
        assert_eq!(resume_id_from_hook(SessionKind::Claude, "Stop", &p), None);
        assert_eq!(resume_id_from_hook(SessionKind::Claude, "SessionStart", &json!({"session_id": "--evil"})), None);
        assert_eq!(
            resume_id_from_hook(SessionKind::Codex, "notify", &json!({"thread-id": "t-1"})).as_deref(),
            Some("t-1")
        );
        // And it goes back as --resume for Claude only.
        assert_eq!(
            agent_args(SessionKind::Claude, Some("opus"), None, Some("abc-1")).unwrap(),
            vec!["--model", "opus", "--resume", "abc-1"]
        );
        assert_eq!(
            agent_args(SessionKind::Codex, None, Some("xhigh"), Some("abc-1")).unwrap(),
            vec!["-c", "model_reasoning_effort=xhigh"]
        );
        assert!(agent_args(SessionKind::Shell, None, None, None).is_err());
    }

    #[test]
    fn registry_tracks_hooks() {
        let store = Store::open_memory().unwrap();
        let reg = Registry::load(&store).unwrap();
        let mut b = bot(SessionKind::Claude);
        b.session_id = Some(5);
        let b = reg.insert(&store, b).unwrap();
        let mut info = test_session(5, None);
        info.kind = SessionKind::Claude;
        reg.on_hook(&store, &info, "SessionStart", &json!({"session_id": "conv-1"}));
        assert_eq!(reg.get("ADA").unwrap().resume_id.as_deref(), Some("conv-1"));
        assert_eq!(store.load_bots().unwrap()[0].resume_id.as_deref(), Some("conv-1"));
        // A delivered message becomes the inbound one when its prompt is submitted...
        let msg = store
            .insert_bot_message(&BotMessage {
                id: 0,
                from: "human".into(),
                from_session: None,
                to_bot: b.id,
                to_session: Some(5),
                origin: Origin::Schedule,
                hop: 1,
                reply_to: None,
                created_at: 0,
                reply: None,
            })
            .unwrap();
        let prompt = format!("{}\nhi", envelope(msg, "human", Origin::Schedule, 1));
        reg.on_hook(&store, &info, "UserPromptSubmit", &json!({"prompt": prompt}));
        assert_eq!(reg.inbound(5), Some(Inbound { msg_id: msg, origin: Origin::Schedule, hop: 1 }));
        // ...its Stop stores the reply...
        reg.on_hook(&store, &info, "Stop", &json!({"last_assistant_message": "All done.\nDetails"}));
        assert_eq!(store.bot_message(msg).unwrap().unwrap().reply.as_deref(), Some("All done.\nDetails"));
        assert_eq!(reg.last_reply(5, 0).as_deref(), Some("All done.\nDetails"));
        assert_eq!(reg.last_reply(5, i64::MAX), None);
        // ...and a prompt the human typed resets it to human-rooted.
        reg.on_hook(&store, &info, "UserPromptSubmit", &json!({"prompt": "thanks"}));
        assert_eq!(reg.inbound(5), None);
        // A header naming another session's message is ignored.
        let other = format!("{}\nx", envelope(msg, "human", Origin::Schedule, 1));
        let mut info6 = info.clone();
        info6.id = 6;
        reg.on_hook(&store, &info6, "UserPromptSubmit", &json!({"prompt": other}));
        assert_eq!(reg.inbound(6), None);
    }

    #[test]
    fn names_and_persona() {
        assert!(check_name("scout-2").is_ok());
        assert!(check_name("-x").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name(&"x".repeat(41)).is_err());
        let t = persona_text("ada", SessionKind::Claude, "Reviews diffs.");
        assert!(t.contains("Reviews diffs.") && t.contains("memory/") && t.contains("resumes"));
        assert_eq!(persona_file(SessionKind::Codex), "AGENTS.md");
        assert_eq!(parse_accepts("human, schedule,human").unwrap(), vec![Origin::Human, Origin::Schedule]);
        assert!(parse_accepts("cron").is_err());
        assert!(parse_accepts("").is_err());
    }
}

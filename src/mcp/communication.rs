//! Durable, explicitly correlated session conversations and pull-based channel subscriptions.
use super::{McpState, backend::Backend, tools::*};
use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub from_workspace: WorkspaceId,
    pub to_workspace: WorkspaceId,
    /// Optionally restrict this directional grant to one caller.
    pub from_session: Option<SessionId>,
    pub message: bool,
    pub read_output: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub from: SessionId,
    pub to: SessionId,
    pub text: String,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub reply: Option<String>,
    pub replied_at: Option<i64>,
    pub error: Option<String>,
    pub timed_out: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub id: u64,
    pub channel_id: ChannelId,
    pub from: SessionId,
    pub text: String,
    pub created_at: i64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub grants: Vec<Grant>,
    pub next_id: u64,
    pub messages: Vec<Message>,
    pub subscriptions: Vec<(SessionId, ChannelId, u64)>,
    pub updates: Vec<Update>,
}
impl State {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}
pub fn allowed<B: Backend>(st: &McpState<B>, caller: SessionId, ch: &Channel, read: bool) -> bool {
    let ws = caller_workspace(st, caller);
    ws == Some(ch.workspace_id)
        || st.guard.lock().communication.grants.iter().any(|g| {
            Some(g.from_workspace) == ws
                && g.to_workspace == ch.workspace_id
                && g.from_session.is_none_or(|s| s == caller)
                && if read { g.read_output } else { g.message }
        })
}
pub fn check<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    s: &SessionInfo,
    read: bool,
) -> Result<(), String> {
    let ch = st
        .backend
        .channel(s.channel_id)
        .ok_or("channel no longer exists")?;
    if ch.archived {
        return Err("channel is archived".into());
    }
    if !allowed(st, caller, &ch, read) {
        return Err(if read {
            "output reading is not shared"
        } else {
            "messaging is not shared"
        }
        .into());
    }
    Ok(())
}
fn message_check<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    s: &SessionInfo,
) -> Result<(), String> {
    check(st, caller, s, false)?;
    if caller == s.id {
        return Err("cannot message yourself".into());
    }
    if s.state == AgentState::Exited {
        return Err("session has exited".into());
    }
    if s.kind == SessionKind::Shell && !st.limits.allow_shell {
        return Err("shell messaging is disabled".into());
    }
    if st
        .backend
        .agents()
        .iter()
        .any(|a| a.session_id == Some(s.id))
    {
        return Err("use ask_agent for this named agent".into());
    }
    super::guard::check_claude_origin(s.kind, caller_origin(st, caller).0)?;
    check_not_more_permissive(
        st,
        &caller_session(st, caller)?,
        &st.backend.channel(s.channel_id).ok_or("channel missing")?,
        s.kind,
    )
}
pub fn reachability<B: Backend>(st: &McpState<B>, caller: SessionId, s: &SessionInfo) -> Value {
    let msg = message_check(st, caller, s);
    let read = check(st, caller, s, true);
    json!({"can_message":msg.is_ok(), "message_reason":msg.err(), "can_read_output":read.is_ok(), "read_output_reason":read.err()})
}
pub fn definitions() -> Vec<Value> {
    let schema = |properties: Value, required: &[&str]| json!({"type":"object", "properties":properties,"required":required,"additionalProperties":false});
    vec![
        tool(
            "request_session",
            "Request a session reply",
            "Send a correlated request to an ordinary session. The recipient explicitly calls reply_session; no terminal scraping. Returns a durable message with progress. A timeout stops waiting, not delivery; use session_messages to retrieve late replies. Use ask_agent for named agents.",
            schema(
                json!({"session_id":{"type":"integer"},"text":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":0,"maximum":1800,"default":0}}),
                &["session_id", "text"],
            ),
            false,
        ),
        tool(
            "reply_session",
            "Reply to session request",
            "Answer a request delivered to you, once, using the request_id in its envelope. The answer is stored in both participants' conversation history; it does not inject another turn. Only the recipient can answer.",
            schema(
                json!({"request_id":{"type":"integer"},"text":{"type":"string"}}),
                &["request_id", "text"],
            ),
            false,
        ),
        tool(
            "session_messages",
            "Session conversations",
            "Read your requests and replies with queued, delivered, answered, failed or timed_out progress. Optional request_id selects one conversation; after_id pages forward. Replies remain retrievable after waiting times out. Retains up to 1000 requests; unresolved requests are never evicted.",
            schema(
                json!({"request_id":{"type":"integer"},"after_id":{"type":"integer"}}),
                &[],
            ),
            true,
        ),
        tool(
            "subscribe_channel",
            "Subscribe to channel updates",
            "Opt in or out of a channel's future posted updates. Updates are pulled with channel_updates and never automatically start agent turns. At most 32 subscriptions per session; access is checked again on reading.",
            schema(
                json!({"channel_id":{"type":"integer"},"enabled":{"type":"boolean"}}),
                &["channel_id", "enabled"],
            ),
            false,
        ),
        tool(
            "channel_updates",
            "Read subscribed updates",
            "Read retained updates from subscribed channels after after_id. Keep next_cursor for the next poll. At most 1000 updates are retained globally; oldest_available_id exposes retention gaps. Your own posts are excluded. Reading does not consume another reader's updates.",
            schema(json!({"after_id":{"type":"integer"}}), &[]),
            true,
        ),
    ]
}
fn unsigned(a: &Value, k: &str) -> Result<u64, String> {
    a[k].as_u64()
        .ok_or_else(|| format!("{k} must be a nonnegative integer"))
}
fn text_arg(a: &Value) -> Result<String, String> {
    let t = clean_text(a["text"].as_str().ok_or("text is required")?);
    if t.trim().is_empty() || t.chars().count() > 20_000 {
        return Err("text must contain 1–20000 characters".into());
    }
    Ok(t)
}
pub fn message_json<B: Backend>(st: &McpState<B>, m: &Message) -> Value {
    let gone = st
        .backend
        .session(m.to)
        .is_none_or(|s| s.state == AgentState::Exited);
    let status = if m.reply.is_some() {
        "answered"
    } else if m.error.is_some() || gone {
        "failed"
    } else if m.timed_out {
        "timed_out"
    } else if m.delivered_at.is_some() {
        "delivered"
    } else {
        "queued"
    };
    let mut v = json!(m);
    v["status"] = json!(status);
    v
}
pub async fn call<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    name: &str,
    a: &Value,
) -> Result<String, String> {
    caller_session(st, caller)?;
    match name {
        "request_session" => {
            let to = u32::try_from(unsigned(a, "session_id")?).map_err(|_| "invalid session_id")?;
            let target = st.backend.session(to).ok_or("session missing")?;
            message_check(st, caller, &target)?;
            let text = text_arg(a)?;
            let timeout = a
                .get("timeout_seconds")
                .map(|_| unsigned(a, "timeout_seconds"))
                .transpose()?
                .unwrap_or(0);
            if timeout > 1800 {
                return Err("timeout_seconds exceeds 1800".into());
            }
            let id = st.edit_guard(|g| -> Result<u64, String> {
                let c = &mut g.communication;
                if c.messages.len() >= 1000 {
                    if let Some(i) = c
                        .messages
                        .iter()
                        .position(|m| m.reply.is_some() || m.error.is_some())
                    {
                        c.messages.remove(i);
                    } else {
                        return Err("inbox capacity reached: resolve outstanding requests".into());
                    }
                }
                let id = c.id();
                c.messages.push(Message {
                    id,
                    from: caller,
                    to,
                    text: text.clone(),
                    created_at: now_ms(),
                    delivered_at: None,
                    reply: None,
                    replied_at: None,
                    error: None,
                    timed_out: false,
                });
                Ok(id)
            })??;
            let envelope = format!(
                "[muster request {id} from session:{caller}]\nReply explicitly with reply_session(request_id={id}, text=your answer).\n{text}"
            );
            let args = json!({"session_id":to,"text":envelope});
            // Reuse the complete send policy, injection queue, provenance and rate limiter.
            let outcome = super::tools::call_tool(st, caller, "send_message", &args);
            let outcome = Box::pin(outcome).await.map_err(|_| "send unavailable")?;
            st.edit_guard(|g| {
                let Some(m) = g.communication.messages.iter_mut().find(|m| m.id == id) else {
                    return;
                };
                if outcome.is_error {
                    m.error = Some(outcome.text.clone());
                } else if serde_json::from_str::<Value>(&outcome.text)
                    .ok()
                    .is_some_and(|v| v["delivered"] == true)
                {
                    m.delivered_at = Some(now_ms());
                }
            })?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let m = st
                    .guard
                    .lock()
                    .communication
                    .messages
                    .iter()
                    .find(|m| m.id == id)
                    .cloned()
                    .ok_or("request expired")?;
                if timeout == 0
                    || m.reply.is_some()
                    || m.error.is_some()
                    || st
                        .backend
                        .session(to)
                        .is_none_or(|s| s.state == AgentState::Exited)
                {
                    return Ok(pretty(&message_json(st, &m)));
                }
                if tokio::time::Instant::now() >= deadline {
                    st.edit_guard(|g| {
                        if let Some(m) = g.communication.messages.iter_mut().find(|m| m.id == id) {
                            m.timed_out = true;
                        }
                    })?;
                    let mut m = m;
                    m.timed_out = true;
                    return Ok(pretty(&message_json(st, &m)));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        "reply_session" => {
            let id = unsigned(a, "request_id")?;
            let text = text_arg(a)?;
            let m = st.edit_guard(|g| -> Result<Message, String> {
                let m = g
                    .communication
                    .messages
                    .iter_mut()
                    .find(|m| m.id == id)
                    .ok_or("request missing")?;
                if m.to != caller {
                    return Err("only the recipient can reply".into());
                }
                if m.reply.is_some() || m.error.is_some() {
                    return Err("request already answered or failed".into());
                }
                m.reply = Some(text);
                m.replied_at = Some(now_ms());
                m.delivered_at.get_or_insert(now_ms());
                Ok(m.clone())
            })??;
            Ok(pretty(&message_json(st, &m)))
        }
        "session_messages" => {
            let messages = st.guard.lock().communication.messages.clone();
            Ok(pretty(&json!(
                messages
                    .iter()
                    .filter(|m| (m.from == caller || m.to == caller)
                        && a["request_id"].as_u64().is_none_or(|id| m.id == id)
                        && m.id > a["after_id"].as_u64().unwrap_or(0))
                    .map(|m| message_json(st, m))
                    .collect::<Vec<_>>()
            )))
        }
        "subscribe_channel" => {
            let ch = ChannelId::try_from(unsigned(a, "channel_id")?)
                .map_err(|_| "invalid channel_id")?;
            let enabled = a["enabled"].as_bool().ok_or("enabled is required")?;
            if enabled {
                let c = st.backend.channel(ch).ok_or("channel missing")?;
                if c.archived || !allowed(st, caller, &c, true) {
                    return Err("channel updates are not shared".into());
                }
            }
            st.edit_guard(|g| -> Result<(), String> {
                let cursor = g.communication.next_id;
                let subs = &mut g.communication.subscriptions;
                if !enabled {
                    subs.retain(|v| v.0 != caller || v.1 != ch);
                } else if !subs.iter().any(|v| v.0 == caller && v.1 == ch) {
                    if subs.iter().filter(|(s, _, _)| *s == caller).count() >= 32 {
                        return Err("subscription limit reached".into());
                    }
                    subs.push((caller, ch, cursor));
                }
                Ok(())
            })??;
            Ok(pretty(&json!({"channel_id":ch,"enabled":enabled})))
        }
        "channel_updates" => {
            let c = st.guard.lock().communication.clone();
            let updates: Vec<_> = c
                .updates
                .iter()
                .filter(|u| {
                    u.id > a["after_id"].as_u64().unwrap_or(0)
                        && u.from != caller
                        && c.subscriptions
                            .iter()
                            .any(|v| v.0 == caller && v.1 == u.channel_id && u.id > v.2)
                        && st
                            .backend
                            .channel(u.channel_id)
                            .is_some_and(|ch| !ch.archived && allowed(st, caller, &ch, true))
                })
                .collect();
            Ok(pretty(
                &json!({"updates":updates,"next_cursor":c.next_id,"oldest_available_id":c.updates.first().map(|u| u.id)}),
            ))
        }
        _ => Err("unknown communication tool".into()),
    }
}
pub fn publish<B: Backend>(
    st: &McpState<B>,
    from: SessionId,
    channel_id: ChannelId,
    text: &str,
) -> Result<(), String> {
    st.edit_guard(|g| {
        let c = &mut g.communication;
        let id = c.id();
        c.updates.push(Update {
            id,
            channel_id,
            from,
            text: text.into(),
            created_at: now_ms(),
        });
        if c.updates.len() > 1000 {
            c.updates.remove(0);
        }
    })
}

/// Record an exact request envelope once the terminal has accepted it.
pub fn accepted(hub: &crate::hub::Hub, session: SessionId, text: &str) {
    let Some(st) = hub.mcp() else { return };
    let Some(id) = text
        .strip_prefix("[muster request ")
        .and_then(|t| t.split(' ').next())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    if let Err(error) = st.edit_guard(|g| {
        if let Some(m) = g.communication.messages.iter_mut().find(|m| m.id == id && m.to == session) {
            let expected = format!("[muster request {} from session:{}]\nReply explicitly with reply_session(request_id={}, text=your answer).\n{}", m.id, m.from, m.id, m.text);
            if text == expected { m.delivered_at.get_or_insert(now_ms()); }
        }
    }) { tracing::warn!("request delivery tracking: {error}"); }
}

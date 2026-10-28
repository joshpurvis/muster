//! Protocol + tool tests against an in-memory mock backend (no live hub needed).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::McpState;
use super::backend::Backend;
use super::protocol::{self, Reply, ReqHeaders};
use crate::hub::{CreateChannel, ServerEvent, SpawnSession};
use crate::inject::{SendMode, SendOutcome};
use crate::model::*;

#[derive(Default)]
struct Data {
    tokens: HashMap<String, SessionId>,
    sessions: BTreeMap<SessionId, SessionInfo>,
    live: HashSet<SessionId>,
    channels: BTreeMap<ChannelId, Channel>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    events: Vec<(Option<ChannelId>, Option<SessionId>, EventKind, Value)>,
    sends: Vec<(SessionId, String, SendMode, String)>,
    next_session: SessionId,
    next_channel: ChannelId,
    diff: String,
    fail_spawn: bool,
    /// The host may have started the process, but its acknowledgement was lost.
    uncertain_spawn: bool,
    fail_send: bool,
    pending_send: bool,
    /// extra_args of the last spawn_session call.
    last_args: Vec<String>,
    /// What the mock's send reports: (delivered, queued).
    send_outcome: Option<(bool, u32)>,
    agents: Vec<crate::agents::Agent>,
    /// Provenance per session, as the real injector sets it on delivery.
    prov: HashMap<SessionId, crate::agents::Prov>,
    /// Source of every send, in order.
    sources: Vec<crate::agents::Source>,
    /// Messages delivered per agent (for the daily cap).
    agent_counts: HashMap<i64, usize>,
    deliveries: Vec<crate::agents::Outgoing>,
    /// Agent session -> the reply its turns report.
    replies: HashMap<SessionId, String>,
    /// Refused messages kept for the log: (to agent, reason).
    refused: Vec<(i64, String)>,
}

struct Mock {
    store: crate::store::Store,
    d: Mutex<Data>,
    tx: broadcast::Sender<ServerEvent>,
}

fn channel(id: ChannelId, name: &str) -> Channel {
    Channel {
        id,
        workspace_id: 1,
        name: name.into(),
        cwd: format!("/repo/.wt/{name}"),
        branch: Some(format!("muster/{name}")),
        base_branch: Some("main".into()),
        owns_worktree: true,
        port_base: 20000,
        archived: false,
        layout: None,
        created_at: 0,
        last_read_at: 0,
        sort_order: 0,
        permissions: Permissions::default(),
        window_id: Some(1),
        auto_named: false,
        first_prompt: None,
    }
}

fn session(
    id: SessionId,
    ch: ChannelId,
    kind: SessionKind,
    parent: Option<SessionId>,
) -> SessionInfo {
    let mut s = super::guard::test_session(id, parent);
    s.channel_id = ch;
    s.kind = kind;
    s
}

impl Mock {
    fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        let mut d = Data {
            next_session: 100,
            next_channel: 50,
            ..Default::default()
        };
        d.workspaces.insert(
            1,
            Workspace {
                account_profile_id: None,
                id: 1,
                name: "repo".into(),
                path: "/repo".into(),
                is_git: true,
                created_at: 0,
            },
        );
        d.channels.insert(10, channel(10, "main-task"));
        for (id, kind, tok) in [
            (1, SessionKind::Claude, "tok1"),
            (2, SessionKind::Codex, "tok2"),
        ] {
            d.sessions.insert(id, session(id, 10, kind, None));
            d.live.insert(id);
            d.tokens.insert(tok.into(), id);
        }
        Arc::new(Self {
            store: crate::store::Store::open_memory().unwrap(),
            d: Mutex::new(d),
            tx,
        })
    }

    fn update(&self, id: SessionId, f: impl FnOnce(&mut SessionInfo)) {
        let info = {
            let mut d = self.d.lock();
            let s = d.sessions.get_mut(&id).unwrap();
            f(s);
            s.clone()
        };
        let _ = self.tx.send(ServerEvent::SessionUpserted(info));
    }
}

impl Backend for Arc<Mock> {
    fn guard_store(&self) -> Option<crate::store::Store> { Some(self.store.clone()) }
    fn session_by_token(&self, token: &str) -> Option<SessionId> {
        self.d.lock().tokens.get(token).copied()
    }
    fn session(&self, id: SessionId) -> Option<SessionInfo> {
        self.d.lock().sessions.get(&id).cloned()
    }
    fn sessions(&self) -> Vec<SessionInfo> {
        self.d.lock().sessions.values().cloned().collect()
    }
    fn live_session_ids(&self) -> Vec<SessionId> {
        self.d.lock().live.iter().copied().collect()
    }
    fn channel(&self, id: ChannelId) -> Option<Channel> {
        self.d.lock().channels.get(&id).cloned()
    }
    fn channels(&self) -> Vec<Channel> {
        self.d.lock().channels.values().cloned().collect()
    }
    fn workspace(&self, id: WorkspaceId) -> Option<Workspace> {
        self.d.lock().workspaces.get(&id).cloned()
    }
    fn screen_text(&self, id: SessionId) -> Option<String> {
        self.d
            .lock()
            .sessions
            .contains_key(&id)
            .then(|| format!("screen of {id}\n\n"))
    }
    fn recent_output(&self, id: SessionId, lines: usize) -> Option<String> {
        self.d
            .lock()
            .sessions
            .contains_key(&id)
            .then(|| format!("last {lines} lines of {id}"))
    }
    fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.tx.subscribe()
    }
    fn log_event(&self, c: Option<ChannelId>, s: Option<SessionId>, k: EventKind, data: Value) {
        self.d.lock().events.push((c, s, k, data));
    }
    async fn create_channel(&self, req: CreateChannel) -> anyhow::Result<Channel> {
        let mut d = self.d.lock();
        d.next_channel += 1;
        let mut c = channel(d.next_channel, req.name.as_deref().unwrap_or("auto"));
        c.auto_named = req.name.is_none();
        c.window_id = req.prefer_window;
        c.workspace_id = req.workspace_id;
        c.permissions = req.permissions.unwrap_or_default();
        if !req.worktree {
            c.cwd = "/repo".into();
        }
        d.channels.insert(c.id, c.clone());
        Ok(c)
    }
    async fn spawn_session(&self, req: SpawnSession) -> anyhow::Result<SessionInfo> {
        let mut d = self.d.lock();
        if d.fail_spawn {
            anyhow::bail!("claude: command not found");
        }
        d.next_session += 1;
        d.last_args = req.extra_args.clone();
        let mut s = session(d.next_session, req.channel_id, req.kind, req.parent_id);
        if let Some(p) = req.prov {
            d.prov.insert(s.id, p);
        }
        s.state = AgentState::Starting;
        if let Some(n) = req.name {
            s.name = n;
        }
        d.sessions.insert(s.id, s.clone());
        d.live.insert(s.id);
        if d.uncertain_spawn {
            return Err(anyhow::Error::new(crate::terminal::SpawnUncertain("lost".into()))
                .context(crate::hub::SpawnRetained(s.id)));
        }
        Ok(s)
    }
    async fn send(
        &self,
        id: SessionId,
        text: String,
        mode: SendMode,
        from: String,
        source: crate::agents::Source,
    ) -> anyhow::Result<SendOutcome> {
        let mut d = self.d.lock();
        if d.pending_send { return Err(crate::inject::DeliveryPending.into()) }
        if d.fail_send {
            anyhow::bail!("pty gone");
        }
        d.sends.push((id, text, mode, from));
        d.sources.push(source);
        match source {
            crate::agents::Source::Chain(p) => d.prov.insert(id, p),
            crate::agents::Source::Human => d.prov.remove(&id),
        };
        let (delivered, queued) = d.send_outcome.unwrap_or((true, 0));
        Ok(SendOutcome { delivered, queued })
    }
    async fn diff(&self, _cwd: String, _base: Option<String>) -> anyhow::Result<String> {
        Ok(self.d.lock().diff.clone())
    }
    async fn archive_channel(&self, id: ChannelId, _remove_worktree: bool) -> anyhow::Result<()> {
        self.d.lock().channels.get_mut(&id).unwrap().archived = true;
        Ok(())
    }
    fn agents(&self) -> Vec<crate::agents::Agent> {
        self.d.lock().agents.clone()
    }
    fn prov(&self, caller: SessionId) -> Option<crate::agents::Prov> {
        self.d.lock().prov.get(&caller).copied()
    }
    fn agent_messages_since(&self, agent_id: i64, _since: i64) -> usize {
        self.d.lock().agent_counts.get(&agent_id).copied().unwrap_or(0)
    }
    async fn agent_deliver(&self, out: crate::agents::Outgoing) -> anyhow::Result<crate::agents::Delivery> {
        let mut d = self.d.lock();
        if d.pending_send { return Err(crate::inject::DeliveryPending.into()) }
        if d.fail_send {
            anyhow::bail!("pty gone");
        }
        let agent = d.agents.iter().find(|b| b.name == out.agent).unwrap().clone();
        let sid = agent.session_id.unwrap();
        *d.agent_counts.entry(agent.id).or_default() += 1;
        d.deliveries.push(out);
        Ok(crate::agents::Delivery {
            msg_id: d.deliveries.len() as i64,
            session_id: sid,
            sent_at_ms: 0,
            delivered: true,
            queued: 0,
            started: false,
            woke: false,
        })
    }
    fn agent_refused(&self, m: crate::agents::AgentMessage, why: &str) {
        self.d.lock().refused.push((m.to_agent, why.to_string()));
    }
    fn agent_message_status(&self, msg_id: i64) -> crate::agents::MsgStatus {
        let d = self.d.lock();
        let out = &d.deliveries[msg_id as usize - 1];
        let sid = d.agents.iter().find(|b| b.name == out.agent).and_then(|b| b.session_id).unwrap();
        crate::agents::MsgStatus {
            delivered_at: Some(0),
            active: true,
            reply: d.replies.get(&sid).cloned(),
        }
    }
}

fn state() -> (Arc<Mock>, McpState<Arc<Mock>>) {
    let m = Mock::new();
    (m.clone(), McpState::new(m))
}

fn auth(tok: &str) -> ReqHeaders {
    ReqHeaders {
        authorization: Some(format!("Bearer {tok}")),
        ..Default::default()
    }
}

async fn post(st: &McpState<Arc<Mock>>, h: &ReqHeaders, body: Value) -> Reply {
    protocol::handle_post(st, h, body.to_string().as_bytes()).await
}

async fn legacy(st: &McpState<Arc<Mock>>, method: &str, params: Value) -> Value {
    let r = post(
        st,
        &auth("tok1"),
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body.unwrap()
}

/// Call a tool as `tok`; returns (text, is_error).
async fn call_as(st: &McpState<Arc<Mock>>, tok: &str, name: &str, args: Value) -> (String, bool) {
    let r = post(
        st,
        &auth(tok),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert_eq!(r.status, 200);
    let b = r.body.unwrap();
    let res = &b["result"];
    assert!(res.is_object(), "not a result: {b}");
    (
        res["content"][0]["text"].as_str().unwrap().to_string(),
        res["isError"].as_bool().unwrap(),
    )
}

async fn call(st: &McpState<Arc<Mock>>, name: &str, args: Value) -> (String, bool) {
    call_as(st, "tok1", name, args).await
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not json ({e}): {text}"))
}

// ----- transport / protocol -----

#[tokio::test]
async fn auth_and_origin() {
    let (_, st) = state();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    assert_eq!(
        post(&st, &ReqHeaders::default(), body.clone()).await.status,
        401
    );
    assert_eq!(post(&st, &auth("wrong"), body.clone()).await.status, 401);
    let mut h = auth("tok1");
    h.origin = Some("http://evil.example".into());
    assert_eq!(post(&st, &h, body.clone()).await.status, 403);
    h.origin = Some("http://127.0.0.1:17633".into());
    assert_eq!(post(&st, &h, body).await.status, 200);
}

#[tokio::test]
async fn legacy_handshake() {
    let (_, st) = state();
    let r = legacy(&st, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})).await;
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r["result"]["serverInfo"]["name"], "muster");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    let r = legacy(&st, "initialize", json!({"protocolVersion": "1999-01-01"})).await;
    assert_eq!(r["result"]["protocolVersion"], protocol::LEGACY_VERSIONS[0]);

    let n = post(
        &st,
        &auth("tok1"),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(
        n,
        Reply {
            status: 202,
            body: None
        }
    );

    assert_eq!(legacy(&st, "ping", json!(null)).await["result"], json!({}));
    let tools = legacy(&st, "tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), super::tools::TOOL_NAMES.len());
    assert!(tools["result"].get("resultType").is_none());

    let r = legacy(&st, "resources/list", json!({})).await;
    assert_eq!(r["error"]["code"], protocol::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn batch_parse_errors_and_responses() {
    let (_, st) = state();
    let r = post(
        &st,
        &auth("tok1"),
        json!([{"jsonrpc": "2.0", "id": "a", "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/initialized"}]),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.body.unwrap(),
        json!([{"jsonrpc": "2.0", "id": "a", "result": {}}])
    );

    let bad = protocol::handle_post(&st, &auth("tok1"), b"{not json").await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body.unwrap()["error"]["code"], protocol::PARSE_ERROR);

    // Client-sent JSON-RPC response: accepted and ignored.
    let r = post(
        &st,
        &auth("tok1"),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
    )
    .await;
    assert_eq!(r.status, 202);

    // Legacy request with an unknown MCP-Protocol-Version header.
    let mut h = auth("tok1");
    h.protocol_version = Some("1900-01-01".into());
    let r = post(
        &st,
        &h,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(r.status, 400);
}

fn modern_headers(method: &str, name: Option<&str>) -> ReqHeaders {
    let mut h = auth("tok1");
    h.protocol_version = Some("2026-07-28".into());
    h.mcp_method = Some(method.into());
    h.mcp_name = name.map(str::to_string);
    h
}

fn modern_body(method: &str, extra: Value) -> Value {
    let mut params = json!({"_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    }});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            params[k] = v;
        }
    }
    json!({"jsonrpc": "2.0", "id": 5, "method": method, "params": params})
}

#[tokio::test]
async fn modern_era() {
    let (_, st) = state();
    let r = post(
        &st,
        &modern_headers("server/discover", None),
        modern_body("server/discover", json!({})),
    )
    .await;
    assert_eq!(r.status, 200);
    let res = &r.body.as_ref().unwrap()["result"];
    assert_eq!(res["resultType"], "complete");
    assert!(
        res["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );
    assert!(
        res["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2025-06-18"))
    );
    assert_eq!(
        res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "muster"
    );

    let r = post(
        &st,
        &modern_headers("tools/list", None),
        modern_body("tools/list", json!({})),
    )
    .await;
    let res = &r.body.as_ref().unwrap()["result"];
    assert_eq!(res["cacheScope"], "private");
    assert!(res["ttlMs"].is_number());
    assert_eq!(res["tools"].as_array().unwrap().len(), super::tools::TOOL_NAMES.len());

    // Missing headers -> HeaderMismatch.
    let r = post(&st, &auth("tok1"), modern_body("tools/list", json!({}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::HEADER_MISMATCH);
    // Method header mismatch.
    let r = post(
        &st,
        &modern_headers("tools/call", None),
        modern_body("tools/list", json!({})),
    )
    .await;
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::HEADER_MISMATCH);

    // Unsupported version.
    let mut b = modern_body("tools/list", json!({}));
    b["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2030-01-01");
    let r = post(&st, &modern_headers("tools/list", None), b).await;
    assert_eq!(r.status, 400);
    let e = &r.body.unwrap()["error"];
    assert_eq!(e["code"], protocol::UNSUPPORTED_VERSION);
    assert_eq!(e["data"]["requested"], "2030-01-01");

    // Unknown method -> 404 + -32601; ping was removed in this revision.
    let r = post(
        &st,
        &modern_headers("ping", None),
        modern_body("ping", json!({})),
    )
    .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::METHOD_NOT_FOUND);

    // tools/call: Mcp-Name required and must match (base64 sentinel accepted).
    let body = modern_body("tools/call", json!({"name": "whoami", "arguments": {}}));
    let r = post(&st, &modern_headers("tools/call", None), body.clone()).await;
    assert_eq!(r.status, 400);
    let r = post(
        &st,
        &modern_headers("tools/call", Some("list_channels")),
        body.clone(),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = post(
        &st,
        &modern_headers("tools/call", Some("=?base64?d2hvYW1p?=")),
        body.clone(),
    )
    .await;
    assert_eq!(r.status, 200);
    let res = &r.body.unwrap()["result"];
    assert_eq!(res["resultType"], "complete");
    assert_eq!(res["isError"], false);
    assert_eq!(
        parse(res["content"][0]["text"].as_str().unwrap())["session"]["id"],
        1
    );
}

// ----- tools -----

#[tokio::test]
async fn whoami_and_lists() {
    let (m, st) = state();
    let (t, err) = call(&st, "whoami", json!({})).await;
    assert!(!err);
    let v = parse(&t);
    assert_eq!(v["session"]["id"], 1);
    assert_eq!(v["channel"]["id"], 10);
    assert_eq!(v["channel"]["branch"], "muster/main-task");
    assert_eq!(v["workspace"]["path"], "/repo");
    assert_eq!(v["spawn_budget"]["caller_depth"], 0);

    m.update(2, |s| s.state = AgentState::NeedsApproval);
    let v = parse(&call(&st, "list_channels", json!({})).await.0);
    assert_eq!(v[0]["state"], "needs_approval");
    assert_eq!(v[0]["live_sessions"], 2);

    let v = parse(
        &call(&st, "list_sessions", json!({"channel_id": 10}))
            .await
            .0,
    );
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["is_me"], true);
    let v = parse(
        &call(&st, "list_sessions", json!({"channel_id": 999}))
            .await
            .0,
    );
    assert_eq!(v, json!([]));

    // Every tools/call is logged in the caller's channel.
    let d = m.d.lock();
    let calls: Vec<_> = d
        .events
        .iter()
        .filter(|e| e.2 == EventKind::McpCall)
        .collect();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].0, Some(10));
    assert_eq!(calls[0].3["tool"], "whoami");
    assert_eq!(calls[0].3["ok"], true);
}

/// `session.move`: a caller's channel comes from its current session info, never its env.
#[tokio::test]
async fn moved_caller_resolves_its_new_channel() {
    let (m, st) = state();
    m.d.lock().channels.insert(11, channel(11, "other"));
    m.update(1, |s| s.channel_id = 11);
    let v = parse(&call(&st, "whoami", json!({})).await.0);
    assert_eq!(v["channel"]["id"], 11);
    let v = parse(&call(&st, "list_sessions", json!({"channel_id": 11})).await.0);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["is_me"], true);
    let d = m.d.lock();
    assert_eq!(d.events.last().unwrap().0, Some(11), "calls are logged in the new channel");
}

#[tokio::test]
async fn unknown_tool_is_protocol_error_and_logged() {
    let (m, st) = state();
    let r = post(
        &st,
        &auth("tok1"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "approve", "arguments": {}}}),
    )
    .await;
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::INVALID_PARAMS);
    let d = m.d.lock();
    assert_eq!(d.events.last().unwrap().3["ok"], false);
}

#[tokio::test]
async fn send_message_rules() {
    let (m, st) = state();
    let (t, err) = call(&st, "send_message", json!({"session_id": 1, "text": "hi"})).await;
    assert!(err && t.contains("yourself"), "{t}");
    let (t, err) = call(&st, "send_message", json!({"session_id": 42, "text": "hi"})).await;
    assert!(err && t.contains("no session"), "{t}");
    let (t, err) = call(&st, "send_message", json!({"session_id": 2, "text": "   "})).await;
    assert!(err, "{t}");

    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "ls\nls"}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(v["delivered"], true);
    assert!(v["sent_at_ms"].as_i64().unwrap() > 0);
    {
        let d = m.d.lock();
        assert_eq!(
            d.sends[0],
            (
                2,
                "ls\nls".to_string(),
                SendMode::Queue,
                "session:1".to_string()
            )
        );
    }

    // Interrupting a permission prompt is refused (only the human answers those).
    m.update(2, |s| s.state = AgentState::NeedsApproval);
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "x", "interrupt": true}),
    )
    .await;
    assert!(err && t.contains("human"), "{t}");
    m.update(2, |s| s.state = AgentState::Working);
    let (_, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "x", "interrupt": true}),
    )
    .await;
    assert!(!err);
    assert_eq!(m.d.lock().sends.last().unwrap().2, SendMode::Interrupt);

    // Exited targets are refused.
    m.update(2, |s| s.state = AgentState::Exited);
    let (t, err) = call(&st, "send_message", json!({"session_id": 2, "text": "x"})).await;
    assert!(err && t.contains("exited"), "{t}");
}

#[tokio::test]
async fn send_rate_limit() {
    let (_, st) = state();
    for i in 0..20 {
        let (t, err) = call(
            &st,
            "send_message",
            json!({"session_id": 2, "text": format!("m{i}")}),
        )
        .await;
        assert!(!err, "{i}: {t}");
    }
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "one too many"}),
    )
    .await;
    assert!(err && t.contains("rate limit"), "{t}");
    // Other callers have their own budget.
    let (_, err) = call_as(
        &st,
        "tok2",
        "send_message",
        json!({"session_id": 1, "text": "hi"}),
    )
    .await;
    assert!(!err);
}

#[tokio::test]
async fn ask_bot_and_list_bots_are_unlisted_aliases() {
    let (m, st) = state();
    with_agents(&m);
    // Older persona files name the first pass's tools (and `bot` as the argument).
    let (t, err) = call(&st, "list_bots", json!({})).await;
    assert!(!err && parse(&t)["agents"].as_array().unwrap().len() >= 2, "{t}");
    let (t, err) = call(&st, "ask_bot", json!({"bot": "ada", "text": "hi"})).await;
    assert!(!err, "{t}");
    assert_eq!(parse(&t)["reply"], "ada says hi");
    // The same guards apply: asking yourself is refused under either name.
    let (t, err) = call_as(&st, "tok3", "ask_bot", json!({"agent": "ada", "text": "hi"})).await;
    assert!(err && t.contains("yourself"), "{t}");
    let names: Vec<String> = crate::mcp::tools::tool_definitions()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.iter().any(|n| n == "ask_bot" || n == "list_bots"));
}

#[tokio::test]
async fn spawn_agent_is_an_unlisted_alias_of_spawn_session() {
    let (m, st) = state();
    // The old name still works (agents written against it keep working)...
    let (t, err) = call(&st, "spawn_agent", json!({"kind": "claude", "model": "sonnet"})).await;
    assert!(!err, "{t}");
    assert_eq!(m.d.lock().last_args, ["--model", "sonnet"]);
    // ...but only spawn_session is listed.
    let names: Vec<String> = crate::mcp::tools::tool_definitions()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"spawn_session".to_string()) && !names.contains(&"spawn_agent".to_string()));
}

#[tokio::test]
async fn spawn_session_model_effort_and_args() {
    let (m, st) = state();
    let (t, err) = call(&st, "spawn_session", json!({"kind": "claude", "model": "sonnet", "effort": "high"})).await;
    assert!(!err, "{t}");
    assert_eq!(m.d.lock().last_args, ["--model", "sonnet", "--effort", "high"]);
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "model": "gpt-6-sol", "effort": "xhigh", "args": ["--no-alt-screen"]}),
    )
    .await;
    assert!(!err, "{t}");
    assert_eq!(m.d.lock().last_args, ["-m", "gpt-6-sol", "-c", "model_reasoning_effort=xhigh", "--no-alt-screen"]);
    // Nothing that widens permissions, and no model for shells.
    for bad in [
        json!({"kind": "codex", "args": ["--sandbox", "danger-full-access"]}),
        json!({"kind": "claude", "args": ["--dangerously-skip-permissions"]}),
        json!({"kind": "claude", "args": "--model x"}),
        json!({"kind": "shell", "model": "sonnet"}),
        json!({"kind": "shell", "args": ["-c", "printf MARKER"]}),
    ] {
        let (t, err) = call(&st, "spawn_session", bad.clone()).await;
        assert!(err, "{bad}: {t}");
    }
}

#[tokio::test]
async fn spawn_session_defaults_and_new_channel() {
    let (m, st) = state();
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "prompt": "do it", "name": "helper"}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(v["session"]["channel_id"], 10);
    assert_eq!(v["session"]["parent_id"], 1);
    assert_eq!(v["session"]["name"], "helper");
    assert_eq!(v["channel_created"], false);

    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "shell", "new_channel": {"name": "side", "worktree": false}}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(v["channel_created"], true);
    assert_eq!(v["channel"]["name"], "side");
    assert_eq!(v["channel"]["cwd"], "/repo");
    assert_ne!(v["session"]["channel_id"], 10);

    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "shell", "channel_id": 10, "new_channel": {"name": "x"}}),
    )
    .await;
    assert!(err && t.contains("either"), "{t}");
    let (t, err) = call(&st, "spawn_session", json!({"kind": "vim"})).await;
    assert!(err && t.contains("unknown kind"), "{t}");
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "shell", "channel_id": 777}),
    )
    .await;
    assert!(err && t.contains("no channel"), "{t}");

    // Depth: build a chain 1 -> 101 -> 201 -> 202; a depth-3 caller can't spawn.
    {
        let mut d = m.d.lock();
        d.sessions
            .insert(201, session(201, 10, SessionKind::Claude, Some(101)));
        d.sessions
            .insert(202, session(202, 10, SessionKind::Claude, Some(201)));
        d.live.extend([201, 202]);
        d.tokens.insert("tok202".into(), 202);
    }
    let (t, err) = call_as(&st, "tok202", "spawn_session", json!({"kind": "shell"})).await;
    assert!(err && t.contains("depth"), "{t}");
}

#[tokio::test]
async fn permissions_are_shown_and_inherited_never_raised() {
    let (m, st) = state();
    let elevated = Permissions {
        codex_sandbox: CodexSandbox::WorkspaceWriteNetwork,
        claude_mode: ClaudeMode::AcceptEdits,
    };
    m.d.lock().channels.get_mut(&10).unwrap().permissions = elevated;
    // Other channel in the workspace, with the defaults.
    m.d.lock().channels.insert(11, channel(11, "plain"));

    let (t, err) = call(&st, "whoami", json!({})).await;
    assert!(!err, "{t}");
    let v = parse(&t);
    let p = &v["channel"]["permissions"];
    assert_eq!(p["codex_sandbox"], "workspace_write_network");
    assert_eq!(p["claude_mode"], "accept_edits");
    assert_eq!(
        p["codex_flags"],
        "--sandbox workspace-write -c sandbox_workspace_write.network_access=true"
    );
    assert_eq!(p["claude_permission_mode"], "acceptEdits");

    let (t, _) = call(&st, "list_channels", json!({})).await;
    let v = parse(&t);
    let by_id = |id: i64| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        by_id(10)["permissions"]["codex_sandbox"],
        "workspace_write_network"
    );
    assert_eq!(by_id(11)["permissions"]["codex_sandbox"], "workspace_write");
    assert_eq!(by_id(11)["permissions"]["claude_mode"], "default");

    // A new channel gets the caller's profile, whatever the agent asks for.
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "net-child"}}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(
        v["channel"]["permissions"]["codex_sandbox"],
        "workspace_write_network"
    );
    let id = v["channel"]["id"].as_i64().unwrap();
    assert_eq!(m.d.lock().channels[&id].permissions, elevated);

    // A caller in a default channel can't get more by asking: there is no such argument.
    {
        let mut d = m.d.lock();
        d.sessions
            .insert(30, session(30, 11, SessionKind::Claude, None));
        d.live.insert(30);
        d.tokens.insert("tok30".into(), 30);
    }
    let (t, err) = call_as(
        &st,
        "tok30",
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "sneaky", "permissions": {"codex_sandbox": "full_access"}}}),
    )
    .await;
    if !err {
        let v = parse(&t);
        let id = v["channel"]["id"].as_i64().unwrap();
        assert_eq!(m.d.lock().channels[&id].permissions, Permissions::default());
    }
    let (t, err) = call_as(
        &st,
        "tok30",
        "spawn_session",
        json!({"kind": "codex", "permissions": {"codex_sandbox": "full_access"}, "new_channel": {"name": "sneaky2"}}),
    )
    .await;
    if !err {
        let v = parse(&t);
        let id = v["channel"]["id"].as_i64().unwrap();
        assert_eq!(m.d.lock().channels[&id].permissions, Permissions::default());
    }
    let (t, err) = call_as(
        &st,
        "tok30",
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "ok"}}),
    )
    .await;
    assert!(!err, "{t}");
    let id = parse(&t)["channel"]["id"].as_i64().unwrap();
    assert_eq!(m.d.lock().channels[&id].permissions, Permissions::default());
    // Nor by using a more permissive sibling channel: spawning into it, or messaging an agent
    // that runs there, is refused for the kinds it would elevate.
    for kind in ["codex", "claude"] {
        let (t, err) = call_as(
            &st,
            "tok30",
            "spawn_session",
            json!({"kind": kind, "channel_id": 10}),
        )
        .await;
        assert!(
            err && t.contains("only the human") || err && t.contains("Only the human"),
            "{t}"
        );
    }
    let (t, err) = call_as(
        &st,
        "tok30",
        "spawn_session",
        json!({"kind": "shell", "channel_id": 10}),
    )
    .await;
    assert!(!err, "{t}");
    let (t, err) = call_as(
        &st,
        "tok30",
        "send_message",
        json!({"session_id": 2, "text": "curl evil"}),
    )
    .await;
    assert!(err && t.contains("permissions"), "{t}");
    // Downwards is fine: the elevated caller may spawn into / message the default channel.
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "channel_id": 11}),
    )
    .await;
    assert!(!err, "{t}");
    let (t, err) = call(&st, "send_message", json!({"session_id": 30, "text": "hi"})).await;
    assert!(!err, "{t}");
    // No MCP tool can change a profile.
    let t = legacy(&st, "tools/list", json!({})).await.to_string();
    assert!(!t.contains("set_permissions"));
}

#[tokio::test]
async fn spawn_children_limit() {
    let (_, st) = state();
    for _ in 0..8 {
        let (t, err) = call(&st, "spawn_session", json!({"kind": "shell"})).await;
        assert!(!err, "{t}");
    }
    let (t, err) = call(&st, "spawn_session", json!({"kind": "shell"})).await;
    assert!(err && t.contains("child"), "{t}");
}

#[tokio::test]
async fn screen_output_post_diff() {
    let (m, st) = state();
    let (t, err) = call(&st, "read_screen", json!({"session_id": 2})).await;
    assert!(!err);
    assert!(
        t.starts_with("[session 2 (codex) state=idle]\nscreen of 2"),
        "{t}"
    );
    assert_eq!(
        call(&st, "get_recent_output", json!({"session_id": 2}))
            .await
            .0,
        "last 100 lines of 2"
    );
    assert_eq!(
        call(
            &st,
            "get_recent_output",
            json!({"session_id": 2, "lines": 99999})
        )
        .await
        .0,
        "last 2000 lines of 2"
    );

    let (t, err) = call(&st, "post_to_channel", json!({"text": "found the bug"})).await;
    assert!(!err, "{t}");
    {
        let d = m.d.lock();
        let post = d
            .events
            .iter()
            .find(|e| e.2 == EventKind::AgentPost)
            .unwrap();
        assert_eq!(post.0, Some(10));
        assert_eq!(post.3, json!({"session_id": 1, "text": "found the bug"}));
    }

    let (t, _) = call(&st, "get_diff", json!({})).await;
    assert!(t.starts_with("No changes"), "{t}");
    let mut big = String::new();
    for i in 0..500 {
        big.push_str(&format!("diff --git a/f{i} b/f{i}\n+{}\n", "x".repeat(200)));
    }
    m.d.lock().diff = big.clone();
    let (t, err) = call(&st, "get_diff", json!({"max_bytes": 5000})).await;
    assert!(!err);
    assert!(t.len() < big.len());
    assert!(t.contains("[diff truncated"), "{}", &t[t.len() - 300..]);
    assert!(
        t.contains("a/f199 b/f199") && t.contains("and 300 more"),
        "{t}"
    );
}

#[tokio::test]
async fn wait_immediate_and_timeout() {
    let (m, st) = state();
    let (t, _) = call(&st, "wait_for_state", json!({"session_id": 2})).await;
    let v = parse(&t);
    assert_eq!(v["reached"], true);
    assert_eq!(v["state"], "idle");

    m.update(2, |s| s.state = AgentState::Working);
    let (t, err) = call(
        &st,
        "wait_for_state",
        json!({"session_id": 2, "timeout_seconds": 1}),
    )
    .await;
    assert!(!err);
    let v = parse(&t);
    assert_eq!(v["timed_out"], true);
    assert_eq!(v["reached"], false);
    assert!(v["waited_ms"].as_u64().unwrap() >= 900);

    let (t, err) = call(
        &st,
        "wait_for_state",
        json!({"session_id": 2, "states": ["bogus"]}),
    )
    .await;
    assert!(err && t.contains("unknown state"), "{t}");
}

#[tokio::test]
async fn wait_ignores_stale_idle_after_send() {
    let (m, st) = state();
    m.update(2, |s| s.state_since = now_ms() - 10_000);
    let (_, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "cargo test"}),
    )
    .await;
    assert!(!err);

    let m2 = m.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        m2.update(2, |s| {
            s.state = AgentState::Working;
            s.state_since = now_ms();
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        m2.update(2, |s| {
            s.state = AgentState::Idle;
            s.state_since = now_ms();
        });
    });
    let (t, _) = call(
        &st,
        "wait_for_state",
        json!({"session_id": 2, "timeout_seconds": 10}),
    )
    .await;
    let v = parse(&t);
    assert_eq!(v["reached"], true, "{t}");
    assert_eq!(v["state"], "idle");
    let waited = v["waited_ms"].as_u64().unwrap();
    assert!((250..5000).contains(&waited), "waited {waited}");

    // after_ms = 0 opts out of the staleness rule.
    call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "again"}),
    )
    .await;
    let v = parse(
        &call(
            &st,
            "wait_for_state",
            json!({"session_id": 2, "after_ms": 0}),
        )
        .await
        .0,
    );
    assert_eq!(v["reached"], true);
    assert!(v["waited_ms"].as_u64().unwrap() < 500);
}

#[tokio::test]
async fn wait_ends_on_exit() {
    let (m, st) = state();
    m.update(2, |s| s.state = AgentState::Working);
    let m2 = m.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        m2.update(2, |s| s.state = AgentState::Exited);
    });
    let v = parse(
        &call(
            &st,
            "wait_for_state",
            json!({"session_id": 2, "states": ["idle"], "timeout_seconds": 5}),
        )
        .await
        .0,
    );
    assert_eq!(v["ended"], "exited");
    assert_eq!(v["reached"], false);
    assert_eq!(v["timed_out"], false);
}

#[tokio::test]
async fn envelope_and_era_strictness() {
    let (_, st) = state();
    // Missing jsonrpc.
    let r = post(&st, &auth("tok1"), json!({"id": 1, "method": "tools/list"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::INVALID_REQUEST);

    // Modern request inside a batch is refused (header checks can't apply) and not executed.
    let r = post(
        &st,
        &auth("tok1"),
        json!([modern_body(
            "tools/call",
            json!({"name": "post_to_channel", "arguments": {"text": "x"}})
        )]),
    )
    .await;
    let b = r.body.unwrap();
    assert_eq!(b[0]["error"]["code"], protocol::INVALID_REQUEST, "{b}");

    // Modern version header but no body _meta: mismatch, not a silent legacy fallback.
    let r = post(
        &st,
        &modern_headers("tools/list", None),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::HEADER_MISMATCH);
}

#[tokio::test]
async fn audit_covers_missing_name_and_cancellation() {
    let (m, st) = state();
    let r = post(
        &st,
        &auth("tok1"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"arguments": {}}}),
    )
    .await;
    assert_eq!(r.body.unwrap()["error"]["code"], protocol::INVALID_PARAMS);
    assert_eq!(m.d.lock().events.last().unwrap().3["ok"], false);

    // A wait that gets cancelled (client went away) still leaves an audit record.
    m.update(2, |s| s.state = AgentState::Working);
    let body = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "wait_for_state", "arguments": {"session_id": 2}}})
    .to_string();
    let h = auth("tok1");
    let fut = protocol::handle_post(&st, &h, body.as_bytes());
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fut)
            .await
            .is_err()
    );
    let d = m.d.lock();
    let last = &d.events.last().unwrap().3;
    assert_eq!(last["tool"], "wait_for_state");
    assert_eq!(last["cancelled"], true);
}

#[tokio::test]
async fn new_channel_is_a_tab_next_to_the_caller_and_may_be_unnamed() {
    let (m, st) = state();
    m.d.lock().channels.get_mut(&10).unwrap().window_id = Some(4);
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "prompt": "fix it", "new_channel": {}}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(v["channel_created"], true);
    let id = v["channel"]["id"].as_i64().unwrap();
    let ch = m.d.lock().channels.get(&id).cloned().unwrap();
    assert!(ch.auto_named);
    assert_eq!(ch.window_id, Some(4));
    // A blank name is the same as none.
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "  "}}),
    )
    .await;
    assert!(!err, "{t}");
    let id = parse(&t)["channel"]["id"].as_i64().unwrap();
    assert!(m.d.lock().channels.get(&id).unwrap().auto_named);
    // A caller shown in another window than its task's: the new task goes to the caller's
    // window (and the hub puts the new session there too, as the caller is its parent).
    m.d.lock().sessions.get_mut(&1).unwrap().window_id = Some(6);
    let (t, err) = call(&st, "spawn_session", json!({"kind": "codex", "new_channel": {}})).await;
    assert!(!err, "{t}");
    let v = parse(&t);
    let id = v["channel"]["id"].as_i64().unwrap();
    assert_eq!(m.d.lock().channels.get(&id).unwrap().window_id, Some(6));
    let sid = v["session"]["id"].as_u64().unwrap() as SessionId;
    assert_eq!(m.d.lock().sessions.get(&sid).unwrap().parent_id, Some(1));
}

#[tokio::test]
async fn failed_spawn_rolls_back_new_channel() {
    let (m, st) = state();
    m.d.lock().fail_spawn = true;
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "claude", "new_channel": {"name": "doomed"}}),
    )
    .await;
    assert!(err && t.contains("removed again"), "{t}");
    let d = m.d.lock();
    let ch = d.channels.values().find(|c| c.name == "doomed").unwrap();
    assert!(ch.archived);
}

#[tokio::test]
async fn uncertain_spawn_keeps_its_channel_and_reservation() {
    let (m, st) = state();
    m.d.lock().uncertain_spawn = true;
    let (t, err) = call(&st, "spawn_session", json!({"kind": "claude", "new_channel": {"name": "maybe-running"}})).await;
    assert!(err && t.contains("unconfirmed") && t.contains("Do not spawn again"), "{t}");
    let d = m.d.lock();
    let ch = d.channels.values().find(|c| c.name == "maybe-running").unwrap();
    // Archiving would retire the run (killing the agent) and could delete its worktree.
    assert!(!ch.archived, "the channel of a possibly running agent is kept");
    let sid = d.next_session;
    assert!(d.sessions.contains_key(&sid));
    drop(d);
    st.edit_guard(|g| {
        assert!(g.spawned_depth.contains_key(&sid), "the retained child's depth is recorded");
        // With one slot per window, the kept reservation leaves none.
        assert!(g.spawns.check_and_record(1, crate::model::now_ms(), 1, 3_600_000).is_err(),
            "the spawn reservation was not refunded");
    }).unwrap();
}

#[tokio::test]
async fn recorded_depth_survives_removed_ancestor() {
    let (m, st) = state();
    // 1 spawns 101 (depth 1); 101 spawns 102 (depth 2); 102 spawns 103 (depth 3).
    m.d.lock().tokens.insert("t101".into(), 101);
    m.d.lock().tokens.insert("t102".into(), 102);
    m.d.lock().tokens.insert("t103".into(), 103);
    assert!(!call(&st, "spawn_session", json!({"kind": "shell"})).await.1);
    assert!(
        !call_as(&st, "t101", "spawn_session", json!({"kind": "shell"}))
            .await
            .1
    );
    assert!(
        !call_as(&st, "t102", "spawn_session", json!({"kind": "shell"}))
            .await
            .1
    );
    // Remove the middle of the chain; 103 must still be at the depth limit.
    m.d.lock().sessions.remove(&101);
    let (t, err) = call_as(&st, "t103", "spawn_session", json!({"kind": "shell"})).await;
    assert!(err && t.contains("depth"), "{t}");
}

#[tokio::test]
async fn wait_not_fooled_by_queue_drain() {
    // Target is busy: the message is queued. It then goes idle (queue still 1), the queue
    // drains (delivery, still idle), and only then works and finishes.
    let (m, st) = state();
    let st = Arc::new(st);
    super::spawn_tracker(&st);
    m.update(2, |s| {
        s.state = AgentState::Working;
        s.state_since = now_ms();
    });
    m.d.lock().send_outcome = Some((false, 1));
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "next task"}),
    )
    .await;
    assert!(!err, "{t}");
    let sent_at = parse(&t)["sent_at_ms"].as_i64().unwrap();

    let m2 = m.clone();
    tokio::spawn(async move {
        let step = Duration::from_millis(120);
        tokio::time::sleep(step).await;
        m2.update(2, |s| {
            s.state = AgentState::Idle;
            s.state_since = now_ms();
            s.queued = 1;
        });
        tokio::time::sleep(step).await;
        m2.update(2, |s| s.queued = 0); // delivered; agent hasn't reacted yet
        tokio::time::sleep(step).await;
        m2.update(2, |s| {
            s.state = AgentState::Working;
            s.state_since = now_ms();
        });
        tokio::time::sleep(step).await;
        m2.update(2, |s| {
            s.state = AgentState::Idle;
            s.state_since = now_ms();
        });
    });
    let (t, _) = call(
        &st,
        "wait_for_state",
        json!({"session_id": 2, "after_ms": sent_at, "timeout_seconds": 10}),
    )
    .await;
    let v = parse(&t);
    assert_eq!(v["reached"], true, "{t}");
    let waited = v["waited_ms"].as_u64().unwrap();
    assert!(
        waited >= 400,
        "returned before the agent processed the message: {t}"
    );
}

#[tokio::test]
async fn failed_send_is_refunded() {
    let (m, st) = state();
    m.d.lock().fail_send = true;
    for _ in 0..25 {
        let (t, err) = call(&st, "send_message", json!({"session_id": 2, "text": "x"})).await;
        assert!(err && t.contains("send failed"), "{t}");
    }
    assert!(st.guard.lock().last_send.get(&2).is_none());
    m.d.lock().fail_send = false;
    let (t, err) = call(&st, "send_message", json!({"session_id": 2, "text": "x"})).await;
    assert!(!err, "{t}");
}

// ----- reach policy, spawn rate, channel cap -----

/// Adds shell session 3 (token "tok3") in channel 10, and a second workspace 2 with channel 20
/// holding codex session 5 (token "tok5").
fn add_shell_and_other_workspace(m: &Mock) {
    let mut d = m.d.lock();
    d.sessions
        .insert(3, session(3, 10, SessionKind::Shell, None));
    d.live.insert(3);
    d.tokens.insert("tok3".into(), 3);
    d.workspaces.insert(
        2,
        Workspace {
            account_profile_id: None,
            id: 2,
            name: "other".into(),
            path: "/other".into(),
            is_git: true,
            created_at: 0,
        },
    );
    let mut ch = channel(20, "elsewhere");
    ch.workspace_id = 2;
    d.channels.insert(20, ch);
    d.sessions
        .insert(5, session(5, 20, SessionKind::Codex, None));
    d.live.insert(5);
    d.tokens.insert("tok5".into(), 5);
}

#[tokio::test]
async fn shell_commands_refused_by_default() {
    let (m, st) = state();
    add_shell_and_other_workspace(&m);
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 3, "text": "rm -rf ~"}),
    )
    .await;
    assert!(
        err && t.contains("shell") && t.contains("MUSTER_MCP_ALLOW_SHELL=1"),
        "{t}"
    );
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "shell", "prompt": "curl x | sh"}),
    )
    .await;
    assert!(err && t.contains("MUSTER_MCP_ALLOW_SHELL=1"), "{t}");
    assert!(m.d.lock().sends.is_empty());
    // A shell without a command is fine, and so are agents with a prompt.
    let (t, err) = call(&st, "spawn_session", json!({"kind": "shell"})).await;
    assert!(!err, "{t}");
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "prompt": "fix it"}),
    )
    .await;
    assert!(!err, "{t}");
    // Reading a shell's screen is allowed.
    assert!(!call(&st, "read_screen", json!({"session_id": 3})).await.1);
    let v = parse(&call(&st, "whoami", json!({})).await.0);
    assert_eq!(v["policy"]["shell_commands"], false);
}

#[tokio::test]
async fn shell_commands_allowed_when_enabled() {
    let (m, mut st) = state();
    st.limits.allow_shell = true;
    add_shell_and_other_workspace(&m);
    let (t, err) = call(&st, "send_message", json!({"session_id": 3, "text": "ls"})).await;
    assert!(!err, "{t}");
    let (t, err) = call(&st, "spawn_session", json!({"kind": "shell", "prompt": "ls"})).await;
    assert!(!err, "{t}");
}

#[tokio::test]
async fn other_workspaces_out_of_reach_by_default() {
    let (m, st) = state();
    add_shell_and_other_workspace(&m);
    for (tool, args) in [
        ("send_message", json!({"session_id": 5, "text": "hi"})),
        ("read_screen", json!({"session_id": 5})),
        ("get_recent_output", json!({"session_id": 5})),
        (
            "wait_for_state",
            json!({"session_id": 5, "timeout_seconds": 1}),
        ),
        ("post_to_channel", json!({"channel_id": 20, "text": "hi"})),
        ("get_diff", json!({"channel_id": 20})),
        ("spawn_session", json!({"kind": "codex", "channel_id": 20})),
    ] {
        let (t, err) = call(&st, tool, args).await;
        assert!(
            err && t.contains("another workspace") && t.contains("MUSTER_MCP_CROSS_WORKSPACE=1"),
            "{tool}: {t}"
        );
    }
    // Lists only show the caller's workspace.
    let v = parse(&call(&st, "list_sessions", json!({})).await.0);
    assert!(v.as_array().unwrap().iter().all(|s| s["id"] != 5), "{v}");
    let v = parse(&call(&st, "list_channels", json!({})).await.0);
    assert!(v.as_array().unwrap().iter().all(|c| c["id"] != 20), "{v}");
    // The other workspace can't reach us either.
    let (t, err) = call_as(&st, "tok5", "read_screen", json!({"session_id": 1})).await;
    assert!(err && t.contains("another workspace"), "{t}");
    assert!(m.d.lock().sends.is_empty());
}

#[tokio::test]
async fn other_workspaces_reachable_when_enabled() {
    let (m, mut st) = state();
    st.limits.cross_workspace = true;
    add_shell_and_other_workspace(&m);
    let (t, err) = call(&st, "send_message", json!({"session_id": 5, "text": "hi"})).await;
    assert!(!err, "{t}");
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "channel_id": 20}),
    )
    .await;
    assert!(!err, "{t}");
    let v = parse(&call(&st, "list_channels", json!({})).await.0);
    assert!(v.as_array().unwrap().iter().any(|c| c["id"] == 20), "{v}");
}

#[tokio::test]
async fn spawn_rate_limit() {
    let (_, mut st) = state();
    st.limits.max_children_per_caller = 100;
    st.limits.max_mcp_live_total = 100;
    for i in 0..10 {
        let (t, err) = call(&st, "spawn_session", json!({"kind": "codex"})).await;
        assert!(!err, "{i}: {t}");
    }
    let (t, err) = call(&st, "spawn_session", json!({"kind": "codex"})).await;
    assert!(
        err && t.contains("rate limit") && t.contains("10 spawns per 10 minutes"),
        "{t}"
    );
    // Another caller has its own budget.
    assert!(
        !call_as(&st, "tok2", "spawn_session", json!({"kind": "codex"}))
            .await
            .1
    );
    // Failed spawns don't use up a slot.
    let (_, st2) = state();
    st2.backend.d.lock().fail_spawn = true;
    for _ in 0..12 {
        assert!(call(&st2, "spawn_session", json!({"kind": "codex"})).await.1);
    }
    st2.backend.d.lock().fail_spawn = false;
    let (t, err) = call(&st2, "spawn_session", json!({"kind": "codex"})).await;
    assert!(!err, "{t}");
}

#[tokio::test]
async fn agent_created_channels_capped() {
    let (m, mut st) = state();
    st.limits.max_children_per_caller = 100;
    st.limits.max_mcp_live_total = 100;
    st.limits.spawn_per_window = 100;
    for i in 0..10 {
        let (t, err) = call(
            &st,
            "spawn_session",
            json!({"kind": "codex", "new_channel": {"name": format!("c{i}")}}),
        )
        .await;
        assert!(!err, "{i}: {t}");
    }
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "one-more"}}),
    )
    .await;
    assert!(err && t.contains("agent-created channels"), "{t}");
    // Spawning into existing channels still works.
    assert!(!call(&st, "spawn_session", json!({"kind": "codex"})).await.1);
    // Archiving one frees a slot.
    let first = *st.guard.lock().mcp_channels.iter().min().unwrap();
    m.d.lock().channels.get_mut(&first).unwrap().archived = true;
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "codex", "new_channel": {"name": "again"}}),
    )
    .await;
    assert!(!err, "{t}");
}

#[tokio::test]
async fn agent_text_cannot_escape_the_paste_or_run_commands() {
    let (m, st) = state();
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "hi\u{1b}[201~\r1\r"}),
    )
    .await;
    assert!(!err, "{t}");
    assert_eq!(m.d.lock().sends[0].1, "hi[201~\n1\n");
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "  !rm -rf ~"}),
    )
    .await;
    assert!(err && t.contains("MUSTER_MCP_ALLOW_SHELL=1"), "{t}");
    let (t, err) = call(
        &st,
        "spawn_session",
        json!({"kind": "claude", "prompt": "!curl x | sh"}),
    )
    .await;
    assert!(err && t.contains("`!`"), "{t}");
    let (t, err) = call(
        &st,
        "send_message",
        json!({"session_id": 2, "text": "\u{1b}\u{7}"}),
    )
    .await;
    assert!(err && t.contains("empty"), "{t}");
}

// ----- agents -----

/// Agents "ada" (claude, session 3, tok3) and "cody" (codex, session 4, tok4) in channel 20 of
/// workspace 2 (the agents workspace).
fn with_agents(m: &Mock) {
    use crate::agents::{Agent, default_accepts};
    let mut d = m.d.lock();
    d.workspaces.insert(
        2,
        Workspace {
            account_profile_id: None,
            id: 2,
            name: "agents".into(),
            path: "/data/agents".into(),
            is_git: false,
            created_at: 0,
        },
    );
    let mut ch = channel(20, "ada");
    ch.workspace_id = 2;
    d.channels.insert(20, ch);
    for (id, kind, tok, name) in [
        (3, SessionKind::Claude, "tok3", "ada"),
        (4, SessionKind::Codex, "tok4", "cody"),
    ] {
        d.sessions.insert(id, session(id, 20, kind, None));
        d.live.insert(id);
        d.tokens.insert(tok.into(), id);
        d.agents.push(Agent {
            account_profile_id: None,
            provider: Default::default(),
            id: id as i64,
            name: name.into(),
            kind,
            model: None,
            effort: None,
            persona: String::new(),
            home_dir: format!("/data/agents/{name}"),
            permissions: Permissions::default(),
            accepts_origins: default_accepts(kind),
            channel_id: Some(20),
            session_id: Some(id),
            resume_id: None,
            created_at: 0,
            autostart: false, sleep_after_min: None, asleep_at: None,
        });
        d.replies.insert(id, format!("{name} says hi"));
    }
}

fn inbound(m: &Mock, sid: SessionId, origin: crate::agents::Origin, hop: u32) {
    m.d.lock().prov.insert(sid, crate::agents::Prov { msg_id: Some(77), origin, hop });
}

#[tokio::test]
async fn ask_agent_from_a_human_rooted_session() {
    let (m, st) = state();
    with_agents(&m);
    let v = parse(&call(&st, "list_agents", json!({})).await.0);
    assert_eq!(v["agents"].as_array().unwrap().len(), 2);
    assert_eq!(v["your_origin"], json!({"origin": "human", "hop": 0}));
    // Session 1 is in another workspace: agents are reachable anyway.
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ADA", "text": "what's new?"})).await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!(v["reply"], "ada says hi");
    assert!(v.get("unattributed_output").is_none());
    assert_eq!((v["origin"].as_str(), v["hop"].as_u64()), (Some("human"), Some(0)));
    let d = m.d.lock();
    let out = &d.deliveries[0];
    assert_eq!((out.agent.as_str(), out.from.as_str(), out.from_session), ("ada", "session:1", Some(1)));
    assert_eq!(out.route.reply_to, None);
}

#[tokio::test]
async fn ask_agent_origin_rule_hops_and_cap() {
    use crate::agents::Origin;
    let (m, st) = state();
    with_agents(&m);
    // Session 2 is handling a scheduled message: a Codex agent takes it one hop on...
    inbound(&m, 2, Origin::Schedule, 1);
    let (t, err) = call_as(&st, "tok2", "ask_agent", json!({"agent": "cody", "text": "x"})).await;
    assert!(!err, "{t}");
    let v = parse(&t);
    assert_eq!((v["origin"].as_str(), v["hop"].as_u64()), (Some("schedule"), Some(2)));
    assert_eq!(m.d.lock().deliveries[0].route.reply_to, Some(77));
    // ...a Claude agent refuses it.
    let (t, err) = call_as(&st, "tok2", "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("accepts only human"), "{t}");
    // So does any Claude agent: no message, no spawn.
    let (t, err) = call_as(&st, "tok2", "send_message", json!({"session_id": 1, "text": "x"})).await;
    assert!(err && t.contains("origin=schedule"), "{t}");
    let (t, err) = call_as(&st, "tok2", "spawn_session", json!({"kind": "claude"})).await;
    assert!(err && t.contains("origin=schedule"), "{t}");
    let (t, err) = call_as(&st, "tok2", "spawn_session", json!({"kind": "codex"})).await;
    assert!(!err, "{t}");
    // A helper spawned under it starts with that provenance and keeps it, even when its
    // parent moves on to human-rooted work.
    let child = parse(&t)["session"]["id"].as_u64().unwrap() as SessionId;
    m.d.lock().tokens.insert("tokc".into(), child);
    m.d.lock().prov.remove(&2);
    let (t, err) = call_as(&st, "tokc", "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("origin=schedule"), "{t}");
    assert!(call_as(&st, "tok2", "ask_agent", json!({"agent": "ada", "text": "x"})).await.1 == false);
    // Hop limit.
    inbound(&m, 1, Origin::Human, crate::mcp::guard::MAX_AGENT_HOPS);
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("too deep"), "{t}");
    m.d.lock().prov.clear();
    // Daily cap.
    m.d.lock().agent_counts.insert(3, crate::mcp::guard::AGENT_DAILY_PROMPT_CAP);
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("last 24 hours"), "{t}");
    // Each policy refusal was kept for the agents panel's log (origin x2, hops, cap).
    let refused = m.d.lock().refused.clone();
    assert_eq!(refused.len(), 4, "{refused:?}");
    assert!(refused[0].1.contains("accepts only human") && refused[3].1.contains("last 24 hours"));
}

#[tokio::test]
async fn ask_agent_guards() {
    let (m, st) = state();
    with_agents(&m);
    let (t, err) = call_as(&st, "tok3", "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("yourself"), "{t}");
    let (t, err) = call(&st, "ask_agent", json!({"agent": "nobody", "text": "x"})).await;
    assert!(err && t.contains("agents: ada, cody"), "{t}");
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "!ls"})).await;
    assert!(err && t.contains("`!`"), "{t}");
    // Agents take messages through ask_agent only.
    let (t, err) = call_as(&st, "tok4", "send_message", json!({"session_id": 3, "text": "x"})).await;
    assert!(err && t.contains("use ask_agent(agent=\"ada\")"), "{t}");
    // The envelope sender is the agent's name.
    let (t, err) = call_as(&st, "tok4", "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(!err, "{t}");
    assert_eq!(m.d.lock().deliveries[0].from, "cody");
    let v = parse(&call_as(&st, "tok4", "whoami", json!({})).await.0);
    assert_eq!(v["agent"], "cody");
    // An agent in a wider tab than the caller's is refused, as for send_message.
    m.d.lock().channels.get_mut(&20).unwrap().permissions.claude_mode = ClaudeMode::Bypass;
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("more than your channel"), "{t}");
    m.d.lock().channels.get_mut(&20).unwrap().permissions.claude_mode = ClaudeMode::Default;
    // ...and so is one whose own tab (its next restart's profile) is wider.
    {
        let mut d = m.d.lock();
        let mut wide = channel(21, "ada-own");
        wide.workspace_id = 2;
        wide.permissions.claude_mode = ClaudeMode::Bypass;
        d.channels.insert(21, wide);
        d.agents[0].channel_id = Some(21);
    }
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("more than your channel"), "{t}");
    m.d.lock().agents[0].channel_id = Some(20);
    // No final message attributed to this message: the terminal tail, labelled as such.
    m.d.lock().replies.clear();
    let v = parse(&call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await.0);
    assert!(v["reply"].is_null());
    assert_eq!(v["unattributed_output"], "last 60 lines of 3");
    assert!(v["note"].as_str().unwrap().contains("may belong to another prompt"));
    // A failed delivery gives the rate-limit slot back and reports the error.
    m.d.lock().fail_send = true;
    let (t, err) = call(&st, "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("could not deliver"), "{t}");
}

#[tokio::test]
async fn provenance_survives_plain_sends() {
    use crate::agents::{Origin, Prov, Source};
    let (m, st) = state();
    with_agents(&m);
    // An ordinary Codex helper (session 5, tok5).
    {
        let mut d = m.d.lock();
        d.sessions.insert(5, session(5, 10, SessionKind::Codex, None));
        d.live.insert(5);
        d.tokens.insert("tok5".into(), 5);
    }
    // Session 2 handles a scheduled message and passes work to the helper with a plain send...
    inbound(&m, 2, Origin::Schedule, 1);
    let (t, err) = call_as(&st, "tok2", "send_message", json!({"session_id": 5, "text": "do it"})).await;
    assert!(!err, "{t}");
    let sent = *m.d.lock().sources.last().unwrap();
    assert_eq!(sent, Source::Chain(Prov { origin: Origin::Schedule, hop: 2, msg_id: None }));
    // ...so the helper can't launder it into Claude work.
    let (t, err) = call_as(&st, "tok5", "send_message", json!({"session_id": 1, "text": "x"})).await;
    assert!(err && t.contains("origin=schedule"), "{t}");
    let (t, err) = call_as(&st, "tok5", "ask_agent", json!({"agent": "ada", "text": "x"})).await;
    assert!(err && t.contains("accepts only human"), "{t}");
    let (t, err) = call_as(&st, "tok5", "spawn_session", json!({"kind": "claude", "prompt": "x"})).await;
    assert!(err && t.contains("origin=schedule"), "{t}");
    // A human-rooted sender's plain send is human-rooted too.
    let (t, err) = call(&st, "send_message", json!({"session_id": 2, "text": "x"})).await;
    assert!(!err, "{t}");
    assert_eq!(*m.d.lock().sources.last().unwrap(), Source::Chain(Prov { origin: Origin::Human, hop: 0, msg_id: None }));
}

#[tokio::test]
async fn backend_replacement_preserves_rate_limits_and_refuses_unsaved_actions() {
    let (m, mut original) = state();
    original.limits.send_per_window = 1;
    assert!(!call(&original, "send_message", json!({"session_id": 2, "text": "first"})).await.1);
    drop(original);
    let mut replacement = McpState::new(m.clone());
    replacement.limits.send_per_window = 1;
    let (message, error) = call(&replacement, "send_message", json!({"session_id": 2, "text": "second"})).await;
    assert!(error && message.contains("rate limit"), "{message}");
    assert_eq!(m.d.lock().sends.len(), 1);
    m.store.fail_runtime_writes_for_test();
    let (message, error) = call(&replacement, "post_to_channel", json!({"text": "must not post"})).await;
    assert!(error && message.contains("persist MCP limits"), "{message}");
    assert!(m.d.lock().events.iter().all(|event| event.2 != EventKind::AgentPost));
}

#[tokio::test]
async fn panel_opens_and_updates_share_the_durable_post_limit() {
    let (m, mut original) = state();
    original.limits.post_per_window = 1;
    assert!(!call(&original, "post_to_channel", json!({"text":"first"})).await.1);
    for name in ["panel_open", "panel_update"] {
        let (message, error) = call(&original, name, json!({})).await;
        assert!(error && message.contains("rate limit"), "{message}");
    }
    let mut replacement = McpState::new(m);
    replacement.limits.post_per_window = 1;
    let (message, error) = call(&replacement, "panel_open", json!({})).await;
    assert!(error && message.contains("rate limit"), "{message}");
}

#[test]
fn backend_replacement_recovers_depth_after_missing_spawn_ack_and_removed_parent() {
    let (m, original) = state();
    original.edit_guard(|guard| { guard.spawned_depth.insert(99, 4); }).unwrap();
    // The child exists but its final guard update never happened. Its parent was removed.
    m.d.lock().sessions.insert(100, session(100, 10, SessionKind::Codex, Some(99)));
    let replacement = McpState::new(m.clone());
    assert_eq!(replacement.guard.lock().spawned_depth[&100], 5);
    let channels = m.store.mcp_channels().unwrap();
    assert!(channels.is_empty());
    let marked = m.store.insert_channel_with_origin(&channel(0, "mcp-created"), true).unwrap();
    // Simulate loss of the in-memory channel acknowledgement after durable creation.
    let again = McpState::new(m);
    assert!(again.guard.lock().mcp_channels.contains(&marked));
}

#[tokio::test]
async fn durably_accepted_delivery_error_is_not_refunded_after_replacement() {
    let (m, mut original) = state();
    original.limits.send_per_window = 1;
    m.d.lock().pending_send = true;
    let (message, error) = call(&original, "send_message", json!({"session_id": 2, "text": "pending"})).await;
    assert!(error && message.contains("durable queue"), "{message}");
    let mut replacement = McpState::new(m.clone());
    replacement.limits.send_per_window = 1;
    m.d.lock().pending_send = false;
    let (message, error) = call(&replacement, "send_message", json!({"session_id": 2, "text": "retry"})).await;
    assert!(error && message.contains("rate limit"), "{message}");
    assert!(m.d.lock().sends.is_empty());
}

#[tokio::test]
async fn panel_update_offers_set_edits_beside_whole_data() {
    let (_, st) = state();
    let tools = legacy(&st, "tools/list", json!({})).await;
    let update = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "panel_update").unwrap();
    let set = &update["inputSchema"]["properties"]["set"];
    assert_eq!((set["type"].as_str(), set["minItems"].as_u64(), set["maxItems"].as_u64()),
        (Some("array"), Some(1), Some(crate::panels::MAX_SET as u64)));
    assert_eq!(set["items"]["required"], json!(["path", "value"]));
    assert_eq!(set["items"]["additionalProperties"], false);
    assert!(update["description"].as_str().unwrap().contains("prs[number=226].status"));
    // A set edit is an update: it shares the post limit with opens, updates and posts.
    let (_m, mut limited) = state();
    limited.limits.post_per_window = 1;
    assert!(!call(&limited, "post_to_channel", json!({"text":"first"})).await.1);
    let (message, error) = call(&limited, "panel_update", json!({"panel_id": 1, "set": [{"path": "prs[number=226].status", "value": "merged"}]})).await;
    assert!(error && message.contains("rate limit"), "{message}");
}

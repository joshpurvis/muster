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
    fail_send: bool,
    /// What the mock's send reports: (delivered, queued).
    send_outcome: Option<(bool, u32)>,
}

struct Mock {
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
            (2, SessionKind::Shell, "tok2"),
        ] {
            d.sessions.insert(id, session(id, 10, kind, None));
            d.live.insert(id);
            d.tokens.insert(tok.into(), id);
        }
        Arc::new(Self {
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
        let mut c = channel(d.next_channel, &req.name);
        c.workspace_id = req.workspace_id;
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
        let mut s = session(d.next_session, req.channel_id, req.kind, req.parent_id);
        s.state = AgentState::Starting;
        if let Some(n) = req.name {
            s.name = n;
        }
        d.sessions.insert(s.id, s.clone());
        d.live.insert(s.id);
        Ok(s)
    }
    async fn send(
        &self,
        id: SessionId,
        text: String,
        mode: SendMode,
        from: String,
    ) -> anyhow::Result<SendOutcome> {
        let mut d = self.d.lock();
        if d.fail_send {
            anyhow::bail!("pty gone");
        }
        d.sends.push((id, text, mode, from));
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
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 10);
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
    assert_eq!(res["tools"].as_array().unwrap().len(), 10);

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
async fn spawn_agent_defaults_and_new_channel() {
    let (m, st) = state();
    let (t, err) = call(
        &st,
        "spawn_agent",
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
        "spawn_agent",
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
        "spawn_agent",
        json!({"kind": "shell", "channel_id": 10, "new_channel": {"name": "x"}}),
    )
    .await;
    assert!(err && t.contains("either"), "{t}");
    let (t, err) = call(&st, "spawn_agent", json!({"kind": "vim"})).await;
    assert!(err && t.contains("unknown kind"), "{t}");
    let (t, err) = call(
        &st,
        "spawn_agent",
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
    let (t, err) = call_as(&st, "tok202", "spawn_agent", json!({"kind": "shell"})).await;
    assert!(err && t.contains("depth"), "{t}");
}

#[tokio::test]
async fn spawn_children_limit() {
    let (_, st) = state();
    for _ in 0..8 {
        let (t, err) = call(&st, "spawn_agent", json!({"kind": "shell"})).await;
        assert!(!err, "{t}");
    }
    let (t, err) = call(&st, "spawn_agent", json!({"kind": "shell"})).await;
    assert!(err && t.contains("child"), "{t}");
}

#[tokio::test]
async fn screen_output_post_diff() {
    let (m, st) = state();
    let (t, err) = call(&st, "read_screen", json!({"session_id": 2})).await;
    assert!(!err);
    assert!(
        t.starts_with("[session 2 (shell) state=idle]\nscreen of 2"),
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
async fn failed_spawn_rolls_back_new_channel() {
    let (m, st) = state();
    m.d.lock().fail_spawn = true;
    let (t, err) = call(
        &st,
        "spawn_agent",
        json!({"kind": "claude", "new_channel": {"name": "doomed"}}),
    )
    .await;
    assert!(err && t.contains("removed again"), "{t}");
    let d = m.d.lock();
    let ch = d.channels.values().find(|c| c.name == "doomed").unwrap();
    assert!(ch.archived);
}

#[tokio::test]
async fn recorded_depth_survives_removed_ancestor() {
    let (m, st) = state();
    // 1 spawns 101 (depth 1); 101 spawns 102 (depth 2); 102 spawns 103 (depth 3).
    m.d.lock().tokens.insert("t101".into(), 101);
    m.d.lock().tokens.insert("t102".into(), 102);
    m.d.lock().tokens.insert("t103".into(), 103);
    assert!(!call(&st, "spawn_agent", json!({"kind": "shell"})).await.1);
    assert!(
        !call_as(&st, "t101", "spawn_agent", json!({"kind": "shell"}))
            .await
            .1
    );
    assert!(
        !call_as(&st, "t102", "spawn_agent", json!({"kind": "shell"}))
            .await
            .1
    );
    // Remove the middle of the chain; 103 must still be at the depth limit.
    m.d.lock().sessions.remove(&101);
    let (t, err) = call_as(&st, "t103", "spawn_agent", json!({"kind": "shell"})).await;
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

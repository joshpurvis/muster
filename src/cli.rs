//! Agent-facing CLI commands (`muster ls/read/send/wait/keys/...`, see docs/CLI.md). Everything
//! goes through `POST /api/rpc`. Daemon replies are read into small local structs (unknown
//! fields ignored, new ones defaulted) so the CLI doesn't depend on the daemon's model types.

use std::io::{IsTerminal, Read};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client;
use crate::paths::Discovery;

/// An error with a specific exit code (2: bad or ambiguous target, 124: timeout).
#[derive(Debug)]
pub struct Exit {
    pub code: u8,
    pub msg: String,
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Exit {}

fn exit(code: u8, msg: impl Into<String>) -> anyhow::Error {
    Exit { code, msg: msg.into() }.into()
}

/// States `wait`/`send --wait` accept (`needs_human` is a group).
pub const STATES: [&str; 9] = [
    "starting",
    "working",
    "needs_approval",
    "asking_question",
    "idle",
    "error",
    "stuck",
    "exited",
    "needs_human",
];

/// What `wait` waits for by default: anything that isn't the agent busy or starting.
const SETTLED: [&str; 6] = ["idle", "needs_approval", "asking_question", "error", "stuck", "exited"];

/// The daemon caps one `session.wait` at an hour; longer waits are done in chunks.
const MAX_WAIT_CHUNK: Duration = Duration::from_secs(3600);

// ----- daemon snapshot -----

#[derive(Debug, Clone, Deserialize)]
pub struct Sess {
    pub id: u32,
    pub channel_id: i64,
    pub kind: String,
    #[serde(default)]
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub state_detail: Option<String>,
    /// Unix ms.
    #[serde(default)]
    pub state_since: i64,
    #[serde(default)]
    pub queued: u32,
    /// The window the session is a tab in.
    #[serde(default)]
    pub window_id: Option<i64>,
    /// A named agent's closed tab, still running (`session.hide`).
    #[serde(default)]
    pub hidden: bool,
    /// The daemon's JSON as received (for `--json`).
    #[serde(skip)]
    pub raw: Value,
}

impl Sess {
    fn live(&self) -> bool {
        self.state != "exited"
    }
    fn is_agent(&self) -> bool {
        self.kind == "claude" || self.kind == "codex"
    }
    fn from_value(v: Value) -> Option<Sess> {
        let mut s: Sess = serde_json::from_value(v.clone()).ok()?;
        s.raw = v;
        Some(s)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chan {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub window_id: Option<i64>,
    #[serde(default)]
    pub archived: bool,
}

/// Sessions and (non-archived) tabs, from `hello`.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub sessions: Vec<Sess>,
    pub channels: Vec<Chan>,
}

impl Snapshot {
    pub fn from_hello(h: &Value) -> Snapshot {
        let arr = |k: &str| h[k].as_array().cloned().unwrap_or_default();
        let channels: Vec<Chan> = arr("channels")
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Chan>(v).ok())
            .filter(|c| !c.archived)
            .collect();
        let sessions = arr("sessions").into_iter().filter_map(Sess::from_value).collect();
        Snapshot { sessions, channels }
    }

    fn fetch(d: &Discovery) -> anyhow::Result<Snapshot> {
        Ok(Snapshot::from_hello(&client::rpc(d, "hello", json!({}))?))
    }

    fn channel(&self, id: i64) -> Option<&Chan> {
        self.channels.iter().find(|c| c.id == id)
    }

    fn tab_name(&self, s: &Sess) -> &str {
        self.channel(s.channel_id).map_or("?", |c| c.name.as_str())
    }

    /// `tab/name` label.
    fn label(&self, s: &Sess) -> String {
        format!("{}/{}", self.tab_name(s), s.name)
    }

    /// Sessions in visible (non-archived) tabs.
    fn visible(&self) -> impl Iterator<Item = &Sess> {
        self.sessions.iter().filter(|s| self.channel(s.channel_id).is_some())
    }

    /// A session's window: its own (a tab is a session), else its tab's (older daemons).
    /// The session's window; a daemon without per-session windows: its channel's. A hidden
    /// agent tab is in none (its null is real, never filled in from the channel).
    fn window_of(&self, s: &Sess) -> Option<i64> {
        if s.hidden {
            return None;
        }
        s.window_id.or_else(|| self.channel(s.channel_id).and_then(|c| c.window_id))
    }

    /// A session's JSON with `channel_name` added, and `window_id` if the daemon had none.
    fn session_json(&self, s: &Sess) -> Value {
        let mut v = s.raw.clone();
        if let Some(o) = v.as_object_mut() {
            let ch = self.channel(s.channel_id);
            o.insert("channel_name".into(), json!(ch.map(|c| c.name.clone())));
            o.insert("window_id".into(), json!(self.window_of(s)));
        }
        v
    }
}

// ----- target resolution -----

fn eq_ci(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn prefix_ci(s: &str, p: &str) -> bool {
    s.to_lowercase().starts_with(&p.to_lowercase())
}

/// Resolve a target (docs/CLI.md "Targets") to a session. `env_session` is
/// `$MUSTER_SESSION_ID`, for `.`. Errors carry exit code 2.
pub fn resolve(snap: &Snapshot, target: &str, env_session: Option<&str>) -> anyhow::Result<Sess> {
    let target = target.trim();
    if target.is_empty() {
        return Err(exit(2, "empty target"));
    }
    if target == "." {
        let id = env_session
            .filter(|v| !v.is_empty())
            .ok_or_else(|| exit(2, "`.` means this session, but $MUSTER_SESSION_ID is not set (not inside a muster session)"))?;
        let id: u32 = id
            .trim()
            .parse()
            .map_err(|_| exit(2, format!("$MUSTER_SESSION_ID is not a session id: {id}")))?;
        return snap
            .visible()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| exit(2, format!("no such session: {id} ($MUSTER_SESSION_ID)")));
    }
    // All digits is always an id, never a name or prefix.
    if let Ok(id) = target.parse::<u32>() {
        return snap
            .visible()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| exit(2, format!("no such session: {id}")));
    }
    // `tab/name`, unless the whole string is exactly a session or tab name ("fix src/main.rs").
    let exact_whole = !exact_names(snap.visible(), target).is_empty()
        || snap.channels.iter().any(|c| eq_ci(&c.name, target));
    let found = match target.split_once('/') {
        Some((tab, name)) if !exact_whole => resolve_tab_name(snap, tab, name),
        _ => resolve_word(snap, target),
    };
    match found {
        Found::One(s) => Ok(s.clone()),
        Found::None(why) => Err(exit(2, why.unwrap_or_else(|| format!("no session matches \"{target}\"")))),
        Found::Many(c) => Err(exit(2, ambiguous(snap, target, &c))),
    }
}

enum Found<'a> {
    One(&'a Sess),
    /// Nothing matched; optionally why.
    None(Option<String>),
    Many(Vec<&'a Sess>),
}

fn one_or_many(mut v: Vec<&Sess>) -> Found<'_> {
    v.sort_by_key(|s| s.id);
    v.dedup_by_key(|s| s.id);
    match v.len() {
        0 => Found::None(None),
        1 => Found::One(v[0]),
        _ => Found::Many(v),
    }
}

/// Exact name matches: live ones win; exited ones count only when no live one matches.
fn exact_names<'a>(it: impl Iterator<Item = &'a Sess>, name: &str) -> Vec<&'a Sess> {
    let all: Vec<&Sess> = it.filter(|s| eq_ci(&s.name, name)).collect();
    let live: Vec<&Sess> = all.iter().copied().filter(|s| s.live()).collect();
    if live.is_empty() { all } else { live }
}

/// Tabs whose name matches exactly, else by prefix.
fn match_tabs<'a>(snap: &'a Snapshot, tab: &str) -> Vec<&'a Chan> {
    let exact: Vec<&Chan> = snap.channels.iter().filter(|c| eq_ci(&c.name, tab)).collect();
    if !exact.is_empty() {
        return exact;
    }
    snap.channels.iter().filter(|c| prefix_ci(&c.name, tab)).collect()
}

/// A tab's session: its only live agent, else its only live session.
fn tab_session<'a>(snap: &'a Snapshot, ch: &Chan) -> Found<'a> {
    let live: Vec<&Sess> = snap.sessions.iter().filter(|s| s.channel_id == ch.id && s.live()).collect();
    let agents: Vec<&Sess> = live.iter().copied().filter(|s| s.is_agent()).collect();
    if agents.len() == 1 {
        return Found::One(agents[0]);
    }
    if agents.len() > 1 {
        return Found::Many(agents);
    }
    match live.len() {
        0 => Found::None(Some(format!("tab \"{}\" has no live session", ch.name))),
        1 => Found::One(live[0]),
        _ => Found::Many(live),
    }
}

fn resolve_word<'a>(snap: &'a Snapshot, word: &str) -> Found<'a> {
    // 1. Exact session name.
    let exact = exact_names(snap.visible(), word);
    if !exact.is_empty() {
        return one_or_many(exact);
    }
    // 2. Exact tab name.
    let tabs: Vec<&Chan> = snap.channels.iter().filter(|c| eq_ci(&c.name, word)).collect();
    if tabs.len() == 1 {
        return tab_session(snap, tabs[0]);
    }
    if tabs.len() > 1 {
        return one_or_many(tabs.iter().flat_map(|c| tab_candidates(snap, c)).collect());
    }
    // 3. Unique prefix of a live session name or a tab name.
    let mut cands: Vec<&Sess> = snap.visible().filter(|s| s.live() && prefix_ci(&s.name, word)).collect();
    let mut tab_error = None;
    for c in snap.channels.iter().filter(|c| prefix_ci(&c.name, word)) {
        match tab_session(snap, c) {
            Found::One(s) => cands.push(s),
            Found::Many(v) => cands.extend(v),
            Found::None(why) => tab_error = why,
        }
    }
    match one_or_many(cands) {
        Found::None(_) => Found::None(tab_error),
        f => f,
    }
}

/// Candidates to list for an ambiguous tab: what `tab_session` would pick from.
fn tab_candidates<'a>(snap: &'a Snapshot, ch: &Chan) -> Vec<&'a Sess> {
    match tab_session(snap, ch) {
        Found::One(s) => vec![s],
        Found::Many(v) => v,
        Found::None(_) => Vec::new(),
    }
}

fn resolve_tab_name<'a>(snap: &'a Snapshot, tab: &str, name: &str) -> Found<'a> {
    let tabs = match_tabs(snap, tab);
    if tabs.is_empty() {
        return Found::None(Some(format!("no tab matches \"{tab}\"")));
    }
    let in_tabs = || snap.sessions.iter().filter(|s| tabs.iter().any(|c| c.id == s.channel_id));
    let exact = exact_names(in_tabs(), name);
    if !exact.is_empty() {
        return one_or_many(exact);
    }
    one_or_many(in_tabs().filter(|s| s.live() && prefix_ci(&s.name, name)).collect())
}

fn ambiguous(snap: &Snapshot, target: &str, cands: &[&Sess]) -> String {
    let rows: Vec<Vec<Cell>> = cands
        .iter()
        .map(|s| {
            vec![
                Cell::plain(s.id.to_string()),
                Cell::plain(snap.label(s)),
                Cell::plain(s.kind.clone()),
                Cell::plain(s.state.clone()),
            ]
        })
        .collect();
    let table = render_table(None, &rows, None, false);
    format!(
        "\"{target}\" is ambiguous ({} sessions); use an id or tab/name:\n{}",
        cands.len(),
        table.trim_end()
    )
}

/// Fetch the snapshot and resolve `target` in it.
fn target(d: &Discovery, target: &str) -> anyhow::Result<(Snapshot, Sess)> {
    let snap = Snapshot::fetch(d)?;
    let env = std::env::var("MUSTER_SESSION_ID").ok();
    let s = resolve(&snap, target, env.as_deref())?;
    Ok((snap, s))
}

// ----- output -----

pub struct Cell {
    text: String,
    /// SGR parameters, applied only when colour is on.
    color: Option<&'static str>,
}

impl Cell {
    fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            color: None,
        }
    }
}

/// Aligned columns separated by two spaces; the last column isn't padded. With `width`, every
/// line is cut to that many characters (ending in `…`). Colour codes wrap only the text.
pub fn render_table(header: Option<&[&str]>, rows: &[Vec<Cell>], width: Option<usize>, color: bool) -> String {
    let head: Option<Vec<Cell>> = header.map(|h| h.iter().map(|t| Cell::plain(*t)).collect());
    let all: Vec<&Vec<Cell>> = head.iter().chain(rows.iter()).collect();
    let ncol = all.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0; ncol];
    for r in &all {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.text.chars().count());
        }
    }
    let mut out = String::new();
    for r in &all {
        let mut line = String::new();
        let mut used = 0usize;
        for (i, c) in r.iter().enumerate() {
            let last = i + 1 == r.len();
            let mut text = c.text.clone();
            let mut pad = if last { 0 } else { widths[i] - text.chars().count() + 2 };
            if let Some(w) = width {
                let room = w.saturating_sub(used);
                let len = text.chars().count();
                if len > room || (len == room && !last) {
                    // Cut here and end the line.
                    text = if room == 0 {
                        String::new()
                    } else {
                        text.chars().take(room - 1).chain(std::iter::once('…')).collect()
                    };
                    pad = 0;
                } else {
                    pad = pad.min(room - len);
                }
            }
            used += text.chars().count() + pad;
            match c.color.filter(|_| color && !text.is_empty()) {
                Some(sgr) => line.push_str(&format!("\x1b[{sgr}m{text}\x1b[0m")),
                None => line.push_str(&text),
            }
            if width.is_some_and(|w| used >= w) {
                break;
            }
            line.extend(std::iter::repeat_n(' ', pad));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `45s`, `3m`, `1h20m`, `2d3h`.
pub fn format_duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => match (s / 60) % 60 {
            0 => format!("{}h", s / 3600),
            m => format!("{}h{m}m", s / 3600),
        },
        _ => match (s / 3600) % 24 {
            0 => format!("{}d", s / 86400),
            h => format!("{}d{h}h", s / 86400),
        },
    }
}

fn state_color(state: &str) -> Option<&'static str> {
    match state {
        "working" => Some("36"),
        "idle" => Some("32"),
        "needs_approval" | "asking_question" => Some("1;33"),
        "error" | "stuck" => Some("1;31"),
        "exited" => Some("2"),
        _ => None,
    }
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Terminal width when stdout is a terminal.
fn term_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: TIOCGWINSZ fills a winsize we own.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        return Some(ws.ws_col as usize);
    }
    std::env::var("COLUMNS").ok()?.trim().parse().ok()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `12  fix-auth/claude  idle  Bash: cargo test`
fn state_line(snap: &Snapshot, s: &Sess) -> String {
    let mut line = format!("{}  {}  {}", s.id, snap.label(s), s.state);
    if let Some(d) = s.state_detail.as_deref().filter(|d| !d.is_empty()) {
        line.push_str("  ");
        line.push_str(d);
    }
    line
}

/// `$MUSTER_SESSION_ID`: the session this command runs in. Sent as `from_session` by `send`,
/// `keys`, `ask` and `new`, so what an agent does through the CLI carries its provenance (and
/// for `send`/`ask` its MCP guards); see docs/CLI.md.
fn env_session() -> Option<u32> {
    std::env::var("MUSTER_SESSION_ID").ok().and_then(|v| v.trim().parse().ok())
}

fn print_json(v: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

// ----- commands -----

#[derive(clap::Subcommand)]
pub enum PanelCmd {
    /// Show an app beside this session: markdown, image, chart, table, pr_decision, pr_overview, board, web_preview or custom. JSON data is passed with --data.
    Open {
        #[arg(long)] title: String,
        #[arg(long, default_value = "markdown")] app: String,
        #[arg(long)] data: Option<String>,
        #[arg(long, conflicts_with = "html_path")] html: Option<String>,
        #[arg(long)] html_path: Option<String>,
        #[arg(long)] csp: Option<String>,
        /// Custom apps: put the Muster Kit (styles, components, fonts, handshake) in front of the HTML.
        #[arg(long)] kit: bool,
    },
    /// Update input, HTML or title of a panel in this task.
    Update {
        panel_id: i64,
        #[arg(long)] title: Option<String>,
        #[arg(long)] data: Option<String>,
        #[arg(long, conflicts_with = "html_path")] html: Option<String>,
        #[arg(long)] html_path: Option<String>,
        /// Custom apps: switch the Muster Kit on (true) or off (false); the app reloads.
        #[arg(long)] kit: Option<bool>,
    },
    /// Close a panel in this task.
    Close { panel_id: i64 },
    /// List the task's open panels.
    List,
    /// Consume queued user events, or wait for them (0–1800 seconds).
    Wait {
        panel_id: Option<i64>,
        #[arg(long, default_value_t = 300)] timeout_seconds: u64,
    },
    /// Fetch an event a panel prompt showed only as a placeholder (too long for the prompt).
    Event { panel_id: i64, event_id: i64 },
}

pub fn panel(d: &Discovery, cmd: PanelCmd) -> anyhow::Result<()> {
    let session = env_session().context("panel commands must run inside a Muster session (MUSTER_SESSION_ID)")?;
    let parse = |s: Option<String>| -> anyhow::Result<Value> {
        s.map(|s| serde_json::from_str(&s).context("invalid JSON argument")).transpose().map(|v| v.unwrap_or(Value::Null))
    };
    let (method, mut args, timeout) = match cmd {
        PanelCmd::Open { title, app, data, html, html_path, csp, kit } => ("panel.open",
            json!({"title":title,"app":app,"data":parse(data)?,"html":html,"html_path":html_path,"csp":parse(csp)?,"kit":kit.then_some(true)}), 30),
        PanelCmd::Update { panel_id, title, data, html, html_path, kit } => ("panel.update",
            json!({"panel_id":panel_id,"title":title,"data":parse(data)?,"html":html,"html_path":html_path,"kit":kit}), 30),
        PanelCmd::Close { panel_id } => ("panel.close", json!({"panel_id":panel_id}), 30),
        PanelCmd::List => ("panel.list", json!({}), 30),
        PanelCmd::Wait { panel_id, timeout_seconds } => {
            anyhow::ensure!(timeout_seconds <= 1800, "timeout_seconds must be at most 1800");
            ("panel.wait", json!({"panel_id":panel_id,"timeout_seconds":timeout_seconds}), timeout_seconds + 10)
        },
        PanelCmd::Event { panel_id, event_id } => ("panel.fetch_event", json!({"panel_id":panel_id,"event_id":event_id}), 30),
    };
    args.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    print_json(&client::rpc_with_timeout(d, method, json!({"from_session":session,"arguments":args}), Duration::from_secs(timeout))?)
}

/// `muster pair`: start a pairing, print how to reach the daemon from the phone (the
/// `tailscale serve` lines when it isn't set up), a QR code and the short code, then wait for
/// the phone.
pub fn pair(d: &Discovery, name: Option<&str>, scope: &str, ttl: u64, url: Option<&str>) -> anyhow::Result<()> {
    let scopes: &[&str] = match scope {
        "view" => &["view"],
        "manage" => &["view", "interact", "manage"],
        _ => &["view", "interact"],
    };
    let mut params = json!({ "ttl_s": ttl, "scopes": scopes });
    if let Some(n) = name {
        params["name"] = json!(n);
    }
    if let Some(u) = url {
        params["url"] = json!(u);
    }
    let p = client::rpc(d, "device.pair_code", params)?;
    let (link, short, id) = (
        p["url"].as_str().unwrap_or_default().to_string(),
        p["short_code"].as_str().unwrap_or_default().to_string(),
        p["pairing_id"].as_u64().unwrap_or_default(),
    );
    let port = p["port"].as_u64().unwrap_or(d.port as u64);
    if url.is_none() && !tailscale_serve_configured(port) {
        println!("Publish muster on your tailnet first (only these paths; never Funnel):\n");
        for path in ["/m", "/ws", "/api/device", "/api/rpc"] {
            println!("  tailscale serve --bg --https=443 --set-path={path} http://127.0.0.1:{port}{path}");
        }
        println!(
            "\n(`sudo tailscale set --operator=$USER` lets you run these without sudo; HTTPS certificates\n\
             must be enabled for the tailnet. The port changes if 7433 is busy: set MUSTER_PORT=7433.)\n"
        );
    }
    let qr = std::process::Command::new("qrencode")
        .args(["-t", "UTF8", "-m", "2", &link])
        .status();
    if !qr.is_ok_and(|s| s.success()) {
        println!("(install `qrencode` to get a QR code here)");
    }
    let pretty: String = short
        .chars()
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-");
    println!("\nScan it with the phone's camera, or open {} and type:  {pretty}", p["origin"].as_str().unwrap_or("").to_string() + "/m/");
    println!("Scope: {scope}. The code works once and expires in {ttl} s.\n");
    let deadline = Instant::now() + Duration::from_secs(ttl + 5);
    loop {
        std::thread::sleep(Duration::from_millis(700));
        let st = client::rpc(d, "device.pair_status", json!({ "pairing_id": id }))?;
        match st["state"].as_str().unwrap_or("") {
            "pending" if Instant::now() < deadline => continue,
            "paired" => {
                let dev = &st["device"];
                println!(
                    "paired: {} (device {}); `muster devices revoke {}` to undo",
                    dev["name"].as_str().unwrap_or("?"),
                    dev["id"],
                    dev["id"]
                );
                return Ok(());
            }
            "burnt" => anyhow::bail!("three wrong codes: this pairing is burnt; run `muster pair` again"),
            "replaced" => anyhow::bail!("another `muster pair` replaced this one"),
            _ => anyhow::bail!("the code expired; run `muster pair` again"),
        }
    }
}

/// Whether `tailscale serve` already proxies to this daemon's port (best effort).
fn tailscale_serve_configured(port: u64) -> bool {
    std::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("127.0.0.1:{port}")))
}

#[derive(Deserialize)]
struct DeviceRow {
    id: i64,
    name: String,
    scopes: Vec<String>,
    last_seen_at: i64,
    last_source: Option<String>,
    expires_at: i64,
    revoked_at: Option<i64>,
}

/// `muster devices`.
pub fn devices(d: &Discovery, as_json: bool) -> anyhow::Result<()> {
    let v = client::rpc(d, "device.list", json!({}))?;
    if as_json {
        return print_json(&v);
    }
    let list: Vec<DeviceRow> = serde_json::from_value(v)?;
    if list.is_empty() {
        println!("no paired devices (`muster pair` pairs a phone)");
        return Ok(());
    }
    let now = now_ms();
    let ago = |t: i64| {
        let s = ((now - t) / 1000).max(0);
        match s {
            0..60 => format!("{s}s ago"),
            60..3600 => format!("{}m ago", s / 60),
            3600..86400 => format!("{}h ago", s / 3600),
            _ => format!("{}d ago", s / 86400),
        }
    };
    let rows: Vec<Vec<Cell>> = list
        .iter()
        .map(|x| {
            let status = match x.revoked_at {
                Some(_) => "revoked".to_string(),
                None if x.expires_at <= now => "expired".to_string(),
                None => x.scopes.last().cloned().unwrap_or_default(),
            };
            vec![
                Cell::plain(x.id.to_string()),
                Cell::plain(x.name.clone()),
                Cell::plain(status),
                Cell::plain(format!(
                    "seen {}{}",
                    ago(x.last_seen_at),
                    x.last_source.as_deref().map(|s| format!(" from {s}")).unwrap_or_default()
                )),
            ]
        })
        .collect();
    print!("{}", render_table(Some(&["ID", "NAME", "ACCESS", "LAST"]), &rows, None, false));
    Ok(())
}

/// `muster devices revoke <id|name|--all>`.
pub fn revoke_device(d: &Discovery, target: Option<&str>, all: bool) -> anyhow::Result<()> {
    let params = if all {
        json!({ "all": true })
    } else {
        let t = target.ok_or_else(|| anyhow::anyhow!("name a device (id or name), or pass --all"))?;
        let list: Vec<DeviceRow> = serde_json::from_value(client::rpc(d, "device.list", json!({}))?)?;
        let live = |x: &&DeviceRow| x.revoked_at.is_none();
        let hits: Vec<&DeviceRow> = match t.parse::<i64>() {
            Ok(id) => list.iter().filter(|x| x.id == id).collect(),
            Err(_) => list.iter().filter(live).filter(|x| x.name == t).collect(),
        };
        match hits.as_slice() {
            [one] => json!({ "id": one.id }),
            [] => {
                return Err(Exit {
                    code: 2,
                    msg: format!("no device {t:?} (see `muster devices`)"),
                }
                .into());
            }
            _ => {
                return Err(Exit {
                    code: 2,
                    msg: format!("several devices are named {t:?}; use the id"),
                }
                .into());
            }
        }
    };
    let r = client::rpc(d, "device.revoke", params)?;
    let n = r["revoked"].as_array().map_or(0, |a| a.len());
    println!("revoked {n} device{}", if n == 1 { "" } else { "s" });
    Ok(())
}

/// `muster ls`. Never starts the daemon.
pub fn ls(d: Option<&Discovery>, all: bool, as_json: bool) -> anyhow::Result<()> {
    let Some(d) = d else {
        if as_json {
            println!("[]");
        } else {
            println!("muster daemon: not running");
        }
        return Ok(());
    };
    let snap = Snapshot::fetch(d)?;
    let order = |s: &Sess| {
        let pos = snap.channels.iter().position(|c| c.id == s.channel_id).unwrap_or(usize::MAX);
        let win = snap.window_of(s).unwrap_or(i64::MAX);
        (win, pos, s.id)
    };
    let mut list: Vec<&Sess> = snap.visible().filter(|s| all || s.live()).collect();
    list.sort_by_key(|s| order(s));
    if as_json {
        return print_json(&Value::Array(list.iter().map(|s| snap.session_json(s)).collect()));
    }
    if list.is_empty() {
        println!("no sessions{}", if all { "" } else { " (--all includes exited ones)" });
        return Ok(());
    }
    let now = now_ms();
    let rows: Vec<Vec<Cell>> = list
        .iter()
        .map(|s| {
            let ch = snap.channel(s.channel_id);
            let mut detail = s.state_detail.clone().unwrap_or_default();
            if s.queued > 0 {
                if !detail.is_empty() {
                    detail.push_str("; ");
                }
                detail.push_str(&format!("{} queued", s.queued));
            }
            vec![
                Cell::plain(s.id.to_string()),
                Cell::plain(if s.hidden { "hidden".into() } else { snap.window_of(s).map_or("-".into(), |w| w.to_string()) }),
                Cell::plain(clip(ch.map_or("?", |c| c.name.as_str()), 28)),
                Cell::plain(clip(&s.name, 20)),
                Cell::plain(s.kind.clone()),
                Cell {
                    text: s.state.clone(),
                    color: state_color(&s.state),
                },
                Cell::plain(format_duration(now - s.state_since)),
                Cell::plain(detail.replace(['\n', '\r'], " ")),
            ]
        })
        .collect();
    let header = ["ID", "WIN", "TAB", "NAME", "KIND", "STATE", "FOR", "DETAIL"];
    print!("{}", render_table(Some(&header), &rows, term_width(), use_color()));
    Ok(())
}

/// Cap a column value at `max` characters.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).chain(std::iter::once('…')).collect()
    }
}

/// `muster read`: `session.output` (or `session.screen`).
pub fn read(d: &Discovery, t: &str, lines: u32, screen: bool, as_json: bool) -> anyhow::Result<()> {
    let (_, s) = target(d, t)?;
    let r = if screen {
        client::rpc(d, "session.screen", json!({ "id": s.id }))?
    } else {
        client::rpc(d, "session.output", json!({ "id": s.id, "lines": lines.clamp(1, 10000) }))?
    };
    let mut out: Vec<String> = r["lines"]
        .as_array()
        .map(|a| a.iter().map(|l| l.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default();
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if as_json {
        return print_json(&json!({ "id": s.id, "lines": out }));
    }
    for l in out {
        println!("{}", l.trim_end());
    }
    Ok(())
}

/// The text to send: TEXT words joined by spaces; none or `-` reads `input` (stdin).
pub fn send_text(words: &[String], input: impl FnOnce() -> std::io::Result<String>) -> anyhow::Result<String> {
    let text = if words.is_empty() || (words.len() == 1 && words[0] == "-") {
        let mut t = input().context("reading stdin")?;
        // `echo hi | muster send x` should send `hi`, not `hi` plus an empty line.
        while t.ends_with('\n') || t.ends_with('\r') {
            t.pop();
        }
        t
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send");
    }
    Ok(text)
}

pub struct SendOpts {
    pub interrupt: bool,
    pub wait: bool,
    pub until: Vec<String>,
    pub timeout: u64,
}

/// `muster send`: `session.send`, then with `--wait` `session.wait` until the agent has
/// picked the prompt up and settled again.
pub fn send(d: &Discovery, t: &str, words: &[String], o: SendOpts) -> anyhow::Result<()> {
    let text = send_text(words, || {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            eprintln!("muster: reading the prompt from stdin (end with Ctrl-D)");
        }
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        Ok(s)
    })?;
    let (snap, s) = target(d, t)?;
    let deadline = deadline_in(o.timeout);
    let sent_at = now_ms();
    let mode = if o.interrupt { "interrupt" } else { "queue" };
    let r = client::rpc(
        d,
        "session.send",
        json!({ "id": s.id, "text": text, "mode": mode, "from_session": env_session() }),
    )?;
    let queued = r["queued"].as_u64().unwrap_or(0);
    let wait = o.wait || !o.until.is_empty();
    if !wait {
        if r["delivered"] == true {
            println!("{}: sent", snap.label(&s));
        } else {
            println!("{}: queued ({queued} waiting)", snap.label(&s));
        }
        return Ok(());
    }
    // Shells go busy only while a foreground job runs, sampled every 250 ms: a quick command
    // (`echo`) never shows as working, so don't wait long for it to.
    let grace = if s.is_agent() { Duration::from_secs(10) } else { Duration::from_millis(1500) };
    if !wait_started(d, s.id, sent_at, queued > 0 || r["delivered"] != true, grace, deadline)? {
        // Timed out before the prompt even started: report the state as it is now.
        let r = session_wait(d, s.id, &[], Duration::ZERO)?;
        return finish_wait(&snap, &json!({ "session": r["session"], "matched": false }), o.timeout);
    }
    let until = if o.until.is_empty() { SETTLED.iter().map(|s| s.to_string()).collect() } else { o.until };
    let r = wait_until(d, s.id, &until, deadline)?;
    finish_wait(&snap, &r, o.timeout)
}

/// After a send: wait until the prompt left the queue and the session reacted. Delivered
/// at once: any state change after `sent_at` counts (working, or already back to idle for a
/// quick answer). Queued behind earlier work: the earlier work ending (idle) also changes the
/// state, and the queue empties before our prompt is typed, so only a non-idle state that
/// began after the queue was last seen non-empty counts. A session that never visibly reacts
/// is let through after `grace`, so the settled wait that follows can't hang on it.
/// `false`: the deadline passed first.
fn wait_started(
    d: &Discovery,
    id: u32,
    sent_at: i64,
    queued: bool,
    grace: Duration,
    deadline: Instant,
) -> anyhow::Result<bool> {
    let mut delivered_at: Option<Instant> = None;
    // Unix ms when the queue was last seen non-empty.
    let mut last_queued = sent_at;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        // Short polls, so the grace period is measured closely.
        let chunk = left.min(Duration::from_secs(1)).min(grace / 3);
        let asked_at = now_ms();
        let asked = Instant::now();
        let r = session_wait(d, id, &["working", "exited"], chunk)?;
        let Some(s) = Sess::from_value(r["session"].clone()) else {
            anyhow::bail!("unexpected session.wait reply: {r}");
        };
        if s.state == "exited" {
            return Ok(true);
        }
        if s.queued > 0 {
            last_queued = asked_at;
            delivered_at = None;
        } else {
            let reacted = if queued {
                s.state != "idle" && s.state_since > last_queued
            } else {
                s.state_since >= sent_at
            };
            if reacted {
                return Ok(true);
            }
            let at = *delivered_at.get_or_insert(asked);
            if at.elapsed() >= grace {
                return Ok(true);
            }
        }
        if r["matched"] == true {
            // Still busy with earlier work (or our prompt is queued behind it): poll gently.
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// One `session.wait` call; the HTTP timeout covers the whole wait.
fn session_wait(d: &Discovery, id: u32, until: &[&str], timeout: Duration) -> anyhow::Result<Value> {
    client::rpc_with_timeout(
        d,
        "session.wait",
        json!({ "id": id, "until": until, "timeout_ms": timeout.as_millis() as u64 }),
        timeout + Duration::from_secs(15),
    )
}

/// `session.wait` until `until` or `deadline`, in chunks the daemon accepts. Returns the last
/// reply (`{session, matched, timed_out}`). Stops early when the daemon says the wait can
/// never match (`timed_out: false` without a match: the session exited).
fn wait_until(d: &Discovery, id: u32, until: &[String], deadline: Instant) -> anyhow::Result<Value> {
    let until: Vec<&str> = until.iter().map(String::as_str).collect();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let r = session_wait(d, id, &until, left.min(MAX_WAIT_CHUNK))?;
        if r["matched"] == true || never_matches(&r) || left <= MAX_WAIT_CHUNK {
            return Ok(r);
        }
    }
}

/// The daemon ended the wait without a match and without timing out: the session exited.
fn never_matches(r: &Value) -> bool {
    r["matched"] != true && r["timed_out"] == false
}

/// The error for an unmatched wait: exit 1 when the session exited first, 124 on timeout.
fn unmatched(r: &Value, s: &Sess, timeout: u64) -> anyhow::Error {
    if never_matches(r) {
        exit(1, format!("session {} exited before reaching the state", s.id))
    } else {
        exit(124, format!("timed out after {timeout}s (session {} is {})", s.id, s.state))
    }
}

/// Print the final state line; exit 124 when the wait timed out, 1 when the session exited.
fn finish_wait(snap: &Snapshot, r: &Value, timeout: u64) -> anyhow::Result<()> {
    let s = Sess::from_value(r["session"].clone()).with_context(|| format!("unexpected session.wait reply: {r}"))?;
    println!("{}", state_line(snap, &s));
    if r["matched"] != true {
        return Err(unmatched(r, &s, timeout));
    }
    Ok(())
}

/// `muster wait`.
pub fn wait(d: &Discovery, t: &str, until: Vec<String>, timeout: u64, as_json: bool) -> anyhow::Result<()> {
    let (snap, s) = target(d, t)?;
    let until = if until.is_empty() { SETTLED.iter().map(|s| s.to_string()).collect() } else { until };
    let r = wait_until(d, s.id, &until, deadline_in(timeout))?;
    if as_json {
        print_json(&r)?;
        if r["matched"] != true {
            let s = Sess::from_value(r["session"].clone()).with_context(|| format!("unexpected session.wait reply: {r}"))?;
            return Err(unmatched(&r, &s, timeout));
        }
        return Ok(());
    }
    finish_wait(&snap, &r, timeout)
}

/// `--timeout` from now, capped at 30 days (a huge value would overflow `Instant`).
fn deadline_in(secs: u64) -> Instant {
    Instant::now() + Duration::from_secs(secs.min(30 * 86400))
}

/// Bytes for one `keys` argument (see docs/CLI.md).
pub fn parse_key(k: &str) -> anyhow::Result<String> {
    if let Some(text) = k.strip_prefix("text:") {
        return Ok(text.to_string());
    }
    let lower = k.to_ascii_lowercase();
    if let Some(seq) = modified_cursor_key(&lower) {
        return Ok(seq);
    }
    if let Some(named) = named_key(&lower) {
        return Ok(named.to_string());
    }
    if let Some(rest) = lower.strip_prefix("ctrl+").or_else(|| lower.strip_prefix("c-")) {
        let mut chars = rest.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            if rest == "space" {
                return Ok("\0".into());
            }
            anyhow::bail!("unknown key: {k} (ctrl+ takes one character, e.g. ctrl+c)");
        };
        let b = match c {
            'a'..='z' => c as u8 - b'a' + 1,
            '@' | ' ' | '2' => 0,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '7' | '/' => 0x1f,
            '?' | '8' => 0x7f,
            _ => anyhow::bail!("unknown key: {k}"),
        };
        return Ok((b as char).to_string());
    }
    if let Some(rest) = k.strip_prefix("alt+").or_else(|| k.strip_prefix("Alt+")).or_else(|| k.strip_prefix("ALT+")) {
        let inner = match named_key(&rest.to_ascii_lowercase()) {
            Some(n) => n.to_string(),
            None if rest.chars().count() == 1 => rest.to_string(),
            None => anyhow::bail!("unknown key: {k} (alt+ takes one character or a key name)"),
        };
        return Ok(format!("\x1b{inner}"));
    }
    // A single character stands for itself (`muster keys 12 y enter`).
    if k.chars().count() == 1 {
        return Ok(k.to_string());
    }
    anyhow::bail!("unknown key: {k} (use text:{k} to type it literally)")
}

/// `ctrl+left`, `alt+up`, `ctrl+shift+end`...: xterm's `CSI 1 ; <mod> <final>`.
fn modified_cursor_key(k: &str) -> Option<String> {
    let (mods, base) = k.rsplit_once('+')?;
    let fin = match base {
        "up" => 'A',
        "down" => 'B',
        "right" => 'C',
        "left" => 'D',
        "home" => 'H',
        "end" => 'F',
        _ => return None,
    };
    let mut m = 1;
    for part in mods.split('+') {
        m += match part {
            "shift" => 1,
            "alt" => 2,
            "ctrl" => 4,
            _ => return None,
        };
    }
    Some(format!("\x1b[1;{m}{fin}"))
}

fn named_key(k: &str) -> Option<&'static str> {
    Some(match k {
        "enter" | "return" => "\r",
        "esc" | "escape" => "\x1b",
        "tab" => "\t",
        "shift+tab" | "backtab" => "\x1b[Z",
        "space" => " ",
        "backspace" | "bs" => "\x7f",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "delete" | "del" => "\x1b[3~",
        "pageup" | "pgup" => "\x1b[5~",
        "pagedown" | "pgdn" => "\x1b[6~",
        _ => return None,
    })
}

/// `muster keys`: `session.input` with the concatenated bytes.
pub fn keys(d: &Discovery, t: &str, keys: &[String]) -> anyhow::Result<()> {
    let data = keys.iter().map(|k| parse_key(k)).collect::<anyhow::Result<String>>()?;
    let (_, s) = target(d, t)?;
    // `keys`: cursor keys follow the terminal's cursor mode, as a keyboard's would.
    client::rpc(
        d,
        "session.input",
        json!({ "id": s.id, "data": data, "keys": true, "from_session": env_session() }),
    )?;
    Ok(())
}

/// `muster approve`.
pub fn approve(d: &Discovery, t: &str, always: bool, deny: bool) -> anyhow::Result<()> {
    let (_, s) = target(d, t)?;
    let decision = if deny {
        "deny"
    } else if always {
        "approve_always"
    } else {
        "approve"
    };
    client::rpc(d, "session.approve", json!({ "id": s.id, "decision": decision }))?;
    Ok(())
}

/// Inspect delivery recovery without implicitly replaying uncertain input.
pub fn queue(d: &Discovery, t: &str, clear: bool, resume: bool) -> anyhow::Result<()> {
    let (_, session) = target(d, t)?;
    if clear { client::rpc(d, "session.clear_queue", json!({"id": session.id}))?; }
    if resume { client::rpc(d, "session.resume_queue", json!({"id": session.id}))?; }
    print_json(&client::rpc(d, "session.queue", json!({"id": session.id}))?)
}

/// `muster interrupt` / `kill` / `restart`: a method that takes just the session id.
pub fn by_id(d: &Discovery, t: &str, method: &str) -> anyhow::Result<()> {
    let (_, s) = target(d, t)?;
    client::rpc(d, method, json!({ "id": s.id }))?;
    Ok(())
}

/// `muster move <target> --window N`: show the session (tab) in window N (`session.set_window`,
/// guarded against a concurrent move). Prints `id  tab/name: window A -> N`.
pub fn move_to_window(d: &Discovery, t: &str, window: i64) -> anyhow::Result<()> {
    let (snap, s) = target(d, t)?;
    let from = snap.window_of(&s);
    let mut p = json!({ "id": s.id, "window_id": window });
    if let Some(f) = from {
        p["from_window_id"] = json!(f);
    }
    client::rpc(d, "session.set_window", p)?;
    let from = from.map_or("-".into(), |w| w.to_string());
    println!("{}  {}: window {from} -> {window}", s.id, snap.label(&s));
    Ok(())
}

/// `muster rename`: the session, or with `tab` its tab.
pub fn rename(d: &Discovery, t: &str, name: &str, tab: bool) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("the new name is empty");
    }
    let (_, s) = target(d, t)?;
    if tab {
        client::rpc(d, "channel.rename", json!({ "id": s.channel_id, "name": name }))?;
    } else {
        client::rpc(d, "session.rename", json!({ "id": s.id, "name": name }))?;
    }
    Ok(())
}

pub struct NewOpts {
    pub profile: Option<String>,
    pub path: std::path::PathBuf,
    pub kind: String,
    pub worktree: bool,
    pub base: Option<String>,
    pub name: Option<String>,
    pub window: Option<i64>,
    pub prompt: Option<String>,
    pub json: bool,
    /// Arguments after `--`, for the agent CLI.
    pub agent_args: Vec<String>,
}

/// `muster new`: `workspace.add`, `channel.create`, then `hello` for the new session.
pub fn new(d: &Discovery, o: NewOpts) -> anyhow::Result<()> {
    anyhow::ensure!(o.profile.is_none() || o.kind == "claude", "--profile needs --kind claude");
    let profile = o.profile.as_deref().map(|p| profile_choice(d, p)).transpose()?;
    let abs = std::fs::canonicalize(&o.path).with_context(|| format!("{}", o.path.display()))?;
    if o.kind == "none" && o.prompt.is_some() {
        anyhow::bail!("--prompt needs an agent (not --kind none)");
    }
    if o.kind == "none" && !o.agent_args.is_empty() {
        anyhow::bail!("arguments after `--` need an agent (not --kind none)");
    }
    let ws = client::rpc(d, "workspace.add", json!({ "path": abs }))?;
    let ws_id = ws["id"].as_i64().context("workspace.add returned no id")?;
    // Inside a muster session, the new tab goes to that session's window (the daemon puts the
    // task's first session in the task's window). Without the session, MUSTER_CHANNEL_ID's task
    // window (stale once the session moved to another task, so only a fallback).
    let env_id = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<i64>().ok());
    let window = match o.window {
        Some(w) => Some(w),
        None => match (env_id("MUSTER_SESSION_ID"), env_id("MUSTER_CHANNEL_ID")) {
            (None, None) => None,
            (sid, ch) => {
                let snap = Snapshot::fetch(d)?;
                match sid.and_then(|sid| snap.sessions.iter().find(|s| i64::from(s.id) == sid)) {
                    Some(s) => snap.window_of(s),
                    None => ch.and_then(|ch| snap.channel(ch)).and_then(|c| c.window_id),
                }
            }
        },
    };
    let mut p = json!({ "workspace_id": ws_id, "worktree": o.worktree });
    if let Some(profile) = profile { p["account_profile"] = profile; }
    let set = |p: &mut Value, k: &str, v: Value| {
        p.as_object_mut().expect("object").insert(k.into(), v);
    };
    if let Some(n) = o.name.filter(|n| !n.trim().is_empty()) {
        set(&mut p, "name", json!(n));
    }
    if let Some(b) = o.base {
        set(&mut p, "base", json!(b));
    }
    if o.kind != "none" {
        set(&mut p, "agent", json!(o.kind));
    }
    if let Some(pr) = o.prompt {
        set(&mut p, "prompt", json!(pr));
    }
    if let Some(w) = window {
        set(&mut p, "window_id", json!(w));
    }
    if !o.agent_args.is_empty() {
        set(&mut p, "extra_args", json!(o.agent_args));
    }
    if let Some(s) = env_session() {
        set(&mut p, "from_session", json!(s));
    }
    let ch = client::rpc(d, "channel.create", p)?;
    let ch_id = ch["id"].as_i64().context("channel.create returned no id")?;
    let snap = Snapshot::fetch(d)?;
    let sess = snap.sessions.iter().filter(|s| s.channel_id == ch_id).min_by_key(|s| s.id);
    if o.json {
        print_json(&json!({ "channel": ch, "session": sess.map(|s| snap.session_json(s)) }))?;
    } else if let Some(s) = sess {
        println!("{}", s.id);
    } else if o.kind == "none" {
        eprintln!(
            "muster: created tab {ch_id} ({}) with no session; no window shows it until one starts in it",
            ch["name"].as_str().unwrap_or("?")
        );
    }
    if o.kind != "none" && sess.is_none() {
        anyhow::bail!("tab {ch_id} was created but {} did not start (see the tab's timeline)", o.kind);
    }
    Ok(())
}

// ----- events -----

/// Which events `muster events` passes on: one session and/or one tab (with both, a session
/// event must match both).
#[derive(Debug, Default, Clone, Copy)]
pub struct EventFilter {
    pub session: Option<u32>,
    pub channel: Option<i64>,
}

/// What `muster events` knows about sessions and tabs, kept current from the events, so a
/// `session_upserted` can be printed as a state change (`from -> to`) with its tab name.
#[derive(Debug, Default)]
pub struct EventTracker {
    filter: EventFilter,
    sessions: std::collections::HashMap<u32, Sess>,
    tabs: std::collections::HashMap<i64, String>,
    /// Last known tab of the `--session` session: its tab and window can close after the
    /// session itself was removed (archiving removes sessions first).
    session_tab: Option<i64>,
    /// Last known window of the `--session` session (kept after it is removed, like its tab).
    session_win: Option<i64>,
    /// Tabs of each window, from `window_upserted`, so a `window_removed` can be matched.
    windows: std::collections::HashMap<i64, Vec<i64>>,
}

/// One event, as far as `muster events` is concerned.
#[derive(Debug, PartialEq)]
pub struct Seen {
    /// Passes the filter (`--json` prints it).
    pub pass: bool,
    /// A session state change that passes: (unix ms, `id  tab/name  from -> to  detail`).
    pub change: Option<(i64, String)>,
}

impl EventTracker {
    pub fn new(snap: &Snapshot, filter: EventFilter) -> EventTracker {
        let mut t = EventTracker {
            filter,
            ..Default::default()
        };
        t.resync(snap);
        t
    }

    /// Replace what is known with a fresh snapshot (after missed events).
    pub fn resync(&mut self, snap: &Snapshot) {
        self.sessions = snap.sessions.iter().map(|s| (s.id, s.clone())).collect();
        self.note_session_tab();
        // Rebuilt from the snapshot: a tab may have changed windows while events were missed.
        self.windows.clear();
        for c in &snap.channels {
            if let Some(w) = c.window_id {
                let tabs = self.windows.entry(w).or_default();
                if !tabs.contains(&c.id) {
                    tabs.push(c.id);
                }
            }
        }
        // Names of tabs closed earlier stay, for labels of their sessions.
        self.tabs.extend(snap.channels.iter().map(|c| (c.id, c.name.clone())));
    }

    fn tab_ok(&self, ch: i64) -> bool {
        self.filter.channel.is_none_or(|c| c == ch)
    }

    /// The tab a tab or window event must concern to pass: the `--tab` one, else the
    /// `--session` session's.
    fn focus_tab(&self) -> Option<i64> {
        self.filter.channel.or(self.session_tab)
    }

    fn note_session_tab(&mut self) {
        if let Some(s) = self.filter.session.and_then(|id| self.sessions.get(&id)) {
            self.session_tab = Some(s.channel_id);
            if s.window_id.is_some() {
                self.session_win = s.window_id;
            }
        }
    }

    /// The `--session` session's own window (a tab can be in another window than its task).
    fn session_window(&self) -> Option<i64> {
        self.session_win
    }

    fn unfiltered(&self) -> bool {
        self.filter.session.is_none() && self.filter.channel.is_none()
    }

    fn label(&self, s: &Sess) -> String {
        format!("{}/{}", self.tabs.get(&s.channel_id).map_or("?", String::as_str), s.name)
    }

    /// Take one event (`{"event", "data"}`) into account.
    pub fn apply(&mut self, ev: &Value) -> Seen {
        let data = &ev["data"];
        let id = data["id"].as_i64();
        let mut change = None;
        let pass = match ev["event"].as_str().unwrap_or("") {
            "session_upserted" => {
                let Some(s) = Sess::from_value(data.clone()) else {
                    return Seen { pass: false, change };
                };
                let prev = self.sessions.get(&s.id);
                // Queued before the snapshot we already have (or a resync): nothing new, and
                // no transition that never happened.
                if prev.is_some_and(|p| s.state_since < p.state_since) {
                    return Seen { pass: false, change };
                }
                // A session moved out of (or into) the tab counts for that tab.
                let pass = self.filter.session.is_none_or(|id| id == s.id)
                    && (self.tab_ok(s.channel_id) || prev.is_some_and(|p| self.tab_ok(p.channel_id)));
                let from = prev.map(|p| p.state.clone());
                if from.as_deref() != Some(s.state.as_str()) {
                    let mut line = format!(
                        "{}  {}  {} -> {}",
                        s.id,
                        self.label(&s),
                        from.as_deref().unwrap_or("new"),
                        s.state
                    );
                    if let Some(d) = s.state_detail.as_deref().filter(|d| !d.is_empty()) {
                        line.push_str("  ");
                        line.push_str(d);
                    }
                    let at = if s.state_since > 0 { s.state_since } else { now_ms() };
                    change = Some((at, line));
                }
                self.sessions.insert(s.id, s);
                self.note_session_tab();
                pass
            }
            "session_removed" => {
                let sid = id.and_then(|i| u32::try_from(i).ok()).unwrap_or(0);
                let pass = self.filter.session.is_none_or(|id| id == sid)
                    && self.sessions.get(&sid).is_none_or(|s| self.tab_ok(s.channel_id));
                if let Some(s) = self.sessions.remove(&sid) {
                    change = Some((now_ms(), format!("{}  {}  {} -> removed", s.id, self.label(&s), s.state)));
                }
                pass
            }
            "channel_upserted" => {
                if let (Some(id), Some(name)) = (id, data["name"].as_str()) {
                    self.tabs.insert(id, name.to_string());
                }
                self.unfiltered() || self.focus_tab().is_some_and(|c| Some(c) == id)
            }
            "channel_removed" => self.unfiltered() || self.focus_tab().is_some_and(|c| Some(c) == id),
            "window_upserted" => {
                let tabs: Vec<i64> = data["channel_ids"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
                    .unwrap_or_default();
                let pass = self.unfiltered()
                    || self.focus_tab().is_some_and(|c| tabs.contains(&c))
                    || self.session_window().is_some_and(|w| Some(w) == id);
                // A window whose last tab just closed keeps its remembered tabs, so its coming
                // `window_removed` still matches the filter.
                if let Some(id) = id
                    && !tabs.is_empty()
                {
                    self.windows.insert(id, tabs);
                }
                pass
            }
            "window_removed" => {
                let tabs = id.and_then(|id| self.windows.remove(&id)).unwrap_or_default();
                self.unfiltered()
                    || self.focus_tab().is_some_and(|c| tabs.contains(&c))
                    || self.session_window().is_some_and(|w| Some(w) == id)
            }
            // Named agents: their record (by its session or tab) and messages to them (by the
            // receiving session). A message line shows in the human stream too.
            "agent_upserted" | "agent_removed" => {
                self.unfiltered()
                    || data["session_id"].as_u64().is_some_and(|s| self.filter.session == Some(s as u32))
                    || data["channel_id"].as_i64().is_some_and(|c| self.focus_tab() == Some(c))
            }
            "agent_message" => {
                let to = data["to_session"].as_u64().map(|s| s as u32);
                let pass = self.unfiltered()
                    || (to.is_some() && to == self.filter.session)
                    || to
                        .and_then(|s| self.sessions.get(&s))
                        .is_some_and(|s| self.filter.channel == Some(s.channel_id));
                change = Some((now_ms(), agent_message_line(data)));
                pass
            }
            "lagged" => true,
            _ => self.unfiltered(),
        };
        let change = change.filter(|_| pass);
        Seen { pass, change }
    }
}

/// `msg 12  human -> ada  origin=human hop=0  first line` (with `refused: why` or `replied`).
pub fn agent_message_line(m: &Value) -> String {
    let first = m["text"].as_str().and_then(|t| t.lines().find(|l| !l.trim().is_empty())).unwrap_or("");
    let first: String = first.chars().take(80).collect();
    let status = match (m["refused"].as_str(), m["reply"].is_string()) {
        (Some(why), _) => format!("  refused: {why}"),
        (None, true) => "  replied".to_string(),
        _ => String::new(),
    };
    format!(
        "msg {}  {} -> {}  origin={} hop={}  {first}{status}",
        m["id"],
        m["from"].as_str().unwrap_or("?"),
        m["to"].as_str().unwrap_or("?"),
        m["origin"].as_str().unwrap_or("?"),
        m["hop"],
    )
}

/// `HH:MM:SS` in local time.
fn clock(ms: i64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills a tm we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "--:--:--".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Resolve `--tab NAME` (exact name, else unique prefix). Errors carry exit code 2.
pub fn resolve_tab(snap: &Snapshot, tab: &str) -> anyhow::Result<i64> {
    match match_tabs(snap, tab.trim()).as_slice() {
        [c] => Ok(c.id),
        [] => Err(exit(2, format!("no tab matches \"{tab}\""))),
        many => Err(exit(
            2,
            format!(
                "\"{tab}\" is ambiguous ({} tabs): {}",
                many.len(),
                many.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// `muster events`: stream `GET /api/events`. Human lines per session state change, or with
/// `--json` every event line as the daemon sent it. Ends (Ok) when the daemon goes away or
/// stdout is closed.
pub fn events(d: &Discovery, as_json: bool, session: Option<&str>, tab: Option<&str>) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    // Subscribed first, so nothing between the snapshot and the stream is lost.
    let body = client::get_stream(d.port, "/api/events", &d.token)?;
    let snap = Snapshot::fetch(d)?;
    let mut filter = EventFilter::default();
    if let Some(t) = session {
        let env = std::env::var("MUSTER_SESSION_ID").ok();
        filter.session = Some(resolve(&snap, t, env.as_deref())?.id);
    }
    if let Some(t) = tab {
        filter.channel = Some(resolve_tab(&snap, t)?);
    }
    let mut tracker = EventTracker::new(&snap, filter);
    let mut out = std::io::stdout().lock();
    for line in std::io::BufReader::new(body).lines() {
        let Ok(line) = line else { break };
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let seen = tracker.apply(&ev);
        if ev["event"] == "lagged" {
            eprintln!("muster: missed {} events; re-synced", ev["data"]["missed"]);
            if let Ok(snap) = Snapshot::fetch(d) {
                tracker.resync(&snap);
            }
        }
        let text = if as_json {
            seen.pass.then_some(line)
        } else {
            seen.change.map(|(at, l)| format!("{}  {l}", clock(at)))
        };
        if let Some(text) = text
            && writeln!(out, "{text}").and_then(|_| out.flush()).is_err()
        {
            // The reader went away (`| head`).
            return Ok(());
        }
    }
    eprintln!("muster: the daemon stopped");
    Ok(())
}

// ----- agents (PLAN.md §10) -----

/// `muster agent new` / `edit` options.
fn profile_records(d: &Discovery) -> anyhow::Result<Vec<crate::account_profiles::AccountProfile>> {
    Ok(serde_json::from_value(client::rpc(d, "account_profile.list", json!({}))?)?)
}

fn profile_choice(d: &Discovery, input: &str) -> anyhow::Result<Value> {
    if input.eq_ignore_ascii_case("default") { return Ok(json!("default")); }
    let profile = profile_records(d)?.into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(input) || p.id.to_string() == input)
        .with_context(|| format!("no Claude profile named {input}; see `muster profile ls`"))?;
    Ok(json!(profile.id))
}

pub fn profile_list(d: &Discovery, json_output: bool) -> anyhow::Result<()> {
    let profiles = profile_records(d)?;
    if json_output { println!("{}", serde_json::to_string_pretty(&profiles)?); }
    else if profiles.is_empty() { println!("No Claude profiles registered. Set up a directory in Claude Code, then use `muster profile add NAME DIRECTORY`."); }
    else {
        for p in profiles { println!("{}\t{}\t{}\t{}", p.id, p.name, if p.enabled { "enabled" } else { "disabled" }, p.config_dir); }
    }
    Ok(())
}

pub fn profile_add(d: &Discovery, name: &str, directory: &std::path::Path) -> anyhow::Result<()> {
    // Only resolve the directory itself; its contents are Claude Code's responsibility.
    let directory = crate::paths::expand_tilde(&directory.to_string_lossy());
    let directory = crate::account_profiles::config_directory(&if directory.is_absolute() {
        directory.to_string_lossy().into_owned()
    } else { std::env::current_dir()?.join(directory).to_string_lossy().into_owned() })?;
    let p = client::rpc(d, "account_profile.create", json!({"name": name, "config_dir": directory}))?;
    println!("Registered {} ({}). Sign in and verify the account inside Claude Code; Muster has not checked or changed authentication.\nRun `muster skills install` to give this profile's sessions Muster's agent skills.", p["name"].as_str().unwrap_or(name), p["id"]);
    // Muster's daemon decides with its own HOME; this shell's HOME is usually the same.
    let shell_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    if crate::account_profiles::launch_home(&shell_env)
        .is_some_and(|home| crate::account_profiles::is_default_folder(std::path::Path::new(&directory), &home))
    {
        println!("This is Claude Code's default folder (~/.claude for this shell's HOME). When Muster runs with the same HOME, its sessions start without CLAUDE_CONFIG_DIR, so Claude Code uses ~/.claude and ~/.claude.json. Sign in to it with plain `claude` (or `env -u CLAUDE_CONFIG_DIR claude`), never `CLAUDE_CONFIG_DIR=~/.claude claude`.");
    }
    Ok(())
}

pub fn profile_update(d: &Discovery, profile: &str, name: Option<String>, enabled: Option<bool>) -> anyhow::Result<()> {
    let id = profile_choice(d, profile)?.as_i64().context("Default is inherited behavior, not an editable profile")?;
    let p = client::rpc(d, "account_profile.update", json!({"id": id, "name": name, "enabled": enabled}))?;
    println!("{}: {}. Running sessions are unchanged.", p["name"].as_str().unwrap_or(profile), if p["enabled"] == true { "enabled" } else { "disabled" });
    Ok(())
}

pub fn profile_default(d: &Discovery, path: &std::path::Path, profile: &str) -> anyhow::Result<()> {
    let choice = profile_choice(d, profile)?;
    let path = std::fs::canonicalize(path)?;
    let ws = client::rpc(d, "workspace.add", json!({"path": path}))?;
    client::rpc(d, "workspace.set_account_profile", json!({"id": ws["id"], "account_profile": choice}))?;
    println!("New Claude sessions in {} will default to {profile}. Existing sessions retain their selection.", path.display());
    Ok(())
}

pub struct AgentOpts {
    pub profile: Option<String>,
    pub name: String,
    pub kind: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub persona: Option<String>,
    pub accepts: Option<String>,
    pub autostart: Option<bool>,
    pub claude_mode: Option<String>,
    pub codex_sandbox: Option<String>,
}

impl AgentOpts {
    /// The request fields both RPCs share (absent ones left out). `base` is the profile a
    /// half-given `--claude-mode` / `--codex-sandbox` completes (both halves are required).
    fn params(&self, base: &Value) -> anyhow::Result<Value> {
        let mut p = json!({ "name": self.name });
        let mut set = |k: &str, v: Value| {
            p[k] = v;
        };
        if let Some(k) = &self.kind {
            set("kind", json!(k));
        }
        if let Some(m) = &self.model {
            set("model", json!(m));
        }
        if let Some(e) = &self.effort {
            set("effort", json!(e));
        }
        if let Some(t) = &self.persona {
            set("persona", json!(t));
        }
        if let Some(a) = &self.accepts {
            set("accepts", json!(crate::agents::parse_accepts(a)?));
        }
        if let Some(a) = self.autostart {
            set("autostart", json!(a));
        }
        if self.claude_mode.is_some() || self.codex_sandbox.is_some() {
            set(
                "permissions",
                json!({
                    "claude_mode": self.claude_mode.as_deref().or(base["claude_mode"].as_str()).unwrap_or("default"),
                    "codex_sandbox": self.codex_sandbox.as_deref().or(base["codex_sandbox"].as_str()).unwrap_or("workspace_write"),
                }),
            );
        }
        Ok(p)
    }
}

fn print_agent(verb: &str, b: &Value, name: &str) {
    println!(
        "{verb} {} ({}, accepts {}{}): home {}",
        b["name"].as_str().unwrap_or(name),
        b["kind"].as_str().unwrap_or("?"),
        b["accepts_origins"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","))
            .unwrap_or_default(),
        if b["autostart"] == true { ", autostart" } else { "" },
        b["home_dir"].as_str().unwrap_or("?")
    );
}

/// `muster agent new`.
pub fn agent_new(d: &Discovery, o: AgentOpts) -> anyhow::Result<()> {
    anyhow::ensure!(o.profile.is_none() || o.kind.as_deref().unwrap_or("claude") == "claude", "--profile needs --kind claude");
    let mut params = o.params(&Value::Null)?;
    if let Some(profile) = &o.profile { params["account_profile"] = profile_choice(d, profile)?; }
    let b = client::rpc(d, "agent.create", params)?;
    print_agent("agent", &b, &o.name);
    Ok(())
}

/// `muster agent edit`.
pub fn agent_edit(d: &Discovery, o: AgentOpts) -> anyhow::Result<()> {
    let base = if o.claude_mode.is_some() != o.codex_sandbox.is_some() {
        let list = client::rpc(d, "agent.list", json!({}))?;
        list.as_array()
            .and_then(|l| l.iter().find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&o.name))))
            .map(|a| a["permissions"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let b = client::rpc(d, "agent.update", o.params(&base)?)?;
    print_agent("updated", &b, &o.name);
    Ok(())
}

/// `muster agent ls`: `NAME  KIND  MODEL  ACCEPTS  SESSION  STATE`.
pub fn agent_ls(d: &Discovery, as_json: bool) -> anyhow::Result<()> {
    let v = client::rpc(d, "agent.list", json!({}))?;
    if as_json {
        return print_json(&v);
    }
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no agents (`muster agent new <name>` creates one)");
        return Ok(());
    }
    let s = |b: &Value, k: &str| b[k].as_str().unwrap_or("-").to_string();
    let rows: Vec<Vec<Cell>> = list
        .iter()
        .map(|b| {
            let accepts = b["accepts_origins"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            let state = s(b, "state");
            vec![
                Cell::plain(s(b, "name")),
                Cell::plain(s(b, "kind")),
                Cell::plain(s(b, "model")),
                Cell::plain(accepts),
                Cell::plain(b["session_id"].as_u64().map_or("-".into(), |i| i.to_string())),
                Cell {
                    color: state_color(&state),
                    text: state,
                },
            ]
        })
        .collect();
    print!(
        "{}",
        render_table(
            Some(&["NAME", "KIND", "MODEL", "ACCEPTS", "SESSION", "STATE"]),
            &rows,
            term_width(),
            use_color()
        )
    );
    Ok(())
}

pub fn agent_rm(d: &Discovery, name: &str) -> anyhow::Result<()> {
    let r = client::rpc(d, "agent.remove", json!({ "name": name }))?;
    println!(
        "removed agent {} (its home folder {} is kept)",
        r["removed"]["name"].as_str().unwrap_or(name),
        r["removed"]["home_dir"].as_str().unwrap_or("?")
    );
    Ok(())
}

/// `muster agent start`: prints the session id (like `muster new`).
pub fn agent_start(d: &Discovery, name: &str, fresh: bool) -> anyhow::Result<()> {
    let r = client::rpc_with_timeout(d, "agent.start", json!({ "name": name, "fresh": fresh }), Duration::from_secs(30))?;
    let id = r["session"]["id"].as_u64().unwrap_or(0);
    if r["started"] == true {
        println!("{id}");
    } else {
        println!("{id} (already running)");
    }
    Ok(())
}

/// `muster ask`: `agent.ask` (waits for the reply), prints the reply. Exit 124 on timeout; 1
/// when the agent stopped without answering (a prompt for the human, an error) or exited.
pub fn ask(d: &Discovery, agent: &str, words: &[String], timeout: u64, as_json: bool) -> anyhow::Result<()> {
    let text = send_text(words, || {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            eprintln!("muster: reading the question from stdin (end with Ctrl-D)");
        }
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        Ok(s)
    })?;
    let timeout = timeout.min(1800);
    // Inside a muster session the ask is that session's (an agent's goes through ask_agent).
    let r = client::rpc_with_timeout(
        d,
        "agent.ask",
        json!({
            "agent": agent,
            "text": text,
            "timeout_ms": timeout * 1000,
            "from_session": env_session(),
        }),
        Duration::from_secs(timeout + 30),
    )?;
    if as_json {
        print_json(&r)?;
    } else if let Some(reply) = r["reply"].as_str() {
        println!("{reply}");
    } else if let Some(tail) = r["unattributed_output"].as_str() {
        eprintln!("muster: {agent} reported no reply for this message; the end of its terminal (may belong to another prompt):");
        println!("{tail}");
        return Ok(());
    }
    if r["timed_out"] == true {
        return Err(exit(124, format!("no reply from {agent} within {timeout}s (msg {})", r["msg_id"])));
    }
    if r["reply"].is_null() {
        return Err(exit(
            1,
            format!(
                "{agent} stopped without a reply: {}{}",
                r["state"].as_str().unwrap_or("?"),
                r["detail"].as_str().map(|d| format!(" ({d})")).unwrap_or_default()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        let h = json!({
            "channels": [
                {"id": 1, "name": "fix-auth", "window_id": 1},
                {"id": 2, "name": "docs", "window_id": 2},
                {"id": 3, "name": "fixtures"},
                {"id": 4, "name": "old", "archived": true},
                {"id": 5, "name": "pair"},
                {"id": 6, "name": "shells"},
                {"id": 7, "name": "Fix src/main.rs"},
            ],
            "sessions": [
                {"id": 10, "channel_id": 1, "kind": "claude", "name": "claude", "state": "working", "state_since": 0},
                {"id": 11, "channel_id": 1, "kind": "shell", "name": "server", "state": "idle", "window_id": 3},
                {"id": 20, "channel_id": 2, "kind": "codex", "name": "writer", "state": "idle"},
                {"id": 21, "channel_id": 2, "kind": "claude", "name": "gone", "state": "exited"},
                {"id": 30, "channel_id": 3, "kind": "shell", "name": "sh", "state": "idle"},
                {"id": 40, "channel_id": 4, "kind": "claude", "name": "ancient", "state": "idle"},
                {"id": 50, "channel_id": 5, "kind": "claude", "name": "left", "state": "idle"},
                {"id": 51, "channel_id": 5, "kind": "codex", "name": "right", "state": "idle"},
                {"id": 60, "channel_id": 6, "kind": "shell", "name": "a", "state": "idle"},
                {"id": 61, "channel_id": 6, "kind": "shell", "name": "b", "state": "idle"},
                {"id": 62, "channel_id": 6, "kind": "shell", "name": "gone2", "state": "exited"},
                {"id": 70, "channel_id": 7, "kind": "claude", "name": "claude", "state": "idle"},
            ]
        });
        Snapshot::from_hello(&h)
    }

    fn ok(t: &str) -> u32 {
        match resolve(&snap(), t, Some("11")) {
            Ok(s) => s.id,
            Err(e) => panic!("{t}: {e:#}"),
        }
    }

    fn code(t: &str, env: Option<&str>) -> u8 {
        let e = resolve(&snap(), t, env).expect_err(t);
        e.downcast_ref::<Exit>().map_or(1, |x| x.code)
    }

    fn ev(kind: &str, data: Value) -> Value {
        json!({ "event": kind, "data": data })
    }

    fn upsert(id: u32, ch: i64, state: &str, detail: Option<&str>) -> Value {
        ev(
            "session_upserted",
            json!({"id": id, "channel_id": ch, "kind": "shell", "name": "sh", "state": state,
                   "state_detail": detail, "state_since": 1_700_000_000_000i64}),
        )
    }

    #[test]
    fn events_print_state_changes_only() {
        let mut t = EventTracker::new(&snap(), EventFilter::default());
        // Same state (e.g. a title change): passes for --json, no human line.
        let s = t.apply(&upsert(30, 3, "idle", None));
        assert!(s.pass && s.change.is_none());
        let s = t.apply(&upsert(30, 3, "working", Some("cargo test")));
        assert_eq!(
            s.change,
            Some((1_700_000_000_000, "30  fixtures/sh  idle -> working  cargo test".into()))
        );
        // A new session, in a tab renamed since the snapshot.
        t.apply(&ev("channel_upserted", json!({"id": 3, "name": "renamed"})));
        let s = t.apply(&upsert(99, 3, "starting", None));
        assert_eq!(s.change.unwrap().1, "99  renamed/sh  new -> starting");
        let s = t.apply(&ev("session_removed", json!({"id": 99})));
        assert!(s.pass);
        assert_eq!(s.change.unwrap().1, "99  renamed/sh  starting -> removed");
        for kind in ["channel_removed", "window_upserted", "window_removed"] {
            let s = t.apply(&ev(kind, json!({"id": 1, "channel_ids": []})));
            assert!(s.pass && s.change.is_none(), "{kind}");
        }
        assert!(t.apply(&ev("lagged", json!({"missed": 3}))).pass);
    }

    #[test]
    fn events_filter_by_session_and_tab() {
        let by_session = EventFilter { session: Some(10), channel: None };
        let mut t = EventTracker::new(&snap(), by_session);
        assert!(t.apply(&upsert(10, 1, "idle", None)).change.is_some());
        let other = t.apply(&upsert(11, 1, "working", None));
        assert!(!other.pass && other.change.is_none());
        assert!(!t.apply(&ev("session_removed", json!({"id": 11}))).pass);
        // Tab and window events pass when they concern the session's tab.
        assert!(t.apply(&ev("channel_upserted", json!({"id": 1, "name": "fix-auth"}))).pass);
        assert!(!t.apply(&ev("channel_upserted", json!({"id": 2, "name": "docs"}))).pass);
        assert!(t.apply(&ev("window_upserted", json!({"id": 1, "channel_ids": [5, 1]}))).pass);
        assert!(!t.apply(&ev("window_upserted", json!({"id": 2, "channel_ids": [2]}))).pass);
        assert!(!t.apply(&ev("window_removed", json!({"id": 2}))).pass);
        assert!(t.apply(&ev("lagged", json!({"missed": 1}))).pass);
        // Closing the session's tab: the session goes first, its tab and window after.
        assert!(t.apply(&ev("session_removed", json!({"id": 10}))).pass);
        assert!(t.apply(&ev("channel_removed", json!({"id": 1}))).pass);
        assert!(t.apply(&ev("window_removed", json!({"id": 1}))).pass);
        // A session shown in another window than its tab's: that window's events pass too.
        let mut t = EventTracker::new(&snap(), EventFilter { session: Some(11), channel: None });
        assert!(t.apply(&ev("window_upserted", json!({"id": 3, "channel_ids": []}))).pass);
        assert!(t.apply(&ev("window_upserted", json!({"id": 1, "channel_ids": [1]}))).pass);
        assert!(!t.apply(&ev("window_upserted", json!({"id": 2, "channel_ids": [2]}))).pass);

        let by_tab = EventFilter { session: None, channel: Some(6) };
        let mut t = EventTracker::new(&snap(), by_tab);
        assert!(t.apply(&upsert(60, 6, "working", None)).pass);
        assert!(!t.apply(&upsert(30, 3, "working", None)).pass);
        // Moving out of the tab still shows (the old channel matches); after that it doesn't.
        assert!(t.apply(&upsert(61, 2, "idle", None)).pass);
        assert!(!t.apply(&upsert(61, 2, "working", None)).pass);
        assert!(t.apply(&ev("channel_removed", json!({"id": 6}))).pass);
        assert!(!t.apply(&ev("channel_removed", json!({"id": 2}))).pass);
        assert!(t.apply(&ev("session_removed", json!({"id": 62}))).pass);
    }

    #[test]
    fn events_window_removed_after_its_last_tab_closed() {
        let by_tab = EventFilter { session: None, channel: Some(6) };
        let mut t = EventTracker::new(&snap(), by_tab);
        assert!(t.apply(&ev("window_upserted", json!({"id": 3, "channel_ids": [6]}))).pass);
        // The tab closes: the window is upserted without it, then removed.
        t.apply(&ev("channel_removed", json!({"id": 6})));
        t.apply(&ev("window_upserted", json!({"id": 3, "channel_ids": []})));
        assert!(t.apply(&ev("window_removed", json!({"id": 3}))).pass);
    }

    #[test]
    fn events_skip_updates_older_than_what_is_known() {
        let mut t = EventTracker::new(&snap(), EventFilter::default());
        let at = |state: &str, since: i64| {
            ev(
                "session_upserted",
                json!({"id": 30, "channel_id": 3, "kind": "shell", "name": "sh", "state": state, "state_since": since}),
            )
        };
        assert!(t.apply(&at("working", 2_000)).change.is_some());
        // Queued before that (e.g. replayed after a snapshot): no made-up `working -> idle`.
        let old = t.apply(&at("idle", 1_000));
        assert!(old.change.is_none() && !old.pass);
        assert!(t.apply(&at("idle", 3_000)).change.is_some());
    }

    #[test]
    fn events_tab_resolution() {
        let snap = snap();
        assert_eq!(resolve_tab(&snap, "docs").unwrap(), 2);
        assert_eq!(resolve_tab(&snap, "SHE").unwrap(), 6);
        // "fix-auth", "fixtures" and "Fix src/main.rs" share the prefix.
        let e = resolve_tab(&snap, "fix").unwrap_err();
        assert_eq!(e.downcast_ref::<Exit>().map(|x| x.code), Some(2));
        assert!(resolve_tab(&snap, "old").is_err(), "archived tabs never match");
    }

    #[test]
    fn clock_is_hh_mm_ss() {
        let c = clock(1_700_000_000_000);
        assert_eq!(c.len(), 8);
        assert_eq!(c.as_bytes()[2], b':');
        assert!(c.ends_with(":20"), "{c}");
    }

    #[test]
    fn targets_resolve() {
        assert_eq!(ok("20"), 20);
        assert_eq!(ok("21"), 21, "exited by id");
        assert_eq!(ok("."), 11);
        assert_eq!(ok("server"), 11, "exact session name");
        assert_eq!(ok("SERVER"), 11, "case-insensitive");
        assert_eq!(ok("gone"), 21, "exited by exact name");
        assert_eq!(ok("fix-auth"), 10, "tab -> its single live agent");
        assert_eq!(ok("docs"), 20, "tab: exited agent doesn't count");
        assert_eq!(ok("fixtures"), 30, "tab without agent -> its only live session");
        assert_eq!(ok("fix-"), 10, "unique tab prefix");
        assert_eq!(ok("serv"), 11, "unique session prefix");
        assert_eq!(ok("wri"), 20);
        assert_eq!(ok("fix-auth/server"), 11, "tab/name");
        assert_eq!(ok("fix/serv"), 11, "both parts prefixes");
        assert_eq!(ok("docs/gone"), 21, "exited by exact tab/name");
        assert_eq!(ok("pair/left"), 50);
        assert_eq!(ok("fix src/main.rs"), 70, "a whole tab name with a slash wins over tab/name");
    }

    #[test]
    fn targets_fail_with_2() {
        assert_eq!(code(".", None), 2, "`.` outside a session");
        assert_eq!(code("fix", None), 2, "fix-auth and fixtures");
        assert_eq!(code("pair", None), 2, "two live agents");
        assert_eq!(code("shells", None), 2, "two live shells");
        assert_eq!(code("gon", None), 2, "exited sessions never match by prefix");
        assert_eq!(code("docs/go", None), 2);
        assert_eq!(code("ancient", None), 2, "archived tab");
        assert_eq!(code("40", None), 2, "archived tab, by id");
        assert_eq!(code("old", None), 2, "archived tab name");
        assert_eq!(code("nope", None), 2);
        assert_eq!(code("999", None), 2, "unknown id");
        assert_eq!(code("", None), 2);
    }

    #[test]
    fn ambiguity_lists_candidates() {
        let e = resolve(&snap(), "pair", None).unwrap_err().to_string();
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("50  pair/left   claude  idle"), "{e}");
        assert!(e.contains("51  pair/right  codex   idle"), "{e}");
    }

    #[test]
    fn keys_parse() {
        let k = |s: &str| parse_key(s).unwrap();
        assert_eq!(k("enter"), "\r");
        assert_eq!(k("Enter"), "\r");
        assert_eq!(k("esc"), "\x1b");
        assert_eq!(k("tab"), "\t");
        assert_eq!(k("space"), " ");
        assert_eq!(k("backspace"), "\x7f");
        assert_eq!(k("up"), "\x1b[A");
        assert_eq!(k("down"), "\x1b[B");
        assert_eq!(k("right"), "\x1b[C");
        assert_eq!(k("left"), "\x1b[D");
        assert_eq!(k("shift+tab"), "\x1b[Z");
        assert_eq!(k("ctrl+c"), "\x03");
        assert_eq!(k("ctrl+C"), "\x03");
        assert_eq!(k("ctrl+a"), "\x01");
        assert_eq!(k("ctrl+["), "\x1b");
        assert_eq!(k("ctrl+space"), "\0");
        assert_eq!(k("alt+x"), "\x1bx");
        assert_eq!(k("alt+X"), "\x1bX");
        assert_eq!(k("alt+enter"), "\x1b\r");
        assert_eq!(k("ctrl+left"), "\x1b[1;5D");
        assert_eq!(k("alt+up"), "\x1b[1;3A");
        assert_eq!(k("ctrl+shift+end"), "\x1b[1;6F");
        assert_eq!(k("text:hello world"), "hello world");
        assert_eq!(k("text:"), "");
        assert_eq!(k("y"), "y");
        assert!(parse_key("ctrl+cc").is_err());
        assert!(parse_key("hyper+x").is_err());
        assert!(parse_key("alt+foo").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(-5), "0s");
        assert_eq!(format_duration(59_999), "59s");
        assert_eq!(format_duration(60_000), "1m");
        assert_eq!(format_duration(3_599_000), "59m");
        assert_eq!(format_duration(3_600_000), "1h");
        assert_eq!(format_duration(4_800_000), "1h20m");
        assert_eq!(format_duration(86_400_000), "1d");
        assert_eq!(format_duration(97_200_000), "1d3h");
    }

    fn row(cells: &[&str]) -> Vec<Cell> {
        cells.iter().map(|c| Cell::plain(*c)).collect()
    }

    #[test]
    fn table_aligns_and_truncates() {
        let rows = vec![row(&["1", "alpha", "a long detail here"]), row(&["123", "b", ""])];
        let t = render_table(Some(&["ID", "NAME", "DETAIL"]), &rows, None, false);
        assert_eq!(t, "ID   NAME   DETAIL\n1    alpha  a long detail here\n123  b\n");
        let t = render_table(Some(&["ID", "NAME", "DETAIL"]), &rows, Some(16), false);
        for l in t.lines() {
            assert!(l.chars().count() <= 16, "{l:?}");
        }
        assert_eq!(t.lines().nth(1), Some("1    alpha  a l…"));
        // A cut in a padded column ends the line there.
        let t = render_table(None, &rows, Some(6), false);
        assert_eq!(t.lines().next(), Some("1    …"));
    }

    #[test]
    fn table_colours_only_text() {
        let rows = vec![vec![
            Cell::plain("1"),
            Cell {
                text: "idle".into(),
                color: Some("32"),
            },
            Cell::plain("x"),
        ]];
        let t = render_table(None, &rows, None, true);
        assert_eq!(t, "1  \x1b[32midle\x1b[0m  x\n");
        let t = render_table(None, &rows, None, false);
        assert_eq!(t, "1  idle  x\n");
    }

    #[test]
    fn send_text_sources() {
        let never = || -> std::io::Result<String> { panic!("stdin read") };
        assert_eq!(send_text(&["fix".into(), "it".into()], never).unwrap(), "fix it");
        assert_eq!(send_text(&[], || Ok("line1\nline2\n\n".into())).unwrap(), "line1\nline2");
        assert_eq!(send_text(&["-".into()], || Ok("hi\r\n".into())).unwrap(), "hi");
        assert!(send_text(&[], || Ok("  \n".into())).is_err());
        assert!(send_text(&["-".into(), "x".into()], never).is_ok(), "`-` among words is text");
    }

    /// A fake daemon on a free port answering `/api/rpc` with `handler(method, params)`.
    fn mock_daemon(handler: impl Fn(&str, &Value) -> Value + Send + 'static) -> Discovery {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let mut c = c.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req: Value = serde_json::from_slice(&buf[head_end..]).unwrap();
                let result = handler(req["method"].as_str().unwrap(), &req["params"]);
                let body = serde_json::to_vec(&json!({ "id": 1, "result": result })).unwrap();
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = c.write_all(&body);
            }
        });
        Discovery {
            pid: 0,
            port,
            panel_port: None,
            token: "t".into(),
            version: "test".into(),
        }
    }

    /// `send --wait` against a scripted session: `states(n)` is the session after the n-th
    /// `session.wait` call. Returns the `until` lists the CLI waited with.
    fn send_wait(
        queued: bool,
        timeout: u64,
        states: impl Fn(usize) -> Value + Send + 'static,
    ) -> (anyhow::Result<()>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let log = calls.clone();
        let d = mock_daemon(move |method, params| match method {
            "hello" => json!({
                "channels": [{"id": 1, "name": "tab"}],
                "sessions": [{"id": 7, "channel_id": 1, "kind": "claude", "name": "claude", "state": "idle", "state_since": 1}],
            }),
            "session.send" => json!({"delivered": !queued, "queued": u32::from(queued)}),
            "session.wait" => {
                let mut l = log.lock().unwrap();
                l.push(params["until"].clone());
                let s = states(l.len());
                let until: Vec<String> = serde_json::from_value(params["until"].clone()).unwrap();
                let matched = until.iter().any(|u| *u == s["state"]);
                json!({"session": s, "matched": matched, "timed_out": !matched})
            }
            m => panic!("unexpected {m}"),
        });
        let o = SendOpts {
            interrupt: false,
            wait: true,
            until: Vec::new(),
            timeout,
        };
        let r = send(&d, "7", &["hi".into()], o);
        let calls = calls.lock().unwrap().clone();
        (r, calls)
    }

    fn sess(state: &str, since: i64, queued: u32) -> Value {
        json!({"id": 7, "channel_id": 1, "kind": "claude", "name": "claude", "state": state, "state_since": since, "queued": queued})
    }

    #[test]
    fn send_wait_sees_work_then_settles() {
        let t = now_ms() + 10_000;
        let (r, calls) = send_wait(false, 5, move |n| match n {
            1 => sess("working", t, 0),
            _ => sess("idle", t + 1, 0),
        });
        r.unwrap();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], json!(["working", "exited"]));
        assert_eq!(calls[1], json!(SETTLED));
    }

    #[test]
    fn send_wait_tolerates_instant_answers() {
        // Already back to idle when first polled, but the state changed after the send.
        let t = now_ms() + 10_000;
        let (r, calls) = send_wait(false, 5, move |_| sess("idle", t, 0));
        r.unwrap();
        assert_eq!(calls.len(), 2, "{calls:?}");
    }

    #[test]
    fn send_wait_waits_out_the_queue() {
        // Busy with earlier work (our prompt queued), then ours runs, then idle.
        let t = now_ms() + 10_000;
        let (r, calls) = send_wait(true, 5, move |n| match n {
            1..=2 => sess("working", 1, 1),
            3 => sess("idle", 2, 1),
            // The earlier work ended and the queue emptied, but ours isn't typed yet.
            4 => sess("idle", t, 0),
            5 => sess("working", t + 1, 0),
            _ => sess("idle", t + 1, 0),
        });
        r.unwrap();
        assert_eq!(calls.len(), 6, "{calls:?}");
        assert_eq!(calls[5], json!(SETTLED));
    }

    #[test]
    fn send_wait_timeout_exits_124() {
        let t = now_ms() + 10_000;
        let (r, _) = send_wait(false, 5, move |n| match n {
            1 => sess("working", t, 0),
            _ => sess("working", t, 0),
        });
        let e = r.unwrap_err();
        assert_eq!(e.downcast_ref::<Exit>().map(|x| x.code), Some(124), "{e:#}");
    }

    #[test]
    fn send_wait_times_out_before_the_prompt_starts() {
        // Still busy with earlier work when the timeout hits: 124, not the idle that follows.
        let (r, calls) = send_wait(true, 1, |_| sess("working", 1, 1));
        let e = r.unwrap_err();
        assert_eq!(e.downcast_ref::<Exit>().map(|x| x.code), Some(124), "{e:#}");
        assert!(calls.iter().all(|c| *c != json!(SETTLED)), "no settled wait");
    }

    #[test]
    fn ls_json_adds_tab_fields() {
        let s = snap();
        let sess = s.sessions.iter().find(|x| x.id == 10).unwrap();
        let v = s.session_json(sess);
        assert_eq!(v["channel_name"], "fix-auth");
        assert_eq!(v["window_id"], 1);
        assert_eq!(v["kind"], "claude");
        // A session's own window wins over its tab's.
        let sess = s.sessions.iter().find(|x| x.id == 11).unwrap();
        assert_eq!(s.window_of(sess), Some(3));
        assert_eq!(s.session_json(sess)["window_id"], 3);
    }

    #[test]
    fn a_hidden_agent_tab_is_in_no_window() {
        // Its task may still name a window (older views): the session's null stays, so
        // `muster move` sends no stale `from_window_id` and `ls --json` reports null.
        let s = Snapshot::from_hello(&json!({
            "channels": [{"id": 1, "name": "ada", "window_id": 2}],
            "sessions": [{"id": 9, "channel_id": 1, "kind": "claude", "name": "ada", "state": "idle",
                "window_id": null, "hidden": true}],
        }));
        let sess = &s.sessions[0];
        assert_eq!(s.window_of(sess), None);
        assert!(s.session_json(sess)["window_id"].is_null());
    }
}

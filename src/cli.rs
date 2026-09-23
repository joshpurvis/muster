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

    /// A session's JSON with `channel_name` and `window_id` added.
    fn session_json(&self, s: &Sess) -> Value {
        let mut v = s.raw.clone();
        if let Some(o) = v.as_object_mut() {
            let ch = self.channel(s.channel_id);
            o.insert("channel_name".into(), json!(ch.map(|c| c.name.clone())));
            o.insert("window_id".into(), json!(ch.and_then(|c| c.window_id)));
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

fn print_json(v: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

// ----- commands -----

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
        let win = snap.channel(s.channel_id).and_then(|c| c.window_id).unwrap_or(i64::MAX);
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
                Cell::plain(ch.and_then(|c| c.window_id).map_or("-".into(), |w| w.to_string())),
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
    let r = client::rpc(d, "session.send", json!({ "id": s.id, "text": text, "mode": mode }))?;
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
    client::rpc(d, "session.input", json!({ "id": s.id, "data": data, "keys": true }))?;
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

/// `muster interrupt` / `kill` / `restart`: a method that takes just the session id.
pub fn by_id(d: &Discovery, t: &str, method: &str) -> anyhow::Result<()> {
    let (_, s) = target(d, t)?;
    client::rpc(d, method, json!({ "id": s.id }))?;
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
    pub path: std::path::PathBuf,
    pub agent: String,
    pub worktree: bool,
    pub base: Option<String>,
    pub name: Option<String>,
    pub window: Option<i64>,
    pub prompt: Option<String>,
    pub json: bool,
}

/// `muster new`: `workspace.add`, `channel.create`, then `hello` for the new session.
pub fn new(d: &Discovery, o: NewOpts) -> anyhow::Result<()> {
    let abs = std::fs::canonicalize(&o.path).with_context(|| format!("{}", o.path.display()))?;
    if o.agent == "none" && o.prompt.is_some() {
        anyhow::bail!("--prompt needs an agent (not --agent none)");
    }
    let ws = client::rpc(d, "workspace.add", json!({ "path": abs }))?;
    let ws_id = ws["id"].as_i64().context("workspace.add returned no id")?;
    // Inside a muster session, the new tab goes to that session's window. The session's current
    // channel wins over MUSTER_CHANNEL_ID, which is stale once the session moved to another tab.
    let env_id = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<i64>().ok());
    let window = match o.window {
        Some(w) => Some(w),
        None => match (env_id("MUSTER_SESSION_ID"), env_id("MUSTER_CHANNEL_ID")) {
            (None, None) => None,
            (sid, ch) => {
                let snap = Snapshot::fetch(d)?;
                sid.and_then(|sid| snap.sessions.iter().find(|s| i64::from(s.id) == sid))
                    .map(|s| s.channel_id)
                    .or(ch)
                    .and_then(|ch| snap.channel(ch))
                    .and_then(|c| c.window_id)
            }
        },
    };
    let mut p = json!({ "workspace_id": ws_id, "worktree": o.worktree });
    let set = |p: &mut Value, k: &str, v: Value| {
        p.as_object_mut().expect("object").insert(k.into(), v);
    };
    if let Some(n) = o.name.filter(|n| !n.trim().is_empty()) {
        set(&mut p, "name", json!(n));
    }
    if let Some(b) = o.base {
        set(&mut p, "base", json!(b));
    }
    if o.agent != "none" {
        set(&mut p, "agent", json!(o.agent));
    }
    if let Some(pr) = o.prompt {
        set(&mut p, "prompt", json!(pr));
    }
    if let Some(w) = window {
        set(&mut p, "window_id", json!(w));
    }
    let ch = client::rpc(d, "channel.create", p)?;
    let ch_id = ch["id"].as_i64().context("channel.create returned no id")?;
    let snap = Snapshot::fetch(d)?;
    let sess = snap.sessions.iter().filter(|s| s.channel_id == ch_id).min_by_key(|s| s.id);
    if o.json {
        print_json(&json!({ "channel": ch, "session": sess.map(|s| snap.session_json(s)) }))?;
    } else if let Some(s) = sess {
        println!("{}", s.id);
    } else if o.agent == "none" {
        eprintln!("muster: created tab {ch_id} ({}) with no session", ch["name"].as_str().unwrap_or("?"));
    }
    if o.agent != "none" && sess.is_none() {
        anyhow::bail!("tab {ch_id} was created but {} did not start (see the tab's timeline)", o.agent);
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
                {"id": 11, "channel_id": 1, "kind": "shell", "name": "server", "state": "idle"},
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
    }
}

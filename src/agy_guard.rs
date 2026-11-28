//! The owner's Claude Code tool and Stop guards, run for Antigravity agents too.
//! Stop hooks use a private transcript of the current turn (see [`stop`]).
//!
//! An owner can register guard scripts as Claude Code `PreToolUse` hooks (settings.json
//! `hooks.PreToolUse`: a tool-name `matcher` and `command`s): a check that stops private details
//! leaving the machine, one that refuses unclear question cards, and so on. Antigravity runs none
//! of them. So for each Antigravity session Muster snapshots those guards at its start ([`load`]:
//! the user's `~/.claude/settings.json`, or the file `MUSTER_ANTIGRAVITY_GUARDS` names; `off` for
//! none) into the session's [`CONFIG`], and registers `muster hook antigravity guard <that file>`
//! as an agy `PreToolUse` hook ([`hooks_json`], written to `.agents/hooks.json` in the session's
//! Muster workspace by src/adapters/antigravity.rs). On each call [`run`] turns agy's payload into
//! Claude Code's ([`claude_call`]), runs every guard whose matcher names that tool, unchanged, with
//! Claude Code's input on stdin, and turns their answers into agy's ([`combine`]).
//!
//! Checked against agy 1.5.13 (26 Nov 2026): a `.agents/hooks.json` in an `--add-dir` folder is
//! loaded; the hook runs in that `.agents` folder with agy's environment (`MUSTER_SESSION_ID` and
//! the rest); its payload is `{toolCall: {id, name, args}, conversationId, transcriptPath, cwd,
//! workspacePaths, hookEventName, ...}`; a `deny` reason reaches the agent as "tool call denied by
//! pre-tool hook: <reason>"; and any failure of the hook itself (output that isn't JSON, a non-zero
//! exit, a timeout) blocks the call. So this command always answers, in time, with JSON.
//!
//! A guard's answer is read as Claude Code reads it: exit 2 refuses (stderr is the reason); exit 0
//! with `hookSpecificOutput.permissionDecision` `deny` / `ask` / `allow` (or the older top-level
//! `decision: "block"`) decides; any other exit, a guard that can't start, or one past its timeout
//! is a non-blocking error, so the call goes on, as it would on a Claude agent. A refusal from any
//! guard refuses the call (their reasons joined); otherwise an `ask` asks the human every time
//! (`force_ask`). When this command can't read agy's payload or its own configuration, it asks
//! too: a human looks before the call runs.

mod stop;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The session's guard list, in its session directory (outside the agent's workspace).
pub const CONFIG: &str = "antigravity-guards.json";
/// agy's tools that [`claude_call`] maps: the hook runs for these only.
pub const AGY_MATCHER: &str =
    "run_command|call_mcp_tool|read_url_content|search_web|write_to_file|replace_file_content|multi_replace_file_content|ask_question";
/// Claude Code's default hook timeout.
const DEFAULT_TIMEOUT_S: u64 = 60;
const MAX_TIMEOUT_S: u64 = 120;
const MAX_GUARDS: usize = 32;
const MAX_STDIN: usize = 16 << 20;
const MAX_OUTPUT: usize = 1 << 20;
const MAX_REASON: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guard {
    /// Claude Code's tool-name matcher (`Bash|WebFetch`, `mcp__.*`).
    pub matcher: String,
    /// A shell command, run with `sh -c`.
    pub command: String,
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The session's working folder: the guards run there, with `CLAUDE_PROJECT_DIR` set to it.
    pub cwd: String,
    pub guards: Vec<Guard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<Guard>,
}

/// The command hooks for one event in a Claude Code settings file.
fn from_claude_settings(v: &Value, event: &str) -> Vec<Guard> {
    let mut out = Vec::new();
    for m in v.get("hooks").and_then(|hooks| hooks.get(event)).and_then(Value::as_array).into_iter().flatten() {
        let matcher = m.get("matcher").and_then(Value::as_str).unwrap_or("").to_string();
        for h in m.get("hooks").and_then(Value::as_array).into_iter().flatten() {
            if h.get("type").and_then(Value::as_str).is_some_and(|t| t != "command") {
                continue;
            }
            let Some(command) = h.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) else { continue };
            let timeout_s = h.get("timeout").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_S).clamp(1, MAX_TIMEOUT_S);
            out.push(Guard { matcher: matcher.clone(), command: command.to_string(), timeout_s });
        }
    }
    out.truncate(MAX_GUARDS);
    out
}

/// The guards a new Antigravity session runs: those of the file `MUSTER_ANTIGRAVITY_GUARDS`
/// names (a Claude Code settings file), else of `~/.claude/settings.json`; none when that
/// variable is `off` (or `0`, `false`, `no`, empty) or the file is missing or unreadable.
pub fn load(cwd: &Path) -> Config {
    let file = match std::env::var("MUSTER_ANTIGRAVITY_GUARDS") {
        Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "off" | "false" | "no") => return Config::default(),
        Ok(v) => PathBuf::from(v.trim()),
        Err(_) => crate::paths::home_dir().join(".claude/settings.json"),
    };
    let settings = crate::adapters::antigravity::read_json(&file).unwrap_or_default();
    Config {
        cwd: cwd.to_string_lossy().into(),
        guards: from_claude_settings(&settings, "PreToolUse"),
        stop: from_claude_settings(&settings, "Stop"),
    }
}

/// The agy hook file that sends the mapped tools' calls through `muster hook antigravity guard`.
pub fn hooks_json(bin: &Path, config_file: &Path, config: &Config) -> Value {
    let q = |p: &Path| crate::adapters::claude::sh_quote(&p.to_string_lossy());
    let handler = |verb: &str, guards: &[Guard]| json!([{
        "type": "command",
        "command": format!("{} hook antigravity {verb} {}", q(bin), q(config_file)),
        "timeout": guards.iter().map(|g| g.timeout_s).max().unwrap_or(0) + 10,
    }]);
    let mut events = json!({});
    if !config.guards.is_empty() {
        events["PreToolUse"] = json!([{"matcher":AGY_MATCHER, "hooks":handler("guard", &config.guards)}]);
    }
    if !config.stop.is_empty() {
        events["Stop"] = handler("stop", &config.stop);
    }
    json!({"muster-guards": events})
}

/// Does Claude Code's `matcher` name `tool`? Alternatives split on `|` (or `,`), anchors dropped,
/// `.*` as a wildcard. Any other regular-expression syntax, grouping too, counts as a match:
/// running a guard for a tool it doesn't check is harmless (each checks the tool name itself),
/// skipping one is not.
pub fn matches(matcher: &str, tool: &str) -> bool {
    let m: String = matcher.trim().chars().filter(|c| !matches!(c, '^' | '$')).collect();
    if m.is_empty() || m == "*" || m.contains(['(', ')']) {
        return true;
    }
    m.split(['|', ',']).map(str::trim).any(|alt| {
        let plain = alt.replace(".*", "");
        if plain.chars().any(|c| "[]{}+?*.\\".contains(c)) {
            return true;
        }
        let parts: Vec<&str> = alt.split(".*").collect();
        if parts.len() == 1 {
            return alt == tool;
        }
        let (first, last) = (parts[0], parts[parts.len() - 1]);
        if !tool.starts_with(first) || !tool[first.len()..].ends_with(last) || tool.len() < first.len() + last.len() {
            return false;
        }
        let mut rest = &tool[first.len()..tool.len() - last.len()];
        for p in &parts[1..parts.len() - 1] {
            match rest.find(p) {
                Some(i) => rest = &rest[i + p.len()..],
                None => return false,
            }
        }
        true
    })
}

/// agy's tool call as Claude Code names it, with Claude Code's arguments. None: a tool no guard
/// is run for. `Err`: a call this can't read (an MCP call without its server or tool name).
pub fn claude_call(name: &str, args: &Value) -> Result<Option<(String, Value)>, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let any = |keys: &[&str]| keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str)).unwrap_or("").to_string();
    Ok(Some(match name {
        "run_command" => ("Bash".into(), json!({"command": s("CommandLine"), "description": s("toolSummary")})),
        "read_url_content" => ("WebFetch".into(), json!({"url": s("Url"), "prompt": ""})),
        "search_web" => {
            let mut input = json!({"query": s("query")});
            if let Some(d) = args.get("domain").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                input["allowed_domains"] = json!([d]);
            }
            ("WebSearch".into(), input)
        }
        "write_to_file" => ("Write".into(), json!({"file_path": s("TargetFile"), "content": s("CodeContent")})),
        "replace_file_content" => ("Edit".into(), json!({
            "file_path": s("TargetFile"),
            "old_string": s("TargetContent"),
            "new_string": s("ReplacementContent"),
            "replace_all": args.get("AllowMultiple").and_then(Value::as_bool).unwrap_or(false),
        })),
        "multi_replace_file_content" => {
            let edits: Vec<Value> = args.get("ReplacementChunks").and_then(Value::as_array).into_iter().flatten()
                .map(|c| json!({
                    "old_string": c.get("TargetContent").and_then(Value::as_str).unwrap_or(""),
                    "new_string": c.get("ReplacementContent").and_then(Value::as_str).unwrap_or(""),
                }))
                .collect();
            ("MultiEdit".into(), json!({"file_path": s("TargetFile"), "edits": edits}))
        }
        "ask_question" => ("AskUserQuestion".into(), json!({"questions": args.get("questions").cloned().unwrap_or(json!([]))})),
        "call_mcp_tool" => {
            let server = any(&["ServerName", "serverName", "server_name"]);
            let tool = any(&["ToolName", "toolName", "tool_name"]);
            if server.is_empty() || tool.is_empty() {
                return Err("an MCP call without its server or tool name".into());
            }
            let input = match args.get("Arguments").or_else(|| args.get("arguments")) {
                Some(Value::String(t)) => serde_json::from_str(t).unwrap_or_else(|_| json!({ "arguments": t })),
                Some(v) => v.clone(),
                None => json!({}),
            };
            (format!("mcp__{server}__{tool}"), input)
        }
        _ => return Ok(None),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// One guard's answer, read as Claude Code reads a `PreToolUse` hook's.
pub fn answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> (Decision, Option<String>) {
    let text = |b: &[u8]| Some(String::from_utf8_lossy(b).trim().to_string()).filter(|t| !t.is_empty());
    match code {
        Some(2) => (Decision::Deny, text(stderr).or_else(|| Some("refused by a guard".into()))),
        Some(0) => {
            let Ok(v) = serde_json::from_slice::<Value>(stdout) else { return (Decision::Allow, None) };
            let reason = |k: &str, o: &Value| o.get(k).and_then(Value::as_str).map(str::to_string);
            if let Some(h) = v.get("hookSpecificOutput") {
                match h.get("permissionDecision").and_then(Value::as_str) {
                    Some("deny") => return (Decision::Deny, reason("permissionDecisionReason", h)),
                    Some("ask") => return (Decision::Ask, reason("permissionDecisionReason", h)),
                    _ => {}
                }
                // A rewrite of the call (a redaction, say) can't be passed on to agy, which would
                // run the call as it was: a human looks instead.
                if h.get("updatedInput").is_some_and(|u| !u.is_null()) {
                    return (Decision::Ask, Some("a guard changed this call, which Muster can't pass on to Antigravity".into()));
                }
            }
            match v.get("decision").and_then(Value::as_str) {
                Some("block") => (Decision::Deny, reason("reason", &v)),
                _ => (Decision::Allow, None),
            }
        }
        _ => (Decision::Allow, None),
    }
}

/// The answer agy reads: the strictest decision, with the reasons for it.
pub fn combine(answers: &[(Decision, Option<String>)]) -> Value {
    let worst = answers.iter().map(|a| a.0).max().unwrap_or(Decision::Allow);
    let reasons: Vec<&str> = answers.iter().filter(|a| a.0 == worst).filter_map(|a| a.1.as_deref()).collect();
    let mut reason = reasons.join(" ");
    if let Some((i, _)) = reason.char_indices().nth(MAX_REASON) {
        reason.truncate(i);
        reason.push('…');
    }
    match worst {
        Decision::Allow => json!({"decision": "allow"}),
        Decision::Ask => json!({"decision": "force_ask", "reason": reason}),
        Decision::Deny => json!({"decision": "deny", "reason": if reason.is_empty() { "refused by a guard".into() } else { reason }}),
    }
}

fn ask(why: &str) -> Value {
    json!({"decision": "force_ask", "reason": format!("Muster couldn't check this call against your guards ({why}); look before approving.")})
}

type ReadAnswer = fn(Option<i32>, &[u8], &[u8]) -> (Decision, Option<String>);

/// Read without letting a descendant holding a pipe open outlive the hook deadline.
fn read_pipe(pipe: Option<impl Read + Send + 'static>) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(capped(pipe)); });
    rx
}

/// Run one guard with Claude Code's input on stdin, in `cwd`, for at most its timeout.
fn run_guard(g: &Guard, input: &[u8], cwd: &Path, read_answer: ReadAnswer) -> (Decision, Option<String>) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&g.command)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .env("CLAUDE_PROJECT_DIR", cwd)
        .process_group(0);
    if cwd.is_dir() {
        cmd.current_dir(cwd);
    }
    let Ok(mut child) = cmd.spawn() else { return (Decision::Allow, None) };
    let pid = child.id() as i32;
    let (mut stdin, out, err) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let stdout = read_pipe(out);
    let stderr = read_pipe(err);
    let data = input.to_vec();
    std::thread::spawn(move || {
        if let Some(mut w) = stdin.take() { let _ = w.write_all(&data); }
    });
    let deadline = Instant::now() + Duration::from_secs(g.timeout_s.clamp(1, MAX_TIMEOUT_S));
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // Past its timeout: the whole group goes, as Claude Code cancels a hook.
                // SAFETY: signalling the process group this guard leads.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.wait();
                return (Decision::Allow, None);
            }
        }
    };
    // Its group too: a guard that left a child holding the pipes open can't hold this answer.
    // SAFETY: as above.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let (Ok((stdout, too_long)), Ok((stderr, _))) =
        (stdout.recv_timeout(remaining()), stderr.recv_timeout(remaining()))
    else { return (Decision::Allow, None) };
    if code == Some(0) && too_long {
        // Its decision is past what this reads: a human looks instead of guessing it was allow.
        return (Decision::Ask, Some("a guard's answer was too long to read".into()));
    }
    read_answer(code, &stdout, &stderr)
}

/// The first `MAX_OUTPUT` bytes of a guard's pipe, and whether it wrote more. The rest is read
/// and dropped: a guard that writes past the cap and then exits 2 must not die of a closed pipe
/// first, which would read as a guard error and let the call go on.
fn capped(r: Option<impl Read>) -> (Vec<u8>, bool) {
    let mut b = Vec::new();
    let Some(mut r) = r else { return (b, false) };
    let _ = r.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut b);
    let more = std::io::copy(&mut r, &mut std::io::sink()).unwrap_or(0) > 0;
    (b, more)
}

/// The answer for agy's `payload`, with the session's `config` (None: it couldn't be read).
pub fn decide(payload: &[u8], config: Option<Config>) -> Value {
    let Some(config) = config else { return ask("its guard list is missing or unreadable") };
    let Ok(p) = serde_json::from_slice::<Value>(payload) else { return ask("agy's hook input isn't JSON") };
    let Some(name) = p.pointer("/toolCall/name").and_then(Value::as_str) else { return ask("agy's hook input names no tool") };
    let args = p.pointer("/toolCall/args").cloned().unwrap_or(json!({}));
    let (tool, tool_input) = match claude_call(name, &args) {
        Ok(Some(call)) => call,
        Ok(None) => return json!({"decision": "allow"}),
        Err(why) => return ask(&why),
    };
    let guards: Vec<&Guard> = config.guards.iter().filter(|g| matches(&g.matcher, &tool)).collect();
    if guards.is_empty() {
        return json!({"decision": "allow"});
    }
    let cwd = PathBuf::from(if config.cwd.is_empty() { p.get("cwd").and_then(Value::as_str).unwrap_or("/") } else { &config.cwd });
    let input = json!({
        "session_id": p.get("conversationId").cloned().unwrap_or(Value::Null),
        "transcript_path": p.get("transcriptPath").cloned().unwrap_or(Value::Null),
        "cwd": cwd.to_string_lossy(),
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": tool_input,
        "tool_use_id": p.pointer("/toolCall/id").cloned().unwrap_or(Value::Null),
    });
    let input = serde_json::to_vec(&input).unwrap_or_default();
    combine(&run_guards(guards, &input, &cwd, answer))
}

fn run_guards(guards: Vec<&Guard>, input: &[u8], cwd: &Path, read_answer: ReadAnswer) -> Vec<(Decision, Option<String>)> {
    std::thread::scope(|s| {
        let runs: Vec<_> = guards.into_iter().map(|g| s.spawn(move || run_guard(g, input, cwd, read_answer))).collect();
        runs.into_iter().map(|r| r.join().unwrap_or((Decision::Allow, None))).collect()
    })
}

fn read_config(file: &str) -> Option<Config> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(file).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    f.take(1 << 20).read_to_end(&mut data).ok()?;
    let config: Config = serde_json::from_slice(&data).ok()?;
    (config.guards.len() <= MAX_GUARDS && config.stop.len() <= MAX_GUARDS).then_some(config)
}

/// Hook payload on stdin, decision on stdout. Leave time inside agy's timeout to
/// answer even if the writer leaves stdin open.
fn stdin_payload() -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut payload = Vec::new();
        if std::io::stdin().take(MAX_STDIN as u64 + 1).read_to_end(&mut payload).is_ok()
            && payload.len() <= MAX_STDIN
        {
            let _ = tx.send(payload);
        }
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
}

pub fn run(config_file: Option<&str>, is_stop: bool) -> i32 {
    let payload = stdin_payload();
    let config = config_file.and_then(read_config);
    let answer = if is_stop {
        config_file.and_then(|p| Path::new(p).parent()).and_then(|dir| {
            stop::decide(&payload, config?, dir).ok()
        }).unwrap_or_else(|| json!({"decision": "allow"}))
    } else {
        decide(&payload, config)
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{answer}").and_then(|()| out.flush());
    0
}

#[cfg(test)]
mod tests;

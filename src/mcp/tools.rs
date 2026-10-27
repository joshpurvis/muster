//! Tool definitions (JSON Schemas) and dispatch. Every tool returns text; structured results
//! are pretty-printed JSON. Failures are `isError: true` tool results, not protocol errors.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::McpState;
use super::backend::Backend;
use super::guard::{self, aggregate_state, check_spawn, spawn_budget};
use super::wait::{self, Verdict, WaitCtx};
use crate::hub::{CreateChannel, SpawnSession};
use crate::inject::SendMode;
use crate::model::*;

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// Tool names in the order `tools/list` returns them (deterministic, per spec guidance).
#[cfg(test)]
pub const TOOL_NAMES: [&str; 28] = [
    "whoami",
    "list_channels",
    "list_sessions",
    "spawn_session",
    "send_message",
    "read_screen",
    "get_recent_output",
    "wait_for_state",
    "post_to_channel",
    "get_diff",
    "list_agents",
    "ask_agent",
    "panel_open", "panel_update", "panel_close", "list_panels", "wait_for_panel_event", "panel_event",
    "note_list", "note_read", "note_write", "note_append", "note_open",
    "list_schedules", "propose_schedule", "set_reminder", "pause_schedule", "delete_schedule",
];

const STATE_NAMES: [&str; 8] = [
    "starting",
    "working",
    "needs_approval",
    "asking_question",
    "idle",
    "error",
    "stuck",
    "exited",
];

fn tool(name: &str, title: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "_meta": {"ui": {"visibility": if name == "get_diff" { vec!["model", "app"] } else { vec!["model"] }}},
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "openWorldHint": false,
        },
    })
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

pub fn tool_definitions() -> Vec<Value> {
    let session_id = json!({"type": "integer", "minimum": 1, "description": "Target session id (see list_sessions)."});
    let mut tools = vec![
        tool(
            "whoami",
            "Who am I",
            "Describe the calling session: its id, kind, channel (including its agent permission profile: Codex sandbox and Claude permission mode, set by the human), workspace, working directory, git branch, parent, and remaining spawn budget. Call this first to orient yourself.",
            empty_schema(),
            true,
        ),
        tool(
            "list_channels",
            "List channels",
            "List active (non-archived) channels. A channel is one unit of work with its own working directory (usually a git worktree + branch). Returns id, name, workspace, branch, cwd, session counts and an aggregate state (the most urgent state of its live sessions: needs_approval > asking_question > error > stuck > working > starting > idle > exited; 'empty' if no sessions).",
            empty_schema(),
            true,
        ),
        tool(
            "list_sessions",
            "List sessions",
            "List terminal sessions (agents and shells), including exited ones. Each has id, channel_id, kind (claude|codex|shell), name, state, detail (e.g. the pending approval or question), queued (messages waiting to be delivered), parent_id (the session that spawned it via MCP) and is_me.",
            json!({
                "type": "object",
                "properties": {
                    "channel_id": {"type": "integer", "description": "Only sessions in this channel. Omit for all channels."}
                },
                "additionalProperties": false
            }),
            true,
        ),
        tool(
            "spawn_session",
            "Spawn session",
            "Start a new agent (claude or codex) or shell session, recorded as your child. By default it starts in YOUR channel (same working directory). Pass channel_id to use another existing channel of your workspace, or new_channel to create a fresh channel (a new tab in your channel's window; by default with its own git worktree + branch, so parallel work doesn't collide; without a name it is named after the agent's task). A Claude child inherits the calling Claude session's account profile, including Default; otherwise the workspace default applies. The agent runs with the target channel's permission profile (Codex sandbox, Claude permission mode; see whoami/list_channels), which only the human can change; a new_channel gets your channel's profile. There is no way to request more: a channel_id whose profile would give this kind of agent more than your channel has is refused. The prompt, if given, is delivered once the agent is ready. A shell can't be given a prompt unless the human allowed it. Returns immediately; use wait_for_state to wait for it. Limits: spawn depth 3, 8 live children per caller, 24 agent-spawned sessions in total, 10 spawns per 10 minutes per caller, 10 open agent-created channels.",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["claude", "codex", "shell"], "description": "What to run."},
                    "prompt": {"type": "string", "description": "Initial task for the agent (or command for a shell)."},
                    "name": {"type": "string", "description": "Display name for the session."},
                    "model": {"type": "string", "description": "Model for the agent, e.g. 'sonnet' or 'opus' (claude), 'gpt-6-sol' (codex). Default: the CLI's own default."},
                    "effort": {"type": "string", "description": "Reasoning effort, e.g. 'low', 'medium', 'high' (claude --effort), 'xhigh' (codex model_reasoning_effort)."},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Extra CLI arguments for the agent. Only an allowlist is accepted (claude: --model, --effort, --fallback-model, --append-system-prompt, --continue, --resume, --verbose, --debug; codex: -m, -c with model/model_reasoning_* keys, -i, --no-alt-screen); permissions always come from the channel's profile."},
                    "channel_id": {"type": "integer", "description": "Existing channel to spawn in. Mutually exclusive with new_channel."},
                    "new_channel": {
                        "type": "object",
                        "description": "Create a new channel in your workspace and spawn there.",
                        "properties": {
                            "name": {"type": "string", "description": "Channel name, e.g. 'fix-login-bug'. Omit to let muster name it from the agent's task."},
                            "worktree": {"type": "boolean", "default": true, "description": "Create a git worktree + branch muster/<channel id>, renamed after the channel name (ignored for non-git workspaces)."},
                            "base": {"type": "string", "description": "Branch or commit to base the worktree on. Default: workspace HEAD."}
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
            false,
        ),
        tool(
            "send_message",
            "Send message",
            "Send a prompt to another agent session in your workspace. It goes through the injection queue: if the agent is busy it is delivered when the agent becomes ready (set interrupt=true to interrupt its current turn first). Cannot target yourself or (unless the human allowed it) shell sessions, and cannot answer permission prompts: only the human approves those. Rate limited to 20 per minute per caller. Returns delivered/queued and sent_at_ms (pass it as after_ms to wait_for_state if you want to be explicit).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": session_id,
                    "text": {"type": "string", "description": "Message text; multi-line is fine."},
                    "interrupt": {"type": "boolean", "default": false, "description": "Interrupt the agent's current turn, then deliver."}
                },
                "required": ["session_id", "text"],
                "additionalProperties": false
            }),
            false,
        ),
        tool(
            "read_screen",
            "Read screen",
            "Return the session's currently visible terminal screen as plain text (what a human would see right now). Good for checking a prompt or question the agent is showing.",
            json!({
                "type": "object",
                "properties": {"session_id": session_id},
                "required": ["session_id"],
                "additionalProperties": false
            }),
            true,
        ),
        tool(
            "get_recent_output",
            "Get recent output",
            "Return the last N lines of a session's terminal output (scrollback + screen) as plain text with ANSI codes stripped.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": session_id,
                    "lines": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 100, "description": "How many lines (max 2000)."}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            true,
        ),
        tool(
            "wait_for_state",
            "Wait for state",
            "Block until a session is in one of the given states, or the timeout passes. Default states: idle, needs_approval, asking_question, error, stuck, exited (i.e. 'finished or needs help'). Returns immediately if already there, except that right after send_message an unchanged 'idle' (or idle with queued input) is treated as stale, so 'send then wait' waits for the agent to actually process the message. Pass after_ms (unix ms, e.g. send_message's sent_at_ms) to require a state entered at or after that time, or after_ms=0 to accept any current matching state. Timing out is not an error: check timed_out in the result.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": session_id,
                    "states": {"type": "array", "items": {"type": "string", "enum": STATE_NAMES}, "minItems": 1, "description": "States to wait for."},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800, "default": 300},
                    "after_ms": {"type": "integer", "minimum": 0, "description": "Only accept a state entered at/after this unix time in ms."}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            true,
        ),
        tool(
            "post_to_channel",
            "Post to channel",
            "Write a short message to a channel's human-visible timeline (status updates, findings, hand-offs). Defaults to your own channel.",
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Message (markdown ok)."},
                    "channel_id": {"type": "integer", "description": "Target channel. Default: your channel."}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            false,
        ),
        tool(
            "get_diff",
            "Get diff",
            "Return the git diff of a channel's working tree against its base branch (committed + uncommitted changes, plus a summary of untracked files). Defaults to your channel. Large diffs are truncated with a note.",
            json!({
                "type": "object",
                "properties": {
                    "channel_id": {"type": "integer", "description": "Channel. Default: your channel."},
                    "max_bytes": {"type": "integer", "minimum": 1000, "maximum": 400000, "default": 60000, "description": "Truncate the diff after this many bytes."}
                },
                "additionalProperties": false
            }),
            true,
        ),
        tool(
            "list_agents",
            "List agents",
            "List the named agents: persistent agents (claude or codex) with their own home folder, memory and resumed conversation, which you can ask things with ask_agent. Each has name, kind, model, accepts_origins (which message origins it takes: human, schedule, webhook), its session and state. Also returns your_origin: the origin and hop a message you send now would carry.",
            empty_schema(),
            true,
        ),
        tool(
            "ask_agent",
            "Ask agent",
            "Ask an agent something and wait for its answer. Starts the agent if it isn't running, delivers the text (after a one-line [muster msg ...] header naming you, the origin and the hop) through its input queue, waits until its turn ends, and returns {reply, state, timed_out, msg_id}. reply is the agent's final message for that turn (null if it stopped on a permission prompt or question: check state). Refused when the agent doesn't accept your origin (a chain started by a schedule or webhook can't reach a Claude agent by default), when the chain is more than 4 hops deep, or past the agent's 200 messages a day. Rate limited with send_message (20 per minute).",
            json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Agent name (see list_agents), case-insensitive."},
                    "text": {"type": "string", "description": "The question or task; multi-line is fine."},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800, "default": 600, "description": "How long to wait for the reply."}
                },
                "required": ["agent", "text"],
                "additionalProperties": false
            }),
            false,
        ),
    ];
    let csp = json!({"type":"object", "properties": {
        "resourceDomains":{"type":"array","items":{"type":"string"}},
        "connectDomains":{"type":"array","items":{"type":"string"}},
        "frameDomains":{"type":"array","items":{"type":"string"}},
        "baseUriDomains":{"type":"array","items":{"type":"string"}}
    },"additionalProperties":false});
    tools.extend([
        tool("panel_open", "Open panel", "Open a sandboxed MCP App beside your terminal, scoped to your task. Built-ins (data schemas in Muster's docs/MCP.md): markdown {markdown}; image {path (file in your worktree, png/jpeg/gif/webp/svg, max 8 MiB) or url (https), alt?, caption?}; chart {type: bar|line|scatter|area, title?, x: {label?, values}, series: [{name, values}], y?: {label?, min?, max?}}; table {title?, columns: [{key, label?, align?}], rows, sort?: {key, dir}}; pr_decision (a PR summary with Merge, Request changes, Ask, Skip and Hold, optional media pictures and charts, and the user's own actions; the decision comes back as an event); pr_overview (every open PR of a repo as cards; the user's pick comes back as an event); board {title, scope?, takeaway?, source?, min?, blocks: [{kind: stats|chart|list|meters|callout|actions, span?: \"full\", ...}]} (a dashboard from plain JSON, no HTML: headline numbers, charts, lists, meters, a callout, and buttons whose click comes back as an event `kind: id`; prefer it over custom HTML for dashboards and status pages); web_preview {url} (your running local dev server, live: http(s)://localhost or 127.0.0.1 with its port (not [::1]: use localhost), e.g. http://localhost:5173; anything else is refused: open other pages in the user's system browser; the result has a `note` when the server is down, refuses framing or redirects away). Custom: html or html_path inside the task cwd. With kit: true Muster puts the Muster Kit in front of your HTML (kit.css, kit.js, fonts; outside your 1 MiB): it does the handshake and theme for you, so do not call ui/initialize yourself; use mk-* classes, <mk-stat>, <mk-chart>, data-send buttons (data-confirm asks first) and mk.onData/mk.send. Without the kit, the page must call ui/initialize and then send ui/notifications/initialized (MCP Apps 2026-01-26), or it stays on \"Loading\" (minimal example in Muster's docs/MCP.md). HTML/data limit 1 MiB each; at most 16 open panels per task. Declared resource domains may load assets; fetching and nested frames stay blocked. The user closing a panel does not prompt you unless you pass notify_on_close: true; otherwise its `closed` event comes with the next panel prompt or wait_for_panel_event.",
            json!({"type":"object","properties":{"title":{"type":"string"},"app":{"type":"string","enum":["markdown","image","chart","table","pr_decision","pr_overview","board","web_preview","custom"]},"data":{"type":"object"},"html":{"type":"string"},"html_path":{"type":"string"},"csp":csp,"kit":{"type":"boolean","description":"custom apps only: inject the Muster Kit in front of the HTML"},"notify_on_close":{"type":"boolean","description":"the user closing the panel prompts you even when idle; off by default (closing then reaches you only with the next panel prompt or wait_for_panel_event)"}},"required":["title","app"],"additionalProperties":false}), false),
        tool("panel_update", "Update panel", "Update a panel in your task. data replaces the whole input (send every field again), and is sent to its app in place; changed HTML reloads the app, as does switching `kit` on or off (custom apps); a web_preview loads its new url (with a `note` as in panel_open). Each update advances the revision.",
            json!({"type":"object","properties":{"panel_id":{"type":"integer"},"title":{"type":"string"},"data":{"type":"object"},"html":{"type":"string"},"html_path":{"type":"string"},"kit":{"type":"boolean"}},"required":["panel_id"],"additionalProperties":false}), false),
        tool("panel_close", "Close panel", "Close a panel belonging to your task. Its app and data are deleted; user events queued before the close are still delivered.",
            json!({"type":"object","properties":{"panel_id":{"type":"integer"}},"required":["panel_id"],"additionalProperties":false}), false),
        tool("list_panels", "List panels", "List the open panels belonging to your task, including their revision, title and opener.", empty_schema(), true),
        tool("wait_for_panel_event", "Wait for panel event", "Wait for queued user events in your task's panels and consume them. Returns whole events (each with an id) up to about 256 KiB and `remaining`, the number still queued. While waiting, matching events come here instead of being typed into the opener; otherwise they reach the idle opener as a marked panel prompt. Shells and exited sessions keep events queued. The wait fails if your session moves to another task or restarts.",
            json!({"type":"object","properties":{"panel_id":{"type":"integer"},"timeout_seconds":{"type":"integer","minimum":0,"maximum":1800,"default":300}},"additionalProperties":false}), true),
        tool("panel_event", "Fetch panel event", "Fetch the full payload of a user event that a panel prompt showed only as a placeholder because it was too long. Fetching consumes it.",
            json!({"type":"object","properties":{"panel_id":{"type":"integer"},"event_id":{"type":"integer"}},"required":["panel_id","event_id"],"additionalProperties":false}), true),
    ]);
    let note_id = json!({"type":"integer","minimum":1,"description":"The note's id (see note_list)."});
    tools.extend([
        tool("note_list", "List notes", "List Muster's notes: plain-text notes the human and every agent share, shown as editor tabs in the UI. Each has id, title, rev (its text revision), bytes, lines, updated_at, updated_by (human, or which agent) and open (shown in a window). No text: use note_read.",
            empty_schema(), true),
        tool("note_read", "Read note", "Read a note's whole text with its current rev. Pass that rev to note_write.",
            json!({"type":"object","properties":{"note_id":note_id},"required":["note_id"],"additionalProperties":false}), true),
        tool("note_write", "Write note", "Without note_id: create a note (title optional) holding text; returns its note_id. With note_id: replace its whole text (and title, if given). rev is required then: the rev from your note_read. If the note changed since (the human typed, or another agent wrote), the write is refused: read it again, redo your change on the new text and write with the new rev. To add lines at the end, use note_append instead. Plain text, at most 1 MiB.",
            json!({"type":"object","properties":{"note_id":note_id,"title":{"type":"string","maxLength":200},"text":{"type":"string"},"rev":{"type":"integer","minimum":1,"description":"The rev note_read returned (required with note_id)."}},"required":["text"],"additionalProperties":false}), false),
        tool("note_append", "Append to note", "Add text at the end of a note, on a new line. Needs no rev and never conflicts: use it for logs, findings and to-do items while the human may be typing.",
            json!({"type":"object","properties":{"note_id":note_id,"text":{"type":"string"}},"required":["note_id","text"],"additionalProperties":false}), false),
        tool("note_open", "Show note", "Show a note to the human as a tab beside your terminal (moving it from another window).",
            json!({"type":"object","properties":{"note_id":note_id},"required":["note_id"],"additionalProperties":false}), false),
    ]);
    let schedule_id = json!({"type":"integer","minimum":1,"description":"The schedule's id (see list_schedules)."});
    let when_props = |p: &mut Map<String, Value>| {
        p.insert("cron".into(), json!({"type":"string","description":"A recurring time: a 5-field cron line (minute hour day-of-month month day-of-week) read in tz, e.g. \"0 16 * * 5\" for Fridays at 16:00. At most every 15 minutes."}));
        p.insert("at".into(), json!({"type":["string","integer"],"description":"A one-off time: \"YYYY-MM-DDTHH:MM\" read in tz, or unix milliseconds."}));
        p.insert("in_minutes".into(), json!({"type":"integer","minimum":1,"description":"A one-off this many minutes from now."}));
        p.insert("tz".into(), json!({"type":"string","description":"IANA time zone for cron and at (default: this computer's)."}));
    };
    let mut propose = Map::new();
    propose.insert("prompt".into(), json!({"type":"string","description":"The message Muster sends the agent at each run (it may use {date}, {last_run} and {last_reply})."}));
    propose.insert("reason".into(), json!({"type":"string","maxLength":1000,"description":"Why, in a sentence or two: the human reads it before approving."}));
    propose.insert("agent".into(), json!({"type":"string","description":"The agent it runs on (default: you). It must take scheduled work."}));
    propose.insert("name".into(), json!({"type":"string","maxLength":80}));
    when_props(&mut propose);
    propose.insert("conversation".into(), json!({"type":"string","enum":["continue","fresh"],"description":"continue (default): each run joins the agent's conversation; fresh: a new conversation each run."}));
    propose.insert("quiet_hours".into(), json!({"type":"boolean","description":"Skip runs in the human's quiet hours (default: on for Claude subscription agents)."}));
    propose.insert("daily_cap".into(), json!({"type":"integer","minimum":1,"maximum":200,"description":"At most this many runs a day."}));
    propose.insert("note_id".into(), json!({"type":"integer","minimum":1,"description":"Also add each reply to this existing note."}));
    let mut remind = Map::new();
    remind.insert("prompt".into(), json!({"type":"string","description":"What Muster sends you then, e.g. \"Check whether CI on master passed and report to the human.\""}));
    remind.insert("in_minutes".into(), json!({"type":"integer","minimum":5,"maximum":1440,"description":"Minutes from now (5 to 1440)."}));
    remind.insert("at".into(), json!({"type":["string","integer"],"description":"Or a time: \"YYYY-MM-DDTHH:MM\" read in tz, or unix milliseconds (5 minutes to 24 hours ahead)."}));
    remind.insert("tz".into(), json!({"type":"string"}));
    remind.insert("name".into(), json!({"type":"string","maxLength":80}));
    tools.extend([
        tool("list_schedules", "List schedules", "List the schedules that run on you or that you made (proposals, reminders): id, agent, name, prompt, when, status (proposed: waiting for the human's approval; active, paused, expired, done), next run, who made it, and the last run's outcome. Also whether you take scheduled work at all (the human sets that).",
            empty_schema(), true),
        tool("propose_schedule", "Propose schedule", "Propose a schedule (recurring with cron, or a one-off with at or in_minutes) for you or another agent that takes scheduled work. It does NOT run until the human approves it with a click in Muster's Agents panel, where they see the prompt, the timing, the agent, your reason and the estimated use; they may edit or dismiss it. Each run is a message marked as scheduled, under every limit the human set (per-agent daily caps, spend caps, quiet hours, a Claude subscription agent's 7-day expiry). Refused while you handle scheduled or triggered work. Limits: 3 proposals waiting, 10 a day, 5 live schedules you made. Triggers and \"Then\" steps are the human's to set up.",
            json!({"type":"object","properties":Value::Object(propose),"required":["prompt","reason"],"additionalProperties":false}), false),
        tool("set_reminder", "Set reminder", "Have Muster send you a prompt once, 5 minutes to 24 hours from now (\"check CI again in 20 minutes\"), without asking the human: a one-off for yourself only, delivered as scheduled work into your current conversation. You must take scheduled work (the human sets that). Refused while you handle scheduled or triggered work, so a reminder can't set the next one. At most 3 waiting, 15 minutes apart, 10 a day. For anything recurring, later or for another agent, use propose_schedule.",
            json!({"type":"object","properties":Value::Object(remind),"required":["prompt"],"additionalProperties":false}), false),
        tool("pause_schedule", "Pause schedule", "Pause a schedule that runs on you or that you made, so it skips its runs. Only the human can resume it.",
            json!({"type":"object","properties":{"schedule_id":schedule_id},"required":["schedule_id"],"additionalProperties":false}), false),
        tool("delete_schedule", "Delete schedule", "Delete a schedule you made (a proposal, a reminder, or a proposal the human approved) and its history. Schedules the human made can only be paused (pause_schedule).",
            json!({"type":"object","properties":{"schedule_id":schedule_id},"required":["schedule_id"],"additionalProperties":false}), false),
    ]);
    tools
}

// ----- argument helpers -----

type TResult = Result<String, String>;

struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }
    /// Integer; tolerates numeric strings ("3") since some models send those.
    fn int(&self, k: &str) -> Result<Option<i64>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
                .map(Some)
                .ok_or_else(|| format!("`{k}` must be an integer")),
            Some(Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("`{k}` must be an integer")),
            Some(_) => Err(format!("`{k}` must be an integer")),
        }
    }
    fn req_int(&self, k: &str) -> Result<i64, String> {
        self.int(k)?
            .ok_or_else(|| format!("missing required argument `{k}`"))
    }
    fn session_id(&self, k: &str) -> Result<SessionId, String> {
        let v = self.req_int(k)?;
        SessionId::try_from(v).map_err(|_| format!("`{k}` out of range"))
    }
    fn str(&self, k: &str) -> Result<Option<&str>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("`{k}` must be a string")),
        }
    }
    fn req_str(&self, k: &str) -> Result<&str, String> {
        self.str(k)?
            .ok_or_else(|| format!("missing required argument `{k}`"))
    }
    fn bool(&self, k: &str) -> Result<Option<bool>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s == "true" || s == "false" => Ok(Some(s == "true")),
            Some(_) => Err(format!("`{k}` must be a boolean")),
        }
    }
}

fn parse_state(s: &str) -> Option<AgentState> {
    if s == "done" {
        return Some(AgentState::Idle);
    }
    serde_json::from_value(Value::String(s.to_string())).ok()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn session_json(s: &SessionInfo, caller: SessionId) -> Value {
    json!({
        "id": s.id,
        "channel_id": s.channel_id,
        "kind": s.kind,
        "account_profile_id": s.account_profile_id,
        "name": s.name,
        "state": s.state,
        "detail": s.state_detail,
        "state_since_ms": s.state_since,
        "queued": s.queued,
        "parent_id": s.parent_id,
        "title": s.title,
        "exit_code": s.exit_code,
        "is_me": s.id == caller,
    })
}

fn channel_json(c: &Channel, ws: Option<&Workspace>) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "workspace_id": c.workspace_id,
        "workspace": ws.map(|w| w.name.clone()),
        "cwd": c.cwd,
        "branch": c.branch,
        "base_branch": c.base_branch,
        "permissions": permissions_json(&c.permissions),
    })
}

/// The channel's agent permission profile, with the CLI flags it maps to, so agents know what
/// they and their children may do. Only the human can change it.
fn permissions_json(p: &Permissions) -> Value {
    let codex = crate::adapters::codex::sandbox_args(p.codex_sandbox);
    json!({
        "codex_sandbox": p.codex_sandbox,
        "claude_mode": p.claude_mode,
        "codex_flags": codex.join(" "),
        "claude_permission_mode": crate::adapters::claude::permission_mode(p.claude_mode).unwrap_or("user default"),
        "set_by": "human (muster UI); agents can't change it",
    })
}

/// Truncate to at most `max` bytes on a char boundary.
pub fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Shorten long strings inside tool args before they go into the event log.
pub fn truncate_args(v: &Value) -> Value {
    const MAX_STR: usize = 500;
    const MAX_TOTAL: usize = 4000;
    fn walk(v: &Value) -> Value {
        match v {
            Value::String(s) if s.len() > MAX_STR => {
                Value::String(format!("{}… [{} bytes]", truncate_str(s, MAX_STR), s.len()))
            }
            Value::Array(a) => Value::Array(a.iter().map(walk).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), walk(v))).collect())
            }
            other => other.clone(),
        }
    }
    let out = walk(v);
    let s = out.to_string();
    if s.len() > MAX_TOTAL {
        json!({"_truncated": format!("{}…", truncate_str(&s, MAX_TOTAL))})
    } else {
        out
    }
}

// ----- dispatch -----

/// Err(()) = unknown tool (a protocol error per spec).
pub async fn call_tool<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    name: &str,
    args: &Value,
) -> Result<ToolOutput, ()> {
    let empty = Map::new();
    let map = match args {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => {
            return Ok(ToolOutput {
                text: "arguments must be an object".into(),
                is_error: true,
            });
        }
    };
    let a = Args(map);
    let res = match name {
        "whoami" => whoami(st, caller),
        "list_channels" => list_channels(st, caller),
        "list_sessions" => list_sessions(st, caller, &a),
        // `spawn_agent`: the tool's name before named agents existed; still accepted, not listed.
        "spawn_session" | "spawn_agent" => spawn_session(st, caller, &a).await,
        "send_message" => send_message(st, caller, &a).await,
        "read_screen" => read_screen(st, caller, &a),
        "get_recent_output" => get_recent_output(st, caller, &a),
        "wait_for_state" => wait_for_state(st, caller, &a).await,
        "post_to_channel" => post_to_channel(st, caller, &a),
        "get_diff" => get_diff(st, caller, &a).await,
        // `list_bots` / `ask_bot`: the first pass's names, which older persona files still use;
        // accepted (same guards), not listed.
        "list_agents" | "list_bots" => list_agents(st, caller),
        "ask_agent" | "ask_bot" => ask_agent(st, caller, &a).await,
        "panel_open" | "panel_update" | "panel_close" | "list_panels" | "wait_for_panel_event" | "panel_event" => panel_tool(st, caller, name, map).await,
        "note_list" | "note_read" | "note_write" | "note_append" | "note_open" => note_tool(st, caller, name, map),
        "list_schedules" | "propose_schedule" | "set_reminder" | "pause_schedule" | "delete_schedule" => schedule_tool(st, caller, name, map),
        _ => return Err(()),
    };
    Ok(match res {
        Ok(text) => ToolOutput {
            text,
            is_error: false,
        },
        Err(text) => ToolOutput {
            text,
            is_error: true,
        },
    })
}

fn caller_session<B: Backend>(st: &McpState<B>, caller: SessionId) -> Result<SessionInfo, String> {
    st.backend
        .session(caller)
        .ok_or_else(|| "calling session no longer exists".to_string())
}

async fn panel_tool<B: Backend>(st: &McpState<B>, caller: SessionId, name: &str, args: &Map<String, Value>) -> TResult {
    let me = caller_session(st, caller)?;
    open_channel(st, me.channel_id)?;
    if matches!(name, "panel_open" | "panel_update") {
        if let Err(wait_s) = st.edit_guard(|g| g.posts.check_and_record(
            caller, now_ms(), st.limits.post_per_window, st.limits.send_window_ms,
        ))? {
            return Err(format!("rate limit: at most {} panel opens/updates or posts per minute; try again in {wait_s}s", st.limits.post_per_window));
        }
    }
    st.backend.panel_tool(caller, name, Value::Object(args.clone())).await
        .map(|v| pretty(&v)).map_err(|e| format!("{e:#}"))
}

/// Notes are shared by every session (not task-scoped). Writes and appends share the post rate
/// limit, so a runaway loop can't flood the human's notes.
fn note_tool<B: Backend>(st: &McpState<B>, caller: SessionId, name: &str, args: &Map<String, Value>) -> TResult {
    caller_session(st, caller)?;
    if matches!(name, "note_write" | "note_append") {
        if let Err(wait_s) = st.edit_guard(|g| g.posts.check_and_record(
            caller, now_ms(), st.limits.post_per_window, st.limits.send_window_ms,
        ))? {
            return Err(format!("rate limit: at most {} note writes, panel updates or posts per minute; try again in {wait_s}s", st.limits.post_per_window));
        }
    }
    st.backend.note_tool(caller, name, Value::Object(args.clone()))
        .map(|v| pretty(&v)).map_err(|e| format!("{e:#}"))
}

/// Schedules for the calling agent (src/schedules/agent.rs has the rules). Proposals and
/// reminders share the post rate limit, so a loop can't flood the human with proposals.
fn schedule_tool<B: Backend>(st: &McpState<B>, caller: SessionId, name: &str, args: &Map<String, Value>) -> TResult {
    caller_session(st, caller)?;
    if matches!(name, "propose_schedule" | "set_reminder") {
        if let Err(wait_s) = st.edit_guard(|g| g.posts.check_and_record(
            caller, now_ms(), st.limits.post_per_window, st.limits.send_window_ms,
        ))? {
            return Err(format!("rate limit: at most {} proposals, reminders, note writes or posts per minute; try again in {wait_s}s", st.limits.post_per_window));
        }
    }
    st.backend.schedule_tool(caller, name, Value::Object(args.clone()))
        .map(|v| pretty(&v)).map_err(|e| format!("{e:#}"))
}

fn target_session<B: Backend>(st: &McpState<B>, id: SessionId) -> Result<SessionInfo, String> {
    st.backend
        .session(id)
        .ok_or_else(|| format!("no session with id {id} (see list_sessions)"))
}

fn open_channel<B: Backend>(st: &McpState<B>, id: ChannelId) -> Result<Channel, String> {
    match st.backend.channel(id) {
        Some(c) if !c.archived => Ok(c),
        Some(_) => Err(format!("channel {id} is archived")),
        None => Err(format!("no channel with id {id} (see list_channels)")),
    }
}

// ----- reach policy -----

const HOW_SHELL: &str =
    "The human can allow it by starting the muster daemon with MUSTER_MCP_ALLOW_SHELL=1.";
const HOW_CROSS: &str =
    "The human can allow it by starting the muster daemon with MUSTER_MCP_CROSS_WORKSPACE=1.";

/// Text an agent sends to another session is typed into its terminal, so it must stay text:
/// escape sequences could end the bracketed paste (`ESC [ 201 ~`) and turn the rest into raw
/// keystrokes (answering a permission dialog, switching modes). Drops ESC and every other C0/C1
/// control except newline and tab; CR and CRLF become newline.
pub fn clean_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .chars()
        .map(|c| if c == '\r' { '\n' } else { c })
        .filter(|&c| c == '\n' || c == '\t' || !(c.is_control()))
        .collect()
}

/// Claude Code and Codex run a message that starts with `!` as a shell command.
fn check_no_shell_escape<B: Backend>(
    st: &McpState<B>,
    kind: SessionKind,
    text: &str,
) -> Result<(), String> {
    if kind != SessionKind::Shell && !st.limits.allow_shell && text.trim_start().starts_with('!') {
        return Err(format!(
            "a message starting with `!` runs as a shell command in {}, which agents may not do. Rephrase it. {HOW_SHELL}",
            kind.as_str()
        ));
    }
    Ok(())
}

const HOW_PERMS: &str = "Only the human can grant that: ask them to change your channel's permissions, or to run it themselves.";

/// Agents can't reach an agent of `kind` that runs with more than they have: spawning into, or
/// messaging a session in, a channel whose profile is more permissive (for that kind of agent)
/// than the caller's channel is refused.
fn check_not_more_permissive<B: Backend>(
    st: &McpState<B>,
    me: &SessionInfo,
    target: &Channel,
    kind: SessionKind,
) -> Result<(), String> {
    check_perms_not_wider(st, me, Some(target.id), &target.permissions, kind, &format!("channel {}", target.id))
}

/// `check_not_more_permissive` for a profile that may not have a channel yet (an agent that
/// isn't running: its tab will get the agent's profile).
fn check_perms_not_wider<B: Backend>(
    st: &McpState<B>,
    me: &SessionInfo,
    target_channel: Option<ChannelId>,
    target: &Permissions,
    kind: SessionKind,
    what: &str,
) -> Result<(), String> {
    let mine = st
        .backend
        .channel(me.channel_id)
        .map(|c| c.permissions)
        .ok_or("your channel no longer exists")?;
    if target_channel != Some(me.channel_id) && target.exceeds_for(&mine, kind) {
        return Err(format!(
            "{what} lets {} do more than your channel does ({}); agents can't use another channel's permissions. {HOW_PERMS}",
            kind.as_str(),
            match kind {
                SessionKind::Codex => format!("sandbox {:?} vs {:?}", target.codex_sandbox, mine.codex_sandbox),
                _ => format!("mode {:?} vs {:?}", target.claude_mode, mine.claude_mode),
            }
        ));
    }
    Ok(())
}

/// Origin and hop of a message the caller sends now (None inbound: human-rooted, hop 0).
fn caller_origin<B: Backend>(st: &McpState<B>, caller: SessionId) -> (crate::agents::Origin, u32) {
    let r = caller_route(st, caller);
    (r.origin, r.hop)
}

/// The route of anything the caller sends or spawns now: its provenance, one hop on.
fn caller_route<B: Backend>(st: &McpState<B>, caller: SessionId) -> crate::agents::Route {
    crate::agents::outgoing(st.backend.prov(caller))
}

/// The caller's workspace (None if its channel is gone).
fn caller_workspace<B: Backend>(st: &McpState<B>, caller: SessionId) -> Option<WorkspaceId> {
    let me = st.backend.session(caller)?;
    st.backend.channel(me.channel_id).map(|c| c.workspace_id)
}

/// Is channel `ch` reachable for `caller`?
fn in_reach<B: Backend>(st: &McpState<B>, caller: SessionId, ch: &Channel) -> bool {
    st.limits.cross_workspace || caller_workspace(st, caller) == Some(ch.workspace_id)
}

fn check_channel_reach<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    ch: &Channel,
) -> Result<(), String> {
    if in_reach(st, caller, ch) {
        return Ok(());
    }
    Err(format!(
        "channel {} is in another workspace; agents can only reach their own workspace. {HOW_CROSS}",
        ch.id
    ))
}

fn check_session_reach<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    s: &SessionInfo,
) -> Result<(), String> {
    if st.limits.cross_workspace {
        return Ok(());
    }
    match st.backend.channel(s.channel_id) {
        Some(ch) if in_reach(st, caller, &ch) => Ok(()),
        _ => Err(format!(
            "session {} is in another workspace; agents can only reach their own workspace. {HOW_CROSS}",
            s.id
        )),
    }
}

/// A session the caller may look at or message.
fn reachable_session<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    id: SessionId,
) -> Result<SessionInfo, String> {
    let s = target_session(st, id)?;
    check_session_reach(st, caller, &s)?;
    Ok(s)
}

fn budget<B: Backend>(st: &McpState<B>, caller: SessionId) -> guard::SpawnBudget {
    let sessions = st.backend.sessions();
    let live: HashSet<SessionId> = st.backend.live_session_ids().into_iter().collect();
    let recorded = st.guard.lock().spawned_depth.clone();
    spawn_budget(&sessions, &live, &recorded, caller, &st.limits)
}

fn whoami<B: Backend>(st: &McpState<B>, caller: SessionId) -> TResult {
    let me = caller_session(st, caller)?;
    let (origin, hop) = caller_origin(st, caller);
    let origin_json = json!({"origin": origin, "hop": hop});
    let ch = st.backend.channel(me.channel_id);
    let ws = ch
        .as_ref()
        .and_then(|c| st.backend.workspace(c.workspace_id));
    Ok(pretty(&json!({
        "session": session_json(&me, caller),
        "cwd": me.cwd,
        "channel": ch.as_ref().map(|c| channel_json(c, ws.as_ref())),
        "workspace": ws.as_ref().map(|w| json!({"id": w.id, "name": w.name, "path": w.path, "is_git": w.is_git})),
        "spawn_budget": budget(st, caller),
        "agents_may_approve_prompts": guard::AGENTS_MAY_APPROVE,
        "agent": st.backend.agents().into_iter().find(|b| b.session_id == Some(caller)).map(|b| b.name),
        "origin": origin_json,
        "policy": {
            "shell_commands": st.limits.allow_shell,
            "other_workspaces": st.limits.cross_workspace,
        },
    })))
}

fn list_channels<B: Backend>(st: &McpState<B>, caller: SessionId) -> TResult {
    let sessions = st.backend.sessions();
    let live: HashSet<SessionId> = st.backend.live_session_ids().into_iter().collect();
    let out: Vec<Value> = st
        .backend
        .channels()
        .into_iter()
        .filter(|c| !c.archived && in_reach(st, caller, c))
        .map(|c| {
            let ws = st.backend.workspace(c.workspace_id);
            let mine: Vec<&SessionInfo> =
                sessions.iter().filter(|s| s.channel_id == c.id).collect();
            let mut v = channel_json(&c, ws.as_ref());
            v["state"] = json!(aggregate_state(mine.iter().map(|s| s.state)));
            v["sessions"] = json!(mine.len());
            v["live_sessions"] = json!(mine.iter().filter(|s| live.contains(&s.id)).count());
            v
        })
        .collect();
    Ok(pretty(&Value::Array(out)))
}

fn list_sessions<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let filter = a.int("channel_id")?;
    let out: Vec<Value> = st
        .backend
        .sessions()
        .iter()
        .filter(|s| filter.is_none_or(|c| s.channel_id == c))
        .filter(|s| check_session_reach(st, caller, s).is_ok())
        .map(|s| session_json(s, caller))
        .collect();
    Ok(pretty(&Value::Array(out)))
}

/// `spawn_session`'s `model`, `effort` and `args` as CLI arguments, checked against the agent
/// argument allowlist (so they can't widen the channel's permission profile).
fn agent_args(kind: SessionKind, a: &Args<'_>) -> Result<Vec<String>, String> {
    let model = a.str("model")?.map(str::trim).filter(|m| !m.is_empty());
    let effort = a.str("effort")?.map(str::trim).filter(|e| !e.is_empty());
    let mut out = Vec::new();
    match (kind, model, effort) {
        (SessionKind::Shell, None, None) => {}
        (SessionKind::Shell, _, _) => return Err("model and effort are for claude and codex agents".into()),
        (SessionKind::Claude, m, e) => {
            if let Some(m) = m {
                out.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = e {
                out.extend(["--effort".to_string(), e.to_string()]);
            }
        }
        (SessionKind::Codex, m, e) => {
            if let Some(m) = m {
                out.extend(["-m".to_string(), m.to_string()]);
            }
            if let Some(e) = e {
                out.extend(["-c".to_string(), format!("model_reasoning_effort={e}")]);
            }
        }
    }
    match a.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(v)) => {
            for x in v {
                out.push(x.as_str().ok_or("`args` must be an array of strings")?.to_string());
            }
        }
        Some(_) => return Err("`args` must be an array of strings".into()),
    }
    crate::adapters::check_extra_args(kind, &out).map_err(|e| format!("{e:#}"))?;
    Ok(out)
}

async fn spawn_session<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let kind_s = a.req_str("kind")?;
    let kind = SessionKind::parse(kind_s)
        .ok_or_else(|| format!("unknown kind `{kind_s}` (expected claude, codex or shell)"))?;
    let prompt = a
        .str("prompt")?
        .map(clean_text)
        .filter(|p| !p.trim().is_empty());
    let name = a
        .str("name")?
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string);
    let extra_args = agent_args(kind, a)?;
    let channel_id = a.int("channel_id")?;
    let new_channel = match a.get("new_channel") {
        None => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return Err("`new_channel` must be an object".into()),
    };
    if channel_id.is_some() && new_channel.is_some() {
        return Err("pass either channel_id or new_channel, not both".into());
    }
    // Arguments to a shell are a command too (`-c ...`, a script path): the same rule.
    if kind == SessionKind::Shell && !extra_args.is_empty() && !st.limits.allow_shell {
        return Err(format!(
            "agents can't pass arguments to a shell (they would run a command); spawn it without args for the human to use, or spawn an agent instead. {HOW_SHELL}"
        ));
    }
    if kind == SessionKind::Shell && prompt.is_some() && !st.limits.allow_shell {
        return Err(format!(
            "agents can't start a shell with a command (prompt); spawn it without a prompt for the human to use, or spawn an agent instead. {HOW_SHELL}"
        ));
    }
    if let Some(p) = &prompt {
        check_no_shell_escape(st, kind, p)?;
    }
    guard::check_claude_origin(kind, caller_origin(st, caller).0)?;
    let me = caller_session(st, caller)?;
    let target_channel = match new_channel {
        Some(_) => None,
        None => Some(open_channel(st, channel_id.unwrap_or(me.channel_id))?),
    };
    if let Some(ch) = &target_channel {
        check_channel_reach(st, caller, ch)?;
        check_not_more_permissive(st, &me, ch, kind)?;
    }

    // Serialize spawns so concurrent calls can't overshoot the limits.
    let _lock = st.spawn_lock.lock().await;
    let b = budget(st, caller);
    check_spawn(&b)?;
    let open_mcp_channels = {
        let ids: Vec<ChannelId> = st.guard.lock().mcp_channels.iter().copied().collect();
        ids.into_iter()
            .filter(|id| st.backend.channel(*id).is_some_and(|c| !c.archived))
            .count()
    };
    if new_channel.is_some() && open_mcp_channels >= st.limits.max_mcp_channels {
        return Err(format!(
            "{open_mcp_channels} agent-created channels are open (max {}). Reuse one with channel_id, or ask the human to archive some.",
            st.limits.max_mcp_channels
        ));
    }
    if let Err(wait_s) = st.edit_guard(|g| {
        let allowed = g.spawns.check_and_record(caller, now_ms(), st.limits.spawn_per_window, st.limits.spawn_window_ms);
        if allowed.is_ok() { g.spawned_depth.insert(caller, b.caller_depth); }
        allowed
    })? {
        return Err(format!(
            "rate limit: at most {} spawns per {} minutes. Try again in {wait_s}s.",
            st.limits.spawn_per_window,
            st.limits.spawn_window_ms / 60_000
        ));
    }
    let refund = || {
        if let Err(error) = st.edit_guard(|g| g.spawns.refund(caller)) { tracing::warn!("{error}"); }
    };

    let (channel, created) = if let Some(nc) = new_channel {
        let nc = Args(nc);
        let parsed = (|| -> Result<CreateChannel, String> {
            // Blank or missing: auto-named from the agent's title / prompt.
            let ch_name = nc
                .str("name")?
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
            let my_ch = st
                .backend
                .channel(me.channel_id)
                .ok_or("your channel no longer exists")?;
            Ok(CreateChannel {
                account_profile: None,
                workspace_id: my_ch.workspace_id,
                name: ch_name,
                worktree: nc.bool("worktree")?.unwrap_or(true),
                base: nc.str("base")?.map(str::to_string),
                agent: None,
                prompt: None,
                extra_args: Vec::new(),
                // Never more than the caller has: the new channel gets the caller's profile.
                permissions: Some(my_ch.permissions),
                window_id: None,
                // The caller's window (its task's, if the caller has none).
                prefer_window: me.window_id.or(my_ch.window_id),
                cols: crate::hub::default_cols(),
                rows: crate::hub::default_rows(),
                from_session: None,
                prov: None,
            })
        })();
        let req = parsed.inspect_err(|_| refund())?;
        let ch = st.backend.create_channel(req).await.map_err(|e| {
            refund();
            format!("could not create channel: {e:#}")
        })?;
        st.edit_guard(|g| g.mcp_channels.insert(ch.id))?;
        (ch, true)
    } else {
        (
            target_channel.expect("set when new_channel is absent"),
            false,
        )
    };

    let req = SpawnSession {
        account_profile: None,
        channel_id: channel.id,
        kind,
        name,
        cwd: None,
        prompt,
        extra_args,
        cols: 120,
        rows: 32,
        parent_id: Some(caller),
        // The caller's window (as the parent): a new tab next to the caller's.
        window_id: None,
        from_session: None,
        // The child starts with the caller's provenance and keeps it (agents.rs).
        prov: Some(caller_route(st, caller).prov(None)),
        agent_id: None,
    };
    let info = match st.backend.spawn_session(req).await {
        Ok(info) => info,
        Err(ref e) if let Some(&crate::hub::SpawnRetained(id)) = e.downcast_ref::<crate::hub::SpawnRetained>() => {
            // The agent may be running its task already. Keep the reservation, the channel
            // and its worktree; cleaning up could kill it and delete its work.
            if let Err(error) = st.edit_guard(|g| g.spawned_depth.insert(id, b.caller_depth + 1)) {
                tracing::warn!(session = id, "unconfirmed spawn; guard snapshot pending: {error}");
            }
            return Err(format!(
                "spawn outcome unconfirmed: session {id} in channel {} was kept and is being confirmed. Do not spawn again; use wait_for_state(session_id={id}).",
                channel.id
            ));
        }
        Err(e) => {
            refund();
            // Don't leave an empty channel/worktree behind for a failed spawn.
            let mut msg = format!("spawn failed: {e:#}");
            if created {
                if let Err(e2) = st.backend.archive_channel(channel.id, true).await {
                    msg.push_str(&format!(
                        " (the new channel {} could not be cleaned up: {e2:#}; pass channel_id={} to retry in it)",
                        channel.id, channel.id
                    ));
                } else {
                    msg.push_str(" (the new channel was removed again)");
                }
            }
            return Err(msg);
        }
    };
    if let Err(error) = st.edit_guard(|g| g.spawned_depth.insert(info.id, b.caller_depth + 1)) {
        // Parent depth was persisted before spawning; reconstruction also finds this child.
        tracing::warn!(session = info.id, "spawn completed; guard snapshot pending: {error}");
    }
    // The spawn's prompt may have named an auto-named channel: report it as it is now.
    let channel = st.backend.channel(channel.id).unwrap_or(channel);
    let ws = st.backend.workspace(channel.workspace_id);
    Ok(pretty(&json!({
        "session": session_json(&info, caller),
        "channel": channel_json(&channel, ws.as_ref()),
        "channel_created": created,
        "next": format!("Use wait_for_state(session_id={}) to wait until it finishes or needs help.", info.id),
    })))
}

async fn send_message<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let target = a.session_id("session_id")?;
    let text = a.req_str("text")?;
    let interrupt = a.bool("interrupt")?.unwrap_or(false);
    if target == caller {
        return Err("you cannot send a message to yourself".into());
    }
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    if text.chars().count() > st.limits.max_message_chars {
        return Err(format!(
            "text too long (max {} characters)",
            st.limits.max_message_chars
        ));
    }
    let info = reachable_session(st, caller, target)?;
    // An agent only takes messages through ask_agent, which applies its origin rules.
    if let Some(b) = st.backend.agents().into_iter().find(|b| b.session_id == Some(target)) {
        return Err(format!(
            "session {target} is the agent {}: use ask_agent(agent=\"{}\") so its origin rules apply and you get its reply",
            b.name, b.name
        ));
    }
    guard::check_claude_origin(info.kind, caller_origin(st, caller).0)?;
    if let (Some(me), Some(ch)) = (
        st.backend.session(caller),
        st.backend.channel(info.channel_id),
    ) {
        check_not_more_permissive(st, &me, &ch, info.kind)?;
    }
    if info.kind == SessionKind::Shell && !st.limits.allow_shell {
        return Err(format!(
            "session {target} is a shell: a message would run as a shell command, which agents may not do. {HOW_SHELL}"
        ));
    }
    let text = clean_text(text);
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    check_no_shell_escape(st, info.kind, &text)?;
    if info.state == AgentState::Exited {
        return Err(format!("session {target} has exited"));
    }
    if interrupt && info.state == AgentState::NeedsApproval {
        return Err(format!(
            "session {target} is waiting on a permission prompt; only the human can answer it. \
             Send without interrupt to queue the message, or post_to_channel to ask the human."
        ));
    }
    let now = now_ms();
    let prev_send = st.edit_guard(|g| {
        if let Err(wait_s) = g.sends.check_and_record(
            caller,
            now,
            st.limits.send_per_window,
            st.limits.send_window_ms,
        ) {
            return Err(format!(
                "rate limit: at most {} messages per minute. Try again in {wait_s}s.",
                st.limits.send_per_window
            ));
        }
        Ok(g.last_send.insert(target, now))
    })??;
    let mode = if interrupt {
        SendMode::Interrupt
    } else {
        SendMode::Queue
    };
    let out = match st
        .backend
        .send(
            target,
            text,
            mode,
            format!("session:{caller}"),
            crate::agents::Source::Chain(caller_route(st, caller).prov(None)),
        )
        .await
    {
        Ok(out) => out,
        Err(e) => {
            // Only a refusal before durable queue acceptance can be refunded. A
            // partially submitted or retained prompt still counts as agent work.
            if e.downcast_ref::<crate::inject::DeliveryPending>().is_none() {
                if let Err(error) = st.edit_guard(|g| {
                    g.sends.refund(caller);
                    match prev_send {
                        Some(t) => g.last_send.insert(target, t),
                        None => g.last_send.remove(&target),
                    };
                }) { tracing::warn!("{error}"); }
            }
            return Err(format!("send failed: {e:#}"));
        }
    };
    if !out.delivered {
        if let Err(error) = st.edit_guard(|g| g.note_queued(target, out.queued)) { tracing::warn!("{error}"); }
    }
    Ok(pretty(&json!({
        "session_id": target,
        "delivered": out.delivered,
        "queued": out.queued,
        "sent_at_ms": now,
    })))
}

fn read_screen<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let id = a.session_id("session_id")?;
    let info = reachable_session(st, caller, id)?;
    let text = st
        .backend
        .screen_text(id)
        .ok_or_else(|| format!("no screen available for session {id}"))?;
    Ok(format!(
        "[session {id} ({}) state={}]\n{}",
        info.kind.as_str(),
        info.state.as_str(),
        text.trim_end()
    ))
}

fn get_recent_output<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let id = a.session_id("session_id")?;
    reachable_session(st, caller, id)?;
    let lines = a
        .int("lines")?
        .unwrap_or(st.limits.recent_default_lines as i64)
        .clamp(1, st.limits.recent_max_lines as i64) as usize;
    st.backend
        .recent_output(id, lines)
        .ok_or_else(|| format!("no output available for session {id}"))
}

async fn wait_for_state<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let id = a.session_id("session_id")?;
    let states: Vec<AgentState> = match a.get("states") {
        None => wait::DEFAULT_STATES.to_vec(),
        Some(Value::Array(items)) if !items.is_empty() => {
            let mut v = Vec::new();
            for it in items {
                let s = it.as_str().ok_or("`states` must be an array of strings")?;
                v.push(parse_state(s).ok_or_else(|| {
                    format!(
                        "unknown state `{s}` (expected one of {})",
                        STATE_NAMES.join(", ")
                    )
                })?);
            }
            v
        }
        Some(Value::String(s)) => {
            vec![parse_state(s).ok_or_else(|| format!("unknown state `{s}`"))?]
        }
        Some(_) => return Err("`states` must be a non-empty array of strings".into()),
    };
    let timeout_s = a
        .int("timeout_seconds")?
        .unwrap_or(st.limits.wait_default_s as i64)
        .clamp(0, st.limits.wait_max_s as i64) as u64;
    let after_ms = a.int("after_ms")?;
    reachable_session(st, caller, id)?;
    let (info, verdict, waited_ms) = wait_loop(st, id, &states, timeout_s, after_ms).await?;
    Ok(pretty(&json!({
        "session_id": id,
        "state": info.state,
        "detail": info.state_detail,
        "state_since_ms": info.state_since,
        "queued": info.queued,
        "reached": verdict == Verdict::Reached,
        "timed_out": verdict == Verdict::Wait,
        "ended": (verdict == Verdict::Ended).then_some("exited"),
        "waited_ms": waited_ms,
    })))
}

/// The wait_for_state loop: until `id` is in one of `states` (staleness rules in mcp::wait),
/// exits, or `timeout_s` passes. Returns the last info, the verdict (`Wait` = timed out) and
/// the time waited.
async fn wait_loop<B: Backend>(
    st: &McpState<B>,
    id: SessionId,
    states: &[AgentState],
    timeout_s: u64,
    after_ms: Option<i64>,
) -> Result<(SessionInfo, Verdict, u64), String> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_s);
    let mut rx = st.backend.subscribe();
    loop {
        let info = st
            .backend
            .session(id)
            .ok_or_else(|| format!("session {id} was removed while waiting"))?;
        let now_unix = now_ms();
        let (last_send_ms, last_delivery_ms) = st.edit_guard(|g| {
            // Also observe here: the tracker task may not have seen this update yet.
            g.observe_session(&info, now_unix);
            (
                g.last_send.get(&id).copied(),
                g.last_delivery.get(&id).copied(),
            )
        })?;
        let ctx = WaitCtx {
            after_ms,
            last_send_ms,
            last_delivery_ms,
            settle_grace_ms: st.limits.settle_grace_ms,
            now_ms: now_unix,
        };
        let verdict = wait::evaluate(&info, states, &ctx);
        let now = Instant::now();
        if verdict != Verdict::Wait || now >= deadline {
            return Ok((info, verdict, started.elapsed().as_millis() as u64));
        }
        // Re-check at least every second (grace expiry, missed events).
        let tick = (deadline - now).min(Duration::from_secs(1));
        wait::next_change(&mut rx, id, tick).await;
    }
}

fn list_agents<B: Backend>(st: &McpState<B>, caller: SessionId) -> TResult {
    let agents: Vec<Value> = st
        .backend
        .agents()
        .iter()
        .map(|b| {
            let s = b.session_id.and_then(|s| st.backend.session(s));
            let mut v = crate::agents::agent_json(b, s.as_ref());
            v["is_me"] = json!(b.session_id == Some(caller));
            v
        })
        .collect();
    let (origin, hop) = caller_origin(st, caller);
    Ok(pretty(&json!({
        "agents": agents,
        "your_origin": {"origin": origin, "hop": hop},
    })))
}

/// Agents are a shared service: any session may ask one, whatever its workspace (asking runs no
/// command anywhere, and the origin rule, hop limit, daily cap and permission check apply).
/// Driving an agent's terminal with the other tools stays under the workspace rule.
async fn ask_agent<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    // `bot`: the argument's name under the old `ask_bot`.
    let name = match a.req_str("agent") {
        Ok(n) => n,
        Err(e) => a.req_str("bot").map_err(|_| e)?,
    };
    let text = a.req_str("text")?;
    let timeout_s = a
        .int("timeout_seconds")?
        .unwrap_or(AGENT_ASK_DEFAULT_S)
        .clamp(1, st.limits.wait_max_s as i64) as u64;
    let agent = st
        .backend
        .agents()
        .into_iter()
        .find(|b| b.name.eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| {
            let names: Vec<String> = st.backend.agents().into_iter().map(|b| b.name).collect();
            format!(
                "no agent named {name} ({})",
                if names.is_empty() { "there are none; the human creates them with `muster agent new`".to_string() } else { format!("agents: {}", names.join(", ")) }
            )
        })?;
    if agent.session_id == Some(caller) {
        return Err("you cannot ask yourself".into());
    }
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    if text.chars().count() > st.limits.max_message_chars {
        return Err(format!("text too long (max {} characters)", st.limits.max_message_chars));
    }
    let text = clean_text(text);
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    check_no_shell_escape(st, agent.kind, &text)?;
    let me = caller_session(st, caller)?;
    let from = crate::agents::sender_label(st.backend.agents().iter().find(|b| b.session_id == Some(caller)), caller);
    // Policy refusals (permissions, origin, hops, cap) are kept for the agents panel's log.
    let refused = |why: String| {
        let proposed = crate::agents::proposed_route(st.backend.prov(caller), None);
        st.backend
            .agent_refused(crate::agents::message_record(&agent, &from, Some(caller), proposed, &text), &why);
        why
    };
    // The widest profile the message could reach: the agent session's current tab and the agent's
    // own tab (what its next restart gets), else the profile its tab will get.
    let session_ch = agent.session_id.and_then(|s| st.backend.session(s)).and_then(|s| st.backend.channel(s.channel_id));
    let own_ch = agent.channel_id.and_then(|c| st.backend.channel(c));
    let (ch_id, perms) = crate::agents::reach_profile(&agent, session_ch.as_ref(), own_ch.as_ref());
    check_perms_not_wider(st, &me, ch_id, &perms, agent.kind, &format!("agent {}'s tab", agent.name)).map_err(refused)?;
    let recent = st
        .backend
        .agent_messages_since(agent.id, now_ms() - guard::AGENT_CAP_WINDOW_MS);
    let route = crate::agents::route(&agent, st.backend.prov(caller), None, recent).map_err(refused)?;
    if let Err(wait_s) = st.edit_guard(|g| g.sends.check_and_record(
        caller, now_ms(), st.limits.send_per_window, st.limits.send_window_ms,
    ))? {
        return Err(format!(
            "rate limit: at most {} messages per minute. Try again in {wait_s}s.",
            st.limits.send_per_window
        ));
    }
    let out = crate::agents::Outgoing {
        agent: agent.name.clone(),
        text,
        from,
        from_session: Some(caller),
        route,
    };
    let d = match st.backend.agent_deliver(out).await {
        Ok(d) => d,
        Err(e) => {
            if e.downcast_ref::<crate::inject::DeliveryPending>().is_none() {
                if let Err(error) = st.edit_guard(|g| g.sends.refund(caller)) { tracing::warn!("{error}"); }
            }
            return Err(format!("could not deliver to agent {}: {e:#}", agent.name));
        }
    };
    if let Err(error) = st.edit_guard(|g| {
        g.last_send.insert(d.session_id, d.sent_at_ms);
        if !d.delivered { g.note_queued(d.session_id, d.queued); }
    }) { tracing::warn!("{error}"); }
    // Wait for this message's own turn (not whatever runs before it in the queue).
    let mut rx = st.backend.subscribe();
    let (info, verdict, waited_ms) = crate::agents::wait_for_reply(
        &mut rx,
        d.session_id,
        Duration::from_secs(timeout_s),
        || st.backend.session(d.session_id),
        || st.backend.agent_message_status(d.msg_id),
    )
    .await?;
    let tail = (verdict == crate::agents::AskVerdict::Unattributed)
        .then(|| st.backend.recent_output(d.session_id, AGENT_FALLBACK_LINES))
        .flatten();
    Ok(pretty(&crate::agents::ask_result(&agent.name, &d, route, &info, &verdict, waited_ms, tail)))
}

/// ask_agent's default wait, and how much terminal output stands in for a reply no hook
/// attributed to the message.
const AGENT_ASK_DEFAULT_S: i64 = 600;
const AGENT_FALLBACK_LINES: usize = 60;

fn post_to_channel<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let text = a.req_str("text")?;
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    if text.chars().count() > st.limits.max_post_chars {
        return Err(format!(
            "text too long (max {} characters)",
            st.limits.max_post_chars
        ));
    }
    let ch_id = match a.int("channel_id")? {
        Some(c) => c,
        None => caller_session(st, caller)?.channel_id,
    };
    let ch = open_channel(st, ch_id)?;
    check_channel_reach(st, caller, &ch)?;
    if let Err(wait_s) = st.edit_guard(|g| g.posts.check_and_record(
        caller, now_ms(), st.limits.post_per_window, st.limits.send_window_ms,
    ))? {
        return Err(format!(
            "rate limit: at most {} posts per minute. Try again in {wait_s}s.",
            st.limits.post_per_window
        ));
    }
    st.backend.log_event(
        Some(ch.id),
        Some(caller),
        EventKind::AgentPost,
        json!({"session_id": caller, "text": text}),
    );
    Ok(format!("Posted to channel {} (#{}).", ch.id, ch.name))
}

async fn get_diff<B: Backend>(st: &McpState<B>, caller: SessionId, a: &Args<'_>) -> TResult {
    let ch_id = match a.int("channel_id")? {
        Some(c) => c,
        None => caller_session(st, caller)?.channel_id,
    };
    let max = a
        .int("max_bytes")?
        .unwrap_or(st.limits.diff_default_bytes as i64)
        .clamp(1000, st.limits.diff_max_bytes as i64) as usize;
    let ch = open_channel(st, ch_id)?;
    check_channel_reach(st, caller, &ch)?;
    let diff = st
        .backend
        .diff(ch.cwd.clone(), ch.base_branch.clone())
        .await
        .map_err(|e| format!("diff failed: {e:#}"))?;
    if diff.trim().is_empty() {
        return Ok(format!(
            "No changes in channel {} against {}.",
            ch.id,
            ch.base_branch.as_deref().unwrap_or("HEAD")
        ));
    }
    if diff.len() <= max {
        return Ok(diff);
    }
    let cut = truncate_str(&diff, max);
    let cut = &cut[..cut.rfind('\n').map(|i| i + 1).unwrap_or(cut.len())];
    let files: Vec<&str> = diff
        .lines()
        .filter_map(|l| l.strip_prefix("diff --git "))
        .collect();
    const MAX_FILES: usize = 200;
    let listed = files.len().min(MAX_FILES);
    let mut file_list = files[..listed].join("\n");
    if files.len() > listed {
        file_list.push_str(&format!("\n… and {} more", files.len() - listed));
    }
    Ok(format!(
        "{cut}\n[diff truncated: showing {} of {} bytes. Files in full diff ({}):\n{}\nRaise max_bytes (max {}) or run `git diff` in {} for the rest.]",
        cut.len(),
        diff.len(),
        files.len(),
        file_list,
        st.limits.diff_max_bytes,
        ch.cwd
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_complete_and_ordered() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            assert_eq!(d["inputSchema"]["type"], "object", "{}", d["name"]);
            assert!(d["description"].as_str().unwrap().len() > 30);
        }
        assert!(!names.iter().any(|n| n.contains("approve")));
    }

    #[test]
    fn args_are_lenient_about_numeric_strings() {
        let m = json!({"session_id": "7", "lines": 5.0, "x": null, "b": "true"});
        let a = Args(m.as_object().unwrap());
        assert_eq!(a.session_id("session_id").unwrap(), 7);
        assert_eq!(a.int("lines").unwrap(), Some(5));
        assert_eq!(a.int("x").unwrap(), None);
        assert_eq!(a.bool("b").unwrap(), Some(true));
        assert!(a.req_str("text").unwrap_err().contains("missing"));
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_str("héllo", 2), "h");
        let big = "x".repeat(2000);
        let t = truncate_args(&json!({"text": big, "n": 1}));
        assert!(t["text"].as_str().unwrap().ends_with("[2000 bytes]"));
        assert_eq!(t["n"], 1);
        let huge = json!({"a": vec!["y".repeat(400); 50]});
        assert!(truncate_args(&huge).get("_truncated").is_some());
    }

    #[test]
    fn clean_text_keeps_text_only() {
        assert_eq!(
            clean_text("a\x1b[201~\r1\r\n\tb\u{9b}c\u{7f}"),
            "a[201~\n1\n\tbc"
        );
        assert_eq!(clean_text("héllo\nwörld"), "héllo\nwörld");
    }

    #[test]
    fn done_alias() {
        assert_eq!(parse_state("done"), Some(AgentState::Idle));
        assert_eq!(
            parse_state("needs_approval"),
            Some(AgentState::NeedsApproval)
        );
        assert_eq!(parse_state("nope"), None);
    }
}

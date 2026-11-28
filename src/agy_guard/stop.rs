//! Adapt agy 1.5.13's current turn and the replies of Claude Code Stop hooks.
use super::{Config, Decision, MAX_STDIN, claude_call, run_guards};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Deserialize)]
struct Step {
    step_index: u64,
    source: String,
    #[serde(rename = "type")]
    kind: String,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<Call>,
    #[serde(default)]
    truncated_fields: Vec<String>,
}

#[derive(Deserialize)]
struct Call {
    name: String,
    args: Map<String, Value>,
}

// Read from the end so old turns neither exhaust the budget nor reach a guard.
fn current_turn(path: &Path) -> Result<Vec<Step>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "expected a transcript file");
    let start = metadata.len().saturating_sub(MAX_STDIN as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.take(metadata.len() - start).read_to_end(&mut tail)?;
    let mut lines = tail.split(|&b| b == b'\n');
    if start != 0 {
        lines.next();
    } // The first line may be partial.
    let mut turn = Vec::new();
    for line in lines.rev().filter(|l| !l.is_empty()) {
        let step: Step = serde_json::from_slice(line)?;
        let user = step.kind == "USER_INPUT" && step.source == "USER_EXPLICIT";
        turn.push(step);
        if user {
            turn.reverse();
            return Ok(turn);
        }
    }
    anyhow::bail!("missing user turn")
}

fn message(out: &mut impl Write, role: &str, blocks: Vec<Value>) -> Result<()> {
    if !blocks.is_empty() {
        serde_json::to_writer(
            &mut *out,
            &json!({"type": role, "message": {"role": role, "content": blocks}}),
        )?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn convert(turn: Vec<Step>, compact: bool, out: &mut impl Write) -> Result<()> {
    let mut pending = VecDeque::new();
    for row in turn {
        let text = row.content.as_deref();
        match (row.kind.as_str(), row.source.as_str()) {
            ("USER_INPUT", "USER_EXPLICIT") => {
                let content = text.context("missing user input")?;
                let request = content
                    .strip_prefix("<USER_REQUEST>")
                    .and_then(|s| {
                        s.rsplit_once("</USER_REQUEST>")
                            .map(|(request, _)| request.trim())
                    })
                    .unwrap_or(content);
                message(out, "user", vec![json!({"type": "text", "text": request})])?;
            }
            ("PLANNER_RESPONSE", "MODEL") => {
                let mut blocks = Vec::new();
                pending.clear();
                if let Some(text) = text {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for (index, mut call) in row.tool_calls.into_iter().enumerate() {
                    if compact {
                        // The compact log renders each argument as JSON text. The full log
                        // stores native values, including literal strings that look like JSON.
                        for value in call.args.values_mut() {
                            if let Some(encoded) = value.as_str() {
                                *value = serde_json::from_str(encoded)?;
                            }
                        }
                    }
                    let args = Value::Object(call.args);
                    let (name, input) = claude_call(&call.name, &args)
                        .map_err(anyhow::Error::msg)?
                        .unwrap_or((call.name, args));
                    let id = format!("call-{}-{index}", row.step_index);
                    blocks.push(json!({"type":"tool_use", "id":id, "name":name, "input":input}));
                    pending.push_back(id);
                }
                message(out, "assistant", blocks)?;
            }
            ("GENERIC", "MODEL") => {
                if let Some(id) = pending.pop_front() {
                    message(
                        out,
                        "user",
                        vec![json!({"type":"tool_result", "tool_use_id":id,
                        "content":text.context("missing tool result")?})],
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn transcript(path: &Path, out: &mut impl Write) -> Result<bool> {
    let mut turn = current_turn(path)?;
    let mut compact = !path.ends_with("transcript_full.jsonl");
    if compact && turn.iter().any(|s| !s.truncated_fields.is_empty()) {
        let full = current_turn(&path.with_file_name("transcript_full.jsonl"))?;
        ensure!(
            turn.iter()
                .map(|s| s.step_index)
                .eq(full.iter().map(|s| s.step_index)),
            "full transcript has a different turn"
        );
        turn = full;
        compact = false;
    }
    // In print mode 1.5.13 sometimes repeats continuationActive=false. Its own
    // typed system message records the continuation in the same user turn.
    let continued = turn.iter().any(|s| {
        s.kind == "SYSTEM_MESSAGE"
            && s.source == "SYSTEM"
            && s.content.as_deref().is_some_and(|text| {
                text.contains("\n<SYSTEM_MESSAGE>\nStop hook blocked termination:")
            })
    });
    convert(turn, compact, out)?;
    Ok(continued)
}

fn answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> (Decision, Option<String>) {
    match code {
        Some(2) => (
            Decision::Deny,
            Some(String::from_utf8_lossy(stderr).trim().into()),
        ),
        Some(0) => {
            if let Ok(v) = serde_json::from_slice::<Value>(stdout)
                && v["decision"] == "block"
            {
                return (Decision::Deny, v["reason"].as_str().map(str::to_owned));
            }
            (Decision::Allow, None)
        }
        _ => (Decision::Allow, None),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    conversation_id: String,
    transcript_path: std::path::PathBuf,
    cwd: std::path::PathBuf,
    #[serde(default)]
    continuation_active: bool,
}

pub(super) fn decide(payload: &[u8], config: Config, dir: &Path) -> Result<Value> {
    let p: Payload = serde_json::from_slice(payload)?;
    let allow = json!({"decision":"allow"});
    if config.stop.is_empty() {
        return Ok(allow);
    }
    let mut file = tempfile::Builder::new()
        .prefix(".agy-stop-")
        .suffix(".jsonl")
        .tempfile_in(dir)?;
    let continued = {
        let mut out = std::io::BufWriter::new(file.as_file_mut());
        let continued = transcript(&p.transcript_path, &mut out)?;
        out.flush()?;
        continued
    };
    let input = serde_json::to_vec(&json!({
        "hook_event_name": "Stop", "session_id": p.conversation_id, "cwd": p.cwd,
        "stop_hook_active": p.continuation_active, "transcript_path": file.path()
    }))?;
    let answers = run_guards(config.stop.iter().collect(), &input, &p.cwd, answer);
    // The follow-up hook still sees the active flag. Even an unconditional blocker gets one retry.
    if !p.continuation_active && !continued && answers.iter().any(|a| a.0 == Decision::Deny) {
        Ok(json!({"decision": "continue", "reason": super::combine(&answers)["reason"]}))
    } else {
        Ok(allow)
    }
}

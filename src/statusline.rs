//! `muster statusline claude`: Muster's usage reporter inside a Claude session's status line
//! (src/usage.rs `reporter_command`). The composed status-line command runs the user's own
//! status line itself and gives this reporter a copy of Claude Code's status-line JSON in the
//! background, with its output discarded: nothing here can change, delay or break what the
//! user sees, and an older Muster binary that rejects the subcommand is harmless too.
//!
//! It POSTs only `rate_limits.five_hour` / `seven_day`, `cost.total_api_duration_ms` (the
//! daemon's evidence that the numbers came with a newer API response) and the conversation's
//! size (`context`: its id, `session_id`, and the tokens of its last request,
//! `context_window.current_usage`, see [`context_of`]) to `$MUSTER_URL/usage/claude` with the
//! session token, through `hookcmd::post`, which connects only to a loopback address. Never the
//! rest of the payload. Before the run's first response there are no windows yet and it sends the
//! API time alone, the baseline the next report is measured against. The daemon attributes it by
//! token; the payload names no account. It prints nothing, always exits 0 and gives up after
//! [`POST_BUDGET`].

use std::time::Duration;

use serde_json::{Value, json};

/// How long the reporter waits for the daemon before it exits anyway.
const POST_BUDGET: Duration = Duration::from_millis(600);

pub fn run(agent: &str) -> i32 {
    let input = crate::hookcmd::read_stdin();
    let Some(body) = (agent == "claude").then(|| report_body(&input)).flatten() else { return 0 };
    let (Some(url), Some(token)) = (std::env::var("MUSTER_URL").ok().filter(|u| !u.is_empty()),
        crate::hookcmd::session_token()) else { return 0 };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::hookcmd::post(&url, "/usage/claude", &token, "claude", &body));
    });
    let r = rx.recv_timeout(POST_BUDGET);
    if std::env::var_os("MUSTER_HOOK_DEBUG").is_some() {
        eprintln!("muster statusline: {r:?}");
    }
    0
}

/// The conversation's size as its last model request saw it: the conversation id (`session_id`,
/// the id Claude Code resumes) and `context_window.current_usage`'s input + cache-write +
/// cache-read tokens, the sum Claude Code's own `total_input_tokens` and context percentage use.
/// None before the conversation's first response (`current_usage` is null then) or when a field
/// is missing. Shape checked against Claude Code 2.1.318's real status-line input
/// (src/fixtures/claude_statusline_2.1.318.json, values invented). `total_input_tokens` itself is
/// not read: older versions summed the whole session there.
pub(crate) fn context_of(v: &Value) -> Option<Value> {
    let id = v.get("session_id").and_then(Value::as_str).filter(|s| crate::agents::valid_resume_id(s))?;
    let u = v.get("context_window")?.get("current_usage")?;
    let mut tokens: u64 = 0;
    for key in ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"] {
        tokens = tokens.checked_add(u.get(key)?.as_u64()?)?;
    }
    Some(json!({"conversation": id, "tokens": tokens}))
}

/// Only the documented usage windows, the run's API time and the conversation's size; None when
/// it has none of them.
pub(crate) fn report_body(input: &[u8]) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(input).ok()?;
    let mut windows = serde_json::Map::new();
    for key in ["five_hour", "seven_day"] {
        let Some(w) = v.get("rate_limits").and_then(|l| l.get(key)) else { continue };
        let (Some(used), Some(resets)) = (w.get("used_percentage").filter(|x| x.is_number()),
            w.get("resets_at").filter(|x| x.is_number())) else { continue };
        windows.insert(key.into(), json!({"used_percentage": used, "resets_at": resets}));
    }
    let api = v.get("cost").and_then(|c| c.get("total_api_duration_ms")).filter(|x| x.is_number());
    let context = context_of(&v);
    if windows.is_empty() && api.is_none() && context.is_none() {
        return None;
    }
    let mut body = json!({"rate_limits": windows, "api_duration_ms": api});
    if let Some(c) = context {
        body["context"] = c;
    }
    serde_json::to_vec(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_carries_only_the_usage_numbers_and_the_api_time() {
        let input = json!({
            "session_id": "abc", "transcript_path": "/home/x/t.jsonl", "cwd": "/home/x/secret-project",
            "cost": {"total_api_duration_ms": 2300, "total_cost_usd": 1.5},
            "rate_limits": {"five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600},
                "spend_limit": {"used_percentage": 3, "resets_at": 1}}
        });
        let body: Value = serde_json::from_slice(&report_body(input.to_string().as_bytes()).unwrap()).unwrap();
        assert_eq!(body, json!({"rate_limits": {
            "five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
            "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}}, "api_duration_ms": 2300}));
        assert!(!body.to_string().contains("secret") && !body.to_string().contains("t.jsonl"));
        // Before the first response: the API time alone, the daemon's baseline.
        let start: Value = serde_json::from_slice(&report_body(
            br#"{"transcript_path": "/t", "cost": {"total_api_duration_ms": 0}}"#).unwrap()).unwrap();
        assert_eq!(start, json!({"rate_limits": {}, "api_duration_ms": 0}));
        // Neither, malformed: nothing to send; partial: only the complete window.
        assert_eq!(report_body(b"{}"), None);
        assert_eq!(report_body(b"not json"), None);
        assert_eq!(report_body(br#"{"rate_limits": {"five_hour": {"used_percentage": "x", "resets_at": 1}}}"#), None);
        let partial: Value = serde_json::from_slice(&report_body(
            br#"{"rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}}}"#).unwrap()).unwrap();
        assert_eq!(partial, json!({"rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}},
            "api_duration_ms": null}));
    }

    /// Claude Code 2.1.318's real status-line input (paths and numbers invented): the
    /// conversation id and the three input counts of its last request, nothing else of it.
    #[test]
    fn the_report_carries_the_conversations_size_from_the_real_input() {
        let real = include_str!("fixtures/claude_statusline_2.1.318.json");
        let body: Value = serde_json::from_slice(&report_body(real.as_bytes()).unwrap()).unwrap();
        assert_eq!(body["context"], json!({"conversation": "11111111-2222-4333-8444-555555555555", "tokens": 216_003}));
        let text = body.to_string();
        assert!(!text.contains("alpha") && !text.contains("demo-app") && !text.contains("opus"), "{text}");
        // Before the conversation's first response Claude Code sends `current_usage: null`.
        let mut start: Value = serde_json::from_str(real).unwrap();
        start["context_window"]["current_usage"] = Value::Null;
        assert_eq!(context_of(&start), None);
        let body: Value = serde_json::from_slice(&report_body(start.to_string().as_bytes()).unwrap()).unwrap();
        assert!(body.get("context").is_none());
        // A missing count, a non-number or an id that isn't a conversation id: no size.
        let mut v: Value = serde_json::from_str(real).unwrap();
        v["context_window"]["current_usage"]["cache_read_input_tokens"] = json!("x");
        assert_eq!(context_of(&v), None);
        let mut v: Value = serde_json::from_str(real).unwrap();
        v["session_id"] = json!("--resume");
        assert_eq!(context_of(&v), None);
        // The size alone is still sent (no usage windows yet, no API time).
        let only = json!({"session_id": "abc-1", "context_window": {"current_usage":
            {"input_tokens": 1, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 9}}});
        let body: Value = serde_json::from_slice(&report_body(only.to_string().as_bytes()).unwrap()).unwrap();
        assert_eq!(body["context"], json!({"conversation": "abc-1", "tokens": 6}));
    }
}

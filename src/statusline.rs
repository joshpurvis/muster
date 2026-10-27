//! `muster statusline claude`: Muster's usage reporter inside a Claude session's status line
//! (src/usage.rs `reporter_command`). The composed status-line command runs the user's own
//! status line itself and gives this reporter a copy of Claude Code's status-line JSON in the
//! background, with its output discarded: nothing here can change, delay or break what the
//! user sees, and an older Muster binary that rejects the subcommand is harmless too.
//!
//! It POSTs only `rate_limits.five_hour` / `seven_day`, `cost.total_api_duration_ms` and the
//! transcript's modification time (when Claude Code last wrote the conversation: evidence of
//! when the numbers were fetched) to `$MUSTER_URL/usage/claude` with the session token. Never the
//! rest of the payload. The daemon attributes it by token; the payload names no account. It
//! prints nothing, always exits 0 and gives up after [`POST_BUDGET`].

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

/// Only the documented usage windows, the run's API time and when the transcript was last
/// written; None when there are no windows.
pub(crate) fn report_body(input: &[u8]) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(input).ok()?;
    let limits = v.get("rate_limits")?;
    let mut windows = serde_json::Map::new();
    for key in ["five_hour", "seven_day"] {
        let Some(w) = limits.get(key) else { continue };
        let (Some(used), Some(resets)) = (w.get("used_percentage").filter(|x| x.is_number()),
            w.get("resets_at").filter(|x| x.is_number())) else { continue };
        windows.insert(key.into(), json!({"used_percentage": used, "resets_at": resets}));
    }
    if windows.is_empty() {
        return None;
    }
    let api = v.get("cost").and_then(|c| c.get("total_api_duration_ms")).filter(|x| x.is_number());
    // Only the time is read (stat), never the transcript itself.
    let data_at = v.get("transcript_path").and_then(Value::as_str)
        .filter(|p| std::path::Path::new(p).is_absolute())
        .and_then(|p| std::fs::metadata(p).ok()?.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    serde_json::to_vec(&json!({"rate_limits": windows, "api_duration_ms": api, "data_at": data_at})).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_carries_only_the_usage_numbers_and_the_transcript_time() {
        let dir = std::env::temp_dir().join(format!("muster-sl-{}", crate::paths::random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t.jsonl");
        std::fs::write(&transcript, "{\"secret\": \"conversation text\"}\n").unwrap();
        let mtime = std::fs::metadata(&transcript).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        let input = json!({
            "session_id": "abc", "transcript_path": transcript, "cwd": "/home/x/secret-project",
            "cost": {"total_api_duration_ms": 2300, "total_cost_usd": 1.5},
            "rate_limits": {"five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600},
                "spend_limit": {"used_percentage": 3, "resets_at": 1}}
        });
        let body: Value = serde_json::from_slice(&report_body(input.to_string().as_bytes()).unwrap()).unwrap();
        assert_eq!(body, json!({"rate_limits": {
            "five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
            "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}}, "api_duration_ms": 2300,
            "data_at": mtime}));
        assert!(!body.to_string().contains("secret"));
        // Absent, partial or malformed: nothing to send, or only the complete window.
        assert_eq!(report_body(b"{}"), None);
        assert_eq!(report_body(b"not json"), None);
        assert_eq!(report_body(br#"{"rate_limits": {"five_hour": {"used_percentage": "x", "resets_at": 1}}}"#), None);
        let partial: Value = serde_json::from_slice(&report_body(
            br#"{"transcript_path": "relative.jsonl", "rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}}}"#).unwrap()).unwrap();
        assert_eq!(partial, json!({"rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}},
            "api_duration_ms": null, "data_at": null}));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

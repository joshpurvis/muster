//! `muster statusline claude [COMMAND]`: the status-line command of every Claude session Muster
//! starts (src/usage.rs `install_reporter`). Claude Code runs it with its status-line JSON on
//! stdin and shows what it prints.
//!
//! - COMMAND is the user's own status line (from their settings): it gets the same stdin, the same
//!   environment and working directory, and its stdout, stderr and exit code are passed through
//!   untouched, so what the user sees does not change. Without one, nothing is printed.
//! - Alongside it, only `rate_limits.five_hour` / `seven_day` and `cost.total_api_duration_ms`
//!   are POSTed to `$MUSTER_URL/usage/claude` with the session token (never the rest of the
//!   payload). The daemon attributes it by token; the payload names no account.
//! - Reporting can never break the line: it runs on its own thread, its errors are silent, and
//!   it gets at most [`POST_BUDGET`] from the start before the process exits anyway. Claude Code
//!   cancels a status-line run by killing it; on Linux the user's command is then signalled too.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long the process may wait for its report after the user's command is done.
const POST_BUDGET: Duration = Duration::from_millis(600);

pub fn run(agent: &str, inner: Option<&str>) -> i32 {
    let start = Instant::now();
    let input = crate::hookcmd::read_stdin();
    let report = (agent == "claude").then(|| report_body(&input)).flatten();
    let posted = report.and_then(|body| {
        let url = std::env::var("MUSTER_URL").ok().filter(|u| !u.is_empty())?;
        let token = crate::hookcmd::session_token()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::hookcmd::post(&url, "/usage/claude", &token, "claude", &body));
        });
        Some(rx)
    });
    let code = match inner.filter(|c| !c.trim().is_empty()) {
        Some(cmd) => run_user_command(cmd, input),
        None => 0,
    };
    if let Some(rx) = posted {
        let left = POST_BUDGET.saturating_sub(start.elapsed());
        let r = rx.recv_timeout(left);
        if std::env::var_os("MUSTER_HOOK_DEBUG").is_some() {
            eprintln!("muster statusline: {r:?}");
        }
    }
    code
}

/// The user's command through `/bin/sh -c`, as Claude Code itself runs status lines.
fn run_user_command(cmd: &str, input: Vec<u8>) -> i32 {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd).stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            c.pre_exec(|| {
                // When Claude Code cancels this run (kills this process), stop the user's too.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let mut child = match c.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("muster statusline: could not run the status line command: {e}");
            return 127;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A command that never reads its input must not block on it.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

/// Only the documented usage windows and the run's API time; None when there are no windows.
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
    serde_json::to_vec(&json!({"rate_limits": windows, "api_duration_ms": api})).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_carries_only_the_usage_numbers() {
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
        // Absent, partial or malformed: nothing to send, or only the complete window.
        assert_eq!(report_body(b"{}"), None);
        assert_eq!(report_body(b"not json"), None);
        assert_eq!(report_body(br#"{"rate_limits": {"five_hour": {"used_percentage": "x", "resets_at": 1}}}"#), None);
        let partial: Value = serde_json::from_slice(&report_body(
            br#"{"rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}}}"#).unwrap()).unwrap();
        assert_eq!(partial, json!({"rate_limits": {"seven_day": {"used_percentage": 5, "resets_at": 9}}, "api_duration_ms": null}));
    }
}

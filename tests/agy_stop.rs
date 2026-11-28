//! Exercise the fast hook dispatcher and an unchanged command reading its transcript.
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn hook(dir: &Path, payload: &[u8]) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_muster"))
        .args(["hook", "antigravity", "stop"])
        .arg(dir.join("guards.json"))
        .env("MUSTER_RUNTIME_DIR", dir.join("run"))
        .env("MUSTER_DATA_DIR", dir.join("data"))
        .env("MUSTER_PORT", "0")
        .env("MUSTER_SESSION_ID", "23")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn stop_guard_sees_the_turn_and_can_continue_once() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::write(
        dir.join("transcript_full.jsonl"),
        include_bytes!("../src/fixtures/antigravity_1.5.13/transcript_full.jsonl"),
    )
    .unwrap();
    std::fs::write(
        dir.join("check.py"),
        r#"import json, os, stat, sys
from pathlib import Path
event = json.load(sys.stdin)
assert event["hook_event_name"] == "Stop" and event["session_id"] == "alpha"
assert Path(event["cwd"]) == Path.cwd() and os.environ["MUSTER_SESSION_ID"] == "23"
path = Path(event["transcript_path"])
assert path.parent == Path.cwd() and stat.S_IMODE(path.stat().st_mode) == 0o600
Path("seen.json").write_text(json.dumps(event))
for line in path.read_text().splitlines():
    for block in json.loads(line)["message"]["content"]:
        if block.get("name") == "mcp__muster__whoami":
            print(json.dumps({"decision": "block", "reason": "Check alpha"}))
            sys.exit()
"#,
    )
    .unwrap();
    let configure = |command: &str| {
        let config =
            json!({"cwd":dir,"guards":[],"stop":[{"matcher":"","command":command,"timeout_s":1}]});
        std::fs::write(dir.join("guards.json"), config.to_string()).unwrap();
    };
    let payload = |active: bool| {
        json!({"conversationId":"alpha","cwd":dir,"transcriptPath":dir.join("transcript_full.jsonl"),
        "fullyIdle":true,"terminationReason":"NO_TOOL_CALL","continuationActive":active})
        .to_string()
    };
    let allow = json!({"decision":"allow"});
    configure("python3 -I check.py");
    for active in [false, true] {
        let answer = hook(dir, payload(active).as_bytes());
        assert_eq!(
            answer,
            if active {
                allow.clone()
            } else {
                json!({"decision":"continue","reason":"Check alpha"})
            }
        );
        let seen: Value =
            serde_json::from_slice(&std::fs::read(dir.join("seen.json")).unwrap()).unwrap();
        assert_eq!(seen["stop_hook_active"], active);
        assert!(!Path::new(seen["transcript_path"].as_str().unwrap()).exists());
        std::fs::remove_file(dir.join("seen.json")).unwrap();
    }
    configure("echo 'Check beta' >&2; exit 2");
    assert_eq!(
        hook(dir, payload(false).as_bytes()),
        json!({"decision":"continue","reason":"Check beta"})
    );
    for command in [
        "echo '{}'",
        "exit 1",
        "printf invalid",
        "echo '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'",
        "sleep 10",
    ] {
        configure(command);
        assert_eq!(hook(dir, payload(false).as_bytes()), allow);
    }
    // Real print-mode agy can echo a false flag even after its own Stop message.
    // Only that system row in the current turn limits the retry.
    configure("exit 2");
    let path = dir.join("transcript_full.jsonl");
    let mut turn = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let notice = "System notice.\n\n<SYSTEM_MESSAGE>\nStop hook blocked termination: Check beta\n</SYSTEM_MESSAGE>";
    writeln!(
        turn,
        "{}",
        json!({"step_index":8,"source":"MODEL","type":"PLANNER_RESPONSE","content":notice})
    )
    .unwrap();
    assert_eq!(hook(dir, payload(false).as_bytes())["decision"], "continue");
    writeln!(
        turn,
        "{}",
        json!({"step_index":9,"source":"SYSTEM","type":"SYSTEM_MESSAGE","content":notice})
    )
    .unwrap();
    assert_eq!(hook(dir, payload(false).as_bytes()), allow);
    writeln!(
        turn,
        "{}",
        json!({"step_index":10,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"Next turn"})
    )
    .unwrap();
    assert_eq!(hook(dir, payload(false).as_bytes())["decision"], "continue");
    assert_eq!(hook(dir, b"invalid"), allow);
    std::fs::write(dir.join("transcript_full.jsonl"), b"broken").unwrap();
    assert_eq!(hook(dir, payload(false).as_bytes()), allow);
    std::fs::remove_file(dir.join("guards.json")).unwrap();
    assert_eq!(hook(dir, payload(false).as_bytes()), allow);
}

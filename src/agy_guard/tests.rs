use super::*;

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("muster-agy-guard-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn guard(matcher: &str, command: &str) -> Guard {
    Guard { matcher: matcher.into(), command: command.into(), timeout_s: 5 }
}

fn payload(name: &str, args: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "toolCall": {"id": "call_1", "name": name, "args": args},
        "conversationId": "c-1", "transcriptPath": "/nowhere/transcript.jsonl", "cwd": "/",
        "hookEventName": "PreToolUse", "workspacePaths": ["/"], "stepIdx": 2,
    }))
    .unwrap()
}

#[test]
fn guards_come_from_a_claude_settings_file() {
    let v = json!({"hooks": {
        "PreToolUse": [
            {"matcher": "Bash|WebFetch", "hooks": [{"type": "command", "command": "guard-a", "timeout": 10}]},
            {"matcher": "AskUserQuestion", "hooks": [{"command": "guard-b"}, {"type": "prompt", "command": "x"}, {"command": "  "}]},
        ],
        "Stop": [{"hooks": [{"command": "stop-guard"}]}],
    }});
    assert_eq!(from_claude_settings(&v, "PreToolUse"), vec![
        Guard { matcher: "Bash|WebFetch".into(), command: "guard-a".into(), timeout_s: 10 },
        Guard { matcher: "AskUserQuestion".into(), command: "guard-b".into(), timeout_s: DEFAULT_TIMEOUT_S },
    ]);
    assert_eq!(from_claude_settings(&v, "Stop")[0].command, "stop-guard");
    assert!(from_claude_settings(&json!({}), "PreToolUse").is_empty());
    assert!(from_claude_settings(&json!({"hooks": {"PreToolUse": "nonsense"}}), "PreToolUse").is_empty());
    let huge = json!({"hooks": {"PreToolUse": [{"matcher": "", "hooks": [{"command": "g", "timeout": 99999}]}]}});
    assert_eq!(from_claude_settings(&huge, "PreToolUse")[0].timeout_s, MAX_TIMEOUT_S);
}

#[test]
fn matchers_read_as_claude_code_reads_them() {
    for (m, tool, yes) in [
        ("Bash|WebFetch|WebSearch|Artifact|mcp__.*", "Bash", true),
        ("Bash|WebFetch|WebSearch|Artifact|mcp__.*", "mcp__other__send", true),
        ("Bash|WebFetch|WebSearch|Artifact|mcp__.*", "Write", false),
        ("mcp__muster__post_question|mcp__muster__post_pr", "mcp__muster__post_pr", true),
        ("mcp__muster__post_question|mcp__muster__post_pr", "mcp__muster__post_prs", false),
        ("Agent|Bash", "Bash", true),
        ("Agent|Bash", "Bashful", false),
        ("^(Edit|Write)$", "Write", true),
        ("", "anything", true),
        ("*", "anything", true),
        ("mcp__.*__post_.*", "mcp__muster__post_pr", true),
        ("mcp__.*__post_.*", "mcp__muster__reply", false),
        // Other regular-expression syntax runs the guard: it checks the tool itself.
        ("Notebook[A-Z]+", "Bash", true),
        ("^mcp__(alpha|beta)__send$", "mcp__alpha__send", true),
        // A comma list, as some settings files write it.
        ("Bash, WebFetch", "WebFetch", true),
        ("Bash, WebFetch", "Write", false),
    ] {
        assert_eq!(matches(m, tool), yes, "{m} {tool}");
    }
}

#[test]
fn agy_calls_map_to_claude_codes_tools() {
    let call = |name: &str, args: Value| claude_call(name, &args).unwrap().unwrap();
    assert_eq!(call("run_command", json!({"CommandLine": "curl x", "Cwd": "/w", "toolSummary": "fetch"})),
        ("Bash".into(), json!({"command": "curl x", "description": "fetch"})));
    assert_eq!(call("read_url_content", json!({"Url": "https://example.com"})).1["url"], "https://example.com");
    assert_eq!(call("search_web", json!({"query": "q", "domain": "example.com"})).1, json!({"query": "q", "allowed_domains": ["example.com"]}));
    assert_eq!(call("write_to_file", json!({"TargetFile": "/a.md", "CodeContent": "hi"})).1, json!({"file_path": "/a.md", "content": "hi"}));
    assert_eq!(call("replace_file_content", json!({"TargetFile": "/a", "TargetContent": "x", "ReplacementContent": "y"})).1["new_string"], "y");
    let multi = call("multi_replace_file_content", json!({"TargetFile": "/a", "ReplacementChunks": [{"TargetContent": "x", "ReplacementContent": "y"}]}));
    assert_eq!(multi, ("MultiEdit".into(), json!({"file_path": "/a", "edits": [{"old_string": "x", "new_string": "y"}]})));
    assert_eq!(call("ask_question", json!({"questions": [{"question": "q?"}]})).0, "AskUserQuestion");
    // An MCP call: its arguments as an object or as JSON text.
    let card = json!({"summary": "s"});
    assert_eq!(call("call_mcp_tool", json!({"ServerName": "muster", "ToolName": "post_pr", "Arguments": card})),
        ("mcp__muster__post_pr".into(), card.clone()));
    assert_eq!(call("call_mcp_tool", json!({"ServerName": "muster", "ToolName": "post_pr", "Arguments": card.to_string()})).1, card);
    assert!(claude_call("call_mcp_tool", &json!({"Arguments": {}})).is_err());
    assert_eq!(claude_call("view_file", &json!({"AbsolutePath": "/a"})).unwrap(), None);
}

#[test]
fn answers_are_read_as_claude_code_reads_them() {
    let deny = br#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no"}}"#;
    let ask = br#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"look"}}"#;
    assert_eq!(answer(Some(0), deny, b""), (Decision::Deny, Some("no".into())));
    assert_eq!(answer(Some(0), ask, b""), (Decision::Ask, Some("look".into())));
    assert_eq!(answer(Some(0), br#"{"decision":"block","reason":"old style"}"#, b""), (Decision::Deny, Some("old style".into())));
    assert_eq!(answer(Some(2), b"", b"  refused: why  \n"), (Decision::Deny, Some("refused: why".into())));
    assert_eq!(answer(Some(2), b"", b"").0, Decision::Deny);
    // A guard that rewrites the call: agy can't take the rewrite, so a human looks.
    let rewrite = br#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"command":"redacted"}}}"#;
    assert_eq!(answer(Some(0), rewrite, b"").0, Decision::Ask);
    // Nothing said, words that aren't JSON, an error exit or no exit at all: the call goes on.
    for (code, out) in [(Some(0), &b""[..]), (Some(0), b"just text"), (Some(1), deny), (None, deny)] {
        assert_eq!(answer(code, out, b"").0, Decision::Allow);
    }
    assert_eq!(combine(&[(Decision::Allow, None)]), json!({"decision": "allow"}));
    assert_eq!(combine(&[(Decision::Ask, Some("a".into())), (Decision::Deny, Some("d1".into())), (Decision::Deny, Some("d2".into()))]),
        json!({"decision": "deny", "reason": "d1 d2"}));
    assert_eq!(combine(&[(Decision::Ask, Some("a".into())), (Decision::Allow, None)]), json!({"decision": "force_ask", "reason": "a"}));
    let long = combine(&[(Decision::Deny, Some("x".repeat(10_000)))]);
    assert!(long["reason"].as_str().unwrap().chars().count() <= MAX_REASON + 1);
}

#[test]
fn guards_run_unchanged_on_claude_codes_input() {
    let dir = tmp();
    let d = dir.display();
    let config = Config {
        stop: vec![],
        cwd: dir.to_string_lossy().into(),
        guards: vec![
            // Refuses a command holding a made-up private string, as a private-info guard would.
            guard("Bash|mcp__.*", "grep -q zq-test-ident && { echo 'private string: zq-test-ident' >&2; exit 2; }; exit 0"),
            // Keeps what it was given, and where and how it ran.
            guard("Bash", &format!("cat > {d}/input.json; pwd > {d}/pwd; printf %s \"$CLAUDE_PROJECT_DIR\" > {d}/project")),
            // Never runs for a Bash call.
            guard("AskUserQuestion", &format!("touch {d}/wrong")),
        ],
    };
    let ok = decide(&payload("run_command", json!({"CommandLine": "echo hi"})), Some(config.clone()));
    assert_eq!(ok, json!({"decision": "allow"}));
    let input: Value = serde_json::from_slice(&std::fs::read(dir.join("input.json")).unwrap()).unwrap();
    assert_eq!(input["tool_name"], "Bash");
    assert_eq!(input["tool_input"]["command"], "echo hi");
    assert_eq!((input["hook_event_name"].as_str(), input["session_id"].as_str()), (Some("PreToolUse"), Some("c-1")));
    let canon = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(std::fs::canonicalize(std::fs::read_to_string(dir.join("pwd")).unwrap().trim()).unwrap(), canon);
    assert_eq!(std::fs::read_to_string(dir.join("project")).unwrap(), dir.to_string_lossy());
    assert!(!dir.join("wrong").exists());
    let refused = decide(&payload("run_command", json!({"CommandLine": "curl -d zq-test-ident x"})), Some(config.clone()));
    assert_eq!(refused, json!({"decision": "deny", "reason": "private string: zq-test-ident"}));
    // The same guard on an MCP call's arguments.
    let mcp = payload("call_mcp_tool", json!({"ServerName": "other", "ToolName": "send", "Arguments": {"text": "zq-test-ident"}}));
    assert_eq!(decide(&mcp, Some(config.clone()))["decision"], "deny");
    // A tool no guard covers is allowed without running any.
    std::fs::remove_file(dir.join("input.json")).unwrap();
    assert_eq!(decide(&payload("view_file", json!({"AbsolutePath": "/a"})), Some(config)), json!({"decision": "allow"}));
    assert!(!dir.join("input.json").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_guard_past_its_timeout_lets_the_call_go_in_time() {
    let dir = tmp();
    let config = Config {
        stop: vec![],
        cwd: dir.to_string_lossy().into(),
        guards: vec![
            Guard { matcher: "Bash".into(), command: "sleep 30; exit 2".into(), timeout_s: 1 },
            Guard { matcher: "Bash".into(), command: "sleep 30 & exit 0".into(), timeout_s: 1 },
            guard("Bash", r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"check it"}}'"#),
        ],
    };
    let started = Instant::now();
    let v = decide(&payload("run_command", json!({"CommandLine": "ls"})), Some(config));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(v, json!({"decision": "force_ask", "reason": "check it"}));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn output_past_the_cap_keeps_the_guards_answer() {
    // `set -e` with a pipe closed after the cap would die of SIGPIPE before `exit 2`.
    let flood = |fd: &str, then: &str| format!("set -e; head -c 3000000 /dev/zero {fd}; {then}");
    let run = |command: String| {
        let config = Config { stop: vec![], cwd: "/".into(), guards: vec![guard("Bash", &command)] };
        decide(&payload("run_command", json!({"CommandLine": "ls"})), Some(config))
    };
    let refused = run(flood(">&2", "echo 'private string' >&2; exit 2"));
    assert_eq!(refused["decision"], "deny");
    assert!(refused["reason"].as_str().unwrap().len() <= MAX_REASON * 4 + 3);
    // An answer on stdout past the cap can't be read: asked about, never taken as allow.
    assert_eq!(run(flood("", "exit 0"))["decision"], "force_ask");
}

#[test]
fn what_cant_be_read_is_asked_about() {
    let config = Config { stop: vec![], cwd: "/".into(), guards: vec![guard("", "exit 0")] };
    for (payload, config) in [
        (payload("run_command", json!({"CommandLine": "ls"})), None),
        (b"not json".to_vec(), Some(config.clone())),
        (br#"{"toolCall": {}}"#.to_vec(), Some(config.clone())),
        (payload("call_mcp_tool", json!({"Arguments": {}})), Some(config.clone())),
    ] {
        let v = decide(&payload, config);
        assert_eq!(v["decision"], "force_ask", "{v}");
        assert!(v["reason"].as_str().unwrap().starts_with("Muster couldn't check this call"), "{v}");
    }
    assert!(read_config("/nonexistent/antigravity-guards.json").is_none());
}

#[test]
fn agys_hook_file_runs_the_shim_longer_than_the_slowest_guard() {
    let config = Config { stop: vec![], cwd: "/w".into(), guards: vec![guard("Bash", "g"), Guard { timeout_s: 30, ..guard("Bash", "h") }] };
    let mut config = config;
    config.stop = vec![guard("", "end")];
    let v = hooks_json(Path::new("/opt/muster bin/muster"), Path::new("/data/s 1/antigravity-guards.json"), &config);
    let h = &v["muster-guards"]["PreToolUse"][0];
    assert_eq!(h["matcher"], AGY_MATCHER);
    assert_eq!(h["hooks"][0]["command"], "'/opt/muster bin/muster' hook antigravity guard '/data/s 1/antigravity-guards.json'");
    assert_eq!(h["hooks"][0]["timeout"], 40);
    assert_eq!(v["muster-guards"]["Stop"], json!([{
        "type": "command", "command": "'/opt/muster bin/muster' hook antigravity stop '/data/s 1/antigravity-guards.json'",
        "timeout": 15
    }]));
}

#[test]
fn stop_transcript_contains_the_current_turn_and_paired_results() {
    let path = Path::new("src/fixtures/antigravity_1.5.13/transcript_full.jsonl");
    let mut out = Vec::new();
    stop::transcript(path, &mut out).unwrap();
    let rows: Vec<Value> = serde_json::Deserializer::from_slice(&out)
        .into_iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows.len(), 5);
    let blocks = |i: usize| &rows[i]["message"]["content"];
    assert_eq!(blocks(0), &json!([{"type":"text", "text":"Check beta"}]));
    assert_eq!(blocks(1)[0]["name"], "mcp__muster__whoami");
    assert_eq!(blocks(1)[0]["input"], json!({}));
    assert_eq!(blocks(1)[1]["name"], "Bash");
    assert_eq!(blocks(1)[1]["input"]["command"], "echo beta");
    for (i, end) in [(2, "alpha"), (3, "beta")] {
        assert_eq!(rows[i]["message"]["role"], "user");
        assert_eq!(blocks(i)[0]["type"], "tool_result");
        assert_eq!(blocks(i)[0]["tool_use_id"], blocks(1)[i-2]["id"]);
        assert!(blocks(i)[0]["content"].as_str().unwrap().ends_with(end));
    }
    assert_eq!(blocks(4), &json!([{"type":"text", "text":"Checked beta"}]));
}

#[test]
fn compact_transcript_decodes_arguments_and_recovers_truncated_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("transcript.jsonl");
    let full = tmp.path().join("transcript_full.jsonl");
    let original = include_str!("../fixtures/antigravity_1.5.13/transcript_full.jsonl");
    std::fs::write(&full, original).unwrap();
    let mut rows: Vec<Value> = serde_json::Deserializer::from_str(original)
        .into_iter().collect::<Result<_, _>>().unwrap();
    for call in rows[3]["tool_calls"].as_array_mut().unwrap() {
        for arg in call["args"].as_object_mut().unwrap().values_mut() {
            *arg = json!(arg.to_string());
        }
    }
    let write = |rows: &[Value]| std::fs::write(&path, rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")).unwrap();
    let mut expected = Vec::new();
    stop::transcript(&full, &mut expected).unwrap();
    write(&rows);
    let mut output = Vec::new();
    stop::transcript(&path, &mut output).unwrap();
    assert_eq!(output, expected);
    rows[3]["truncated_fields"] = json!(["tool_calls"]);
    rows[3]["tool_calls"][0]["args"]["Arguments"] = json!("{...");
    write(&rows);
    output.clear();
    stop::transcript(&path, &mut output).unwrap();
    assert_eq!(output, expected);
    rows.last_mut().unwrap()["step_index"] = json!(20);
    write(&rows);
    assert!(stop::transcript(&path, &mut Vec::new()).is_err());
}

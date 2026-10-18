//! Process-boundary tests. Every executable receives all three temporary Muster settings.
//! This suite never discovers, invokes, or signals the user's running Muster instance.
use muster_terminal_protocol::{
    Command, Request, Response, RunId, SpawnSpec, TerminalStatus, VERSION,
    client::Client,
    wire::{self, Frame},
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    root: PathBuf,
    child: Child,
    client: Client,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "muster-host-test-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let child = Self::launch(&root);
        let mut starting = Starting {
            child: Some(child),
            root: root.clone(),
        };
        let client = until("host startup", || {
            Client::connect(&root.join("run/terminal.sock")).ok()
        });
        let child = starting.child.take().unwrap();
        Self {
            root,
            child,
            client,
        }
    }

    fn launch(root: &std::path::Path) -> Child {
        Process::new(env!("CARGO_BIN_EXE_muster-terminal-host"))
            .args([
                "--runtime-dir",
                root.join("run").to_str().unwrap(),
                "--data-dir",
                root.join("data").to_str().unwrap(),
            ])
            .env("MUSTER_RUNTIME_DIR", root.join("run"))
            .env("MUSTER_DATA_DIR", root.join("data"))
            .env("MUSTER_PORT", "0")
            .env_remove("MUSTER_URL")
            .env_remove("MUSTER_TOKEN")
            .env_remove("MUSTER_BIN")
            .env_remove("MUSTER_SESSION_ID")
            .env_remove("MUSTER_CHANNEL_ID")
            .env_remove("MUSTER_RUN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }

    fn spec(&self, id: u32, generation: u64, script: &str) -> SpawnSpec {
        SpawnSpec {
            run: RunId { id, generation },
            program: "/bin/bash".into(),
            args: vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                script.into(),
            ],
            env: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("TERM".into(), "xterm-256color".into()),
            ]),
            cwd: self.root.clone(),
            cols: 80,
            rows: 24,
        }
    }

    fn spawn(&self, spec: SpawnSpec) -> TerminalStatus {
        match self.client.call(Command::Spawn(spec)).unwrap() {
            Response::Status(status) => status,
            other => panic!("{other:?}"),
        }
    }

    fn status(&self, run: RunId) -> TerminalStatus {
        match self.client.call(Command::Status { run }).unwrap() {
            Response::Status(status) => status,
            other => panic!("{other:?}"),
        }
    }

    fn text(&self, run: RunId) -> String {
        match self
            .client
            .call(Command::RecentText { run, lines: 100 })
            .unwrap()
        {
            Response::Text(text) => text,
            other => panic!("{other:?}"),
        }
    }
}

// Keeps startup failures from leaking a child, even before the first handshake succeeds.
struct Starting {
    child: Option<Child>,
    root: PathBuf,
}
impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            stop_owned(child);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        stop_owned(&mut self.child);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn stop_owned(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // SAFETY: this unreaped child was spawned by this fixture; its pid cannot be reused.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}
fn until<T>(what: &str, mut test: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = test() {
            return value;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn terminal_survives_all_clients_disconnecting_and_replays_output() {
    let fixture = Fixture::new();
    let status = fixture.spawn(fixture.spec(1, 1,
        "printf 'ready\\n'; sleep 0.2; printf 'while-disconnected\\n'; while IFS= read -r line; do printf 'answer:%s\\n' \"$line\"; done"));
    let mut stream = fixture
        .client
        .subscribe(Command::Attach {
            run: status.run,
            replay_bytes: None,
        })
        .unwrap();
    assert!(matches!(
        wire::read(&mut stream).unwrap(),
        Frame::Replay { .. }
    ));
    drop(stream);
    // Client is only configuration: it holds no open socket. The host has zero clients here.
    std::thread::sleep(Duration::from_millis(350));
    let replacement = Client::connect(&fixture.root.join("run/terminal.sock")).unwrap();
    assert_eq!(replacement.hello.instance, fixture.client.hello.instance);
    let current = fixture.status(status.run);
    assert_eq!(current.pid, status.pid);
    assert!(current.alive);
    let mut stream = replacement
        .subscribe(Command::Attach {
            run: status.run,
            replay_bytes: None,
        })
        .unwrap();
    let mut offset = match wire::read(&mut stream).unwrap() {
        Frame::Replay { next_offset, data } => {
            assert!(String::from_utf8_lossy(&data).contains("while-disconnected"));
            next_offset
        }
        other => panic!("{other:?}"),
    };
    replacement
        .call(Command::Input {
            run: status.run,
            data: b"hello\n".to_vec(),
        })
        .unwrap();
    let mut output = vec![];
    while !String::from_utf8_lossy(&output).contains("answer:hello") {
        match wire::read(&mut stream).unwrap() {
            Frame::Output {
                offset: start,
                data,
            } => {
                assert_eq!(
                    start, offset,
                    "replay/live stream must have no gaps or duplicates"
                );
                offset += data.len() as u64;
                output.extend(data);
            }
            Frame::Ping => {}
            other => panic!("{other:?}"),
        }
    }
    replacement
        .call(Command::Resize {
            run: status.run,
            cols: 101,
            rows: 37,
        })
        .unwrap();
    let current = fixture.status(status.run);
    assert_eq!((current.cols, current.rows), (101, 37));
    replacement.call(Command::Flush).unwrap();
    let log = std::fs::read(fixture.root.join("data/runs/1-1/output.log")).unwrap();
    assert!(String::from_utf8_lossy(&log).contains("answer:hello"));
}

#[test]
fn spawn_is_retry_safe_and_old_generations_cannot_affect_replacements() {
    let fixture = Fixture::new();
    let spec = fixture.spec(1, 1, "sleep 100");
    let first = fixture.spawn(spec.clone());
    let retry = fixture.spawn(spec.clone());
    assert_eq!(first.pid, retry.pid);
    let mut changed = spec.clone();
    changed.cols = 90;
    assert!(fixture.client.call(Command::Spawn(changed)).is_err());
    assert!(
        fixture
            .client
            .call(Command::Spawn(fixture.spec(1, 2, "sleep 100")))
            .is_err()
    );
    fixture
        .client
        .call(Command::Terminate {
            run: first.run,
            force: true,
        })
        .unwrap();
    until("old process exit and output drain", || {
        let status = fixture.status(first.run);
        (!status.has_processes && status.drained).then_some(())
    });
    let second = fixture.spawn(fixture.spec(1, 2, "sleep 100"));
    assert_ne!(first.pid, second.pid);
    assert!(
        fixture
            .client
            .call(Command::Input {
                run: first.run,
                data: b"oops".to_vec()
            })
            .is_err()
    );
    fixture
        .client
        .call(Command::Terminate {
            run: first.run,
            force: true,
        })
        .unwrap();
    assert!(fixture.status(second.run).alive);
    fixture
        .client
        .call(Command::Forget { run: first.run })
        .unwrap();
    assert!(fixture.client.call(Command::Spawn(spec)).is_err());
    assert!(
        fixture
            .client
            .call(Command::Forget { run: second.run })
            .is_err()
    );
}

#[test]
fn records_exits_and_keeps_background_children_under_host_control() {
    let fixture = Fixture::new();
    let first = fixture.spawn(fixture.spec(1, 1, "printf 'final-message\\n'; exit 7"));
    until("recorded exit", || {
        (fixture.status(first.run).exit_code == Some(7)).then_some(())
    });
    assert!(fixture.text(first.run).contains("final-message"));
    let background = fixture.spawn(fixture.spec(
        2,
        1,
        "(trap '' HUP; sleep 100) & printf 'background:%s\\n' \"$!\"; exit 0",
    ));
    until("leader exit", || {
        (fixture.status(background.run).exit_code == Some(0)).then_some(())
    });
    assert!(fixture.status(background.run).has_processes);
    // A completed leader does not permit archiving while its descendants still
    // exist, even if they are not producing output.
    std::thread::sleep(Duration::from_millis(3300));
    assert!(!fixture.root.join("data/runs/2-1/archive.json").exists());
    assert!(
        fixture
            .client
            .call(Command::Spawn(fixture.spec(2, 2, "sleep 100")))
            .is_err()
    );
    fixture
        .client
        .call(Command::Terminate {
            run: background.run,
            force: true,
        })
        .unwrap();
    until("descendant cleanup", || {
        (!fixture.status(background.run).has_processes).then_some(())
    });
}

#[test]
fn exited_terminals_archive_without_losing_replay_or_disconnecting_existing_streams() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        1,
        1,
        r"printf '\033[?2004h\033[?1hbefore-exit\n'; sleep 0.2; printf 'last-output\n'; exit 7",
    );
    let started = fixture.spawn(spec.clone());
    let mut stream = fixture
        .client
        .subscribe(Command::Attach {
            run: started.run,
            replay_bytes: None,
        })
        .unwrap();
    let (mut offset, mut output) = match wire::read(&mut stream).unwrap() {
        Frame::Replay { next_offset, data } => (next_offset, data),
        other => panic!("{other:?}"),
    };
    while !String::from_utf8_lossy(&output).contains("last-output") {
        match wire::read(&mut stream).unwrap() {
            Frame::Output {
                offset: start,
                data,
            } => {
                assert_eq!(start, offset);
                offset += data.len() as u64;
                output.extend(data);
            }
            Frame::Ping => {}
            other => panic!("{other:?}"),
        }
    }
    until("final replay archived", || {
        fixture
            .root
            .join("data/runs/1-1/archive.json")
            .exists()
            .then_some(())
    });
    assert_eq!(fixture.status(started.run).exit_code, Some(7));
    assert_eq!(fixture.text(started.run), "before-exit\nlast-output");
    let final_screen = fixture
        .client
        .call(Command::Screen { run: started.run })
        .unwrap();
    match final_screen {
        Response::Screen(screen) => {
            assert!(screen.lines.iter().any(|line| line == "last-output"));
            assert!(!screen.foreground_busy);
        }
        other => panic!("{other:?}"),
    }
    let mut restored = fixture
        .client
        .subscribe(Command::Attach {
            run: started.run,
            replay_bytes: None,
        })
        .unwrap();
    match wire::read(&mut restored).unwrap() {
        Frame::Replay { next_offset, data } => {
            assert_eq!(next_offset, offset);
            assert!(String::from_utf8_lossy(&data).contains("before-exit"));
            assert!(data.ends_with(b"last-output\r\n"));
            assert!(data.windows(8).any(|s| s == b"\x1b[?2004h"));
        }
        other => panic!("{other:?}"),
    }
    // This stream was attached before exit and remains usable after the owner
    // has been released; the client does not enter an endless reconnect loop.
    assert!(matches!(wire::read(&mut stream).unwrap(), Frame::Ping));
    let retry = fixture.spawn(spec);
    assert_eq!(retry.pid, started.pid);
    assert_eq!(retry.exit_code, Some(7));
    assert!(
        fixture
            .client
            .call(Command::Input {
                run: started.run,
                data: b"no".to_vec()
            })
            .is_err()
    );
    fixture
        .client
        .call(Command::Forget { run: started.run })
        .unwrap();
    assert!(!fixture.root.join("data/runs/1-1").exists());
}

#[test]
fn host_identity_and_protocol_are_checked_before_mutations() {
    let fixture = Fixture::new();
    let first = fixture.spawn(fixture.spec(1, 1, "sleep 100"));
    for request in [
        Request {
            version: VERSION + 1,
            instance: Some(fixture.client.hello.instance.clone()),
            command: Command::Shutdown,
        },
        Request {
            version: VERSION,
            instance: Some("old-host-instance".into()),
            command: Command::Terminate {
                run: first.run,
                force: true,
            },
        },
    ] {
        let mut stream =
            std::os::unix::net::UnixStream::connect(fixture.root.join("run/terminal.sock"))
                .unwrap();
        wire::write(&mut stream, &Frame::json(&request).unwrap()).unwrap();
        assert!(matches!(
            wire::read(&mut stream)
                .unwrap()
                .decode::<Response>()
                .unwrap(),
            Response::Error { .. }
        ));
        assert!(fixture.status(first.run).alive);
    }
}

#[test]
fn a_second_host_cannot_reap_the_first_hosts_processes() {
    let fixture = Fixture::new();
    let first = fixture.spawn(fixture.spec(1, 1, "sleep 100"));
    let mut contender = Fixture::launch(&fixture.root);
    let exit = contender.wait().unwrap();
    assert!(!exit.success());
    assert!(fixture.status(first.run).alive);
    let alt = fixture.root.join("alternate-run");
    let mut contender = Process::new(env!("CARGO_BIN_EXE_muster-terminal-host"))
        .arg("--runtime-dir")
        .arg(&alt)
        .arg("--data-dir")
        .arg(fixture.root.join("data"))
        .env("MUSTER_RUNTIME_DIR", &alt)
        .env("MUSTER_DATA_DIR", fixture.root.join("data"))
        .env("MUSTER_PORT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(!contender.wait().unwrap().success());
    assert!(fixture.status(first.run).alive);
}

#[test]
fn explicit_shutdown_terminates_host_owned_processes() {
    let mut fixture = Fixture::new();
    fixture.spawn(fixture.spec(1, 1, "sleep 100"));
    assert!(matches!(
        fixture.client.call(Command::Shutdown).unwrap(),
        Response::Ok
    ));
    let status = until("host shutdown", || fixture.child.try_wait().unwrap());
    assert!(status.success());
    assert!(!fixture.root.join("run/terminal.json").exists());
}

#[test]
fn no_clients_are_needed_for_terminal_queries_or_periodic_log_flush() {
    let fixture = Fixture::new();
    let status = fixture.spawn(fixture.spec(1, 1,
        "stty raw -echo; printf '\\033[5n'; answer=$(dd bs=1 count=4 2>/dev/null); if [[ $answer == $'\\033[0n' ]]; then printf 'QUERY-OK'; else printf 'QUERY-BAD'; fi; sleep 100"));
    until("host-only terminal query response", || {
        fixture.text(status.run).contains("QUERY-OK").then_some(())
    });
    let log = fixture.root.join("data/runs/1-1/output.log");
    // The fixture never asks for Flush: only the host's timer can persist this output.
    until("autonomous log flush", || {
        std::fs::read(&log)
            .ok()
            .filter(|bytes| String::from_utf8_lossy(bytes).contains("QUERY-OK"))
    });
}

#[test]
fn output_is_bounded_and_replay_survives_a_flood_without_subscribers() {
    let fixture = Fixture::new();
    let status = fixture.spawn(fixture.spec(
        1,
        1,
        "head -c 3145728 /dev/zero | tr '\\0' x; printf '\\nAFTER-FLOOD\\n'; sleep 100",
    ));
    until("flood completes", || {
        fixture
            .text(status.run)
            .contains("AFTER-FLOOD")
            .then_some(())
    });
    let mut stream = fixture
        .client
        .subscribe(Command::Attach {
            run: status.run,
            replay_bytes: None,
        })
        .unwrap();
    match wire::read(&mut stream).unwrap() {
        Frame::Replay { next_offset, data } => {
            assert!(next_offset > 3 * 1024 * 1024);
            assert!(data.len() <= 2 * 1024 * 1024 + 256);
            assert!(String::from_utf8_lossy(&data).contains("AFTER-FLOOD"));
        }
        other => panic!("{other:?}"),
    }
    assert!(fixture.status(status.run).alive);
}

#[test]
fn host_crash_is_distinct_from_client_loss_and_rejects_stale_clients() {
    let mut fixture = Fixture::new();
    let first = fixture.spawn(fixture.spec(
        1,
        1,
        "trap '' HUP; printf 'target-ready\\n'; while :; do sleep 1; done",
    ));
    until("process ready", || {
        fixture
            .text(first.run)
            .contains("target-ready")
            .then_some(())
    });
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    fixture.child = Fixture::launch(&fixture.root);
    let replacement = until("replacement host", || {
        Client::connect(&fixture.root.join("run/terminal.sock")).ok()
    });
    assert_ne!(replacement.hello.instance, fixture.client.hello.instance);
    assert!(
        fixture
            .client
            .call(Command::Spawn(fixture.spec(2, 1, "sleep 100")))
            .is_err()
    );
    assert!(
        matches!(replacement.call(Command::Inventory).unwrap(), Response::Inventory(runs) if runs.is_empty())
    );
    let old_marker = format!("MUSTER_RUN={}-1-1", fixture.client.hello.instance);
    let old_environment =
        std::fs::read(format!("/proc/{}/environ", first.pid.unwrap())).unwrap_or_default();
    assert!(
        !old_environment
            .split(|byte| *byte == 0)
            .any(|value| value == old_marker.as_bytes()),
        "the old marked process must be gone before the replacement host serves"
    );
    // Reaping belongs to host restart; a fresh controller in the first test preserves this same process.
    fixture.client = replacement;
}

#[test]
fn bootstrap_waits_for_owned_pid_and_never_executes_for_a_dead_parent() {
    for (publish, vanished_cwd) in [(true, false), (false, false), (true, true)] {
        let root = std::env::temp_dir().join(format!(
            "muster-bootstrap-test-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let cwd = root.join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let journal = root.join("owner.json");
        let result = root.join("target-pid");
        let marker = format!("bootstrap-{:032x}", rand::random::<u128>());
        let mut owner = serde_json::json!({"run":{"id":9,"generation":1},"pid":null,
            "markers":[format!("MUSTER_RUN={marker}"),"MUSTER_SESSION_ID=9"]});
        muster_terminal_core::private_fs::write_private(
            &journal,
            &serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();
        let child = Process::new(env!("CARGO_BIN_EXE_muster-terminal-host"))
            .arg("--owned-child")
            .arg(if publish { std::process::id() } else { 0 }.to_string())
            .arg(&journal)
            .arg(&cwd)
            .args(["/bin/sh", "-c", "printf '%s' \"$$\" > \"$1\"", "fixture"])
            .arg(&result)
            .env_clear()
            .env("MUSTER_RUNTIME_DIR", root.join("run"))
            .env("MUSTER_DATA_DIR", root.join("data"))
            .env("MUSTER_PORT", "0")
            .env("MUSTER_RUN", &marker)
            .env("MUSTER_SESSION_ID", "9")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut guard = Starting {
            child: Some(child),
            root,
        };
        let child = guard.child.as_mut().unwrap();
        if publish {
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                child.try_wait().unwrap().is_none() && !result.exists(),
                "target ran before ownership was committed"
            );
            owner["pid"] = child.id().into();
            if vanished_cwd {
                std::fs::remove_dir(&cwd).unwrap();
            }
            muster_terminal_core::private_fs::write_private(
                &journal,
                &serde_json::to_vec(&owner).unwrap(),
            )
            .unwrap();
            let status = until("owned target exits", || child.try_wait().unwrap());
            if vanished_cwd {
                assert!(
                    !status.success() && !result.exists(),
                    "a vanished cwd must not fall back to another directory"
                );
            } else {
                assert!(status.success());
                assert_eq!(
                    std::fs::read_to_string(&result).unwrap(),
                    child.id().to_string(),
                    "exec preserves the journaled PID"
                );
            }
        } else {
            let status = until("orphan bootstrap exits", || child.try_wait().unwrap());
            assert!(
                !status.success() && !result.exists(),
                "unowned target must never execute"
            );
        }
    }
}

#[test]
fn bootstrap_exits_when_its_owner_dies_while_the_journal_is_uncommitted() {
    use std::os::unix::process::CommandExt;
    let root = std::env::temp_dir().join(format!(
        "muster-bootstrap-test-{:032x}",
        rand::random::<u128>()
    ));
    std::fs::create_dir(&root).unwrap();
    let journal = root.join("owner.json");
    let result = root.join("target-ran");
    let child_pid = root.join("bootstrap-pid");
    let marker = format!("bootstrap-{:032x}", rand::random::<u128>());
    let owner = serde_json::json!({"run":{"id":9,"generation":1},"pid":null,
        "markers":[format!("MUSTER_RUN={marker}"),"MUSTER_SESSION_ID=9"]});
    muster_terminal_core::private_fs::write_private(&journal, &serde_json::to_vec(&owner).unwrap())
        .unwrap();
    let script = r#"
        "$1" --owned-child "$$" "$2" "$3" /bin/sh -c 'printf target > "$1"' fixture "$4" &
        printf '%s' "$!" > "$5"
        wait
    "#;
    let child = Process::new("/bin/sh")
        .args(["-c", script, "fixture"])
        .arg(env!("CARGO_BIN_EXE_muster-terminal-host"))
        .arg(&journal)
        .arg(&root)
        .arg(&result)
        .arg(&child_pid)
        .process_group(0)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("MUSTER_RUNTIME_DIR", root.join("run"))
        .env("MUSTER_DATA_DIR", root.join("data"))
        .env("MUSTER_PORT", "0")
        .env("MUSTER_RUN", &marker)
        .env("MUSTER_SESSION_ID", "9")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = Starting {
        child: Some(child),
        root,
    };
    // The PID file is used only for read-only observations, never as a signal target.
    let pid: u32 = until("bootstrap PID", || {
        std::fs::read_to_string(&child_pid)
            .ok()
            .and_then(|s| s.parse().ok())
    });
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_muster-terminal-host")).unwrap();
    until("bootstrap reached its exec guard", || {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .is_ok_and(|path| path == executable)
            .then_some(())
    });
    assert!(!result.exists());
    let parent = guard.child.as_mut().unwrap();
    parent.kill().unwrap();
    parent.wait().unwrap();
    let marker = format!("MUSTER_RUN={marker}");
    until("uncommitted bootstrap exits after owner death", || {
        let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        (!env
            .split(|byte| *byte == 0)
            .any(|item| item == marker.as_bytes()))
        .then_some(())
    });
    assert!(
        !result.exists(),
        "the target must never run after its owning process dies"
    );
}

#[test]
fn input_stream_applies_every_keystroke_in_order_and_survives_rejections() {
    let fixture = Fixture::new();
    let received = fixture.root.join("received");
    let status = fixture.spawn(fixture.spec(1, 1, &format!(
        "stty raw -echo; head -c 600 > '{}'; printf 'INPUT-DONE\\n'; sleep 30", received.display())));
    let mut stream = fixture.client.input_stream().unwrap();
    let expected: Vec<u8> = (0..600).map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789"[i % 36]).collect();
    for (i, byte) in expected.iter().enumerate() {
        Client::stream_input(&mut stream, &Command::Input { run: status.run, data: vec![*byte] }).unwrap();
        if i == 300 {
            // A rejected item is answered and leaves the stream usable.
            let unknown = Command::Input { run: RunId { id: 99, generation: 1 }, data: b"x".to_vec() };
            assert!(matches!(Client::stream_input(&mut stream, &unknown),
                Err(muster_terminal_protocol::client::StreamError::Host(_))));
            assert!(matches!(Client::stream_input(&mut stream, &Command::Flush),
                Err(muster_terminal_protocol::client::StreamError::Host(_))));
            // An idle stream stays open.
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    until("all input applied", || fixture.text(status.run).contains("INPUT-DONE").then_some(()));
    assert_eq!(std::fs::read(&received).unwrap(), expected, "every byte, once, in order");
}

#[test]
fn host_answers_only_after_taking_its_fence_and_refuses_a_held_one() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let root = std::env::temp_dir().join(format!("muster-host-fence-{}-{:016x}", std::process::id(), rand::random::<u64>()));
    std::fs::create_dir_all(root.join("run")).unwrap();
    let fence = root.join("run/daemon.lock");
    let launch = || {
        Process::new(env!("CARGO_BIN_EXE_muster-terminal-host"))
            .args(["--runtime-dir", root.join("run").to_str().unwrap(), "--data-dir", root.join("data").to_str().unwrap()])
            .env("MUSTER_RUNTIME_DIR", root.join("run")).env("MUSTER_DATA_DIR", root.join("data")).env("MUSTER_PORT", "0")
            .env("MUSTER_TERMINAL_FENCE_LOCK", &fence)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    };
    // A legacy daemon holds the fence: the host never answers and exits with FENCE_BUSY.
    let legacy = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&fence).unwrap();
    assert_eq!(unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let mut refused = launch();
    let status = until("refused host exits", || refused.try_wait().unwrap());
    assert_eq!(status.code(), Some(muster_terminal_protocol::EXIT_FENCE_BUSY));
    assert!(Client::connect(&root.join("run/terminal.sock")).is_err(), "a refused host never answered");
    drop(legacy);
    // With the fence free, the first answer already reports it held.
    let mut child = launch();
    let client = until("host startup", || Client::connect(&root.join("run/terminal.sock")).ok());
    let reported = client.hello.fence.clone().expect("fence reported");
    assert_eq!(reported.ino, std::fs::metadata(&fence).unwrap().ino());
    let probe = std::fs::OpenOptions::new().write(true).open(&fence).unwrap();
    assert_ne!(unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0, "the host holds it");
    // A second host for the same runtime directory reports HOST_BUSY, not a fence problem.
    let mut second = launch();
    let status = until("second host exits", || second.try_wait().unwrap());
    assert_eq!(status.code(), Some(muster_terminal_protocol::EXIT_HOST_BUSY));
    stop_owned(&mut child);
    let _ = std::fs::remove_dir_all(&root);
}

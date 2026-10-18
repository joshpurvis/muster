//! A terminal target cannot execute before the host publishes its ownership PID.
//! The bootstrap is an exec mode of the host binary, not another long-lived service.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use muster_terminal_core::pty;
use muster_terminal_protocol::{RunId, SpawnSpec};

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct OwnedProcess {
    pub run: RunId,
    pub pid: Option<u32>,
    pub markers: Vec<String>,
}

/// Check the target before creating a PTY, preserving an immediate error for a
/// missing/non-executable program. Exec still handles changes after this check.
pub(crate) fn check_program(spec: &SpawnSpec) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let executable = |path: &Path| {
        if !path.is_file() {
            return false;
        }
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: the NUL-terminated path lives across this read-only access query.
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    };
    let resolve = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            spec.cwd.join(path)
        }
    };
    let found = if spec.program.contains('/') {
        executable(&resolve(PathBuf::from(&spec.program)))
    } else {
        std::env::split_paths(OsStr::new(
            spec.env
                .get("PATH")
                .map(String::as_str)
                .unwrap_or("/usr/bin:/bin"),
        ))
        .any(|dir| executable(&resolve(dir.join(&spec.program))))
    };
    anyhow::ensure!(
        found,
        "program is missing or not executable: {}",
        spec.program
    );
    Ok(())
}

pub fn launch_owned_child(
    parent: u32,
    journal: &Path,
    cwd: &Path,
    program: &OsStr,
    args: &[OsString],
) -> anyhow::Result<()> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let markers = pty::run_markers(&env);
    anyhow::ensure!(
        env.contains_key("MUSTER_RUN") && env.contains_key("MUSTER_SESSION_ID"),
        "terminal ownership markers are missing"
    );
    let same_parent = || {
        // SAFETY: getppid is a side-effect-free query. A dead parent is reparented;
        // PID reuse cannot make an unrelated process become this child's parent.
        parent > 1 && unsafe { libc::getppid() } as u32 == parent
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        anyhow::ensure!(
            same_parent(),
            "terminal host exited before launch ownership was confirmed"
        );
        let owner: OwnedProcess = serde_json::from_slice(&std::fs::read(journal)?)?;
        anyhow::ensure!(
            owner.markers == markers,
            "terminal ownership markers changed before launch"
        );
        match owner.pid {
            Some(pid) => {
                anyhow::ensure!(
                    pid == std::process::id(),
                    "terminal ownership PID changed before launch"
                );
                anyhow::ensure!(same_parent(), "terminal host exited before launch");
                // Exec keeps the same PID, session, terminal and ownership markers.
                // Refuse a vanished cwd instead of falling back to the home directory.
                let error = std::process::Command::new(program)
                    .args(args)
                    .current_dir(cwd)
                    .exec();
                return Err(error.into());
            }
            None => {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "terminal host did not publish launch ownership"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

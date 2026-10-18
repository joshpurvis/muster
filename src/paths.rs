//! Filesystem locations and the daemon discovery file. Every location can be overridden by
//! environment variables so parallel (test) daemons never collide:
//! - `MUSTER_DATA_DIR` (default `$XDG_DATA_HOME/muster`): database, session logs, worktrees.
//! - `MUSTER_RUNTIME_DIR` (default `$XDG_RUNTIME_DIR/muster`, fallback `~/.local/state/muster`):
//!   `daemon.json`, lock file, daemon log.
//! - `MUSTER_PORT` (default 7433; when unset and busy, a free port is picked).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 7433;

/// Env override, made absolute (a relative dir would resolve differently for `git -C`).
fn env_path(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var).filter(|v| !v.is_empty())?);
    Some(std::path::absolute(&p).unwrap_or(p))
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn data_dir() -> PathBuf {
    // Unit tests never touch the user's data (session logs, worktrees).
    #[cfg(test)]
    return std::env::temp_dir().join(format!("muster-unit-{}", std::process::id()));
    #[cfg(not(test))]
    env_path("MUSTER_DATA_DIR")
        .or_else(|| dirs::data_dir().map(|d| d.join("muster")))
        .unwrap_or_else(|| home_dir().join(".local/share/muster"))
}

pub fn runtime_dir() -> PathBuf {
    env_path("MUSTER_RUNTIME_DIR")
        .or_else(|| dirs::runtime_dir().map(|d| d.join("muster")))
        .unwrap_or_else(|| home_dir().join(".local/state/muster"))
}

/// Port requested via `MUSTER_PORT`, if any. `Err` if set but unparsable.
pub fn port_override() -> anyhow::Result<Option<u16>> {
    match std::env::var("MUSTER_PORT") {
        Ok(v) if !v.trim().is_empty() => {
            Ok(Some(v.trim().parse().map_err(|_| {
                anyhow::anyhow!("MUSTER_PORT is not a valid port: {v}")
            })?))
        }
        _ => Ok(None),
    }
}

pub fn discovery_path() -> PathBuf {
    runtime_dir().join("daemon.json")
}
/// Held by the terminal host for its whole life (see `terminal::ensure_host`). Older,
/// monolithic builds take this lock to start, so they refuse to run beside surviving
/// host-owned terminals and can't autostart duplicates of them.
pub fn lock_path() -> PathBuf {
    runtime_dir().join("daemon.lock")
}
/// One application backend per runtime directory.
pub fn backend_lock_path() -> PathBuf {
    runtime_dir().join("backend.lock")
}
pub fn log_path() -> PathBuf {
    runtime_dir().join("daemon.log")
}
pub fn db_path() -> PathBuf {
    data_dir().join("muster.db")
}
pub fn client_token_path() -> PathBuf {
    data_dir().join("client_token")
}
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}
pub fn session_dir(id: crate::model::SessionId) -> PathBuf {
    sessions_dir().join(id.to_string())
}
pub fn worktrees_dir() -> PathBuf {
    data_dir().join("worktrees")
}
/// Agent home directories (`agents/<name>/`), also the "agents" workspace (PLAN.md §10).
pub fn agents_dir() -> PathBuf {
    data_dir().join("agents")
}
pub fn browser_profile_dir() -> PathBuf {
    data_dir().join("browser-profile")
}

/// Create a directory (and parents) with mode 0700.
/// An existing directory is tightened to 0700 too (it holds tokens and terminal logs).
pub fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)?;
    let meta = std::fs::metadata(p)?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// chmod 0600, ignoring a missing file.
pub fn make_private_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

/// Write a file atomically (tmp + rename) with mode 0600.
pub fn write_private(p: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent() {
        ensure_private_dir(dir)?;
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)
}

/// Contents of `daemon.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub version: String,
}

impl Discovery {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    /// UI URL carrying the client token itself (`muster url`: the user asked for it).
    pub fn ui_url(&self) -> String {
        format!("{}/#token={}", self.base_url(), self.token)
    }
    /// UI URL carrying a single-use launch code (safe to put on a browser command line), and
    /// the window to claim, if any (`#code=...&window=3`).
    pub fn launch_url(&self, code: &str, window: Option<i64>) -> String {
        match window {
            Some(w) => format!("{}/#code={code}&window={w}", self.base_url()),
            None => format!("{}/#code={code}", self.base_url()),
        }
    }
}

pub fn read_discovery() -> Option<Discovery> {
    let raw = std::fs::read(discovery_path()).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn write_discovery(d: &Discovery) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(d).map_err(std::io::Error::other)?;
    write_private(&discovery_path(), &json)
}

/// Remove daemon.json, but only if it still describes this process.
pub fn remove_discovery_if_ours() {
    if let Some(d) = read_discovery()
        && d.pid != std::process::id()
    {
        return;
    }
    let _ = std::fs::remove_file(discovery_path());
}

/// Expand a leading `~` / `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    }
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks for existence/permission.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 32 random bytes as lowercase hex.
pub fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Length-independent-ish comparison for secrets.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn launch_url_carries_window() {
        let d = super::Discovery {
            pid: 1,
            port: 7433,
            token: "t".into(),
            version: "0".into(),
        };
        assert_eq!(d.launch_url("abc", None), "http://127.0.0.1:7433/#code=abc");
        assert_eq!(d.launch_url("abc", Some(3)), "http://127.0.0.1:7433/#code=abc&window=3");
    }
}

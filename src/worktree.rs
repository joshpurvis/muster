//! Git helpers (shelling out to `git`). OWNER: core implementer. `diff` is used by mcp.rs.

use std::path::Path;

/// Diff of the worktree (including uncommitted and untracked-file summary) against `base`.
pub async fn diff(_cwd: &Path, _base: Option<&str>) -> anyhow::Result<String> {
    anyhow::bail!("not implemented")
}

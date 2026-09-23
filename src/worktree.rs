//! Git helpers (shelling out to `git`). OWNER: core implementer. `diff` is used by mcp.rs.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::process::Command;

/// Cap on the diff text returned to clients / agents.
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// Every git invocation runs with these, so a repository's own config can't make muster run
/// code: no fsmonitor daemon command, no hooks (e.g. post-checkout on `worktree add`).
const SAFE_CONFIG: &[&str] = &["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"];

fn git(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.args(SAFE_CONFIG)
        .arg("-C")
        .arg(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    c
}

/// `-c` overrides that disable every filter driver the repo configures
/// (`filter.<name>.clean/smudge/process`): `.gitattributes` would otherwise make `diff` and
/// `worktree add` run them.
async fn no_filters(cwd: &Path) -> Vec<String> {
    let Ok(out) = git(cwd)
        .args(["config", "--name-only", "--get-regexp", r"^filter\."])
        .output()
        .await
    else {
        return Vec::new();
    };
    let names = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            l.strip_prefix("filter.")
                .and_then(|r| r.rsplit_once('.'))
                .map(|(n, _)| n.to_string())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut args = Vec::new();
    for n in names {
        for (k, v) in [("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")] {
            args.push("-c".to_string());
            args.push(format!("filter.{n}.{k}={v}"));
        }
    }
    args
}

/// `run` with the repo's filter drivers disabled (for commands that read or write work tree
/// files).
async fn run_unfiltered(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let pre = no_filters(cwd).await;
    let mut all: Vec<&str> = pre.iter().map(String::as_str).collect();
    all.extend_from_slice(args);
    run(cwd, &all).await
}

/// Run git, return trimmed stdout, or an error carrying stderr.
async fn run(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = git(cwd).args(args).output().await.context("running git")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

async fn succeeds(cwd: &Path, args: &[&str]) -> bool {
    matches!(git(cwd).args(args).output().await, Ok(o) if o.status.success())
}

/// True if `path` is inside a git work tree.
pub async fn is_git_repo(path: &Path) -> bool {
    matches!(
        run(path, &["rev-parse", "--is-inside-work-tree"]).await.as_deref(),
        Ok("true")
    )
}

/// Current branch name, or the commit sha when HEAD is detached.
pub async fn current_head(repo: &Path) -> anyhow::Result<String> {
    let name = run(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if name == "HEAD" || name.is_empty() {
        run(repo, &["rev-parse", "HEAD"]).await
    } else {
        Ok(name)
    }
}

/// Lowercase ascii slug: [a-z0-9-], no leading/trailing/double dashes, max 40 chars.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "channel".to_string() } else { out }
}

#[derive(Debug, Clone)]
pub struct CreatedWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// What the worktree was based on (branch name or sha).
    pub base: String,
}

/// `git worktree add -b muster/<slug> <root>/<slug> <base>`, deduplicating branch and path
/// with -2, -3, ...
pub async fn create(repo: &Path, root: &Path, slug: &str, base: Option<&str>) -> anyhow::Result<CreatedWorktree> {
    let base = match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => {
            if b.starts_with('-') {
                anyhow::bail!("invalid base: {b}");
            }
            if !succeeds(repo, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")]).await {
                anyhow::bail!("base '{b}' is not a commit in {}", repo.display());
            }
            b.to_string()
        }
        None => current_head(repo)
            .await
            .context("repository has no commits yet (make an initial commit first)")?,
    };
    crate::paths::ensure_private_dir(root)?;
    for n in 1..1000 {
        let name = if n == 1 {
            slug.to_string()
        } else {
            format!("{slug}-{n}")
        };
        let branch = format!("muster/{name}");
        let path = root.join(&name);
        if path.exists()
            || succeeds(
                repo,
                &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
            )
            .await
        {
            continue;
        }
        let path_s = path.to_string_lossy().to_string();
        match run_unfiltered(repo, &["worktree", "add", "-b", &branch, &path_s, &base]).await {
            Ok(_) => return Ok(CreatedWorktree { path, branch, base }),
            // Lost a race with a concurrent create: try the next suffix.
            Err(e) if e.to_string().contains("already exists") => continue,
            Err(e) => return Err(e),
        }
    }
    anyhow::bail!("could not find a free worktree name for {slug}")
}

/// Remove a worktree (keeps the branch). Falls back to deleting the directory + prune.
pub async fn remove(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let path_s = path.to_string_lossy().to_string();
    if run(repo, &["worktree", "remove", "--force", &path_s]).await.is_ok() {
        return Ok(());
    }
    if path.exists() {
        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("removing {}", path.display()))?;
    }
    run(repo, &["worktree", "prune"]).await?;
    Ok(())
}

/// Diff of the worktree (including uncommitted and untracked-file summary) against `base`.
pub async fn diff(cwd: &Path, base: Option<&str>) -> anyhow::Result<String> {
    if !is_git_repo(cwd).await {
        anyhow::bail!("{} is not a git repository", cwd.display());
    }
    let has_head = succeeds(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]).await;
    // Diff against the merge-base so changes made on `base` since branching don't show up.
    let from = match base.map(str::trim).filter(|b| !b.is_empty() && !b.starts_with('-')) {
        Some(b) if has_head => match run(cwd, &["merge-base", "HEAD", b]).await {
            Ok(mb) if !mb.is_empty() => Some(mb),
            _ => Some("HEAD".to_string()),
        },
        _ if has_head => Some("HEAD".to_string()),
        _ => None,
    };
    let mut out = match &from {
        // No external diff drivers or textconv filters (both run repo-configured commands).
        Some(from) => {
            run_unfiltered(
                cwd,
                &["diff", "--no-color", "--no-ext-diff", "--no-textconv", from, "--"],
            )
            .await?
        }
        None => String::new(),
    };
    let untracked = run(cwd, &["ls-files", "--others", "--exclude-standard"])
        .await
        .unwrap_or_default();
    let untracked: Vec<&str> = untracked.lines().filter(|l| !l.is_empty()).collect();
    if !untracked.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("# Untracked files ({}):\n", untracked.len()));
        for f in untracked {
            out.push_str("#   ");
            out.push_str(f);
            out.push('\n');
        }
    }
    if out.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n# [diff truncated]\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success();
        assert!(ok, "{script}");
    }

    #[tokio::test]
    async fn repo_config_cannot_run_commands() {
        let base = std::env::temp_dir().join(format!("muster-wt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let pwned = base.join("pwned");
        let p = pwned.display();
        sh(
            &repo,
            &format!(
                "git init -q -b main && git config user.name t && git config user.email t@t && \
                 printf 'a\\n' > f.txt && git add f.txt && git commit -qm init && \
                 echo 'f.txt filter=evil diff=evil' > .gitattributes && git add .gitattributes && git commit -qm attrs && \
                 git config filter.evil.clean 'touch {p}.clean; cat' && \
                 git config filter.evil.smudge 'touch {p}.smudge; cat' && \
                 git config diff.evil.textconv 'touch {p}.textconv; cat' && \
                 git config diff.external 'touch {p}.ext' && \
                 git config core.fsmonitor 'touch {p}.fsmonitor; false' && \
                 mkdir -p .git/hooks && printf '#!/bin/sh\\ntouch {p}.hook\\n' > .git/hooks/post-checkout && \
                 chmod +x .git/hooks/post-checkout && printf 'b\\n' >> f.txt"
            ),
        );
        let d = diff(&repo, Some("main")).await.unwrap();
        assert!(d.contains("+b"), "{d}");
        create(&repo, &base.join("wts"), "x", None).await.unwrap();
        let hits: Vec<String> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("pwned"))
            .collect();
        assert!(hits.is_empty(), "repo config ran commands: {hits:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}

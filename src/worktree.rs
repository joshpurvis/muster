//! Git helpers (shelling out to `git`). OWNER: core implementer. `diff` is used by mcp.rs.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::process::Command;

/// Cap on the diff text returned to clients / agents.
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// Every git invocation runs with these, so a repository's own config can't make muster run
/// code: no fsmonitor daemon command, no hooks (e.g. post-checkout on `worktree add`).
const SAFE_CONFIG: &[&str] = &["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"];

#[cfg(test)]
static TEST_ENV: parking_lot::Mutex<Vec<(PathBuf, Vec<(String, String)>)>> = parking_lot::Mutex::new(Vec::new());

fn git(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    // Tests give their own repos a private HOME / global config without touching the
    // process environment (other tests run git concurrently).
    #[cfg(test)]
    for (prefix, env) in TEST_ENV.lock().iter() {
        if cwd.starts_with(prefix) {
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
    }
    c.args(SAFE_CONFIG)
        .arg("-C")
        .arg(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    c
}

/// Config scopes a repository controls (a cloned or downloaded repo's own settings). Filter
/// drivers from system/global config (e.g. git-lfs) are the user's own and stay in effect.
const REPO_SCOPES: &[&str] = &["local", "worktree"];
const FILTER_KEYS: &[&str] = &["clean", "smudge", "process", "required"];

/// `-c` overrides for `git config --show-scope -z --get-regexp '^filter\.'` output: every
/// filter driver defined (or redefined) in the repo's local/worktree config is neutralized
/// (`.gitattributes` would otherwise make `diff` and `worktree add` run it). A driver that
/// system/global config also defines gets those values back instead of the repo's.
fn filter_overrides(listing: &[u8]) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    // (name, key) -> last value outside the repo's scopes.
    let mut user: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut repo_names: BTreeSet<String> = BTreeSet::new();
    let mut it = listing.split(|b| *b == 0);
    while let (Some(scope), Some(entry)) = (it.next(), it.next()) {
        let scope = String::from_utf8_lossy(scope);
        let entry = String::from_utf8_lossy(entry);
        let (key, value) = entry.split_once('\n').unwrap_or((&entry, "true"));
        let Some((name, k)) = key.strip_prefix("filter.").and_then(|r| r.rsplit_once('.')) else {
            continue;
        };
        if REPO_SCOPES.contains(&scope.as_ref()) {
            repo_names.insert(name.to_string());
        } else {
            user.insert((name.to_string(), k.to_ascii_lowercase()), value.to_string());
        }
    }
    let mut args = Vec::new();
    for n in repo_names {
        for k in FILTER_KEYS {
            let v = match user.get(&(n.clone(), k.to_string())) {
                Some(v) => v.clone(),
                None if *k == "required" => "false".to_string(),
                None => String::new(),
            };
            args.push("-c".to_string());
            args.push(format!("filter.{n}.{k}={v}"));
        }
    }
    args
}

/// `-c` overrides that disable the filter drivers the repository itself configures. Fails
/// closed: if the scoped listing isn't available (git < 2.26) or fails, every configured
/// filter driver is disabled.
async fn no_filters(cwd: &Path) -> Vec<String> {
    let scoped = git(cwd)
        .args(["config", "--show-scope", "-z", "--get-regexp", r"^filter\."])
        .output()
        .await;
    match scoped {
        Ok(out) if out.status.success() => return filter_overrides(&out.stdout),
        // Exit 1: no matching keys.
        Ok(out) if out.status.code() == Some(1) => return Vec::new(),
        _ => {}
    }
    let names = match git(cwd)
        .args(["config", "--name-only", "-z", "--get-regexp", r"^filter\."])
        .output()
        .await
    {
        Ok(out) => out.stdout,
        Err(_) => Vec::new(),
    };
    disable_all(&names)
}

/// Overrides that neutralize every filter driver in a `--name-only -z` listing.
fn disable_all(names: &[u8]) -> Vec<String> {
    let names: std::collections::BTreeSet<String> = names
        .split(|b| *b == 0)
        .filter_map(|k| {
            String::from_utf8_lossy(k)
                .strip_prefix("filter.")
                .and_then(|r| r.rsplit_once('.'))
                .map(|(n, _)| n.to_string())
        })
        .collect();
    let mut args = Vec::new();
    for n in names {
        for k in FILTER_KEYS {
            let v = if *k == "required" { "false" } else { "" };
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

    #[test]
    fn filter_overrides_keep_user_drivers() {
        let listing = b"global\0filter.lfs.clean\ngit-lfs clean -- %f\0global\0filter.lfs.required\ntrue\0\
local\0filter.evil.smudge\ntouch x\0local\0filter.lfs.smudge\nevil\0worktree\0filter.w.x.clean\nbad\0\
system\0filter.sys.clean\nok\0local\0filter.bare.required\0";
        let o = filter_overrides(listing);
        let pairs: Vec<&str> = o
            .chunks(2)
            .map(|c| {
                assert_eq!(c[0], "-c");
                c[1].as_str()
            })
            .collect();
        // Repo-defined drivers are neutralized; a redefined user driver gets the user's values.
        for want in [
            "filter.evil.clean=",
            "filter.evil.smudge=",
            "filter.evil.process=",
            "filter.evil.required=false",
            "filter.lfs.clean=git-lfs clean -- %f",
            "filter.lfs.smudge=",
            "filter.lfs.required=true",
            "filter.w.x.clean=",
            "filter.bare.required=false",
        ] {
            assert!(pairs.contains(&want), "{want} missing from {pairs:?}");
        }
        // Drivers only the user defines are left alone.
        assert!(!pairs.iter().any(|p| p.starts_with("filter.sys.")));
        assert_eq!(pairs.len(), 4 * 4);
    }

    #[test]
    fn fallback_disables_every_driver() {
        let o = disable_all(b"filter.lfs.clean\0filter.a.b.smudge\0");
        assert_eq!(o.len(), 2 * 4 * 2);
        assert!(o.contains(&"filter.lfs.clean=".to_string()));
        assert!(o.contains(&"filter.a.b.required=false".to_string()));
    }

    #[tokio::test]
    async fn global_filters_apply_repo_filters_dont() {
        let base = std::env::temp_dir().join(format!("muster-wt-filters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let home = base.join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let gcfg = home.join("gitconfig");
        let pwned = base.join("pwned");
        // A benign, globally configured LFS-like filter: stores "POINTER <content>", checks out
        // the content. Plus a repo-local evil filter.
        std::fs::write(
            &gcfg,
            "[filter \"fake\"]\n\tclean = sed -e 's/^/POINTER /'\n\tsmudge = sed -e 's/^POINTER //'\n\trequired = true\n",
        )
        .unwrap();
        let env = vec![
            ("HOME".to_string(), home.display().to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), gcfg.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ];
        TEST_ENV.lock().push((base.clone(), env.clone()));
        let run = |script: &str| {
            let ok = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(&repo)
                .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .status()
                .unwrap()
                .success();
            assert!(ok, "{script}");
        };
        let p = pwned.display();
        run(&format!(
            "git init -q -b main && git config user.name t && git config user.email t@t && \
             printf '*.bin filter=fake\\nevil.txt filter=evil\\n' > .gitattributes && \
             printf 'data\\n' > a.bin && printf 'e\\n' > evil.txt && git add -A && git commit -qm init && \
             git config filter.evil.clean 'touch {p}.clean; cat' && \
             git config filter.evil.smudge 'touch {p}.smudge; cat' && \
             printf 'x\\n' >> evil.txt"
        ));
        // The committed blob is the pointer; the work tree has the content.
        let blob = std::process::Command::new("git")
            .args(["-C", &repo.display().to_string(), "cat-file", "-p", "HEAD:a.bin"])
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&blob.stdout), "POINTER data\n");
        let wt = create(&repo, &base.join("wts"), "x", None).await.unwrap();
        // The global filter ran on checkout: content, not the pointer.
        assert_eq!(std::fs::read_to_string(wt.path.join("a.bin")).unwrap(), "data\n");
        // And diff doesn't report the filtered file as changed.
        let d = diff(&repo, Some("main")).await.unwrap();
        assert!(!d.contains("a.bin"), "{d}");
        assert!(d.contains("+x"), "{d}");
        let d = diff(&wt.path, Some("main")).await.unwrap();
        assert!(!d.contains("a.bin"), "{d}");
        let hits: Vec<String> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("pwned"))
            .collect();
        assert!(hits.is_empty(), "the repo's filter ran: {hits:?}");
        TEST_ENV.lock().retain(|(b, _)| b != &base);
        let _ = std::fs::remove_dir_all(&base);
    }
}

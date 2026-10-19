//! `muster pr list` / `muster pr snapshot` end to end, with stub `gh` and `curl` scripts first
//! (and alone) on PATH. Every run gets temp MUSTER_RUNTIME_DIR, MUSTER_DATA_DIR and MUSTER_PORT
//! (AGENTS.md): these commands never talk to a daemon, and must not reach the user's.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// Answers by the GraphQL operation name or REST path in its arguments: `<key>.json`, or, when
/// `<key>.exit` exists, `<key>.out` / `<key>.err` and that exit code. `<key>.once` is printed
/// (with exit 1) on the first call only. Every call's argv is appended to `gh-calls`
/// (arguments end in \x1f, calls in \x1e).
const GH_STUB: &str = r#"#!/bin/bash
d="$(cd "$(dirname "$0")" && pwd)"
printf '%s\x1f' "$@" >> "$d/gh-calls"; printf '\x1e\n' >> "$d/gh-calls"
key=other
for a in "$@"; do
  case "$a" in
    *"query PrSnapshot"*) key=snapshot;;
    *"query PrHead"*) key=head;;
    *"query PrChecks"*) key=checks;;
    *"query PrListBatch"*) key=batch;;
    *"query PrList("*) key=list;;
    "Accept: application/vnd.github.diff") key=diff;;
  esac
done
if [ "$key" = other ]; then case "$*" in *"/compare/"*) key=compare;; esac; fi
if [ "$key" = checks ]; then case "$*" in *"after="*) key=checks2;; esac; fi
if [ -f "$d/$key.once" ]; then cat "$d/$key.once"; rm -f "$d/$key.once"; echo "gh: slow down (HTTP 403)" >&2; exit 1; fi
if [ -f "$d/$key.exit" ]; then
  [ -f "$d/$key.out" ] && cat "$d/$key.out"
  [ -f "$d/$key.err" ] && cat "$d/$key.err" >&2
  exit "$(cat "$d/$key.exit")"
fi
cat "$d/$key.json"
"#;

/// Reads the URL from the config on stdin (`-K -`) and prints `img-<last path segment>`, else
/// fails like curl -f. Records its argv in `curl-calls` and its stdin in `curl-stdin`.
const CURL_STUB: &str = r#"#!/bin/bash
d="$(cd "$(dirname "$0")" && pwd)"
printf '%s\x1f' "$@" >> "$d/curl-calls"; printf '\x1e\n' >> "$d/curl-calls"
cfg="$(cat)"; printf '%s\x1e\n' "$cfg" >> "$d/curl-stdin"
url="$(printf '%s\n' "$cfg" | sed -n 's/^url = "\(.*\)"$/\1/p')"
seg="${url%%\?*}"; seg="${seg##*/}"
[ -f "$d/img-$seg" ] || { echo "curl: (22) The requested URL returned error: 404" >&2; exit 22; }
cat "$d/img-$seg"
"#;

struct Fx {
    root: PathBuf,
    bin: PathBuf,
    cwd: PathBuf,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value { serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}\n{}", self.stdout, self.stderr)) }
}

impl Fx {
    fn new(name: &str) -> Fx {
        let root = std::env::temp_dir().join(format!("muster-pr-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bin, cwd) = (root.join("bin"), root.join("cwd"));
        for d in [&bin, &cwd, &root.join("run"), &root.join("data"), &root.join("home")] { std::fs::create_dir_all(d).unwrap(); }
        for (file, text) in [("gh", GH_STUB), ("curl", CURL_STUB)] {
            std::fs::write(bin.join(file), text).unwrap();
            std::fs::set_permissions(bin.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Fx { root, bin, cwd }
    }

    fn set(&self, file: &str, data: impl AsRef<[u8]>) { std::fs::write(self.bin.join(file), data).unwrap(); }
    fn json(&self, key: &str, v: Value) { self.set(&format!("{key}.json"), v.to_string()); }
    fn fail(&self, key: &str, code: i32, stderr: &str) {
        self.set(&format!("{key}.exit"), code.to_string());
        self.set(&format!("{key}.err"), stderr);
    }
    fn calls(&self, tool: &str) -> String { std::fs::read_to_string(self.bin.join(format!("{tool}-calls"))).unwrap_or_default() }
    fn count(&self, tool: &str) -> usize { self.calls(tool).matches('\x1e').count() }

    fn run_with_path(&self, path: &Path, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_muster"))
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("MUSTER_RUNTIME_DIR", self.root.join("run"))
            .env("MUSTER_DATA_DIR", self.root.join("data"))
            .env("MUSTER_PORT", "0")
            .output()
            .unwrap();
        Run { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into(), stderr: String::from_utf8_lossy(&out.stderr).into() }
    }

    /// The stubs come first on PATH, so the real gh and curl are never reached.
    fn run(&self, args: &[&str]) -> Run {
        let path = format!("{}:/usr/bin:/bin", self.bin.display());
        self.run_with_path(Path::new(&path), args)
    }

    /// The `.muster-pr-*` directories in the working directory.
    fn media_dirs(&self) -> Vec<String> {
        std::fs::read_dir(&self.cwd).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".muster-pr-")).collect()
    }

    /// A standard open PR: snapshot, head (unchanged), checks and a two-file diff.
    fn open_pr(&self, title: &str, body: &str, head_branch: &str) {
        self.json("snapshot", json!({"data": {"repository": {
            "viewerDefaultMergeMethod": "MERGE", "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": true,
            "deleteBranchOnMerge": true,
            "pullRequest": {"number": 812, "title": title, "url": "https://github.com/acme/api/pull/812", "state": "OPEN", "isDraft": false,
                "body": body, "bodyHTML": "", "mergeable": "MERGEABLE", "reviewDecision": "APPROVED", "author": {"login": "dana"},
                "baseRefName": "master", "baseRefOid": BASE, "headRefName": head_branch, "headRefOid": HEAD,
                "mergedAt": null, "closedAt": null, "mergeCommit": null,
                "latestReviews": {"nodes": [{"author": {"login": "lee"}, "state": "APPROVED"}, {"author": null, "state": "COMMENTED"}]}}}}}));
        self.open_pr_rest();
    }

    /// Everything but the snapshot read: head unchanged, two checks, a two-file diff.
    fn open_pr_rest(&self) {
        self.head(HEAD, "OPEN");
        self.json("checks", json!({"data": {"repository": {"object": {"statusCheckRollup": {"contexts": {
            "totalCount": 2, "pageInfo": {"hasNextPage": false, "endCursor": "x"},
            "nodes": [
                {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci.example/1",
                 "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
                {"__typename": "StatusContext", "context": "deploy/preview", "state": "PENDING", "targetUrl": "https://preview.example"}]}}}}}}));
        self.set("diff.json", "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+more\n\
diff --git a/logo.png b/logo.png\nnew file mode 100644\nindex 0..1\nBinary files /dev/null and b/logo.png differ\n");
    }

    fn head(&self, sha: &str, state: &str) {
        self.json("head", json!({"data": {"repository": {"pullRequest": {"state": state, "headRefOid": sha, "baseRefName": "master", "baseRefOid": BASE}}}}));
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        if !std::thread::panicking() { let _ = std::fs::remove_dir_all(&self.root); }
    }
}

fn snapshot(fx: &Fx, extra: &[&str]) -> Run {
    let mut args = vec!["pr", "snapshot", "--repo", "acme/api", "812"];
    args.extend_from_slice(extra);
    fx.run(&args)
}

#[test]
fn snapshot_happy_path_shape_and_pictures() {
    let fx = Fx::new("happy");
    let body = "Fixes login.\n![Login page](https://github.com/user-attachments/assets/aaaa-1111)\n\
        ![evil](https://evil.example/x.png) ![big](https://raw.githubusercontent.com/a/b/big.png)\n\
        <img alt=\"page\" src=\"https://raw.githubusercontent.com/a/b/page.png\"> ![plain](http://user-images.githubusercontent.com/1/p.png)";
    fx.open_pr("Fix login", body, "dana/login");
    let mut png = PNG.to_vec();
    png.extend_from_slice(&[0; 100]);
    fx.set("img-aaaa-1111", &png);
    let mut big = PNG.to_vec();
    big.resize(2_000_000, 0);
    fx.set("img-big.png", &big);
    fx.set("img-page.png", "<html>not a picture</html>");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    for (k, want) in [("repo", json!("acme/api")), ("number", json!(812)), ("title", json!("Fix login")), ("author", json!("dana")),
        ("state", json!("OPEN")), ("draft", json!(false)), ("base", json!("master")), ("base_sha", json!(BASE)), ("head", json!("dana/login")),
        ("head_sha", json!(HEAD)), ("mergeable", json!("MERGEABLE")), ("merge_method_default", json!("squash")),
        ("delete_branch_default", json!(true)), ("body", json!(body)), ("files_complete", json!(true))] {
        assert_eq!(v[k], want, "{k}");
    }
    assert_eq!(v["checks_complete"], true);
    assert_eq!(v["checks_total"], Value::Null);
    assert_eq!(v["checks"], json!({"state": "pending", "items": [
        {"name": "CI / test", "state": "pass", "url": "https://ci.example/1"},
        {"name": "deploy/preview", "state": "pending", "url": "https://preview.example"}]}));
    assert_eq!(v["reviews"], json!({"decision": "APPROVED", "items": [{"author": "lee", "state": "APPROVED"}, {"author": "ghost", "state": "COMMENTED"}]}));
    assert_eq!(v["files"], json!([
        {"file": "src/a.rs", "status": "modified", "additions": 2, "deletions": 1, "binary": false, "truncated": false,
         "diff": "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+more\n"},
        {"file": "logo.png", "status": "added", "additions": 0, "deletions": 0, "binary": true, "truncated": false,
         "note": "binary file: no text diff", "diff": "diff --git a/logo.png b/logo.png\nnew file mode 100644\nindex 0..1\nBinary files /dev/null and b/logo.png differ\n"}]));
    assert!(v["fetched_at"].as_str().is_some_and(|t| t.len() == 20 && t.ends_with('Z')));

    // One picture downloaded into a fresh private directory here; the rest skipped with reasons.
    let dirs = fx.media_dirs();
    assert_eq!(dirs.len(), 1);
    let dir = &dirs[0];
    assert_eq!(v["media_dir"], json!(dir));
    assert_eq!(v["media"], json!([{"kind": "image", "path": format!("{dir}/1.png"), "alt": "Login page"}]));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fx.cwd.join(dir)), 0o700);
    assert_eq!(mode(&fx.cwd.join(dir).join("1.png")), 0o600);
    assert_eq!(std::fs::read(fx.cwd.join(dir).join("1.png")).unwrap(), png);
    let skipped: Vec<(String, String)> = v["media_skipped"].as_array().unwrap().iter()
        .map(|s| (s["url"].as_str().unwrap().to_string(), s["reason"].as_str().unwrap().to_string())).collect();
    let reason = |url: &str| skipped.iter().find(|s| s.0 == url).map(|s| s.1.clone()).unwrap_or_else(|| panic!("{url} not skipped: {skipped:?}"));
    assert!(reason("https://evil.example/x.png").contains("not on GitHub"));
    assert!(reason("http://user-images.githubusercontent.com/1/p.png").contains("https"));
    assert!(reason("https://raw.githubusercontent.com/a/b/big.png").contains("larger than 1.5 MB"));
    assert!(reason("https://raw.githubusercontent.com/a/b/page.png").contains("not a PNG"));

    // curl only ever saw allowlisted URLs, with https-only redirects and the caps.
    let curl = fx.calls("curl");
    assert_eq!(fx.count("curl"), 3, "{curl}");
    assert!(!curl.contains("evil.example") && !curl.contains("http://"), "{curl}");
    for flag in ["-q\x1f", "--proto\x1f=https\x1f", "--proto-redir\x1f=https\x1f", "--max-filesize\x1f1500000\x1f", "--max-time\x1f20\x1f", "-K\x1f-\x1f"] {
        assert!(curl.split('\x1e').filter(|c| !c.trim().is_empty()).all(|c| c.contains(flag)), "{flag:?} in {curl}");
    }
    assert!(curl.split('\x1e').all(|c| c.trim_start().is_empty() || c.trim_start().starts_with("-q\x1f")), "-q comes first: {curl}");
    // The URLs went on stdin, never in argv.
    assert!(!curl.contains("https://"), "{curl}");
    let stdin = std::fs::read_to_string(fx.bin.join("curl-stdin")).unwrap();
    assert!(stdin.contains("url = \"https://github.com/user-attachments/assets/aaaa-1111\""), "{stdin}");
    // gh: the PR, the diff and checks for exactly the head SHA, then the PR again. Read-only.
    let gh = fx.calls("gh");
    assert_eq!(fx.count("gh"), 4, "{gh}");
    assert!(gh.contains(&format!("repos/acme/api/compare/{BASE}...{HEAD}\x1f")), "{gh}");
    assert!(gh.contains(&format!("oid={HEAD}\x1f")), "{gh}");
    for write in ["merge", "review", "comment", "-X", "--method", "POST", "PATCH", "edit"] {
        assert!(!gh.split(['\x1f', '\x1e', '\n']).any(|a| a == write), "{write} in {gh}");
    }
}

#[test]
fn signed_attachment_links_stay_out_of_every_argv_and_output() {
    let fx = Fx::new("signed");
    fx.open_pr("T", "![shot](https://github.com/user-attachments/assets/aaaa1111-2222)", "dana/x");
    let mut snap: Value = serde_json::from_slice(&std::fs::read(fx.bin.join("snapshot.json")).unwrap()).unwrap();
    snap["data"]["repository"]["pullRequest"]["bodyHTML"] =
        json!("<p><img alt=\"shot\" src=\"https://private-user-images.githubusercontent.com/9/77-aaaa1111-2222.png?jwt=SECRETJWT.part2-x\"></p>");
    fx.json("snapshot", snap);
    fx.set("img-77-aaaa1111-2222.png", PNG);
    let r = snapshot(&fx, &["--verbose"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["media"][0]["alt"], "shot");
    assert_eq!(fx.count("curl"), 1);
    let stdin = std::fs::read_to_string(fx.bin.join("curl-stdin")).unwrap();
    assert!(stdin.contains("url = \"https://private-user-images.githubusercontent.com/9/77-aaaa1111-2222.png?jwt=SECRETJWT.part2-x\""), "{stdin}");
    for (what, text) in [("curl argv", fx.calls("curl")), ("gh argv", fx.calls("gh")), ("stdout", r.stdout.clone()), ("stderr", r.stderr.clone())] {
        assert!(!text.contains("SECRETJWT"), "the signed link leaked into {what}: {text}");
    }
    // A failed download's reason never carries it either.
    let fx2 = Fx::new("signed-fail");
    fx2.set("snapshot.json", std::fs::read(fx.bin.join("snapshot.json")).unwrap());
    fx2.open_pr_rest();
    let r = snapshot(&fx2, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.json()["media_skipped"][0]["reason"].as_str().unwrap().contains("refused"));
    assert!(!r.stdout.contains("SECRETJWT") && !r.stderr.contains("SECRETJWT"), "{}", r.stdout);
}

#[test]
fn no_shell_ever_sees_pr_text() {
    let fx = Fx::new("noshell");
    let title = "Add $(touch PWNED1) and `touch PWNED2` support";
    let body = "Run $(touch PWNED3); `touch PWNED4` ${HOME}\n<img src=\"https://raw.githubusercontent.com/a/$(touch%20PWNED5).png\">";
    fx.open_pr(title, body, "dana/$(touch PWNED6)");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["title"], json!(title));
    assert_eq!(v["body"], json!(body));
    assert_eq!(v["head"], json!("dana/$(touch PWNED6)"));
    assert_eq!(v["media_skipped"][0]["url"], json!("https://raw.githubusercontent.com/a/$(touch%20PWNED5).png"));
    // Nothing ran: no file anywhere under the fixture, and no PR text in any command line.
    let mut stack = vec![fx.root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            assert!(!e.file_name().to_string_lossy().contains("PWNED"), "{:?} was created", e.path());
            if e.file_type().unwrap().is_dir() { stack.push(e.path()); }
        }
    }
    assert!(!fx.calls("gh").contains("PWNED") && fx.calls("curl").is_empty());
}

#[test]
fn head_moved_exits_3_and_removes_the_pictures() {
    let fx = Fx::new("moved");
    fx.open_pr("T", "![a](https://github.com/user-attachments/assets/aaaa-1111)", "dana/x");
    fx.set("img-aaaa-1111", PNG);
    fx.head("cccccccccccccccccccccccccccccccccccccccc", "OPEN");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 3, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["error"], "head_moved");
    assert_eq!(v["head_sha"], HEAD);
    assert_eq!(v["head_sha_now"], "cccccccccccccccccccccccccccccccccccccccc");
    assert!(r.stderr.contains("run it again"), "{}", r.stderr);
    assert!(fx.media_dirs().is_empty(), "{:?}", fx.media_dirs());
}

#[test]
fn closed_pr_exits_4_with_its_state_and_fetches_nothing_else() {
    let fx = Fx::new("closed");
    fx.open_pr("T", "", "dana/x");
    let mut snap: Value = serde_json::from_slice(&std::fs::read(fx.bin.join("snapshot.json")).unwrap()).unwrap();
    let pr = &mut snap["data"]["repository"]["pullRequest"];
    pr["state"] = json!("MERGED");
    pr["mergedAt"] = json!("2026-10-18T09:30:00Z");
    pr["mergeCommit"] = json!({"oid": "dddddddddddddddddddddddddddddddddddddddd"});
    fx.json("snapshot", snap);
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 4, "{}", r.stderr);
    let v = r.json();
    assert_eq!((v["state"].as_str(), v["merge_commit"].as_str(), v["head_sha"].as_str()),
        (Some("MERGED"), Some("dddddddddddddddddddddddddddddddddddddddd"), Some(HEAD)));
    assert_eq!(fx.count("gh"), 1);
    // Closed while reading: the full snapshot, with the new state, still exit 4.
    let fx = Fx::new("closed-late");
    fx.open_pr("T", "", "dana/x");
    fx.json("head", json!({"data": {"repository": {"pullRequest": {"state": "MERGED", "headRefOid": HEAD, "baseRefName": "master",
        "baseRefOid": BASE, "mergedAt": "2026-10-19T08:00:00Z", "closedAt": "2026-10-19T08:00:00Z",
        "mergeCommit": {"oid": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}}}}));
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 4, "{}", r.stderr);
    let v = r.json();
    assert_eq!((v["state"].as_str(), v["merged_at"].as_str(), v["closed_at"].as_str(), v["merge_commit"].as_str()),
        (Some("MERGED"), Some("2026-10-19T08:00:00Z"), Some("2026-10-19T08:00:00Z"), Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")));
    assert!(v["files"].is_array());
    let fx = Fx::new("closed-late2");
    fx.open_pr("T", "", "dana/x");
    fx.head(HEAD, "CLOSED");
    let v = snapshot(&fx, &[]).json();
    assert_eq!((v["state"].as_str(), v["merge_commit"].clone(), v.get("merged_at").is_some()), (Some("CLOSED"), Value::Null, true));
}

#[test]
fn gh_failures_exit_5_with_json() {
    let fx = Fx::new("ghfail");
    fx.open_pr("T", "", "dana/x");
    fx.fail("diff", 1, "gh: Server Error (HTTP 500)");
    fx.fail("compare", 1, "gh: Server Error (HTTP 500)");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert_eq!(r.json()["error"], "gh_failed");
    assert!(r.json()["detail"].as_str().unwrap().contains("HTTP 500"));
    assert!(!fx.calls("gh").contains("per_page=1"), "only a too-large diff falls back to the files API");

    let fx = Fx::new("ghauth");
    fx.fail("snapshot", 4, "To get started with GitHub CLI, please run:  gh auth login");
    let r = snapshot(&fx, &[]);
    assert_eq!((r.code, r.json()["error"].as_str()), (5, Some("gh_auth")), "{}", r.stderr);
    assert!(r.stderr.contains("gh auth login"));

    let fx = Fx::new("ghnotfound");
    fx.fail("snapshot", 1, "gh: Could not resolve to a PullRequest with the number of 812.");
    assert_eq!(snapshot(&fx, &[]).json()["error"], "not_found");

    // No gh at all: PATH has nothing in it.
    let fx = Fx::new("ghmissing");
    let empty = fx.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    for args in [&["pr", "snapshot", "--repo", "acme/api", "812"][..], &["pr", "list", "--repo", "acme/api"]] {
        let r = fx.run_with_path(&empty, args);
        assert_eq!((r.code, r.json()["error"].as_str()), (5, Some("gh_missing")), "{}", r.stderr);
    }
}

#[test]
fn bad_arguments_exit_2_before_any_gh_call() {
    let fx = Fx::new("badargs");
    fx.open_pr("T", "", "dana/x");
    let cases: &[&[&str]] = &[
        &["pr", "snapshot", "--repo", "acme", "812"],
        &["pr", "snapshot", "--repo", "acme/api/x", "812"],
        &["pr", "snapshot", "--repo", "acme;rm/api", "812"],
        &["pr", "snapshot", "--repo", "acme/$(x)", "812"],
        &["pr", "snapshot", "--repo", "acme/api", "abc"],
        &["pr", "snapshot", "--repo", "acme/api", "0"],
        &["pr", "snapshot", "--repo", "acme/api", "-5"],
        &["pr", "snapshot", "--repo", "acme/api", "812", "--parallel", "5"],
        &["pr", "snapshot", "--repo", "acme/api", "812", "--media-dir", "../out"],
        &["pr", "snapshot", "--repo", "acme/api"],
        &["pr", "list", "--repo", "acme"],
        &["pr", "list", "--repo", "acme/api", "--limit", "0"],
        &["pr", "list", "--repo", "acme/api", "--limit", "201"],
    ];
    for args in cases {
        let r = fx.run(args);
        assert_eq!(r.code, 2, "{args:?}: {}", r.stderr);
        assert!(r.stdout.is_empty(), "{args:?}: {}", r.stdout);
    }
    std::fs::create_dir(fx.cwd.join("shots")).unwrap();
    assert_eq!(snapshot(&fx, &["--media-dir", "shots"]).code, 2);
    assert_eq!(fx.calls("gh"), "", "gh ran for a bad argument");
}

#[test]
fn media_dir_is_created_where_asked() {
    let fx = Fx::new("mediadir");
    fx.open_pr("T", "![a](https://github.com/user-attachments/assets/aaaa-1111)", "dana/x");
    fx.set("img-aaaa-1111", PNG);
    let r = snapshot(&fx, &["--media-dir", "./pics"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["media_dir"], "pics");
    assert_eq!(r.json()["media"][0]["path"], "pics/1.png");
    assert_eq!(std::fs::metadata(fx.cwd.join("pics")).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(fx.media_dirs().is_empty());
}

#[test]
fn media_dir_never_goes_through_a_symlink() {
    let fx = Fx::new("medialink");
    fx.open_pr("T", "![a](https://github.com/user-attachments/assets/aaaa-1111)", "dana/x");
    fx.set("img-aaaa-1111", PNG);
    let outside = fx.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fx.cwd.join("shots")).unwrap();
    for dir in ["shots/new", "shots"] {
        let r = snapshot(&fx, &["--media-dir", dir]);
        assert_eq!(r.code, 2, "{dir}: {}", r.stderr);
        assert!(r.stderr.contains("symlink") || r.stderr.contains("already exists"), "{dir}: {}", r.stderr);
    }
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing was created outside");
    assert_eq!(fx.count("gh"), 0);
}

#[test]
fn diffs_are_capped_and_marked_truncated() {
    let fx = Fx::new("truncate");
    fx.open_pr("T", "No pictures.", "dana/x");
    let mut diff = String::from("diff --git a/small.txt b/small.txt\n--- a/small.txt\n+++ b/small.txt\n@@ -1 +1 @@\n-a\n+b\n");
    diff.push_str("diff --git a/big.txt b/big.txt\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,500 @@\n");
    for i in 0..500 { diff.push_str(&format!("+line {i}\n")); }
    fx.set("diff.json", &diff);
    let r = snapshot(&fx, &["--max-diff-bytes", "1000"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    let files = v["files"].as_array().unwrap();
    assert_eq!((files[0]["truncated"].as_bool(), files[1]["truncated"].as_bool()), (Some(false), Some(true)));
    assert_eq!(files[1]["additions"], 500, "counts come from the whole diff");
    let total: usize = files.iter().map(|f| f["diff"].as_str().unwrap().len()).sum();
    assert!(total <= 1000 && files[1]["diff"].as_str().unwrap().ends_with('\n'), "{total}");
    assert_eq!(v["media_dir"], Value::Null);
    assert_eq!(v["media"], json!([]));
}

#[test]
fn a_refused_diff_falls_back_to_the_files_api() {
    let fx = Fx::new("fallback");
    fx.open_pr("T", "", "dana/x");
    fx.fail("diff", 1, "gh: Sorry, this diff is taking too long to generate. (HTTP 406)");
    fx.json("compare", json!({"files": [
        {"filename": "a.rs", "status": "renamed", "previous_filename": "old.rs", "additions": 1, "deletions": 0, "changes": 1, "patch": "@@ -1 +1,2 @@\n x\n+y"},
        {"filename": "huge.lock", "status": "modified", "additions": 9000, "deletions": 10, "changes": 9010},
        {"filename": "pic.png", "status": "added", "additions": 0, "deletions": 0, "changes": 0},
        {"filename": "new.txt", "status": "renamed", "previous_filename": "old.txt", "additions": 0, "deletions": 0, "changes": 0},
        {"filename": "run.sh", "status": "changed", "additions": 0, "deletions": 0, "changes": 0}]}));
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["files"][0], json!({"file": "a.rs", "status": "renamed", "previous_file": "old.rs", "additions": 1, "deletions": 0,
        "binary": false, "truncated": false, "diff": "--- a/old.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n x\n+y\n"}));
    let f = |i: usize| (v["files"][i]["binary"].as_bool(), v["files"][i]["truncated"].as_bool(), v["files"][i]["diff"].as_str(), v["files"][i]["note"].as_str());
    assert_eq!(f(1), (Some(false), Some(true), Some(""), Some("GitHub left out this file's diff (too large)")));
    // Without a patch and without changed lines nothing proves a file binary.
    assert_eq!(f(2), (Some(false), Some(false), Some(""), Some("new empty file (or a binary file: GitHub's file list doesn't say)")));
    assert_eq!(f(3), (Some(false), Some(false), Some(""), Some("renamed from old.txt, content unchanged")));
    assert_eq!(v["files"][3]["previous_file"], "old.txt");
    assert_eq!(f(4), (Some(false), Some(false), Some(""), Some("file mode changed only")));
    assert!(fx.calls("gh").contains(&format!("repos/acme/api/compare/{BASE}...{HEAD}?per_page=1\x1f")));
}

#[test]
fn rate_limits_wait_or_stop_with_the_reset_time() {
    // Secondary limit with Retry-After: waited out, then it works.
    let fx = Fx::new("ratelimit2");
    fx.open_pr("T", "", "dana/x");
    fx.set("checks.once", "HTTP/2.0 403 Forbidden\r\nRetry-After: 1\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit.\"}");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stderr.contains("waiting 1 s"), "{}", r.stderr);
    assert_eq!(fx.count("gh"), 5);
    // Primary limit used up: stop at once and say when it resets.
    let fx = Fx::new("ratelimit1");
    fx.open_pr("T", "", "dana/x");
    fx.fail("snapshot", 1, "gh: API rate limit exceeded for user ID 1. (HTTP 403)");
    fx.set("snapshot.out", "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1792390335\r\n\r\n{\"message\":\"API rate limit exceeded\"}");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert_eq!(r.json()["error"], "rate_limited");
    assert_eq!(r.json()["reset_at"], "2026-10-19T06:12:15Z");
    assert_eq!(fx.count("gh"), 1);
}

#[test]
fn a_rate_limited_diff_never_falls_back() {
    // Secondary limit asking for longer than 120 s: stop, exit 5, no files API call.
    let fx = Fx::new("diff429");
    fx.open_pr("T", "", "dana/x");
    fx.fail("diff", 1, "gh: Too Many Requests (HTTP 429)");
    fx.set("diff.out", "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 999\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit.\"}");
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert_eq!(r.json()["error"], "rate_limited");
    assert!(r.json()["reset_at"].is_string());
    assert!(!fx.calls("gh").contains("per_page=1"), "{}", fx.calls("gh"));
    // Primary limit used up on the diff: the same.
    let fx = Fx::new("diffprimary");
    fx.open_pr("T", "", "dana/x");
    fx.fail("diff", 1, "gh: API rate limit exceeded for user ID 1. (HTTP 403)");
    fx.set("diff.out", "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1792390335\r\n\r\n{\"message\":\"API rate limit exceeded\"}");
    let r = snapshot(&fx, &[]);
    assert_eq!((r.code, r.json()["error"].as_str(), r.json()["reset_at"].as_str()), (5, Some("rate_limited"), Some("2026-10-19T06:12:15Z")));
    assert!(!fx.calls("gh").contains("per_page=1"));
}

fn check_page(states: &[&str], total: usize, next: bool) -> Value {
    let nodes: Vec<Value> = states.iter().enumerate().map(|(i, st)| json!({"__typename": "CheckRun", "name": format!("job{i}"),
        "status": "COMPLETED", "conclusion": st, "detailsUrl": "https://ci.example/1"})).collect();
    json!({"data": {"repository": {"object": {"statusCheckRollup": {"contexts": {
        "totalCount": total, "pageInfo": {"hasNextPage": next, "endCursor": "c"}, "nodes": nodes}}}}}})
}

#[test]
fn checks_are_paged_and_never_passing_when_incomplete() {
    // 211 checks over two pages, the only failure on the last: failing, and it is kept.
    let fx = Fx::new("checks211");
    fx.open_pr("T", "", "dana/x");
    fx.json("checks", check_page(&["SUCCESS"; 100], 211, true));
    let mut last = vec!["SUCCESS"; 110];
    last.push("FAILURE");
    fx.json("checks2", check_page(&last, 211, false));
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!((v["checks"]["state"].as_str(), v["checks_complete"].as_bool(), v["checks_total"].as_u64()), (Some("failing"), Some(true), Some(211)));
    let items = v["checks"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 200);
    assert!(items.iter().any(|i| i["state"] == "fail"));
    // More pages than it reads: all green so far is still only pending, and it says so.
    let fx = Fx::new("checksmany");
    fx.open_pr("T", "", "dana/x");
    fx.json("checks", check_page(&["SUCCESS"; 100], 5000, true));
    fx.json("checks2", check_page(&["SUCCESS"; 100], 5000, true));
    let r = snapshot(&fx, &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!((v["checks"]["state"].as_str(), v["checks_complete"].as_bool(), v["checks_total"].as_u64()), (Some("pending"), Some(false), Some(5000)));
    assert_eq!(fx.calls("gh").matches("query PrChecks").count(), 20, "bounded");
    // A failure already seen stays failing.
    let fx = Fx::new("checksfail");
    fx.open_pr("T", "", "dana/x");
    fx.json("checks", check_page(&["FAILURE"], 5000, true));
    fx.json("checks2", check_page(&["SUCCESS"; 100], 5000, true));
    assert_eq!(snapshot(&fx, &[]).json()["checks"]["state"], "failing");
}

#[test]
fn list_prints_cards_for_the_overview() {
    let fx = Fx::new("list");
    fx.json("list", json!({"data": {"repository": {"pullRequests": {"totalCount": 3, "pageInfo": {"hasNextPage": true, "endCursor": "c2"},
        "nodes": [{"number": 815}, {"number": 812}]}}}}));
    fx.json("batch", json!({"data": {"repository": {
        "p0": {"number": 815, "state": "OPEN", "title": "Bump `x` $(y)", "url": "https://github.com/acme/api/pull/815", "createdAt": "2026-10-18T09:30:00Z",
               "isDraft": true, "additions": 4, "deletions": 4, "changedFiles": 2, "reviewDecision": null, "headRefOid": HEAD, "baseRefName": "master",
               "author": {"login": "dependabot"}, "labels": {"nodes": [{"name": "deps"}]},
               "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}},
        "p1": {"number": 812, "state": "OPEN", "title": "Fix login", "url": "https://github.com/acme/api/pull/812", "createdAt": "2026-10-15T09:00:00Z",
               "isDraft": false, "additions": 120, "deletions": 14, "changedFiles": 6, "reviewDecision": "REVIEW_REQUIRED", "headRefOid": BASE,
               "baseRefName": "master", "author": {"login": "dana"}, "labels": {"nodes": []}, "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}}}}}));
    let r = fx.run(&["pr", "list", "--repo", "acme/api", "--limit", "2"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json(), json!({"repo": "acme/api", "total": 3, "truncated": true, "prs": [
        {"number": 815, "title": "Bump `x` $(y)", "author": "dependabot", "url": "https://github.com/acme/api/pull/815", "created_at": "2026-10-18T09:30:00Z",
         "draft": true, "additions": 4, "deletions": 4, "files": 2, "checks": "failing", "review": "", "labels": ["deps"], "head_sha": HEAD, "base": "master"},
        {"number": 812, "title": "Fix login", "author": "dana", "url": "https://github.com/acme/api/pull/812", "created_at": "2026-10-15T09:00:00Z",
         "draft": false, "additions": 120, "deletions": 14, "files": 6, "checks": "none", "review": "REVIEW_REQUIRED", "labels": [], "head_sha": BASE,
         "base": "master"}]}));
    let gh = fx.calls("gh");
    assert_eq!(fx.count("gh"), 2, "{gh}");
    assert!(gh.contains("first=2\x1f") && gh.contains("n0=815\x1f") && gh.contains("n1=812\x1f"), "{gh}");
}

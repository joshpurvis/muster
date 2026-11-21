//! Tests for src/prs.rs: the four rules against real `gh pr view` output (gh 2.102.1, names and
//! links replaced: src/prs_fixtures/), the check, posting again, and merges through a fake `gh`.

use super::*;
use crate::agents::Agent;
use crate::pr::merge_args;
use crate::relay::DeliveryStatus;
use crate::agents::Origin;
use std::path::PathBuf;
use std::time::Duration;

const CHECKS_PASSED: &str = include_str!("prs_fixtures/open_checks_passed.json");
const STATUS_FAILED: &str = include_str!("prs_fixtures/open_status_failed.json");
const CHECK_RUNNING: &str = include_str!("prs_fixtures/open_check_running.json");
const DRAFT: &str = include_str!("prs_fixtures/draft.json");
const DRAFT_CONFLICTING: &str = include_str!("prs_fixtures/draft_conflicting.json");
const CLOSED: &str = include_str!("prs_fixtures/closed.json");
const MERGED: &str = include_str!("prs_fixtures/merged.json");
const NOT_FOUND: &str = include_str!("prs_fixtures/not_found.stderr");
/// `Gh::queue`'s question for Muster's own #20 (gh 2.102.1): no merge queue.
const QUEUE_NONE: &str = include_str!("prs_fixtures/queue_none.json");
/// The same reply for a pull request whose base merges through a queue (no repo at hand has one,
/// so this one is the captured reply with the value flipped).
const QUEUE_ON: &str = r#"{"data":{"repository":{"pullRequest":{"isMergeQueueEnabled":true,"isInMergeQueue":false}}}}"#;

/// The head commit of `open_checks_passed.json`: the card's posted commit in the merge tests.
const HEAD: &str = "48dfa61af872fdf6d21c1e80ced33fdd50948ace";
const OTHER: &str = "1111111111111111111111111111111111111111";
const MERGE_COMMIT: &str = "abcdef0123456789abcdef0123456789abcdef01";

fn read(fixture: &str) -> Read {
    read_view(&serde_json::from_str(fixture).unwrap(), 1)
}

fn head_of(fixture: &str) -> String {
    serde_json::from_str::<Value>(fixture).unwrap()["headRefOid"].as_str().unwrap().to_string()
}

#[test]
fn the_four_rules_from_real_gh_output() {
    // Every check passed or was skipped: Merge on.
    let r = read(CHECKS_PASSED);
    assert_eq!((r.state.as_str(), r.base.as_str(), r.head_sha.as_str()), ("OPEN", "trunk", HEAD));
    assert_eq!((r.checks.total, r.checks.passed, r.checks.failed, r.checks.running), (3, 3, 0, 0));
    let v = verdict(&r, HEAD);
    assert!(v.ok, "{v:?}");
    assert_eq!((v.state, v.commit, v.base, v.checks), (Mark::Pass, Mark::Pass, Mark::Pass, Mark::Pass));
    assert!(r.url.starts_with("https://github.com/"), "{}", r.url);
    // The same read for a commit the agent didn't post: new commits.
    let v = verdict(&r, OTHER);
    assert_eq!((v.ok, v.commit, v.reason.as_deref()), (false, Mark::Warn, Some("it has new commits since it was posted")));

    // A StatusContext FAILURE among passing ones.
    let r = read(STATUS_FAILED);
    assert_eq!((r.checks.total, r.checks.passed, r.checks.failed), (3, 2, 1));
    assert_eq!(r.checks.failing, vec!["license/agreement".to_string()]);
    let v = verdict(&r, &head_of(STATUS_FAILED));
    assert_eq!((v.ok, v.checks, v.reason.as_deref()), (false, Mark::Fail, Some("1 of 3 checks failed")));

    // A CheckRun still queued: gh gives it an empty conclusion.
    let r = read(CHECK_RUNNING);
    assert_eq!((r.checks.total, r.checks.running), (1, 1));
    let v = verdict(&r, &head_of(CHECK_RUNNING));
    assert_eq!((v.ok, v.checks, v.reason.as_deref()), (false, Mark::Wait, Some("a check is still running")));

    // A draft, and a draft that also conflicts (ACTION_REQUIRED counts as failed): the state first.
    let v = verdict(&read(DRAFT), &head_of(DRAFT));
    assert_eq!((v.ok, v.state, v.reason.as_deref()), (false, Mark::Fail, Some("it is still a draft")));
    let r = read(DRAFT_CONFLICTING);
    let v = verdict(&r, &head_of(DRAFT_CONFLICTING));
    assert_eq!((v.state, v.base, v.checks), (Mark::Fail, Mark::Fail, Mark::Fail));
    assert_eq!(v.reason.as_deref(), Some("it is still a draft"));
    // The same, not a draft: the conflict is the reason, named with the base branch.
    let mut undrafted = r.clone();
    undrafted.draft = false;
    assert_eq!(verdict(&undrafted, &head_of(DRAFT_CONFLICTING)).reason.as_deref(), Some("it conflicts with main"));

    // Closed and merged.
    let v = verdict(&read(CLOSED), &head_of(CLOSED));
    assert_eq!((v.ok, v.reason.as_deref()), (false, Some("it is closed on GitHub")));
    let r = read(MERGED);
    assert_eq!(r.merge_commit.as_deref(), Some("15163c9f7575cf6d62224bc1f814aafb7cad8b48"));
    assert_eq!((r.mergeable.as_str(), r.checks.total), ("UNKNOWN", 0));
    let v = verdict(&r, &head_of(MERGED));
    assert_eq!(v.reason.as_deref(), Some("it is merged on GitHub already"));
    // No checks at all is fine ("GitHub runs no checks here"), and UNKNOWN doesn't refuse on its own.
    let mut open = r.clone();
    open.state = "OPEN".into();
    let v = verdict(&open, &head_of(MERGED));
    assert!(v.ok, "{v:?}");
    assert_eq!((v.base, v.checks), (Mark::Wait, Mark::None));
}

#[test]
fn a_check_muster_cant_read_as_passed_counts_as_failed() {
    let one = |c: Value| read_view(&json!({"state": "OPEN", "headRefOid": HEAD, "mergeable": "MERGEABLE", "statusCheckRollup": [c]}), 1);
    for c in [json!({"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "CANCELLED", "name": "a"}),
        json!({"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "", "name": "a"}),
        json!({"__typename": "StatusContext", "state": "ERROR", "context": "a"}),
        json!({"__typename": "SomethingNew", "name": "a"}), json!({})]
    {
        assert_eq!(verdict(&one(c.clone()), HEAD).checks, Mark::Fail, "{c}");
    }
    for c in [json!({"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""}),
        json!({"__typename": "StatusContext", "state": "PENDING"}), json!({"__typename": "StatusContext", "state": "EXPECTED"})]
    {
        assert_eq!(verdict(&one(c.clone()), HEAD).checks, Mark::Wait, "{c}");
    }
    // GitHub's text is cleaned, and a head that isn't a commit id never matches.
    let r = read_view(&json!({"state": "OPEN", "headRefOid": "HEAD; rm -rf /", "title": "a\u{1b}[31mred\nline",
        "url": "javascript:alert(1)", "mergeable": "MERGEABLE"}), 1);
    assert_eq!(r.head_sha, "");
    assert_eq!(r.url, "");
    assert!(!r.title.contains('\u{1b}'), "{:?}", r.title);
    assert_eq!(verdict(&r, HEAD).commit, Mark::Warn);
}

// ----- the hub -----

/// Sessions 1 shell, 2 claude, 3 codex, 4 claude started by 2, all in channel 1.
fn fixture() -> Arc<Hub> {
    fixture_on(crate::store::Store::open_memory().unwrap())
}

fn fixture_on(store: crate::store::Store) -> Arc<Hub> {
    if store.load_sessions().map(|s| s.is_empty()).unwrap_or(true) {
        let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
        let mut ch = crate::mcp::guard::test_channel(0);
        ch.workspace_id = ws;
        store.insert_channel(&ch).unwrap();
        for (kind, parent) in [(SessionKind::Shell, None), (SessionKind::Claude, None), (SessionKind::Codex, None),
            (SessionKind::Claude, Some(2))]
        {
            let mut info = crate::mcp::guard::test_session(0, parent);
            info.kind = kind;
            store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
        }
    }
    let cfg = crate::hub::HubConfig {
        port: 0,
        client_token: "t".into(),
        muster_bin: PathBuf::from("/bin/true"),
        terminal_host: None,
        openrouter_key: None,
    };
    Hub::new(cfg, store).unwrap()
}

fn agent(hub: &Hub, name: &str, kind: SessionKind, sid: SessionId) -> Agent {
    let a = agents::create(hub, agents::NewAgent {
        working_location: None, account_profile: None, provider: None, name: name.into(), kind: Some(kind),
        model: None, effort: None, persona: None, accepts: None, permissions: None, autostart: false, sleep_after_min: None,
    })
    .unwrap();
    hub.agents().link_session(hub.store(), a.id, Some(sid)).unwrap();
    hub.agents().by_id(a.id).unwrap()
}

fn temp_dir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("muster-prs-{what}-{}-{:x}", std::process::id(), rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn good() -> Value {
    json!({
        "repo": "acme/notes-app",
        "number": 15068,
        "head_sha": HEAD,
        "summary": "The sign-up card now shows on every error page, not only the first one.",
        "who_notices": "Anyone who lands on an error page.",
        "tests": "Ran the unit tests (212 passed) and opened each error page in the mock.",
        "undo": "Revert the merge commit.",
        "after_merge": "I pull and rebuild; then reload the window.",
    })
}

fn with(f: impl FnOnce(&mut Value)) -> Value {
    let mut v = good();
    f(&mut v);
    v
}

fn checked(v: Value) -> Result<Checked, Vec<String>> {
    check(serde_json::from_value(v).unwrap())
}

#[test]
fn the_check_lists_every_problem_at_once() {
    assert!(checked(good()).is_ok());
    // Trimmed and lowercased where GitHub doesn't care.
    let c = checked(with(|v| v["head_sha"] = json!(format!(" {} ", HEAD.to_uppercase())))).unwrap();
    assert_eq!(c.head_sha, HEAD);
    for (key, needle) in [("summary", "summary is missing"), ("who_notices", "who_notices is missing"),
        ("tests", "tests is missing"), ("undo", "undo is missing"), ("after_merge", "after_merge is missing")]
    {
        let e = checked(with(|v| { v.as_object_mut().unwrap().remove(key); })).unwrap_err();
        assert!(e.iter().any(|p| p.contains(needle)), "{key}: {e:?}");
    }
    for (bad, needle) in [(json!({"repo": "notes-app"}), "repo: OWNER/NAME"), (json!({"repo": "acme/notes app"}), "repo: OWNER/NAME"),
        (json!({"repo": "acme/notes-app/x"}), "repo: OWNER/NAME"), (json!({"number": 0}), "number:"),
        (json!({"head_sha": "48dfa61"}), "head_sha:"), (json!({"head_sha": "HEAD"}), "head_sha:"),
        (json!({"summary": "short"}), "summary is too short")]
    {
        let e = checked(with(|v| for (k, x) in bad.as_object().unwrap() { v[k] = x.clone(); })).unwrap_err();
        assert!(e.iter().any(|p| p.contains(needle)), "{bad}: {e:?}");
    }
    // Every problem at once.
    let e = checked(json!({"repo": "x", "number": 0, "head_sha": "y"})).unwrap_err();
    assert!(e.len() >= 8, "{e:?}");
    // Unknown fields are refused, not dropped.
    assert!(serde_json::from_value::<NewPr>(with(|v| v["merge_method"] = json!("squash"))).is_err());
}

#[test]
fn pictures_are_images_at_absolute_paths_and_at_most_six() {
    let dir = temp_dir("pics");
    let png = dir.join("before.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    let pic = |path: &str, side: &str| json!({"path": path, "caption": "The error page", "side": side});
    let p = png.to_str().unwrap();
    assert_eq!(checked(with(|v| v["pictures"] = json!([pic(p, "before"), pic(p, "after")]))).unwrap().pictures.len(), 2);
    let text = dir.join("notes.png");
    std::fs::write(&text, b"secret=hunter2").unwrap();
    let link = dir.join("link.png");
    std::os::unix::fs::symlink(&png, &link).unwrap();
    for (path, needle) in [("before.png", "absolute path"), (text.to_str().unwrap(), "picture 1:"),
        (link.to_str().unwrap(), "not a symlink"), ("/etc/passwd", "png, jpg, gif, webp or svg only"),
        (dir.join("gone.png").to_str().unwrap(), "picture 1:")]
    {
        let e = checked(with(|v| v["pictures"] = json!([pic(path, "after")]))).unwrap_err();
        assert!(e.iter().any(|x| x.contains(needle)), "{path}: {e:?}");
    }
    let e = checked(with(|v| v["pictures"] = json!(vec![pic(p, "after"); 7]))).unwrap_err();
    assert!(e.iter().any(|x| x.contains("at most 6 pictures")), "{e:?}");
    let e = checked(with(|v| v["pictures"] = json!([{"path": p, "caption": "", "side": "after"}]))).unwrap_err();
    assert!(e.iter().any(|x| x.contains("caption is missing")), "{e:?}");
    assert!(serde_json::from_value::<NewPr>(with(|v| v["pictures"] = json!([{"path": p, "caption": "x", "side": "during"}]))).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn posting_again_keeps_the_card_and_only_its_poster_can() {
    let hub = fixture();
    let builder = agent(&hub, "builder-2", SessionKind::Codex, 3);
    let dir = temp_dir("repost");
    let png = dir.join("after.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    let out = post(&hub, 3, with(|v| v["pictures"] = json!([{"path": png.to_str().unwrap(), "caption": "After", "side": "after"}]))).unwrap();
    assert_eq!((out["card"].as_str(), out["replaced"].as_bool()), (Some("P1"), Some(false)));
    assert!(out["result_goes_to"].as_str().unwrap().contains("builder-2, by name"), "{out}");
    // Muster's own copy: the agent's file changing later doesn't change the card.
    std::fs::write(&png, b"secret=hunter2").unwrap();
    let pic = picture(&hub, 1, 1, 0).unwrap();
    assert!(pic["src"].as_str().unwrap().starts_with("data:image/png;base64,"), "{pic}");
    assert!(hub.store().pr_dir(1).join("1/0.png").is_file());
    // A shell can't post; another session can't post over it; GitHub's names ignore case.
    assert!(post(&hub, 1, good()).unwrap_err().to_string().contains("only agent sessions"));
    let e = post(&hub, 2, with(|v| v["repo"] = json!("ACME/Notes-App"))).unwrap_err().to_string();
    assert!(e.contains("builder-2 posted"), "{e}");
    // The same agent, from a fresh session, posts it again for a new commit: the same card, post
    // 2, without pictures; the first post's pictures go.
    hub.agents().link_session(hub.store(), builder.id, Some(4)).unwrap();
    hub.store().update_pr(1, |p| { p.later = Some(5); p.stale_told = true; Ok(()) }).unwrap();
    let out = post(&hub, 4, with(|v| v["head_sha"] = json!(OTHER))).unwrap();
    assert_eq!((out["card"].as_str(), out["replaced"].as_bool()), (Some("P1"), Some(true)));
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!((pr.post, pr.head_sha.as_str(), pr.later, pr.stale_told, pr.pictures.len()), (2, OTHER, None, false, 0));
    assert!(!hub.store().pr_dir(1).join("1").exists());
    // While it is being merged, nobody posts over it.
    hub.store().update_pr(1, |p| { p.status = Status::Merging; Ok(()) }).unwrap();
    assert!(post(&hub, 4, good()).unwrap_err().to_string().contains("merging"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(hub.store().prs_root());
}

#[test]
fn withdrawing_is_the_posters_and_list_shows_open_then_done() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    post(&hub, 3, with(|v| v["number"] = json!(2))).unwrap();
    assert!(withdraw(&hub, 2, "acme/notes-app", 15068, "not mine to take").unwrap_err().to_string().contains("not you"));
    assert!(withdraw(&hub, 3, "acme/notes-app", 15068, "x").unwrap_err().to_string().contains("reason"));
    let w = withdraw(&hub, 3, "Acme/Notes-App", 15068, "it will be posted again later").unwrap();
    assert_eq!(w["status"], "withdrawn");
    assert!(withdraw(&hub, 3, "acme/notes-app", 15068, "again, twice").is_err());
    // The list holds open cards and finished ones, not withdrawn ones.
    let list = list(&hub).unwrap();
    let cards: Vec<&str> = list["prs"].as_array().unwrap().iter().map(|p| p["card"].as_str().unwrap()).collect();
    assert_eq!(cards, vec!["P2"]);
    // The window never sees the words sent to the poster (their delivery, yes).
    hub.store().update_pr(2, |p| { push(p, NoteKind::Stale, "muster", "secret words".into()); Ok(()) }).unwrap();
    let v = get(&hub, 2).unwrap();
    assert!(!v.to_string().contains("secret words"), "{v}");
    assert_eq!(v["outbox"][0]["delivery"]["status"], "pending");
}

// ----- a fake gh -----

/// A `gh` that answers `api` (the merge queue question) with `queue.json` (default: none, the
/// captured reply) or fails with `queue.err`, answers `pr view` with `view.json` (or fails with `view.err`, exit 1) and logs
/// every call's arguments to `calls`. `pr merge` sleeps `merge.sleep` seconds, prints `merge.err`
/// and exits with `merge.exit` (default 0); on exit 0 it swaps `merged.json` in as the view, and
/// `later.err` in as `view.err` (the reads after it fail).
struct FakeGh {
    dir: PathBuf,
    gh: Arc<Gh>,
}

impl FakeGh {
    fn new(view: &str) -> FakeGh {
        let dir = temp_dir("gh");
        std::fs::write(dir.join("view.json"), view).unwrap();
        std::fs::write(dir.join("queue_none.json"), QUEUE_NONE).unwrap();
        let d = dir.display();
        let script = format!(r#"#!/bin/sh
echo "$@" >> '{d}/calls'
if [ "$1" = api ]; then
  if [ -f '{d}/queue.err' ]; then cat '{d}/queue.err' >&2; exit 1; fi
  if [ -f '{d}/queue.json' ]; then cat '{d}/queue.json'; else cat '{d}/queue_none.json'; fi
  exit 0
fi
case "$2" in
view)
  if [ -f '{d}/view.err' ]; then cat '{d}/view.err' >&2; exit 1; fi
  cat '{d}/view.json';;
merge)
  [ -f '{d}/merge.sleep' ] && sleep "$(cat '{d}/merge.sleep')"
  code=0; [ -f '{d}/merge.exit' ] && code="$(cat '{d}/merge.exit')"
  [ -f '{d}/merge.err' ] && cat '{d}/merge.err' >&2
  if [ "$code" = 0 ]; then
    [ -f '{d}/merged.json' ] && cp '{d}/merged.json' '{d}/view.json'
    [ -f '{d}/later.err' ] && cp '{d}/later.err' '{d}/view.err'
  fi
  exit "$code";;
esac
"#);
        let path = dir.join("gh");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let gh = Arc::new(Gh::new(path.into_os_string(), dir.clone()));
        FakeGh { dir, gh }
    }

    fn set(&self, file: &str, body: &str) {
        std::fs::write(self.dir.join(file), body).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn merges(&self) -> Vec<String> {
        self.calls().into_iter().filter(|c| c.starts_with("pr merge")).collect()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `open_checks_passed.json` after a merge of its head as `MERGE_COMMIT`.
fn merged_view() -> String {
    view_with(|v| {
        v["state"] = json!("MERGED");
        v["mergeable"] = json!("UNKNOWN");
        v["mergeCommit"] = json!({"oid": MERGE_COMMIT});
    })
}

fn view_with(f: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(CHECKS_PASSED).unwrap();
    f(&mut v);
    v.to_string()
}

/// Card `id`'s last GitHub read as if it were a minute old (Check again reads at most every 3 s).
fn age(hub: &Hub, id: i64) {
    hub.store().update_pr(id, |p| {
        if let Some(r) = p.github.as_mut() { r.at -= 60_000; }
        if let Some(e) = p.read_error.as_mut() { e.at -= 60_000; }
        Ok(())
    }).unwrap();
}

/// Click Merge on card `id` as the window would (the commit, base and post it shows), and wait
/// for the merge to end. Err: refused at the click.
async fn click(hub: &Arc<Hub>, gh: &FakeGh, id: i64, sha: &str) -> anyhow::Result<Value> {
    let p = hub.store().pr(id)?.unwrap();
    let base = p.github.as_ref().map_or("trunk".to_string(), |r| r.base.clone());
    let v = merge(hub, gh.gh.clone(), id, sha, &base, p.post)?;
    assert_eq!(v["status"], "merging", "{v}");
    for _ in 0..1000 {
        let p = hub.store().pr(id)?.unwrap();
        if p.status != Status::Merging {
            // The poster's message is on its way once the card says so.
            tokio::time::sleep(Duration::from_millis(20)).await;
            return get(hub, id);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the merge never ended");
}

/// `pr.refresh` of every card, waiting for its reads (they go on after it answers).
async fn refresh_all(hub: &Arc<Hub>, gh: &FakeGh) {
    refresh(hub, gh.gh.clone(), None, false).await.unwrap();
    let key = Arc::as_ptr(hub) as usize;
    for _ in 0..500 {
        if !reading().lock().unwrap().iter().any(|(k, _)| *k == key) { return }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the reads never ended");
}

/// What the window shows before a click: a first read of card `id`.
async fn first_read(hub: &Arc<Hub>, gh: &FakeGh, id: i64) {
    refresh(hub, gh.gh.clone(), Some(id), true).await.unwrap();
}

fn notes(hub: &Hub, id: i64, kind: NoteKind) -> Vec<Note> {
    hub.store().pr(id).unwrap().unwrap().outbox.into_iter().filter(|n| n.kind == kind).collect()
}

#[tokio::test]
async fn a_click_merges_exactly_the_posted_commit_and_tells_the_poster() {
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    gh.set("merged.json", &merged_view());
    // Not read from GitHub yet: no base to show, so nothing to click.
    assert!(merge(&hub, gh.gh.clone(), 1, HEAD, "trunk", 1).unwrap_err().to_string().contains("changed since you looked"));
    first_read(&hub, &gh, 1).await;
    // The window saw another commit, another post or another base: refused before anything runs.
    let before = gh.calls().len();
    for (sha, base, post) in [(OTHER, "trunk", 1), (HEAD, "main", 1), (HEAD, "trunk", 2)] {
        let e = merge(&hub, gh.gh.clone(), 1, sha, base, post).unwrap_err().to_string();
        assert!(e.contains("changed since you looked"), "{e}");
    }
    assert_eq!(gh.calls().len(), before);
    let v = click(&hub, &gh, 1, &HEAD.to_uppercase()).await.unwrap();
    assert_eq!(v["status"], "merged", "{v}");
    assert_eq!(v["merge"]["commit"], MERGE_COMMIT);
    assert!(v["merge"]["sent_at"].as_i64().is_some());
    // Exactly one write, with exactly these flags: a merge commit, this commit.
    assert_eq!(gh.merges(), vec![format!("pr merge 15068 --repo=acme/notes-app --merge --match-head-commit={HEAD}")]);
    // The merge queue question first (with variables, never in the query), then the write.
    let calls = gh.calls();
    let queue = calls.iter().position(|c| c.starts_with("api graphql -f query=") && c.ends_with("-f owner=acme -f name=notes-app -F number=15068"))
        .expect("queue question");
    let write = calls.iter().position(|c| c.starts_with("pr merge")).unwrap();
    assert!(queue < write);
    assert_eq!(calls.iter().filter(|c| c.starts_with("pr view 15068 --repo=acme/notes-app --json=")).count(), 3);
    let n = notes(&hub, 1, NoteKind::Merged);
    assert_eq!(n.len(), 1);
    assert!(n[0].text.starts_with("Merge result for acme/notes-app #15068: merged."), "{}", n[0].text);
    for needle in [HEAD, MERGE_COMMIT, "into trunk", "I pull and rebuild; then reload the window.", "Card P1",
        "(Muster's note 1 on card P1.)", "Muster didn't delete the branch"]
    {
        assert!(n[0].text.contains(needle), "{needle} missing from {}", n[0].text);
    }
    // Sent by name (this fixture has no terminal host, so it ends undelivered there).
    assert_eq!(n[0].delivery.to.as_deref(), Some("builder-2"));
    assert!(!n[0].delivery.unfinished(), "{:?}", n[0].delivery);
    // A second click: merged already, no second write.
    assert!(merge(&hub, gh.gh.clone(), 1, HEAD, "trunk", 1).unwrap_err().to_string().contains("merged already"));
    assert_eq!(gh.merges().len(), 1);
}

#[test]
fn the_merge_command_has_no_other_flags() {
    let args = merge_args("acme/notes-app", 15068, HEAD);
    assert_eq!(args.join(" "), format!("pr merge 15068 --repo=acme/notes-app --merge --match-head-commit={HEAD}"));
    for flag in ["--admin", "--auto", "--delete-branch", "-d", "--squash", "-s", "--rebase", "-r"] {
        assert!(!args.iter().any(|a| a == flag || a.starts_with(&format!("{flag}="))), "{flag}");
    }
    // Nothing but a checked repo, number and commit reaches the command line.
    let gh = Gh::new("/nonexistent/gh", std::env::temp_dir());
    assert!(gh.merge("acme/notes-app --admin", 1, HEAD).is_err());
    assert!(gh.merge("acme/notes-app", 1, "--admin").is_err());
    assert!(gh.merge("acme/notes-app", 0, HEAD).is_err());
    assert!(matches!(gh.merge("acme/notes-app", 1, HEAD), Err((GhError::Missing, false))));
    assert!(gh.queue("acme/notes-app --x", 1).is_err());
}

#[tokio::test]
async fn a_failing_check_new_commits_or_a_conflict_refuse_without_a_write() {
    for (view, reason) in [
        (view_with(|v| v["statusCheckRollup"][1]["conclusion"] = json!("FAILURE")), "1 of 3 checks failed"),
        (view_with(|v| v["headRefOid"] = json!(OTHER)), "it has new commits since it was posted"),
        (view_with(|v| v["mergeable"] = json!("CONFLICTING")), "it conflicts with trunk"),
        (view_with(|v| v["isDraft"] = json!(true)), "it is still a draft"),
        (view_with(|v| v["statusCheckRollup"][1]["status"] = json!("IN_PROGRESS")), "a check is still running"),
        (view_with(|v| v["autoMergeRequest"] = json!({"enabledAt": "2026-11-18T04:00:00Z", "mergeMethod": "SQUASH"})),
            "auto-merge is on for it on GitHub, so GitHub would merge it by itself"),
        (view_with(|v| v["baseRefName"] = json!("release")), "its base branch changed to release since you looked (you saw trunk)"),
        (view_with(|v| v["url"] = json!("https://github.com/acme/notes-app-2/pull/15068")),
            "GitHub knows it by another name now (https://github.com/acme/notes-app-2/pull/15068); post the card again with that name"),
        (view_with(|v| { v.as_object_mut().unwrap().remove("isDraft"); }), "GitHub's reply lacked something Muster checks; Check again"),
    ] {
        let hub = fixture();
        post(&hub, 3, good()).unwrap();
        let gh = FakeGh::new(CHECKS_PASSED);
        first_read(&hub, &gh, 1).await;
        gh.set("view.json", &view);
        let v = click(&hub, &gh, 1, HEAD).await.unwrap();
        assert!(gh.merges().is_empty(), "{reason}: {:?}", gh.calls());
        assert_eq!(v["refused"]["reason"], reason, "{v}");
        assert!(v["refused"].get("unknown").is_none(), "{v}");
        let pr = hub.store().pr(1).unwrap().unwrap();
        let n = notes(&hub, 1, NoteKind::Refused);
        assert!(n[0].text.contains(&format!("not merged: {reason}")), "{}", n[0].text);
        assert!(n[0].text.contains("Don't merge it yourself"), "{}", n[0].text);
        if reason.contains("new commits") {
            // Stale, and that message is the poster's only one about it.
            assert_eq!((pr.status, pr.stale_told), (Status::Stale, true));
        } else {
            assert_eq!(pr.status, Status::Waiting);
        }
        // Once fixed, a read that passes clears the refusal: Merge is a click away again.
        if reason.contains("checks failed") {
            gh.set("view.json", CHECKS_PASSED);
            // Check again within 3 s of the last read doesn't read again.
            refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
            assert!(hub.store().pr(1).unwrap().unwrap().refused.is_some());
            age(&hub, 1);
            refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
            let pr = hub.store().pr(1).unwrap().unwrap();
            assert!(pr.refused.is_none() && pr.verdict.unwrap().ok);
        }
    }
}

#[tokio::test]
async fn a_merge_queue_or_an_unanswered_queue_question_refuse_before_the_write() {
    for (queue, err, reason) in [
        (Some(QUEUE_ON), None, "it merges through a merge queue on GitHub, which Muster doesn't use: merge it there"),
        (Some(r#"{"data":{"repository":{"pullRequest":{"isMergeQueueEnabled":false}}}}"#), None,
            "Muster couldn't check on GitHub whether it merges through a merge queue"),
        (None, Some("HTTP 403: Resource not accessible by integration"), "Muster couldn't check on GitHub whether it merges through a merge queue"),
    ] {
        let hub = fixture();
        post(&hub, 3, good()).unwrap();
        let gh = FakeGh::new(CHECKS_PASSED);
        if let Some(r) = queue { gh.set("queue.json", r) }
        if let Some(e) = err { gh.set("queue.err", e) }
        first_read(&hub, &gh, 1).await;
        let v = click(&hub, &gh, 1, HEAD).await.unwrap();
        assert!(gh.merges().is_empty(), "{:?}", gh.calls());
        assert_eq!(v["refused"]["reason"], reason, "{v}");
    }
    // The captured reply (no queue): it merges.
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    gh.set("merged.json", &merged_view());
    first_read(&hub, &gh, 1).await;
    assert_eq!(click(&hub, &gh, 1, HEAD).await.unwrap()["status"], "merged");
}

#[tokio::test]
async fn githubs_refusal_a_timeout_and_a_merge_left_open() {
    // gh exits 1 (branch protection): a refusal; GitHub's words whole for the human, and to the
    // poster without gh's suggestion of other flags.
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &gh, 1).await;
    gh.set("merge.exit", "1");
    gh.set("merge.err", "X Pull request acme/notes-app#15068 is not mergeable: the base branch policy prohibits the merge.\n\
        To have the pull request merged after all the requirements have been met, add the `--auto` flag.\n\
        To use administrator privileges to immediately merge the pull request, add the `--admin` flag.");
    let v = click(&hub, &gh, 1, HEAD).await.unwrap();
    assert_eq!(v["status"], "waiting");
    assert_eq!(v["refused"]["reason"], "GitHub refused the merge");
    assert!(v["refused"].get("unknown").is_none(), "{v}");
    assert!(v["refused"]["github_said"].as_str().unwrap().contains("--admin"), "the human sees it whole: {v}");
    let n = &notes(&hub, 1, NoteKind::Refused)[0].text;
    assert!(n.contains("the base branch policy prohibits the merge") && !n.contains("--admin") && !n.contains("--auto"), "{n}");
    assert_eq!(gh.merges().len(), 1);

    // gh never answers: killed after its timeout; the card isn't left merging, and the result is
    // unknown (it may have merged).
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let mut slow = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &slow, 1).await;
    slow.set("merge.sleep", "5");
    let mut g = (*slow.gh).clone();
    g.merge_timeout = Duration::from_millis(300);
    slow.gh = Arc::new(g);
    let started = std::time::Instant::now();
    let v = click(&hub, &slow, 1, HEAD).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(9), "{:?}", started.elapsed());
    assert_eq!((v["status"].as_str(), v["refused"]["unknown"].as_bool()), (Some("waiting"), Some(true)), "{v}");
    assert!(v["refused"]["github_said"].as_str().unwrap().contains("took longer"), "{v}");
    let n = &notes(&hub, 1, NoteKind::Refused)[0].text;
    assert!(n.contains("not known yet") && n.contains("Don't push to its branch"), "{n}");

    // gh exits 0 and GitHub still shows it open (a queue, auto-merge): unknown, never "nothing
    // was merged".
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &gh, 1).await;
    let v = click(&hub, &gh, 1, HEAD).await.unwrap();
    assert_eq!(v["refused"]["unknown"], true, "{v}");
    assert!(v["refused"]["reason"].as_str().unwrap().contains("hasn't merged it"), "{v}");
    assert!(!notes(&hub, 1, NoteKind::Refused)[0].text.contains("Nothing was merged"));
}

#[tokio::test]
async fn a_merge_whose_result_couldnt_be_read_is_found_by_the_next_read() {
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &gh, 1).await;
    gh.set("merged.json", &merged_view());
    gh.set("later.err", "HTTP 502: Bad Gateway (https://api.github.com/graphql)");
    let v = click(&hub, &gh, 1, HEAD).await.unwrap();
    assert_eq!((v["status"].as_str(), v["refused"]["unknown"].as_bool()), (Some("waiting"), Some(true)), "{v}");
    assert!(v["refused"]["reason"].as_str().unwrap().contains("Check again shows whether it merged"), "{v}");
    // GitHub answers again: merged by this card's click.
    std::fs::remove_file(gh.dir.join("view.err")).unwrap();
    age(&hub, 1);
    let v = refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert_eq!(v["status"], "merged", "{v}");
    assert_eq!(v["merge"]["commit"], MERGE_COMMIT);
    assert!(v.get("refused").is_none(), "{v}");
    assert_eq!(notes(&hub, 1, NoteKind::Merged).len(), 1);
    assert_eq!(gh.merges().len(), 1);
}

#[tokio::test]
async fn a_refused_click_never_takes_credit_for_a_merge_done_elsewhere() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &gh, 1).await;
    // Refused before any write (a check still running).
    gh.set("view.json", &view_with(|v| v["statusCheckRollup"][1]["status"] = json!("IN_PROGRESS")));
    assert_eq!(click(&hub, &gh, 1, HEAD).await.unwrap()["refused"]["reason"], "a check is still running");
    // Then merged on GitHub's website: closed, not "merged by your click", and no Merged note.
    gh.set("view.json", &merged_view());
    age(&hub, 1);
    let v = refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert_eq!((v["status"].as_str(), v["closed"]["reason"].as_str()), (Some("closed"), Some("merged on GitHub, not from this card")));
    assert!(notes(&hub, 1, NoteKind::Merged).is_empty());
    assert!(gh.merges().is_empty());
}

#[tokio::test]
async fn a_restart_while_merging_asks_github_what_happened() {
    let merging = |hub: &Hub, sent: bool| hub.store().update_pr(1, |p| {
        p.status = Status::Merging;
        p.merge = Some(MergeTry { at: now_ms(), sha: HEAD.into(), post: 1, base: "trunk".into(), sent_at: sent.then(now_ms),
            done_at: None, commit: None });
        Ok(())
    }).unwrap();
    // The write was sent and GitHub shows it merged: merged, and the poster is told.
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    post(&hub, 3, good()).unwrap();
    merging(&hub, true);
    let u = unfinished(&hub);
    assert_eq!((u.len(), u[0].id, u[0].merging.is_some()), (1, 1, true));
    let gh = FakeGh::new(&merged_view());
    resume_one(&hub, &gh.gh, &u[0]).await.unwrap();
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!(pr.status, Status::Merged);
    assert_eq!(pr.merge.unwrap().commit.as_deref(), Some(MERGE_COMMIT));
    assert_eq!(notes(&hub, 1, NoteKind::Merged).len(), 1);
    assert!(gh.merges().is_empty(), "never merged again");
    assert!(unfinished(&hub).is_empty());

    // The restart came before the write: merged on GitHub means someone else did it.
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    merging(&hub, false);
    let gh = FakeGh::new(&merged_view());
    resume_one(&hub, &gh.gh, &unfinished(&hub)[0]).await.unwrap();
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!(pr.status, Status::Closed);
    assert!(pr.outbox.is_empty());

    // Not merged: back to waiting, saying why. Cut off before the write, the poster isn't told
    // (it never heard of the click); after it, it is told the result isn't known yet.
    for sent in [false, true] {
        let hub = fixture();
        post(&hub, 3, good()).unwrap();
        merging(&hub, sent);
        let gh = FakeGh::new(CHECKS_PASSED);
        resume_one(&hub, &gh.gh, &unfinished(&hub)[0]).await.unwrap();
        let pr = hub.store().pr(1).unwrap().unwrap();
        assert_eq!(pr.status, Status::Waiting);
        let f = pr.refused.unwrap();
        assert!(f.not_told != sent && f.unknown == sent, "{f:?}");
        assert!(f.reason.contains("Muster restarted"), "{f:?}");
        assert_eq!(pr.outbox.len(), usize::from(sent));
        assert!(gh.merges().is_empty());
    }

    // A merge this backend started after the restart is never taken for the one cut off.
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    hub.store().update_pr(1, |p| { push(p, NoteKind::Stale, "muster", "s".into()); Ok(()) }).unwrap();
    let u = unfinished(&hub);
    assert_eq!(u[0].merging, None);
    merging(&hub, true);
    let gh = FakeGh::new(CHECKS_PASSED);
    resume_one(&hub, &gh.gh, &u[0]).await.unwrap();
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Merging);
}

#[tokio::test]
async fn a_restart_resends_only_what_the_previous_backend_was_delivering() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    hub.store().update_pr(1, |p| {
        push(p, NoteKind::Stale, "muster", "first".into());
        push(p, NoteKind::Refused, "muster", "second".into());
        p.outbox[0].delivery.status = DeliveryStatus::Delivering;
        Ok(())
    }).unwrap();
    let cards = unfinished(&hub);
    assert_eq!(cards, vec![Unfinished { id: 1, delivering: vec![0], merging: None }]);
    // This backend claims note 2 before the restart's work reaches the card: it isn't reset.
    hub.store().update_pr(1, |p| { p.outbox[1].delivery.status = DeliveryStatus::Delivering; Ok(()) }).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    resume_one(&hub, &gh.gh, &cards[0]).await.unwrap();
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!(pr.outbox[1].delivery.status, DeliveryStatus::Delivering);
    assert_ne!(pr.outbox[0].delivery.status, DeliveryStatus::Delivering);
    // Two notes with the same words are still told apart.
    assert_ne!(pr.outbox[0].text.lines().last(), pr.outbox[1].text.lines().last());
}

#[tokio::test]
async fn a_stale_notice_travels_with_the_posters_own_provenance() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    hub.store().update_pr(1, |p| {
        p.origin = Origin::Schedule;
        p.hop = 2;
        push(p, NoteKind::Stale, "muster", "s".into());
        push(p, NoteKind::Merged, "muster", "m".into());
        Ok(())
    }).unwrap();
    let pr = hub.store().pr(1).unwrap().unwrap();
    let stale = note_prov(&pr, &pr.outbox[0]).unwrap();
    assert_eq!((stale.origin, stale.hop), (Origin::Schedule, 2));
    assert!(note_prov(&pr, &pr.outbox[1]).is_none(), "a result of the human's click is the human's own");
}

#[test]
fn a_failed_picture_copy_stores_nothing_and_keeps_the_card_it_would_replace() {
    let hub = fixture();
    let dir = temp_dir("copyfail");
    let png = dir.join("after.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    let pics = json!([{"path": png.to_str().unwrap(), "caption": "After", "side": "after"}]);
    post(&hub, 3, with(|v| v["pictures"] = pics.clone())).unwrap();
    // Post 2's directory can't be made (a file is in its way).
    std::fs::write(hub.store().pr_dir(1).join("2"), b"x").unwrap();
    let e = post(&hub, 3, with(|v| { v["pictures"] = pics.clone(); v["head_sha"] = json!(OTHER); })).unwrap_err().to_string();
    assert!(e.contains("the card was not posted"), "{e}");
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!((pr.status, pr.post, pr.head_sha.as_str()), (Status::Waiting, 1, HEAD));
    assert!(picture(&hub, 1, 1, 0).is_ok());
    // A window still showing another post is told to look again, never sent this post's picture.
    assert!(picture(&hub, 1, 2, 0).unwrap_err().to_string().contains("posted again"));
    // Withdrawn: its pictures go.
    withdraw(&hub, 3, "acme/notes-app", 15068, "not needed any more").unwrap();
    assert!(!hub.store().pr_dir(1).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn once_its_session_ended_the_posters_lead_posts_it_again() {
    let hub = fixture();
    // Session 4 (started by session 2) posts, then ends.
    post(&hub, 4, good()).unwrap();
    // (This fixture's sessions don't run, so session 4 counts as ended; a running poster is
    // covered by the named-agent case in posting_again_keeps_the_card_and_only_its_poster_can.)
    hub.store().update_pr(1, |p| { p.poster.session_id = 99; p.poster.parent_id = Some(2); Ok(()) }).unwrap();
    // Another session can't; the lead can.
    assert!(post(&hub, 3, with(|v| v["head_sha"] = json!(OTHER))).is_err());
    let out = post(&hub, 2, with(|v| v["head_sha"] = json!(OTHER))).unwrap();
    assert_eq!((out["card"].as_str(), out["replaced"].as_bool()), (Some("P1"), Some(true)));
}

#[tokio::test]
async fn a_card_waiting_for_changes_closes_when_github_closes_it() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    ask_changes(&hub, 1, 1, "Keep the old heading.").await.unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    refresh_all(&hub, &gh).await;
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::ChangesAsked, "still open on GitHub: it waits");
    gh.set("view.json", CLOSED.replace("15089", "15068").as_str());
    age(&hub, 1);
    refresh_all(&hub, &gh).await;
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Closed);
}

#[tokio::test]
async fn a_slow_read_never_overwrites_a_newer_one() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let old = read(CHECKS_PASSED);
    let mut newer = read(&view_with(|v| v["headRefOid"] = json!(OTHER)));
    newer.at = 10;
    apply_read(&hub, 1, 1, Ok(newer)).await.unwrap();
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Stale);
    // Started earlier (at 1), arrives later: dropped.
    apply_read(&hub, 1, 1, Ok(old)).await.unwrap();
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Stale);
}

#[test]
fn direction_controls_never_reach_the_card() {
    let c = checked(with(|v| v["summary"] = json!("The tests \u{202e}deliaf\u{202c} all passed on every page."))).unwrap();
    assert!(!c.summary.contains('\u{202e}') && !c.summary.contains('\u{202c}'), "{:?}", c.summary);
}

#[tokio::test]
async fn a_write_github_refused_never_takes_credit_for_a_later_merge() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    first_read(&hub, &gh, 1).await;
    gh.set("merge.exit", "1");
    gh.set("merge.err", "X Pull request acme/notes-app#15068 is not mergeable: the base branch policy prohibits the merge.");
    assert_eq!(click(&hub, &gh, 1, HEAD).await.unwrap()["refused"]["reason"], "GitHub refused the merge");
    gh.set("view.json", &merged_view());
    age(&hub, 1);
    let v = refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert_eq!(v["status"], "closed", "{v}");
    assert!(notes(&hub, 1, NoteKind::Merged).is_empty());
}

#[tokio::test]
async fn asking_for_changes_or_not_now_is_for_the_post_the_human_saw() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    post(&hub, 3, with(|v| v["head_sha"] = json!(OTHER))).unwrap();
    assert!(ask_changes(&hub, 1, 1, "Keep the old heading.").await.unwrap_err().to_string().contains("posted again"));
    assert!(later(&hub, 1, 1, true).unwrap_err().to_string().contains("posted again"));
    assert_eq!(later(&hub, 1, 2, true).unwrap()["post"], 2);
}

#[tokio::test]
async fn a_stale_notice_never_goes_on_to_the_lead_of_an_ended_poster() {
    let hub = fixture();
    // Session 4 (started by 2) posts; this fixture's sessions don't run, so its words would go to 2.
    post(&hub, 4, good()).unwrap();
    let gh = FakeGh::new(&view_with(|v| v["headRefOid"] = json!(OTHER)));
    refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    let n = notes(&hub, 1, NoteKind::Stale);
    assert_eq!(n[0].delivery.status, DeliveryStatus::Undelivered, "{:?}", n[0].delivery);
    // Its recipient and provenance were fixed when it was written.
    assert_eq!(n[0].to.as_ref().unwrap().session_id, 4);
}

#[test]
fn a_reply_without_a_field_the_rules_need_or_with_a_disguised_base_fails() {
    let without = read_view(&serde_json::from_str::<Value>(&view_with(|v| { v.as_object_mut().unwrap().remove("autoMergeRequest"); })).unwrap(), 1);
    assert_eq!(verdict(&without, HEAD).state, Mark::Fail);
    let bidi = read_view(&serde_json::from_str::<Value>(&view_with(|v| v["baseRefName"] = json!("ma\u{202e}in"))).unwrap(), 1);
    assert!(bidi.incomplete);
    assert_eq!(verdict(&bidi, HEAD).state, Mark::Fail);
    // A plain name with other characters git allows is kept exactly.
    assert_eq!(read_view(&serde_json::from_str::<Value>(&view_with(|v| v["baseRefName"] = json!("release+2026"))).unwrap(), 1).base,
        "release+2026");
}

#[tokio::test]
async fn new_commits_make_the_card_stale_and_the_poster_is_told_once() {
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(&view_with(|v| v["headRefOid"] = json!(OTHER)));
    for _ in 0..3 {
        refresh_all(&hub, &gh).await;
        age(&hub, 1);
    }
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert_eq!(pr.status, Status::Stale);
    let n = notes(&hub, 1, NoteKind::Stale);
    assert_eq!(n.len(), 1, "told once");
    assert!(n[0].text.contains("Changed since you posted it") && n[0].text.contains("1111111"), "{}", n[0].text);
    // A stale card can't be merged.
    assert!(merge(&hub, gh.gh.clone(), 1, HEAD, "trunk", 1).unwrap_err().to_string().contains("new commits"));
    // The head goes back (a force-push undone): waiting again, no second message.
    gh.set("view.json", CHECKS_PASSED);
    refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Waiting);
    assert_eq!(notes(&hub, 1, NoteKind::Stale).len(), 1);
    // Without force, a fresh read isn't repeated.
    let before = gh.calls().len();
    refresh_all(&hub, &gh).await;
    assert_eq!(gh.calls().len(), before);
}

#[tokio::test]
async fn a_read_of_an_earlier_post_or_a_closed_pr_is_handled() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    // A read that started before the card was posted again is dropped.
    post(&hub, 3, with(|v| v["head_sha"] = json!(OTHER))).unwrap();
    apply_read(&hub, 1, 1, Ok(read(CHECKS_PASSED))).await.unwrap();
    let pr = hub.store().pr(1).unwrap().unwrap();
    assert!(pr.github.is_none() && pr.status == Status::Waiting, "{pr:?}");
    // Merged on GitHub by something else (no click on this card): closed, not merged.
    let gh = FakeGh::new(&merged_view().replace(HEAD, OTHER));
    let v = refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert_eq!(v["status"], "closed", "{v}");
    assert_eq!(v["closed"]["reason"], "merged on GitHub, not from this card");
    // A failed read is kept beside the last good one.
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let gh = FakeGh::new(CHECKS_PASSED);
    gh.set("view.err", NOT_FOUND);
    let v = refresh(&hub, gh.gh.clone(), Some(1), true).await.unwrap();
    assert!(v["read_error"]["text"].as_str().unwrap().contains("Could not resolve to a PullRequest"), "{v}");
    assert_eq!(v["status"], "waiting");
}

#[tokio::test]
async fn asking_for_changes_and_not_now() {
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    post(&hub, 3, good()).unwrap();
    let v = later(&hub, 1, 1, true).unwrap();
    assert!(v["later"].as_i64().is_some());
    assert!(later(&hub, 1, 1, false).unwrap().get("later").is_none());
    assert!(ask_changes(&hub, 1, 1, "  ").await.is_err());
    let v = ask_changes(&hub, 1, 1, "Keep the old heading on the 404 page.").await.unwrap();
    assert_eq!(v["status"], "changes_asked");
    let n = notes(&hub, 1, NoteKind::ChangesAsked);
    assert_eq!(n[0].from, "human");
    assert!(n[0].text.starts_with("Changes asked on acme/notes-app #15068")
        && n[0].text.contains("\"Keep the old heading on the 404 page.\""), "{}", n[0].text);
    // Out of the list until it is posted again; then it comes back as the same card.
    assert!(list(&hub).unwrap()["prs"].as_array().unwrap().is_empty());
    let gh = FakeGh::new(CHECKS_PASSED);
    assert!(merge(&hub, gh.gh.clone(), 1, HEAD, "trunk", 1).unwrap_err().to_string().contains("asked for changes"));
    let out = post(&hub, 3, good()).unwrap();
    assert_eq!(out["card"], "P1");
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Waiting);
}

#[tokio::test]
async fn nothing_but_a_click_in_the_window_merges() {
    let hub = fixture();
    post(&hub, 3, good()).unwrap();
    let local = crate::devices::Principal::Local;
    for method in ["pr.merge", "pr.ask_changes", "pr.later"] {
        let Err(e) = crate::rpc::dispatch(&hub, &local, method, json!({"id": 1, "sha": HEAD})).await else { panic!("{method}") };
        assert_eq!(e.0, MERGE_BY_CLICK, "{method}");
        let Err(e) = crate::rpc::dispatch_via(&hub, &local, method, json!({"id": 1, "sha": HEAD}), crate::rpc::Via::Window).await
        else { panic!("{method}") };
        assert_eq!(e.0, MERGE_BY_CLICK, "{method} through dispatch, even marked as the window");
    }
    assert_eq!(hub.store().pr(1).unwrap().unwrap().status, Status::Waiting);
    // Reads work from the human's own client.
    let Ok(v) = crate::rpc::dispatch(&hub, &local, "pr.list", json!({})).await else { panic!("pr.list") };
    assert_eq!(v["prs"].as_array().unwrap().len(), 1);
}

#[test]
fn cards_and_their_messages_survive_a_restart() {
    let dir = temp_dir("restart");
    let db = dir.join("muster.db");
    let hub = fixture_on(crate::store::Store::open(&db).unwrap());
    post(&hub, 3, good()).unwrap();
    hub.store().update_pr(1, |p| { push(p, NoteKind::Stale, "muster", "words".into()); Ok(()) }).unwrap();
    drop(hub);
    let store = crate::store::Store::open(&db).unwrap();
    let pr = store.pr(1).unwrap().unwrap();
    assert_eq!((pr.repo.as_str(), pr.outbox.len()), ("acme/notes-app", 1));
    assert_eq!(pr.outbox[0].delivery.status, DeliveryStatus::Pending);
    assert_eq!(store.unfinished_prs().unwrap(), vec![1]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A PNG's first 33 bytes: the signature and the IHDR chunk (`width` x `height`), all that
/// `post_pr` reads of it.
fn png_of(width: u32, height: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(width.to_be_bytes());
    b.extend(height.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

#[test]
fn a_full_page_phone_picture_is_left_out_and_its_poster_told() {
    let hub = fixture();
    agent(&hub, "builder-2", SessionKind::Codex, 3);
    let dir = temp_dir("tall");
    let file = |name: &str, w: u32, h: u32| {
        let f = dir.join(name);
        std::fs::write(&f, png_of(w, h)).unwrap();
        f.to_str().unwrap().to_string()
    };
    let (desktop, page, phone, wide) =
        (file("desktop.png", 1600, 1000), file("page.png", 390, 8000), file("phone.png", 390, 844), file("wide.png", 2400, 600));
    let pics = json!([
        {"path": desktop, "caption": "Sign-up page on a laptop", "side": "before"},
        {"path": page, "caption": "Sign-up page on a phone", "side": "after"},
        {"path": phone, "caption": "One phone screen", "side": "before"},
        {"path": wide, "caption": "The wide banner", "side": "after"},
    ]);
    let out = post(&hub, 3, with(|v| v["pictures"] = pics)).unwrap();
    // The card is posted with the rest, and the poster is told which picture stayed out and why.
    assert_eq!(out["card"].as_str(), Some("P1"), "{out}");
    let left = out["left_out"].as_array().unwrap_or_else(|| panic!("no left_out in {out}"));
    assert_eq!(left.len(), 1, "{out}");
    let said = left[0].as_str().unwrap();
    assert!(said.contains(&page) && said.contains("390 × 8000") && said.contains("too tall") && said.contains("crop"), "{said}");
    assert!(out["note"].as_str().unwrap().contains("left out"), "{out}");
    // Its place is kept, so the pairs stay as posted (before 1 with after 1, before 2 with after 2),
    // but Muster keeps no copy of it and never sends it to the window.
    let v = get(&hub, 1).unwrap();
    let stored = v["pictures"].as_array().unwrap();
    assert_eq!(stored.len(), 4, "{v}");
    assert_eq!(stored[1]["too_tall"], json!({"width": 390, "height": 8000}), "{v}");
    assert!(stored.iter().enumerate().all(|(i, x)| (i == 1) == x.get("too_tall").is_some()), "{v}");
    assert!(!hub.store().pr_dir(1).join("1/1.png").exists());
    let tall = picture(&hub, 1, 1, 1).unwrap();
    assert!(tall.get("src").is_none(), "{tall}");
    assert_eq!(tall["too_tall"], json!({"width": 390, "height": 8000}), "{tall}");
    // A laptop picture, one phone screen and a wide picture are shown as before.
    for i in [0, 2, 3] {
        assert!(picture(&hub, 1, 1, i).unwrap()["src"].as_str().unwrap().starts_with("data:image/png;base64,"), "picture {i}");
    }
    // A card stored before this rule, with a tall picture in Muster's copy, never sends it either.
    std::fs::write(hub.store().pr_dir(1).join("1/0.png"), png_of(390, 8000)).unwrap();
    let old = picture(&hub, 1, 1, 0).unwrap();
    assert!(old.get("src").is_none() && old["too_tall"] == json!({"width": 390, "height": 8000}), "{old}");
    // A full-page screenshot over 8 MiB is refused, and the problem says what to post instead.
    let mut big = png_of(1170, 24000);
    big.resize(9 << 20, 0);
    let huge = dir.join("huge.png");
    std::fs::write(&huge, big).unwrap();
    let e = checked(with(|v| v["pictures"] = json!([{"path": huge.to_str().unwrap(), "caption": "Whole page", "side": "after"}])))
        .unwrap_err();
    assert!(e.iter().any(|x| x.contains("larger than 8 MiB") && x.contains("crop of one screen")), "{e:?}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(hub.store().prs_root());
}

//! Plain-text notes the human and agents share (docs/PROTOCOL.md "Notes", docs/MCP.md "Notes").
//! The daemon holds each note's text and a revision number that counts text changes. A full
//! write names the revision it replaces and is refused when the note changed since, so nobody
//! overwrites text they haven't seen; an append needs no revision and always lands at the end.
//! The UI merges the human's unsaved typing onto a newer revision before it writes (web/src/notes.ts).
//! Every change goes to every client as `note_upserted` with the full note.

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hub::{Hub, ServerEvent};
use crate::model::*;

/// Largest note text, in bytes of UTF-8.
pub const MAX_BODY: usize = 1024 * 1024;
/// Notes in all, so a runaway agent can't fill the disk.
pub const MAX_NOTES: usize = 1000;
const MAX_TITLE: usize = 200;

/// Serializes read-modify-write of a note (revision checks, appends) and orders its events.
#[derive(Default)]
pub struct Notes {
    lock: Mutex<()>,
}

/// Plain text only: CRLF and CR become LF; NUL, ESC and every other control character except
/// tab and newline are dropped.
pub fn clean(s: &str) -> String {
    s.replace("\r\n", "\n")
        .chars()
        .map(|c| if c == '\r' { '\n' } else { c })
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

fn title(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    ensure!(!s.is_empty() && s.len() <= MAX_TITLE && !s.chars().any(char::is_control),
        "a note title must be 1–{MAX_TITLE} bytes of text on one line");
    Ok(s.to_string())
}

fn check_size(body: &str) -> anyhow::Result<()> {
    ensure!(body.len() <= MAX_BODY, "note text would be {} bytes; the limit is 1 MiB ({MAX_BODY} bytes)", body.len());
    Ok(())
}

fn who(n: &NoteAuthor) -> String {
    match (&n.name, n.kind.as_str()) {
        (Some(name), _) => name.clone(),
        (None, "human") => "you".into(),
        (None, kind) => kind.into(),
    }
}

/// Who a change is from: the human (no session, or a shell tab's CLI), else the agent of
/// `from_session`.
pub fn author(hub: &Hub, from_session: Option<SessionId>) -> anyhow::Result<NoteAuthor> {
    let Some(id) = from_session else { return Ok(NoteAuthor::human()) };
    let s = hub.session(id).context("calling session no longer exists")?;
    if s.kind == SessionKind::Shell {
        return Ok(NoteAuthor { kind: "human".into(), name: None, session_id: Some(id) });
    }
    let name = hub.agents().by_session(id).map(|a| a.name).unwrap_or_else(|| {
        let kind = if s.kind == SessionKind::Claude { "Claude" } else { "Codex" };
        match hub.channel(s.channel_id) {
            Some(c) if !c.name.is_empty() => format!("{kind} ({})", c.name),
            _ => kind.to_string(),
        }
    });
    Ok(NoteAuthor { kind: "agent".into(), name: Some(name), session_id: Some(id) })
}

/// A note whose window is gone is shown nowhere: it reads as closed.
fn view(hub: &Hub, mut n: Note) -> Note {
    if n.window_id.is_some_and(|w| hub.window(w).is_none()) {
        n.window_id = None;
        n.beside = None;
    }
    n
}

pub fn list(hub: &Hub) -> anyhow::Result<Vec<Note>> {
    Ok(hub.store().load_notes()?.into_iter().map(|n| view(hub, n)).collect())
}

pub fn get(hub: &Hub, id: NoteId) -> anyhow::Result<Note> {
    let n = hub.store().load_note(id)?.with_context(|| format!("no note with id {id} (see note_list)"))?;
    Ok(view(hub, n))
}

/// What `note_list` / `note.list` show of a note: everything but the text.
pub fn summary(n: &Note) -> Value {
    json!({
        "id": n.id, "title": n.title, "rev": n.rev, "bytes": n.body.len(), "lines": n.body.lines().count(),
        "created_at": n.created_at, "updated_at": n.updated_at, "updated_by": n.updated_by,
        "open": n.window_id.is_some(), "window_id": n.window_id,
    })
}

fn save(hub: &Hub, n: Note) -> anyhow::Result<Note> {
    hub.store().save_note(&n)?;
    let n = view(hub, n);
    hub.emit(ServerEvent::NoteUpserted(n.clone()));
    Ok(n)
}

fn window_exists(hub: &Hub, window: WindowId) -> anyhow::Result<()> {
    ensure!(hub.window(window).is_some(), "no window with id {window}");
    Ok(())
}

/// A new note, shown in `window` (beside session `beside`) when given. No title: "Note <id>".
pub fn create(hub: &Hub, title_in: Option<&str>, body: &str, by: NoteAuthor, window: Option<WindowId>,
    beside: Option<SessionId>) -> anyhow::Result<Note> {
    let named = title_in.filter(|t| !t.trim().is_empty()).map(title).transpose()?;
    let body = clean(body);
    check_size(&body)?;
    if let Some(w) = window { window_exists(hub, w)?; }
    let _g = hub.notes.lock.lock();
    ensure!(hub.store().count_notes()? < MAX_NOTES, "Muster already has {MAX_NOTES} notes; delete some first");
    let now = now_ms();
    let mut n = Note { id: 0, title: named.clone().unwrap_or_else(|| "Note".into()), body, rev: 1, created_at: now,
        updated_at: now, updated_by: by, window_id: window, beside: window.and(beside) };
    hub.store().create_note(&mut n)?;
    if named.is_none() {
        n.title = format!("Note {}", n.id);
        hub.store().save_note(&n)?;
    }
    hub.emit(ServerEvent::NoteUpserted(n.clone()));
    Ok(n)
}

/// Replace the text. `base_rev` is the revision the new text was made from: a note changed
/// since is refused, and the caller reads it again. The same text again changes nothing.
pub fn write(hub: &Hub, id: NoteId, body: &str, base_rev: i64, by: NoteAuthor, new_title: Option<&str>) -> anyhow::Result<Note> {
    let body = clean(body);
    check_size(&body)?;
    let new_title = new_title.map(title).transpose()?;
    let _g = hub.notes.lock.lock();
    let mut n = get(hub, id)?;
    ensure!(n.rev == base_rev,
        "note {id} changed since revision {base_rev}: it is at revision {} (last edited by {}). Read it again (note_read) and write from that revision",
        n.rev, who(&n.updated_by));
    let mut changed = false;
    if let Some(t) = new_title.filter(|t| *t != n.title) {
        n.title = t;
        changed = true;
    }
    if body != n.body {
        n.body = body;
        n.rev += 1;
        n.updated_at = now_ms();
        n.updated_by = by;
        changed = true;
    }
    if !changed { return Ok(n) }
    save(hub, n)
}

/// Add `text` at the end, on a new line when the note doesn't already end with one.
pub fn append(hub: &Hub, id: NoteId, text: &str, by: NoteAuthor) -> anyhow::Result<Note> {
    let text = clean(text);
    ensure!(!text.is_empty(), "nothing to append");
    let _g = hub.notes.lock.lock();
    let mut n = get(hub, id)?;
    let sep = if n.body.is_empty() || n.body.ends_with('\n') { "" } else { "\n" };
    check_size(&format!("{}{sep}{text}", n.body))?;
    n.body.push_str(sep);
    n.body.push_str(&text);
    n.rev += 1;
    n.updated_at = now_ms();
    n.updated_by = by;
    save(hub, n)
}

pub fn rename(hub: &Hub, id: NoteId, new_title: &str) -> anyhow::Result<Note> {
    let t = title(new_title)?;
    let _g = hub.notes.lock.lock();
    let mut n = get(hub, id)?;
    if n.title == t { return Ok(n) }
    n.title = t;
    save(hub, n)
}

/// Show the note's tab in `window` (moving it from another window), beside session `beside`
/// when that is in the window too.
pub fn open(hub: &Hub, id: NoteId, window: WindowId, beside: Option<SessionId>) -> anyhow::Result<Note> {
    window_exists(hub, window)?;
    let _g = hub.notes.lock.lock();
    let mut n = get(hub, id)?;
    if n.window_id == Some(window) && (beside.is_none() || n.beside == beside) { return Ok(n) }
    n.window_id = Some(window);
    n.beside = beside;
    save(hub, n)
}

/// Close its tab. The note stays (`delete` removes it).
pub fn close(hub: &Hub, id: NoteId) -> anyhow::Result<Note> {
    let _g = hub.notes.lock.lock();
    let mut n = get(hub, id)?;
    if n.window_id.is_none() { return Ok(n) }
    n.window_id = None;
    n.beside = None;
    save(hub, n)
}

pub fn delete(hub: &Hub, id: NoteId) -> anyhow::Result<()> {
    let _g = hub.notes.lock.lock();
    ensure!(hub.store().delete_note(id)?, "no note with id {id}");
    hub.emit(ServerEvent::NoteRemoved { id });
    Ok(())
}

/// The window session `id`'s tab is in, for showing a note beside it.
pub fn window_of(hub: &Hub, id: SessionId) -> anyhow::Result<WindowId> {
    let s = hub.session(id).context("calling session no longer exists")?;
    s.window_id.or_else(|| if s.hidden { None } else { hub.channel(s.channel_id).and_then(|c| c.window_id) })
        .context("this session's tab isn't shown in any window (it is hidden), so the note can't be shown beside it")
}

/// `note.delete` from a session: only the human deletes notes (a guard against an agent's
/// mistake, not a security boundary: any process with the client token can call it).
pub fn check_delete_from(hub: &Hub, from_session: Option<SessionId>) -> anyhow::Result<()> {
    ensure!(author(hub, from_session)?.kind == "human", "only the human deletes notes (an agent can still rewrite one with note_write)");
    Ok(())
}

/// The MCP note tools, run as session `caller` (src/mcp/tools.rs checks the rate limit).
pub fn tool(hub: &Hub, caller: SessionId, name: &str, args: Value) -> anyhow::Result<Value> {
    let by = || author(hub, Some(caller));
    match name {
        "note_list" => {
            ensure!(args.as_object().is_none_or(|o| o.is_empty()), "note_list takes no arguments");
            Ok(json!({"notes": list(hub)?.iter().map(summary).collect::<Vec<_>>()}))
        }
        "note_read" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { note_id: NoteId }
            let p: P = serde_json::from_value(args)?;
            let n = get(hub, p.note_id)?;
            Ok(json!({"note_id": n.id, "title": n.title, "rev": n.rev, "text": n.body, "updated_at": n.updated_at,
                "updated_by": n.updated_by, "open": n.window_id.is_some()}))
        }
        "note_write" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { note_id: Option<NoteId>, title: Option<String>, text: String, rev: Option<i64> }
            let p: P = serde_json::from_value(args)?;
            let n = match p.note_id {
                None => {
                    ensure!(p.rev.is_none(), "rev is only for replacing an existing note (give note_id)");
                    create(hub, p.title.as_deref(), &p.text, by()?, None, None)?
                }
                Some(id) => {
                    let rev = p.rev.context("rev is required to replace a note: pass the rev note_read returned (or use note_append to add at the end)")?;
                    write(hub, id, &p.text, rev, by()?, p.title.as_deref())?
                }
            };
            Ok(json!({"note_id": n.id, "title": n.title, "rev": n.rev}))
        }
        "note_append" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { note_id: NoteId, text: String }
            let p: P = serde_json::from_value(args)?;
            let n = append(hub, p.note_id, &p.text, by()?)?;
            Ok(json!({"note_id": n.id, "rev": n.rev}))
        }
        "note_open" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { note_id: NoteId }
            let p: P = serde_json::from_value(args)?;
            let window = window_of(hub, caller)?;
            let n = open(hub, p.note_id, window, Some(caller))?;
            Ok(json!({"note_id": n.id, "window_id": window}))
        }
        _ => anyhow::bail!("unknown note tool"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StoredSession};

    fn fixture() -> std::sync::Arc<Hub> {
        let store = Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        let mut channel = crate::mcp::guard::test_channel(1);
        channel.window_id = Some(1);
        store.insert_channel(&channel).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.channel_id = 1;
        info.window_id = Some(1);
        info.kind = SessionKind::Claude;
        store.insert_session(&StoredSession { info, extra_args: vec![] }).unwrap();
        Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None }, store).unwrap()
    }

    #[test]
    fn writes_need_the_current_revision_and_appends_never_do() {
        let hub = fixture();
        let n = create(&hub, None, "a\r\nb\u{1b}[31m", NoteAuthor::human(), None, None).unwrap();
        assert_eq!((n.title.as_str(), n.body.as_str(), n.rev), (format!("Note {}", n.id).as_str(), "a\nb[31m", 1));
        let n2 = write(&hub, n.id, "one", 1, NoteAuthor::human(), None).unwrap();
        assert_eq!(n2.rev, 2);
        // A writer still at revision 1 is refused and told the current one.
        let e = write(&hub, n.id, "stale", 1, NoteAuthor::human(), None).unwrap_err().to_string();
        assert!(e.contains("changed since revision 1") && e.contains("revision 2"), "{e}");
        // The same text again is no change.
        assert_eq!(write(&hub, n.id, "one", 2, NoteAuthor::human(), None).unwrap().rev, 2);
        let agent = NoteAuthor { kind: "agent".into(), name: Some("bot".into()), session_id: Some(1) };
        let n3 = append(&hub, n.id, "two", agent.clone()).unwrap();
        assert_eq!((n3.body.as_str(), n3.rev, n3.updated_by.clone()), ("one\ntwo", 3, agent));
        assert_eq!(append(&hub, n.id, "three\n", NoteAuthor::human()).unwrap().body, "one\ntwo\nthree\n");
        assert_eq!(append(&hub, n.id, "four", NoteAuthor::human()).unwrap().body, "one\ntwo\nthree\nfour");
        assert!(append(&hub, n.id, "\u{7}", NoteAuthor::human()).is_err());
        // A rename doesn't change the revision.
        assert_eq!(rename(&hub, n.id, " Plan ").unwrap().rev, 5);
        assert_eq!(get(&hub, n.id).unwrap().title, "Plan");
        assert!(rename(&hub, n.id, "a\nb").is_err());
    }

    #[test]
    fn size_and_count_are_capped() {
        let hub = fixture();
        let big = "x".repeat(MAX_BODY);
        let n = create(&hub, Some("Big"), &big, NoteAuthor::human(), None, None).unwrap();
        assert!(append(&hub, n.id, "y", NoteAuthor::human()).unwrap_err().to_string().contains("1 MiB"));
        assert!(write(&hub, n.id, &format!("{big}y"), 1, NoteAuthor::human(), None).is_err());
        assert!(create(&hub, Some("Bigger"), &format!("{big}y"), NoteAuthor::human(), None, None).is_err());
        assert!(create(&hub, Some(&"t".repeat(201)), "", NoteAuthor::human(), None, None).is_err());
    }

    #[test]
    fn open_close_and_delete_and_survive_a_new_hub() {
        let hub = fixture();
        let n = create(&hub, Some("Shared"), "", NoteAuthor::human(), None, None).unwrap();
        assert!(open(&hub, n.id, 9, None).is_err(), "unknown window");
        let o = open(&hub, n.id, 1, Some(1)).unwrap();
        assert_eq!((o.window_id, o.beside), (Some(1), Some(1)));
        let rebuilt = Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None }, hub.store().clone()).unwrap();
        assert_eq!(get(&rebuilt, n.id).unwrap(), o);
        let c = close(&hub, n.id).unwrap();
        assert_eq!((c.window_id, c.beside), (None, None));
        delete(&hub, n.id).unwrap();
        assert!(get(&hub, n.id).is_err() && delete(&hub, n.id).is_err());
    }

    #[test]
    fn mcp_tools_round_trip() {
        let hub = fixture();
        let made = tool(&hub, 1, "note_write", json!({"title": "Todo", "text": "- a"})).unwrap();
        let id = made["note_id"].as_i64().unwrap();
        let read = tool(&hub, 1, "note_read", json!({"note_id": id})).unwrap();
        assert_eq!((read["text"].as_str(), read["rev"].as_i64(), read["open"].as_bool()), (Some("- a"), Some(1), Some(false)));
        assert_eq!((read["updated_by"]["kind"].as_str(), read["updated_by"]["name"].as_str()), (Some("agent"), Some("Claude (c1)")));
        let e = tool(&hub, 1, "note_write", json!({"note_id": id, "text": "x"})).unwrap_err().to_string();
        assert!(e.contains("rev is required"), "{e}");
        assert_eq!(tool(&hub, 1, "note_write", json!({"note_id": id, "text": "- b", "rev": 1})).unwrap()["rev"], 2);
        assert_eq!(tool(&hub, 1, "note_append", json!({"note_id": id, "text": "- c"})).unwrap()["rev"], 3);
        let listed = tool(&hub, 1, "note_list", json!({})).unwrap();
        assert_eq!(listed["notes"][0]["bytes"], 7);
        assert!(listed["notes"][0].get("text").is_none());
        assert_eq!(tool(&hub, 1, "note_open", json!({"note_id": id})).unwrap()["window_id"], 1);
        assert_eq!(get(&hub, id).unwrap().beside, Some(1));
        assert!(tool(&hub, 1, "note_read", json!({"note_id": id, "extra": 1})).is_err());
    }
}

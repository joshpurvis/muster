//! Links that bring a note, panel, tab or agent to the front of Muster's window (docs/UI.md
//! "Links"). Agents mention things to the human by title, as a Markdown link:
//! `[Pairs to review](muster://note/91)`. Claude Code shows the title as a terminal hyperlink
//! (OSC 8, see `adapters::claude`); Codex shows `Pairs to review (muster://note/91)`; Muster's
//! terminal makes both clickable (web/src/links.ts, which also parses them).
//!
//! A link names a kind and an id (an agent: its name) and nothing else: no access token, no
//! window, nothing private, since it ends up in transcripts and may be pasted elsewhere.
//! Clicking one only shows what it names.
//!
//! Tool and command results that return a note, panel or session also carry `link` (the
//! address) and `mention` (the ready Markdown with the title), so agents never build them.

use serde_json::{Value, json};

use crate::model::SessionId;

pub fn note(id: i64) -> String {
    format!("muster://note/{id}")
}

pub fn panel(id: i64) -> String {
    format!("muster://panel/{id}")
}

/// A session's tab.
pub fn tab(id: SessionId) -> String {
    format!("muster://tab/{id}")
}

/// A named agent's tab, by name (names are `agents::check_name`'s: letters, digits, `-`, `_`).
pub fn agent(name: &str) -> String {
    format!("muster://agent/{name}")
}

/// `[title](link)`, with the title on one line and its Markdown characters escaped, so it can't
/// end the link early or turn into formatting. An empty title shows the address.
pub fn mention(title: &str, link: &str) -> String {
    let flat: String = title.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(flat.len() + link.len() + 4);
    out.push('[');
    for c in if flat.is_empty() { link } else { flat.as_str() }.chars() {
        if matches!(c, '\\' | '[' | ']' | '`' | '*' | '_' | '<' | '>') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("](");
    out.push_str(link);
    out.push(')');
    out
}

/// Add `link` and `mention` to a JSON object (anything else is left alone).
pub fn add(v: &mut Value, link: String, title: &str) {
    if let Some(o) = v.as_object_mut() {
        o.insert("mention".into(), json!(mention(title, &link)));
        o.insert("link".into(), json!(link));
    }
}

/// A session's tab title as the human sees it: its name, else its kind.
pub fn tab_title(name: &str, kind: &str) -> String {
    let n = name.trim();
    if n.is_empty() { kind.to_string() } else { n.to_string() }
}

/// The sentence every agent's instructions carry (src/agents.rs persona, the MCP server's
/// instructions): mention things by title, as a link.
pub const INSTRUCTION: &str = "When you mention a note, panel, tab or agent to the human, write \
its title as a Markdown link, never its bare number: [Pairs to review](muster://note/91). The \
human clicks it to see that note. Muster's tool and command results give the ready link as \
`mention` (the address alone as `link`); `muster link note 91` prints one.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_carry_only_kind_and_id() {
        assert_eq!(note(91), "muster://note/91");
        assert_eq!(panel(3), "muster://panel/3");
        assert_eq!(tab(12), "muster://tab/12");
        assert_eq!(agent("reviewer"), "muster://agent/reviewer");
    }

    #[test]
    fn mentions_stay_one_markdown_link() {
        assert_eq!(mention("Pairs to review", &note(91)), "[Pairs to review](muster://note/91)");
        assert_eq!(mention("a]b[c", &note(1)), r"[a\]b\[c](muster://note/1)");
        assert_eq!(mention("two\nlines\t here", &note(1)), "[two lines here](muster://note/1)");
        assert_eq!(mention("*bold* `code` <x> \\", &panel(2)), r"[\*bold\* \`code\` \<x\> \\](muster://panel/2)");
        assert_eq!(mention("  ", &tab(4)), "[muster://tab/4](muster://tab/4)");
    }

    #[test]
    fn add_puts_both_fields_on_an_object() {
        let mut v = json!({"note_id": 5});
        add(&mut v, note(5), "Plan");
        assert_eq!(v, json!({"note_id": 5, "link": "muster://note/5", "mention": "[Plan](muster://note/5)"}));
        let mut not_object = json!([1]);
        add(&mut not_object, note(5), "Plan");
        assert_eq!(not_object, json!([1]));
    }

    #[test]
    fn the_instruction_shows_the_form() {
        assert!(INSTRUCTION.contains("[Pairs to review](muster://note/91)"));
        assert!(INSTRUCTION.contains("never its bare number"));
    }
}

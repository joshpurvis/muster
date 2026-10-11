//! Shared helpers for reading agent TUIs from a plain-text screen snapshot.

use crate::model::Decision;

/// One numbered choice in a TUI menu, e.g. `❯ 1. Yes` or `  2. Yes, and don't ask again (p)`.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuOption {
    /// The digit shown before the label.
    pub digit: char,
    pub label: String,
    /// Keyboard shortcut shown in trailing parentheses, e.g. `(y)` or `(esc)`, if simple.
    pub shortcut: Option<String>,
    /// True if the selection marker (`❯`/`›`/`>`) is on this row.
    pub selected: bool,
}

const MARKERS: [char; 4] = ['❯', '›', '>', '▶'];

/// Parse a numbered menu row. Returns None for anything else.
pub fn parse_option(line: &str) -> Option<MenuOption> {
    let mut s = line.trim_start();
    let mut selected = false;
    if let Some(c) = s.chars().next()
        && MARKERS.contains(&c)
    {
        selected = true;
        s = s[c.len_utf8()..].trim_start();
    }
    let mut chars = s.chars();
    let digit = chars.next()?;
    if !digit.is_ascii_digit() || digit == '0' {
        return None;
    }
    let rest = chars.as_str();
    let label = rest.strip_prefix(". ")?.trim().to_string();
    if label.is_empty() {
        return None;
    }
    let shortcut = trailing_shortcut(&label);
    Some(MenuOption {
        digit,
        label,
        shortcut,
        selected,
    })
}

/// `"Yes, proceed (y)"` -> `Some("y")`; `"... (esc)"` -> `Some("esc")`. Multi-key chords like
/// `(shift+tab)` are ignored.
fn trailing_shortcut(label: &str) -> Option<String> {
    let inner = label.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let key = &inner[open + 1..];
    if key == "esc" || (key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        Some(key.to_string())
    } else {
        None
    }
}

/// Numbered options found in `lines[from..]` (stops scanning at `to`).
pub fn options_in(lines: &[String], from: usize, to: usize) -> Vec<MenuOption> {
    lines
        .iter()
        .take(to.min(lines.len()))
        .skip(from)
        .filter_map(|l| parse_option(l))
        .collect()
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// True if the label is a "yes, and stop asking" style option.
pub fn is_always_label(label: &str) -> bool {
    let l = lower(label);
    l.starts_with("yes")
        && [
            "don't ask again",
            "don’t ask again",
            "do not ask again",
            "always",
            "accept edits",
            "auto-accept",
            "allow all",
            "for this session",
        ]
        .iter()
        .any(|k| l.contains(k))
}

pub fn is_yes_label(label: &str) -> bool {
    lower(label).starts_with("yes")
}

pub fn is_no_label(label: &str) -> bool {
    let l = lower(label);
    l.starts_with("no") || l.starts_with("deny") || l.starts_with("reject")
}

/// Pick the option that answers `decision`.
pub fn choose(options: &[MenuOption], decision: Decision) -> Option<&MenuOption> {
    match decision {
        // Never fall back to a persistent "always" option for a one-time approval.
        Decision::Approve => options
            .iter()
            .find(|o| is_yes_label(&o.label) && !is_always_label(&o.label)),
        Decision::ApproveAlways => options.iter().find(|o| is_always_label(&o.label)),
        Decision::Deny => options.iter().find(|o| is_no_label(&o.label)),
    }
}

/// Keys that pick `opt`: its single-key shortcut if shown (preferred), else its digit.
pub fn option_keys(opt: &MenuOption, prefer_shortcut: bool) -> Vec<u8> {
    if prefer_shortcut && let Some(k) = &opt.shortcut {
        if k == "esc" {
            return vec![0x1b];
        }
        return k.as_bytes().to_vec();
    }
    vec![opt.digit as u8]
}

/// Is this a horizontal rule row (box border)?
pub fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 10 && t.chars().all(|c| matches!(c, '─' | '━' | '╌' | '-' | '═'))
}

/// A solid `────` rule (not the dashed `╌╌╌` borders around diffs).
pub fn is_solid_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─' || c == '━')
}

/// Index of the last line matching `pred`.
pub fn rfind(lines: &[String], pred: impl Fn(&str) -> bool) -> Option<usize> {
    lines.iter().rposition(|l| pred(l))
}

/// Shorten to at most `max` chars, single line.
pub fn clip(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Spinner / progress glyphs that animate while agents work. Stripped (with digits) when
/// deciding whether a screen changed "meaningfully".
pub const SPINNER_CHARS: &str = "·✢✳✶✻✽*•◦⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⠁⠂⠄⡀⢀⠠⠐⠈◐◓◑◒|/\\-";

/// Screen text with animation noise removed (digits, spinner glyphs), for stuck detection.
pub fn normalized_for_change(lines: &[String]) -> String {
    let mut out = String::new();
    for l in lines {
        for c in l.chars() {
            if c.is_ascii_digit() || SPINNER_CHARS.contains(c) {
                continue;
            }
            out.push(c);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
pub mod testutil {
    use crate::adapters::ScreenSnapshot;

    pub fn snap(text: &str) -> ScreenSnapshot {
        let lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
        ScreenSnapshot {
            rows: lines.len() as u16,
            cols: 120,
            lines,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options() {
        let o = parse_option(" ❯ 1. Yes").unwrap();
        assert_eq!((o.digit, o.label.as_str(), o.selected), ('1', "Yes", true));
        let o = parse_option(
            "  2. Yes, and don't ask again for commands that start with `touch zzz.txt` (p)",
        )
        .unwrap();
        assert_eq!(o.shortcut.as_deref(), Some("p"));
        assert!(is_always_label(&o.label));
        let o = parse_option("  3. No, and tell Codex what to do differently (esc)").unwrap();
        assert_eq!(o.shortcut.as_deref(), Some("esc"));
        let o = parse_option("   2. Yes, and switch to accept edits (auto-approve file edits) for this session (shift+tab)").unwrap();
        assert_eq!(o.shortcut, None);
        assert!(parse_option("  1 hi").is_none());
        assert!(parse_option("Version 2.1.280").is_none());
    }

    #[test]
    fn clip_works() {
        assert_eq!(clip("a\n b   c", 10), "a b c");
        assert_eq!(clip("abcdefghijk", 5), "abcd…");
    }
}

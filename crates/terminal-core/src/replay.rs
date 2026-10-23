//! What a client is sent on attach to show a terminal (its replay).
//!
//! A program's raw output only redraws correctly in a terminal of the size it was written for,
//! starting from the screen it started on. Full-screen programs (Claude Code, vim) put every
//! character at an absolute position and repaint only what changed, so their output replayed
//! at another size, or from the middle of the stream, lands scattered across the screen. So a
//! replay is the raw output only while all of it is there and was written at one size.
//! Otherwise it is the screen as an emulator holds it, drawn line by line, which a terminal of
//! any size shows correctly.
//!
//! Marks record the size and the screen (normal or alternate) at points in the output, so the
//! output can be re-run through an emulator as the terminal saw it: after a restart (from
//! output.log and its marks file) as well as while it runs.
use std::path::{Path, PathBuf};

use crate::pty::{TitleCb, clamp_size};

/// Scrollback lines a re-run keeps (the web client keeps 10000).
const RERUN_SCROLLBACK: usize = 10_000;
/// Most marks kept per terminal. Past this the oldest go, and a replay of the oldest output may
/// start at the wrong size.
pub(crate) const MAX_MARKS: usize = 4096;
/// Bytes compared to recognize the output.log a marks file was written for.
pub(crate) const HEAD_LEN: usize = 32;

/// The terminal's size and screen from raw output offset `offset` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mark {
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
    pub alternate_screen: bool,
}

/// The state at `start`: the last mark at or before it (the first one if all are later).
fn mark_at(marks: &[Mark], start: u64) -> Option<Mark> {
    marks.iter().rev().find(|m| m.offset <= start).or(marks.first()).copied()
}

/// True when replaying `body` raw, from a fresh terminal of the current size, shows what the
/// program drew: it is all the output since the start, written at one size.
pub(crate) fn raw_is_faithful(start: u64, marks: &[Mark]) -> bool {
    start == 0 && marks.windows(2).all(|w| (w[0].cols, w[0].rows) == (w[1].cols, w[1].rows))
}

/// Where a re-run starts: the size and screen at `start` from the marks, or for output saved
/// without marks (by an older version), `fallback` and a guess from the bytes.
fn start_state(body: &[u8], start: u64, marks: &[Mark], fallback: (u16, u16)) -> (u16, u16, bool) {
    match mark_at(marks, start) {
        Some(m) => (m.cols, m.rows, m.alternate_screen),
        None => (fallback.0, fallback.1, guess_starts_in_alt(body)),
    }
}

/// Without marks: does `body` start inside the alternate screen? Yes if its first switch
/// leaves it. A first switch that enters it proves nothing (Claude Code enters it again on
/// every full repaint), so then, as with no switch at all, a long stretch of output without a
/// single line feed says full-screen program.
fn guess_starts_in_alt(body: &[u8]) -> bool {
    let before = match first_alt_switch(body) {
        Some((_, _, false)) => return true,
        Some((at, _, true)) => &body[..at],
        None => body,
    };
    before.len() > 4096 && !before.contains(&b'\n')
}

/// The first alternate-screen switch (`CSI ? 47/1047/1049 h|l`) in `data`: where its sequence
/// starts and ends, and whether it enters (true) or leaves the alternate screen.
pub(crate) fn first_alt_switch(data: &[u8]) -> Option<(usize, usize, bool)> {
    alt_switches(data).next()
}

fn alt_switches(data: &[u8]) -> impl Iterator<Item = (usize, usize, bool)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] != 0x1b || data[i + 1] != b'[' || data[i + 2] != b'?' {
                i += 1;
                continue;
            }
            let at = i;
            let mut j = i + 3;
            let mut params: Vec<u32> = Vec::new();
            let mut cur: Option<u32> = None;
            while j < data.len() {
                match data[j] {
                    d @ b'0'..=b'9' => cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add((d - b'0') as u32)),
                    b';' => params.push(cur.take().unwrap_or(0)),
                    _ => break,
                }
                j += 1;
            }
            if let Some(c) = cur {
                params.push(c);
            }
            i = j.max(i + 1);
            if let Some(&f) = data.get(j)
                && (f == b'h' || f == b'l')
                && params.iter().any(|p| matches!(p, 47 | 1047 | 1049))
            {
                return Some((at, j + 1, f == b'h'));
            }
        }
        None
    })
}

/// `body` (raw output from offset `start`) run through an emulator as the terminal saw it.
pub(crate) struct Rerun {
    pub parser: vt100::Parser<TitleCb>,
    /// The program's last full-screen view, if it left the alternate screen in `body`, and
    /// where it goes among the normal screen's lines: before the line this many from the end.
    pub left_alt: Option<(Vec<Vec<u8>>, usize)>,
}

enum Cut {
    Resize(u16, u16),
    /// The last switch that leaves the alternate screen; it ends at this index.
    Leave(usize),
}

/// Re-run `body` from the size and screen at `start`, resizing where the marks say. None if
/// the emulator panicked.
pub(crate) fn rerun(body: &[u8], start: u64, marks: &[Mark], fallback: (u16, u16)) -> Option<Rerun> {
    let (cols, rows, alt) = start_state(body, start, marks, fallback);
    let (cols, rows) = clamp_size(cols, rows);
    let mut parser = vt100::Parser::new_with_callbacks(rows, cols, RERUN_SCROLLBACK, TitleCb::default());
    if alt {
        parser.process(b"\x1b[?1049h");
    }
    let end = start + body.len() as u64;
    let mut cuts: Vec<(usize, Cut)> = marks
        .iter()
        .filter(|m| m.offset > start && m.offset < end)
        .map(|m| ((m.offset - start) as usize, Cut::Resize(m.cols, m.rows)))
        .collect();
    if let Some((at, until, _)) = alt_switches(body).filter(|s| !s.2).last() {
        cuts.push((at, Cut::Leave(until)));
        cuts.sort_by_key(|c| c.0);
    }
    let mut left_alt = None;
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pos = 0;
        for (at, cut) in cuts {
            // A mark inside the switch sequence just handled applies right after it.
            let at = at.max(pos);
            parser.process(&body[pos..at]);
            pos = at;
            match cut {
                Cut::Resize(cols, rows) => {
                    let (cols, rows) = clamp_size(cols, rows);
                    parser.screen_mut().set_size(rows, cols);
                }
                Cut::Leave(until) => {
                    let was_alt = parser.screen().alternate_screen();
                    let view = was_alt.then(|| visible_lines(parser.screen_mut()));
                    parser.process(&body[pos..until]);
                    pos = until;
                    if let Some(view) = view
                        && !parser.screen().alternate_screen()
                    {
                        // The view goes where the program was started: at the cursor the
                        // normal screen gets back.
                        let screen = parser.screen_mut();
                        let (rows, _) = screen.size();
                        let below = rows.saturating_sub(screen.cursor_position().0) as usize;
                        left_alt = Some((view, below, scrollback_len(screen)));
                    }
                }
            }
        }
        parser.process(&body[pos..]);
        // End any sequence the output was cut off in.
        parser.process(b"\x18");
    }));
    if ran.is_err() {
        return None;
    }
    // Lines that scrolled into the scrollback since then are below the view too (fewer once
    // the scrollback is full and drops its oldest).
    let now = scrollback_len(parser.screen_mut());
    let left_alt = left_alt.map(|(view, below, sb)| (view, below + now.saturating_sub(sb)));
    Some(Rerun { parser, left_alt })
}

fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

/// Each row of the active screen's scrollback and then of the screen itself, oldest first, as
/// escape codes that draw it from the start of an empty line.
pub(crate) fn all_lines(screen: &mut vt100::Screen) -> Vec<Vec<u8>> {
    let (rows, _) = screen.size();
    let rows = rows as usize;
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let sb = screen.scrollback();
    let mut lines = Vec::with_capacity(sb + rows);
    // A view at offset `o` shows combined lines [sb - o, sb - o + rows).
    while lines.len() < sb + rows {
        let o = sb - lines.len().min(sb);
        screen.set_scrollback(o);
        let first = sb - o;
        let skip = lines.len() - first;
        lines.extend(row_lines(screen).into_iter().skip(skip));
    }
    screen.set_scrollback(prev);
    lines
}

/// The visible rows of the active screen, trailing blank ones dropped.
fn visible_lines(screen: &mut vt100::Screen) -> Vec<Vec<u8>> {
    let prev = screen.scrollback();
    screen.set_scrollback(0);
    let mut lines = row_lines(screen);
    screen.set_scrollback(prev);
    trim_blank_tail(&mut lines);
    lines
}

fn row_lines(screen: &vt100::Screen) -> Vec<Vec<u8>> {
    let (_, cols) = screen.size();
    // One column more than the screen has: vt100 then treats no row as continuing the one
    // above, so each row draws on its own from column 0 instead of assuming the cursor wrapped
    // there (which would emit absolute moves).
    screen.rows_formatted(0, cols.saturating_add(1)).collect()
}

fn trim_blank_tail(lines: &mut Vec<Vec<u8>>) {
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
}

/// Draw `lines` one per terminal line, each starting from default attributes.
fn draw(out: &mut Vec<u8>, lines: &[Vec<u8>]) {
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\x1b[m\r\n");
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(b"\x1b[m");
}

/// Drop scrollback lines from the front, keeping at least `keep`, until drawing the rest takes
/// at most `max` bytes.
fn fit(lines: &mut Vec<Vec<u8>>, keep: usize, max: Option<usize>) {
    let Some(max) = max else { return };
    let mut size: usize = lines.iter().map(|l| l.len() + 5).sum();
    let mut drop = 0;
    while size > max && lines.len() - drop > keep {
        size -= lines[drop].len() + 5;
        drop += 1;
    }
    lines.drain(..drop);
}

/// Modes vt100 doesn't track that a client should get back: focus reporting, cursor style.
fn extra_modes(cb: &TitleCb) -> Vec<u8> {
    let mut v = Vec::new();
    if cb.focus_reporting {
        v.extend_from_slice(b"\x1b[?1004h");
    }
    if let Some(style) = cb.cursor_style {
        v.extend_from_slice(format!("\x1b[{style} q").as_bytes());
    }
    v
}

/// Replay of a live terminal on the normal screen, from its own emulator.
pub(crate) fn live_normal(live: &mut vt100::Parser<TitleCb>, max: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    draw_normal(&mut out, live, max);
    out.extend_from_slice(&live.screen().input_mode_formatted());
    out.extend_from_slice(&extra_modes(live.callbacks()));
    out
}

/// The alternate screen of a live terminal, from its own emulator, to follow `behind`.
pub(crate) fn live_alt(live: &vt100::Parser<TitleCb>) -> Vec<u8> {
    let mut out = b"\x1b[?1049h".to_vec();
    out.extend_from_slice(&live.screen().contents_formatted());
    out.extend_from_slice(&live.screen().input_mode_formatted());
    out.extend_from_slice(&extra_modes(live.callbacks()));
    out
}

/// The normal screen behind a live alternate screen, from a re-run of the ring (the live
/// emulator only shows the screen in use), so it is there when the program leaves.
pub(crate) fn behind(body: &[u8], start: u64, marks: &[Mark], size: (u16, u16), max: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(mut run) = rerun(body, start, marks, size) {
        if run.parser.screen().alternate_screen() {
            run.parser.process(b"\x1b[?47l");
        }
        draw_normal(&mut out, &mut run.parser, max);
    }
    out
}

/// The normal screen with its scrollback, the cursor where it was and the attributes it had.
fn draw_normal(out: &mut Vec<u8>, parser: &mut vt100::Parser<TitleCb>, max: Option<usize>) {
    let screen = parser.screen_mut();
    let (rows, _) = screen.size();
    let mut lines = all_lines(screen);
    fit(&mut lines, rows as usize, max);
    draw(out, &lines);
    // From the last row (whatever height the client has) back to the cursor.
    let (row, col) = screen.cursor_position();
    out.push(b'\r');
    let up = rows.saturating_sub(1).saturating_sub(row);
    if up > 0 {
        out.extend_from_slice(format!("\x1b[{up}A").as_bytes());
    }
    if col > 0 {
        out.extend_from_slice(format!("\x1b[{}G", col + 1).as_bytes());
    }
    out.extend_from_slice(&screen.attributes_formatted());
    if screen.hide_cursor() {
        out.extend_from_slice(b"\x1b[?25l");
    }
}

/// Replay of a terminal whose program has ended: everything as plain lines on the normal
/// screen, so it all scrolls. A full-screen program's last view stays where it ran, as it
/// would in a terminal without an alternate screen; after a full-screen program that was
/// still running, it comes last. None if the emulator panicked.
pub fn render_ended(body: &[u8], start: u64, marks: &[Mark], fallback: (u16, u16), max: Option<usize>) -> Option<Vec<u8>> {
    let Rerun { mut parser, left_alt } = rerun(body, start, marks, fallback)?;
    let mut lines;
    if parser.screen().alternate_screen() {
        let view = visible_lines(parser.screen_mut());
        parser.process(b"\x1b[?47l");
        lines = all_lines(parser.screen_mut());
        trim_blank_tail(&mut lines);
        lines.extend(view);
    } else {
        lines = all_lines(parser.screen_mut());
        if let Some((view, below)) = left_alt {
            let at = lines.len().saturating_sub(below);
            lines.splice(at..at, view);
        }
        trim_blank_tail(&mut lines);
    }
    fit(&mut lines, 1, max);
    let mut out = Vec::new();
    draw(&mut out, &lines);
    Some(out)
}

/// The marks file next to an output.log.
pub fn marks_path(log: &Path) -> PathBuf {
    log.with_extension("marks.json")
}

/// What output.log's marks file holds: the raw output offset of the log's first byte, the
/// log's first bytes (to recognize a marks file left from an earlier log), and the marks.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct LogMarks {
    pub start: u64,
    pub head: Vec<u8>,
    pub marks: Vec<Mark>,
}

impl LogMarks {
    pub fn new(start: u64, log_head: &[u8], marks: Vec<Mark>) -> Self {
        Self { start, head: log_head[..log_head.len().min(HEAD_LEN)].to_vec(), marks }
    }

    /// The marks file for `log`, if it was written for the log as it is now.
    pub fn read(log: &Path) -> Option<Self> {
        let m: Self = serde_json::from_slice(&std::fs::read(marks_path(log)).ok()?).ok()?;
        let mut head = vec![0; m.head.len()];
        use std::io::Read;
        std::fs::File::open(log).ok()?.read_exact(&mut head).ok()?;
        (head == m.head).then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(replay: &[u8], cols: u16, rows: u16) -> (bool, Vec<String>) {
        let mut p = vt100::Parser::new(rows, cols, 1000);
        p.process(replay);
        let alt = p.screen().alternate_screen();
        (alt, p.screen().rows(0, cols).map(|r| r.trim_end().to_string()).collect())
    }

    fn mark(offset: u64, cols: u16, rows: u16, alt: bool) -> Mark {
        Mark { offset, cols, rows, alternate_screen: alt }
    }

    #[test]
    fn alt_switches_find_enters_and_leaves() {
        assert_eq!(first_alt_switch(b"x\x1b[?25;1049hy"), Some((1, 12, true)));
        assert_eq!(first_alt_switch(b"x\x1b[?25h\x1b[?47l"), Some((7, 13, false)));
        assert_eq!(first_alt_switch(b"\x1b[?2004h"), None);
    }

    #[test]
    fn guesses_alt_start_without_marks() {
        // Leaves first: it was in the alternate screen.
        assert!(guess_starts_in_alt(b"app\x1b[?1049lshell\r\n"));
        // Enters first after line output: normal screen.
        assert!(!guess_starts_in_alt(b"shell\r\n\x1b[?1049happ"));
        // A repaint that enters again after a long stretch of positioned output (Claude Code).
        let mut body = b"\x1b[5;3Hx".repeat(1000);
        body.extend_from_slice(b"\x1b[?1049h\x1b[2Jframe");
        assert!(guess_starts_in_alt(&body));
        assert!(!guess_starts_in_alt(b"short\x1b[?1049h"));
    }

    #[test]
    fn rerun_follows_resizes() {
        // Written at 20 columns, then at 10 after a resize: the later text wraps at 10.
        let before = b"\x1b[1;15Hright\x1b[2J\x1b[H";
        let body = [&before[..], b"abcdefghijkl"].concat();
        let marks = [mark(0, 20, 3, false), mark(before.len() as u64, 10, 3, false)];
        let r = rerun(&body, 0, &marks, (80, 24)).unwrap();
        let screen = r.parser.screen();
        assert_eq!(screen.size(), (3, 10));
        assert_eq!(screen.rows(0, 10).take(2).collect::<Vec<_>>(), ["abcdefghij", "kl"]);
        // Starting later, the state comes from the last mark before the start.
        let r = rerun(b"x", 100, &marks, (80, 24)).unwrap();
        assert_eq!(r.parser.screen().size(), (3, 10));
    }

    #[test]
    fn ended_full_screen_program_shows_its_last_view_then_what_followed() {
        // A redundant enter while already in the alternate screen (what Claude Code sends on a
        // repaint) must not move its drawing onto the normal screen.
        let body = b"\x1b[1;1Hold frame\x1b[?1049h\x1b[2J\x1b[1;1Hlast frame\x1b[2;1Hstatus\x1b[?1049l\r\nResume with: x\r\n";
        let out = render_ended(body, 1000, &[mark(0, 30, 4, true)], (80, 24), None).unwrap();
        let (alt, rows) = text(&out, 30, 6);
        assert!(!alt);
        assert_eq!(rows[..4], ["last frame", "status", "", "Resume with: x"]);
        assert!(!String::from_utf8_lossy(&out).contains("old frame"));
    }

    #[test]
    fn ended_shell_keeps_view_where_the_program_ran() {
        let body = b"$ vim\r\n\x1b[?1049h\x1b[Hfile text\x1b[?1049l$ ls\r\na b\r\n$ ";
        let out = render_ended(body, 0, &[mark(0, 20, 5, false)], (80, 24), None).unwrap();
        let (_, rows) = text(&out, 20, 8);
        assert_eq!(rows[..5], ["$ vim", "file text", "$ ls", "a b", "$"]);
    }

    #[test]
    fn ended_in_alt_screen_shows_normal_screen_then_the_view() {
        let body = b"$ top\r\n\x1b[?1049h\x1b[Hcpu 3%";
        let out = render_ended(body, 0, &[mark(0, 20, 5, false)], (80, 24), None).unwrap();
        let (alt, rows) = text(&out, 20, 5);
        assert!(!alt);
        assert_eq!(rows[..2], ["$ top", "cpu 3%"]);
    }

    #[test]
    fn ended_replay_fits_the_byte_limit_by_dropping_old_lines() {
        let body: Vec<u8> = (0..200).flat_map(|i| format!("line {i}\r\n").into_bytes()).collect();
        let out = render_ended(&body, 0, &[mark(0, 20, 5, false)], (80, 24), Some(200)).unwrap();
        assert!(out.len() <= 200, "{}", out.len());
        assert!(String::from_utf8_lossy(&out).contains("line 199"));
    }

    #[test]
    fn live_replay_restores_screen_cursor_and_the_normal_screen_behind() {
        let mut live = vt100::Parser::new_with_callbacks(4, 20, 100, TitleCb::default());
        live.process(b"one\r\ntwo\r\nth");
        let out = live_normal(&mut live, None);
        let mut p = vt100::Parser::new(4, 20, 100);
        p.process(&out);
        p.process(b"ree");
        assert_eq!(p.screen().rows(0, 20).take(3).collect::<Vec<_>>(), ["one", "two", "three"]);
        // In the alternate screen: the view now, and the shell behind it once the program leaves.
        let body = b"$ app\r\n\x1b[?1049h\x1b[?1004h\x1b[2;1Hview";
        let mut live = vt100::Parser::new_with_callbacks(4, 20, 100, TitleCb::default());
        live.process(body);
        let mut out = behind(body, 0, &[mark(0, 20, 4, false)], (80, 24), None);
        out.extend(live_alt(&live));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?1004h"), "{s:?}");
        let mut p = vt100::Parser::new(4, 20, 100);
        p.process(&out);
        assert!(p.screen().alternate_screen());
        assert_eq!(p.screen().rows(0, 20).nth(1).unwrap(), "view");
        p.process(b"\x1b[?1049l");
        assert_eq!(p.screen().rows(0, 20).next().unwrap(), "$ app");
    }

    #[test]
    fn raw_is_faithful_only_from_the_start_at_one_size() {
        assert!(raw_is_faithful(0, &[mark(0, 80, 24, false), mark(9, 80, 24, true)]));
        assert!(!raw_is_faithful(5, &[mark(0, 80, 24, false)]));
        assert!(!raw_is_faithful(0, &[mark(0, 80, 24, false), mark(9, 100, 24, false)]));
    }
}

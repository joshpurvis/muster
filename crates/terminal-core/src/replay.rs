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
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::pty::{TitleCb, clamp_size};

/// Replays are at most this many bytes unless the client asks for fewer (a host frame holds
/// 16 MiB). The oldest history goes first.
pub const REPLAY_BUDGET: usize = 2 * 1024 * 1024;
/// Cells a re-run's scrollback holds at most (8000 lines of a 250-column terminal), and lines.
const RERUN_CELLS: usize = 2_000_000;
const RERUN_LINES: usize = 10_000;
/// Most marks kept per terminal. Past this the oldest go, and a replay of the oldest output may
/// start at the wrong size.
pub(crate) const MAX_MARKS: usize = 4096;
/// Bytes per drawn line beyond its own (`draw`'s line break and attribute reset).
const LINE_OVERHEAD: usize = 5;

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

/// True when replaying the output raw, from a fresh terminal of the current size, shows what
/// the program drew: it is all the output since the start, written at one size.
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
/// repaints), so then, as with no switch at all, a long stretch of escape sequences without a
/// single line feed says full-screen program.
fn guess_starts_in_alt(body: &[u8]) -> bool {
    let before = match first_alt_switch(body) {
        Some((_, _, false)) => return true,
        Some((at, _, true)) => &body[..at],
        None => body,
    };
    before.len() > 4096 && !before.contains(&b'\n') && before.iter().filter(|&&c| c == 0x1b).count() >= 32
}

/// The first alternate-screen switch (`CSI ? 47/1047/1049 h|l`) in `data`: where its sequence
/// starts and ends, and whether it enters (true) or leaves the alternate screen.
pub(crate) fn first_alt_switch(data: &[u8]) -> Option<(usize, usize, bool)> {
    alt_switches(data).next()
}

fn alt_switches(data: &[u8]) -> impl Iterator<Item = (usize, usize, bool)> + '_ {
    csis(data).filter_map(|c| {
        (c.prefix == Some(b'?') && matches!(c.fin, b'h' | b'l') && c.params.iter().any(|p| matches!(p, 47 | 1047 | 1049)))
            .then_some((c.at, c.end, c.fin == b'h'))
    })
}

/// A complete control sequence (`CSI`), or a reset (`ESC c`: `csi` false).
struct Csi {
    at: usize,
    end: usize,
    csi: bool,
    prefix: Option<u8>,
    params: Vec<u16>,
    intermediate: Option<u8>,
    fin: u8,
}

/// The control sequences and resets in `data`, in order. One cut off at the end isn't returned.
fn csis(data: &[u8]) -> impl Iterator<Item = Csi> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 1 < data.len() {
            if data[i] != 0x1b {
                i += 1;
                continue;
            }
            let at = i;
            if data[i + 1] == b'c' {
                i += 2;
                return Some(Csi { at, end: i, csi: false, prefix: None, params: Vec::new(), intermediate: None, fin: b'c' });
            }
            if data[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let mut j = i + 2;
            let prefix = data.get(j).filter(|c| (0x3c..=0x3f).contains(*c)).copied();
            if prefix.is_some() {
                j += 1;
            }
            let mut params = Vec::new();
            let mut cur: Option<u16> = None;
            let mut intermediate = None;
            while j < data.len() {
                match data[j] {
                    d @ b'0'..=b'9' => cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add((d - b'0') as u16)),
                    b';' | b':' => params.push(cur.take().unwrap_or(0)),
                    c @ 0x20..=0x2f => intermediate = Some(c),
                    _ => break,
                }
                j += 1;
            }
            let &fin = data.get(j)?;
            i = j + 1;
            if !(0x40..=0x7e).contains(&fin) {
                continue;
            }
            if let Some(c) = cur {
                params.push(c);
            }
            return Some(Csi { at, end: i, csi: true, prefix, params, intermediate, fin });
        }
        None
    })
}

/// Feed `data` to `parser` the way xterm.js reads it, and track what vt100 doesn't. Unlike
/// vt100, xterm.js leaves the alternate screen alone when told to enter it while already
/// there (Claude Code does that on repaints). Returns the end of each switch that changed the
/// screen, with the screen after it.
pub(crate) fn process(parser: &mut vt100::Parser<TitleCb>, data: &[u8]) -> Vec<(usize, bool)> {
    let mut changes = Vec::new();
    let mut pos = 0;
    for c in csis(data) {
        let cb = parser.callbacks_mut();
        match (c.csi, c.prefix, c.intermediate, c.fin) {
            (false, ..) => cb.tracked = Default::default(),
            (true, Some(b'?'), None, b'h' | b'l') => {
                let on = c.fin == b'h';
                if c.params.contains(&1004) {
                    cb.tracked.focus_reporting = on;
                }
                if !c.params.iter().any(|p| matches!(p, 47 | 1047 | 1049)) {
                    continue;
                }
                parser.process(&data[pos..c.at]);
                pos = c.end;
                let was = parser.screen().alternate_screen();
                if on && was && c.params.len() == 1 {
                    // Already there: 1049 only saves the cursor.
                    if c.params == [1049] {
                        parser.process(b"\x1b7");
                    }
                } else {
                    parser.process(&data[c.at..c.end]);
                }
                let now = parser.screen().alternate_screen();
                if now != was {
                    changes.push((c.end, now));
                }
            }
            (true, None, Some(b' '), b'q') => {
                cb.tracked.cursor_style = c.params.first().copied().filter(|&p| p > 0);
            }
            (true, None, None, b'r') => {
                cb.tracked.margins = match c.params[..] {
                    [] | [0] | [0, 0] => None,
                    [top] => Some((top, 0)),
                    [top, bottom, ..] => Some((top, bottom)),
                };
            }
            _ => {}
        }
    }
    parser.process(&data[pos..]);
    changes
}

/// Where the output's unfinished part starts: an escape sequence or UTF-8 character it ends
/// in the middle of (the emulator holds that part until the rest arrives). `data.len()` if none.
pub(crate) fn unfinished_from(data: &[u8]) -> usize {
    let from = data.len().saturating_sub(4096);
    if let Some(i) = data[from..].iter().rposition(|&c| c == 0x1b).map(|p| p + from)
        && !escape_complete(&data[i..])
    {
        return i;
    }
    // A UTF-8 lead byte near the end without all its continuation bytes.
    for k in (data.len().saturating_sub(3)..data.len()).rev() {
        let b = data[k];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let need = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if data.len() - k < need { k } else { data.len() };
    }
    data.len()
}

/// `s` starts with ESC and has no other ESC: is the sequence it starts complete?
fn escape_complete(s: &[u8]) -> bool {
    let Some(&kind) = s.get(1) else { return false };
    match kind {
        b'[' => s[2..].iter().any(|c| !(0x20..=0x3f).contains(c)),
        // OSC, DCS, APC, PM, SOS: end at BEL (an ST would be the last ESC itself).
        b']' | b'P' | b'_' | b'^' | b'X' => s[2..].contains(&0x07),
        0x20..=0x2f => s[2..].iter().any(|c| !(0x20..=0x2f).contains(c)),
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing rows.

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    fn of(c: &vt100::Cell) -> Self {
        Self {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            dim: c.dim(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
        }
    }

    /// Visible even on an empty cell.
    fn shows_blank(&self) -> bool {
        self.bg != vt100::Color::Default || self.inverse || self.underline
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (on, code) in [(self.bold, "1"), (self.dim, "2"), (self.italic, "3"), (self.underline, "4"), (self.inverse, "7")] {
            if on {
                out.push(b';');
                out.extend_from_slice(code.as_bytes());
            }
        }
        for (color, base, bright, ext) in [(self.fg, 30, 90, 38), (self.bg, 40, 100, 48)] {
            let s = match color {
                vt100::Color::Default => continue,
                vt100::Color::Idx(i) if i < 8 => format!(";{}", base + i as u16),
                vt100::Color::Idx(i) if i < 16 => format!(";{}", bright + i as u16 - 8),
                vt100::Color::Idx(i) => format!(";{ext};5;{i}"),
                vt100::Color::Rgb(r, g, b) => format!(";{ext};2;{r};{g};{b}"),
            };
            out.extend_from_slice(s.as_bytes());
        }
        out.push(b'm');
    }
}

/// Row `row` of the screen's current view, drawn from column 0 with printable characters only
/// (gaps as spaces), so it also reads right wrapped in a narrower terminal.
fn row_bytes(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<u8> {
    let shows = |col: u16| screen.cell(row, col).is_some_and(|c| c.has_contents() || Style::of(c).shows_blank());
    let Some(last) = (0..cols).rev().find(|&c| shows(c)) else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = Style::default();
    for col in 0..=last {
        let Some(cell) = screen.cell(row, col) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        let style = Style::of(cell);
        if style != cur {
            style.write(&mut out);
            cur = style;
        }
        out.extend_from_slice(if cell.has_contents() { cell.contents().as_bytes() } else { b" " });
    }
    out
}

fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

/// The active screen's scrollback, oldest first: as much of the newest as fits `budget`
/// bytes once drawn (taken from it).
fn scrollback_lines(screen: &mut vt100::Screen, budget: &mut usize) -> Vec<Vec<u8>> {
    let (rows, cols) = screen.size();
    let rows = rows as usize;
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let sb = screen.scrollback();
    let mut newest_first = Vec::new();
    // Line `idx - 1` is the newest one left. A view at offset `o` shows lines from `sb - o`.
    let mut idx = sb;
    'all: while idx > 0 {
        let o = (sb + rows - idx).min(sb);
        screen.set_scrollback(o);
        let first = sb - o;
        for line in (first..idx).rev() {
            let b = row_bytes(screen, (line - first) as u16, cols);
            if b.len() + LINE_OVERHEAD > *budget {
                break 'all;
            }
            *budget -= b.len() + LINE_OVERHEAD;
            newest_first.push(b);
        }
        idx = first;
    }
    screen.set_scrollback(prev);
    newest_first.reverse();
    newest_first
}

/// The rows of the active screen as it is now.
fn visible_rows(screen: &mut vt100::Screen) -> Vec<Vec<u8>> {
    let prev = screen.scrollback();
    screen.set_scrollback(0);
    let (rows, cols) = screen.size();
    let lines = (0..rows).map(|r| row_bytes(screen, r, cols)).collect();
    screen.set_scrollback(prev);
    lines
}

fn trim_blank_tail(lines: &mut Vec<Vec<u8>>) {
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
}

fn drawn_len(lines: &[Vec<u8>]) -> usize {
    lines.iter().map(|l| l.len() + LINE_OVERHEAD).sum()
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

// ---------------------------------------------------------------------------------------------
// Re-running output.

/// `body` (raw output from offset `start`) run through an emulator as the terminal saw it.
pub(crate) struct Rerun {
    pub parser: vt100::Parser<TitleCb>,
    /// Normal-screen history from before the terminal last got narrower, oldest first, drawn
    /// at the width it had (vt100 cuts rows to a smaller width; xterm.js rewraps them).
    frozen: VecDeque<Vec<u8>>,
    frozen_bytes: usize,
    /// The program's last full-screen view, if it left the alternate screen in `body`, and
    /// where it goes among the normal screen's lines: before the line this many from the end.
    left_alt: Option<(Vec<Vec<u8>>, usize)>,
}

enum Cut {
    Resize(u16, u16),
    /// The last switch that leaves the alternate screen; it ends at this index.
    Leave(usize),
}

fn scrollback_cap(cols: u16) -> usize {
    (RERUN_CELLS / cols.max(1) as usize).clamp(500, RERUN_LINES)
}

fn rerun_parser(rows: u16, cols: u16, cb: TitleCb) -> vt100::Parser<TitleCb> {
    vt100::Parser::new_with_callbacks(rows, cols, scrollback_cap(cols), cb)
}

impl Rerun {
    /// Normal-screen lines in the scrollback, frozen ones included, and whether the
    /// scrollback is full (so stopped growing).
    fn history_len(&mut self) -> (usize, bool) {
        let alt = self.parser.screen().alternate_screen();
        if alt {
            self.parser.process(b"\x1b[?47l");
        }
        let sb = scrollback_len(self.parser.screen_mut());
        if alt {
            self.parser.process(b"\x1b[?47h");
        }
        let (_, cols) = self.parser.screen().size();
        (self.frozen.len() + sb, sb >= scrollback_cap(cols))
    }

    /// The normal screen's history (frozen lines, then the scrollback), oldest first: as much
    /// of the newest as fits `budget`. Call on the normal screen.
    fn history(&mut self, budget: &mut usize) -> Vec<Vec<u8>> {
        let sb = scrollback_lines(self.parser.screen_mut(), budget);
        let mut frozen = Vec::new();
        for line in self.frozen.iter().rev() {
            if line.len() + LINE_OVERHEAD > *budget {
                break;
            }
            *budget -= line.len() + LINE_OVERHEAD;
            frozen.push(line.clone());
        }
        frozen.reverse();
        frozen.extend(sb);
        frozen
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = clamp_size(cols, rows);
        let (old_rows, old_cols) = self.parser.screen().size();
        // xterm.js resets the scroll margins on a resize.
        self.parser.callbacks_mut().tracked.margins = None;
        if cols >= old_cols {
            self.parser.screen_mut().set_size(rows, cols);
            return;
        }
        // Getting narrower: keep the history as drawn so far, and go on in a fresh emulator
        // that holds the screen as it is (cut to the new width, as a resize does).
        let alt = self.parser.screen().alternate_screen();
        if alt {
            self.parser.process(b"\x1b[?47l");
        }
        let mut budget = REPLAY_BUDGET;
        for line in scrollback_lines(self.parser.screen_mut(), &mut budget) {
            self.frozen_bytes += line.len() + LINE_OVERHEAD;
            self.frozen.push_back(line);
        }
        while self.frozen_bytes > REPLAY_BUDGET
            && let Some(old) = self.frozen.pop_front()
        {
            self.frozen_bytes -= old.len() + LINE_OVERHEAD;
        }
        let cb = std::mem::take(self.parser.callbacks_mut());
        let mut next = rerun_parser(old_rows, old_cols, cb);
        next.process(&self.parser.screen().contents_formatted());
        next.process(&self.parser.screen().attributes_formatted());
        if alt {
            self.parser.process(b"\x1b[?47h");
            next.process(b"\x1b[?1049h");
            next.process(&self.parser.screen().contents_formatted());
            next.process(&self.parser.screen().attributes_formatted());
        }
        next.process(&self.parser.screen().input_mode_formatted());
        next.screen_mut().set_size(rows, cols);
        self.parser = next;
    }
}

/// Re-run `body` from the size and screen at `start`, resizing where the marks say. None if
/// the emulator panicked.
pub(crate) fn rerun(body: &[u8], start: u64, marks: &[Mark], fallback: (u16, u16)) -> Option<Rerun> {
    let (cols, rows, alt) = start_state(body, start, marks, fallback);
    let (cols, rows) = clamp_size(cols, rows);
    let mut run = Rerun { parser: rerun_parser(rows, cols, TitleCb::default()), frozen: VecDeque::new(), frozen_bytes: 0, left_alt: None };
    if alt {
        run.parser.process(b"\x1b[?1049h");
    }
    let end = start + body.len() as u64;
    let mut cuts: Vec<(usize, Cut)> = marks
        .iter()
        .filter(|m| m.offset > start && m.offset < end)
        .map(|m| ((m.offset - start) as usize, Cut::Resize(m.cols, m.rows)))
        .collect();
    let last_leave = alt_switches(body).filter(|s| !s.2).last();
    if let Some((at, until, _)) = last_leave {
        cuts.push((at, Cut::Leave(until)));
        cuts.sort_by_key(|c| c.0);
    }
    // The view, the lines below it then, the history then.
    let mut left: Option<(Vec<Vec<u8>>, usize, usize)> = None;
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pos = 0;
        for (at, cut) in cuts {
            // A mark inside the switch sequence just handled applies right after it.
            let at = at.max(pos);
            process(&mut run.parser, &body[pos..at]);
            pos = at;
            match cut {
                Cut::Resize(cols, rows) => run.resize(cols, rows),
                Cut::Leave(until) => {
                    let was_alt = run.parser.screen().alternate_screen();
                    let view = was_alt.then(|| visible_rows(run.parser.screen_mut()));
                    process(&mut run.parser, &body[pos..until]);
                    pos = until;
                    if let Some(mut view) = view
                        && !run.parser.screen().alternate_screen()
                    {
                        trim_blank_tail(&mut view);
                        // The view goes where the program was started: at the cursor the
                        // normal screen gets back.
                        let (rows, _) = run.parser.screen().size();
                        let below = rows.saturating_sub(run.parser.screen().cursor_position().0) as usize;
                        left = Some((view, below, run.history_len().0));
                    }
                }
            }
        }
        process(&mut run.parser, &body[pos..]);
        // End any sequence the output was cut off in.
        run.parser.process(b"\x18");
    }));
    if ran.is_err() {
        return None;
    }
    if let Some((view, below, then)) = left {
        // Lines that went into the history since are below the view too. A full scrollback
        // stops growing; then count line feeds, which scroll once the cursor is at the bottom.
        let (now, full) = run.history_len();
        let gained = if full {
            let after = last_leave.map_or(&body[..0], |(_, until, _)| &body[until..]);
            after.iter().filter(|&&c| c == b'\n').count().saturating_sub(below.saturating_sub(1))
        } else {
            now.saturating_sub(then)
        };
        run.left_alt = Some((view, below + gained));
    }
    Some(run)
}

// ---------------------------------------------------------------------------------------------
// Replays.

/// Modes a client gets back after the screen: the input modes, focus reporting, cursor style.
fn modes(parser: &vt100::Parser<TitleCb>) -> Vec<u8> {
    let mut v = parser.screen().input_mode_formatted();
    let t = &parser.callbacks().tracked;
    if t.focus_reporting {
        v.extend_from_slice(b"\x1b[?1004h");
    }
    if let Some(style) = t.cursor_style {
        v.extend_from_slice(format!("\x1b[{style} q").as_bytes());
    }
    v
}

fn margins(m: Option<(u16, u16)>) -> Vec<u8> {
    match m {
        Some((top, 0)) => format!("\x1b[{top}r").into_bytes(),
        Some((top, bottom)) => format!("\x1b[{top};{bottom}r").into_bytes(),
        None => Vec::new(),
    }
}

/// The screen of a live terminal, taken under its lock: only what is on screen, the cursor
/// and modes (cheap); the history comes later from a re-run of the ring.
pub(crate) struct LiveScreen {
    rows: Vec<Vec<u8>>,
    size: (u16, u16),
    cursor: (u16, u16),
    /// What to print to put the cursor back in the pending-wrap state at the end of its row.
    wrap_cell: Option<Vec<u8>>,
    attrs: Vec<u8>,
    hide_cursor: bool,
    margins: Option<(u16, u16)>,
    /// The alternate screen in use, drawn whole (`contents_formatted`), if any.
    alt: Option<Vec<u8>>,
    modes: Vec<u8>,
}

impl LiveScreen {
    pub(crate) fn capture(live: &mut vt100::Parser<TitleCb>) -> Self {
        let margins = live.callbacks().tracked.margins;
        let modes = modes(live);
        let screen = live.screen_mut();
        let (rows, cols) = screen.size();
        let mut s = Self { rows: Vec::new(), size: (cols, rows), cursor: (0, 0), wrap_cell: None, attrs: Vec::new(), hide_cursor: false, margins, alt: None, modes };
        if screen.alternate_screen() {
            s.alt = Some(screen.contents_formatted());
        } else {
            s.cursor = screen.cursor_position();
            s.rows = visible_rows(screen);
            s.wrap_cell = wrap_cell(screen, s.cursor);
            s.attrs = screen.attributes_formatted();
            s.hide_cursor = screen.hide_cursor();
        }
        s
    }

    fn fixed_len(&self) -> usize {
        drawn_len(&self.rows) + self.alt.as_ref().map_or(0, Vec::len) + self.modes.len() + 64
    }
}

/// For a cursor past the last column (vt100's pending wrap): the last cell, to print again.
fn wrap_cell(screen: &vt100::Screen, (row, col): (u16, u16)) -> Option<Vec<u8>> {
    let (_, cols) = screen.size();
    if col < cols {
        return None;
    }
    let mut at = cols - 1;
    if at > 0 && screen.cell(row, at).is_some_and(|c| c.is_wide_continuation()) {
        at -= 1;
    }
    let cell = screen.cell(row, at)?;
    let mut out = format!("\x1b[{}G", at + 1).into_bytes();
    Style::of(cell).write(&mut out);
    out.extend_from_slice(if cell.has_contents() { cell.contents().as_bytes() } else { b" " });
    Some(out)
}

/// Put the cursor at `(row, col)` of a `rows`-high screen just drawn down to its last row:
/// counted up from there (the client may be another height), or absolutely within margins.
fn place_cursor(out: &mut Vec<u8>, rows: u16, (row, col): (u16, u16), wrap: Option<&[u8]>, absolute: bool) {
    if absolute {
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
    } else {
        out.push(b'\r');
        let up = rows.saturating_sub(1).saturating_sub(row);
        if up > 0 {
            out.extend_from_slice(format!("\x1b[{up}A").as_bytes());
        }
    }
    match wrap {
        Some(cell) => out.extend_from_slice(cell),
        None if col > 0 => out.extend_from_slice(format!("\x1b[{}G", col + 1).as_bytes()),
        None => {}
    }
}

/// Replay of a live terminal that raw output can't show (see the module docs), at most `max`
/// (or `REPLAY_BUDGET`) bytes: its history from a re-run of `body` (the ring, from offset
/// `start`), then its screen as `live` holds it. On the alternate screen the normal screen
/// behind it comes from the re-run too, so it is there when the program leaves. The output's
/// unfinished end goes last, for the client to read on into the next live bytes.
pub(crate) fn render_live(live: LiveScreen, body: &[u8], start: u64, marks: &[Mark], max: Option<usize>) -> Vec<u8> {
    let max = max.unwrap_or(REPLAY_BUDGET).min(REPLAY_BUDGET);
    let unfinished = &body[unfinished_from(body)..];
    let mut budget = max.saturating_sub(live.fixed_len() + unfinished.len());
    let mut run = rerun(body, start, marks, live.size);
    if let Some(run) = run.as_mut()
        && run.parser.screen().alternate_screen()
    {
        run.parser.process(b"\x1b[?47l");
    }
    let mut out = Vec::new();
    match live.alt {
        None => {
            let mut lines = run.as_mut().map(|r| r.history(&mut budget)).unwrap_or_default();
            lines.extend(live.rows);
            draw(&mut out, &lines);
            out.extend_from_slice(&margins(live.margins));
            place_cursor(&mut out, live.size.1, live.cursor, live.wrap_cell.as_deref(), live.margins.is_some());
            out.extend_from_slice(&live.attrs);
            if live.hide_cursor {
                out.extend_from_slice(b"\x1b[?25l");
            }
        }
        Some(alt) => {
            if let Some(run) = run.as_mut() {
                let screen = run.parser.screen_mut();
                let (rows, _) = screen.size();
                let cursor = screen.cursor_position();
                let wrap = wrap_cell(screen, cursor);
                let mut shown = visible_rows(screen);
                budget = budget.saturating_sub(drawn_len(&shown));
                let mut lines = run.history(&mut budget);
                lines.append(&mut shown);
                draw(&mut out, &lines);
                place_cursor(&mut out, rows, cursor, wrap.as_deref(), false);
            }
            out.extend_from_slice(b"\x1b[?1049h");
            // Margins first: setting them homes the cursor, which the screen then places.
            out.extend_from_slice(&margins(live.margins));
            out.extend_from_slice(&alt);
        }
    }
    out.extend_from_slice(&live.modes);
    out.extend_from_slice(unfinished);
    out
}

/// Replay of a terminal whose program has ended, at most `REPLAY_BUDGET` bytes: everything as
/// plain lines on the normal screen, so it all scrolls. A full-screen program's last view stays
/// where it ran, as it would in a terminal without an alternate screen; after a full-screen
/// program that was still running, it comes last. None if the emulator panicked.
/// `trim_replay` cuts it to a client's limit.
pub fn render_ended(body: &[u8], start: u64, marks: &[Mark], fallback: (u16, u16)) -> Option<Vec<u8>> {
    let mut run = rerun(body, start, marks, fallback)?;
    let mut tail = Vec::new();
    if run.parser.screen().alternate_screen() {
        tail = visible_rows(run.parser.screen_mut());
        trim_blank_tail(&mut tail);
        run.parser.process(b"\x1b[?47l");
    }
    let mut shown = visible_rows(run.parser.screen_mut());
    let view = run.left_alt.take();
    let fixed = drawn_len(&shown) + drawn_len(&tail) + view.as_ref().map_or(0, |v| drawn_len(&v.0));
    let mut budget = REPLAY_BUDGET.saturating_sub(fixed);
    let mut lines = run.history(&mut budget);
    lines.append(&mut shown);
    if let Some((view, below)) = view {
        let at = lines.len().saturating_sub(below);
        lines.splice(at..at, view);
    }
    trim_blank_tail(&mut lines);
    lines.extend(tail);
    let mut out = Vec::new();
    draw(&mut out, &lines);
    Some(out)
}

/// An ended terminal's replay (`render_ended`) cut to at most `max` bytes, dropping whole lines
/// from the front (each starts from default attributes). Keeps at least the last line.
pub fn trim_replay(replay: &[u8], max: Option<usize>) -> Vec<u8> {
    let Some(max) = max.filter(|&m| replay.len() > m) else { return replay.to_vec() };
    let from = replay.len() - max;
    let cut = replay[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| from + p + 2)
        .or_else(|| replay.windows(2).rposition(|w| w == b"\r\n").map(|p| p + 2))
        .unwrap_or(0);
    replay[cut..].to_vec()
}

/// The marks file next to an output.log.
pub fn marks_path(log: &Path) -> PathBuf {
    log.with_extension("marks.json")
}

/// What output.log's marks file holds: the raw output offset of the log's first byte, the
/// log's inode (a rewrite renames a new file into place, so a marks file left from an earlier
/// log names another inode), and the marks.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct LogMarks {
    pub start: u64,
    pub inode: u64,
    pub marks: Vec<Mark>,
}

impl LogMarks {
    pub fn inode(log: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(log).ok().map(|m| m.ino())
    }

    /// The marks file for `log`, if it was written for the log as it is now.
    pub fn read(log: &Path) -> Option<Self> {
        let m: Self = serde_json::from_slice(&std::fs::read(marks_path(log)).ok()?).ok()?;
        (Self::inode(log) == Some(m.inode)).then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(replay: &[u8], cols: u16, rows: u16) -> (bool, Vec<String>) {
        let mut p = vt100::Parser::new(rows, cols, 1000);
        p.process(replay);
        let alt = p.screen().alternate_screen();
        (alt, p.screen().rows(0, cols).map(|r| r.trim_end().to_string()).collect())
    }

    fn mark(offset: u64, cols: u16, rows: u16, alt: bool) -> Mark {
        Mark { offset, cols, rows, alternate_screen: alt }
    }

    fn live(data: &[u8], cols: u16, rows: u16) -> vt100::Parser<TitleCb> {
        let mut p = vt100::Parser::new_with_callbacks(rows, cols, 100, TitleCb::default());
        process(&mut p, data);
        p
    }

    #[test]
    fn alt_switches_find_enters_and_leaves() {
        assert_eq!(first_alt_switch(b"x\x1b[?25;1049hy"), Some((1, 12, true)));
        assert_eq!(first_alt_switch(b"x\x1b[?25h\x1b[?47l"), Some((7, 13, false)));
        assert_eq!(first_alt_switch(b"\x1b[?2004h"), None);
    }

    #[test]
    fn guesses_alt_start_without_marks() {
        assert!(guess_starts_in_alt(b"app\x1b[?1049lshell\r\n"));
        assert!(!guess_starts_in_alt(b"shell\r\n\x1b[?1049happ"));
        // A repaint that enters again after a long stretch of positioned output (Claude Code).
        let mut body = b"\x1b[5;3Hx".repeat(1000);
        body.extend_from_slice(b"\x1b[?1049h\x1b[2Jframe");
        assert!(guess_starts_in_alt(&body));
        assert!(!guess_starts_in_alt(b"short\x1b[?1049h"));
        // Long printable output without line feeds (base64) is not a full-screen program.
        assert!(!guess_starts_in_alt(&b"QUJD".repeat(2000)));
    }

    #[test]
    fn process_keeps_the_alt_screen_on_a_repeated_enter_and_tracks_modes() {
        let mut p = live(b"\x1b[?1049h\x1b[1;1Htop\x1b[?1049h\x1b[2;1Hbottom\x1b[?1004h\x1b[3 q\x1b[2;5r", 20, 4);
        assert_eq!(p.screen().rows(0, 20).take(2).collect::<Vec<_>>(), ["top", "bottom"]);
        let t = &p.callbacks().tracked;
        assert!(t.focus_reporting);
        assert_eq!((t.cursor_style, t.margins), (Some(3), Some((2, 5))));
        process(&mut p, b"\x1bc");
        assert!(!p.callbacks().tracked.focus_reporting);
        let changes = process(&mut p, b"ab\x1b[?1049hcd\x1b[?1049l");
        assert_eq!(changes, [(10, true), (20, false)]);
    }

    #[test]
    fn unfinished_output_is_found() {
        assert_eq!(unfinished_from(b"done\x1b[31"), 4);
        assert_eq!(unfinished_from(b"x\x1b"), 1);
        assert_eq!(unfinished_from(b"x\x1b[31m"), 6);
        assert_eq!(unfinished_from(b"\x1b]0;title"), 0);
        assert_eq!(unfinished_from(b"\x1b]0;title\x1b\\"), 11);
        assert_eq!(unfinished_from("é".as_bytes()), 2);
        assert_eq!(unfinished_from(&"é".as_bytes()[..1]), 0);
    }

    #[test]
    fn rows_draw_gaps_as_spaces_and_keep_colors() {
        let p = live(b"left\x1b[1;17H\x1b[31mred", 20, 2);
        let s = String::from_utf8(row_bytes(p.screen(), 0, 20)).unwrap();
        assert!(s.starts_with("left            \x1b[0;31mred"), "{s:?}");
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
    fn narrowing_keeps_history_whole() {
        let before = b"abcdefghijklmnopqrst\r\n1\r\n2\r\n3\r\n";
        let body = [&before[..], b"$ "].concat();
        let marks = [mark(0, 20, 3, false), mark(before.len() as u64, 10, 3, false)];
        let out = render_ended(&body, 0, &marks, (80, 24)).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("abcdefghijklmnopqrst"), "{out:?}");
    }

    #[test]
    fn ended_full_screen_program_shows_its_last_view_then_what_followed() {
        let body = b"\x1b[1;1Hold frame\x1b[?1049h\x1b[2J\x1b[1;1Hlast frame\x1b[2;1Hstatus\x1b[?1049l\r\nResume with: x\r\n";
        let out = render_ended(body, 1000, &[mark(0, 30, 4, true)], (80, 24)).unwrap();
        let (alt, rows) = screen_text(&out, 30, 6);
        assert!(!alt);
        assert_eq!(rows[..4], ["last frame", "status", "", "Resume with: x"]);
        assert!(!String::from_utf8_lossy(&out).contains("old frame"));
    }

    #[test]
    fn ended_shell_keeps_view_where_the_program_ran() {
        let body = b"$ vim\r\n\x1b[?1049h\x1b[Hfile text\x1b[?1049l$ ls\r\na b\r\n$ ";
        let out = render_ended(body, 0, &[mark(0, 20, 5, false)], (80, 24)).unwrap();
        let (_, rows) = screen_text(&out, 20, 8);
        assert_eq!(rows[..5], ["$ vim", "file text", "$ ls", "a b", "$"]);
    }

    #[test]
    fn ended_in_alt_screen_shows_normal_screen_then_the_view() {
        let body = b"$ top\r\n\x1b[?1049h\x1b[Hcpu 3%";
        let out = render_ended(body, 0, &[mark(0, 20, 5, false)], (80, 24)).unwrap();
        let (alt, rows) = screen_text(&out, 20, 5);
        assert!(!alt);
        assert_eq!(rows[..2], ["$ top", "cpu 3%"]);
    }

    #[test]
    fn trimmed_replay_drops_whole_old_lines() {
        let body: Vec<u8> = (0..200).flat_map(|i| format!("line {i}\r\n").into_bytes()).collect();
        let out = render_ended(&body, 0, &[mark(0, 20, 5, false)], (80, 24)).unwrap();
        let short = trim_replay(&out, Some(200));
        assert!(short.len() <= 200, "{}", short.len());
        assert!(short.starts_with(b"line "));
        assert!(String::from_utf8_lossy(&short).contains("line 199"));
    }

    #[test]
    fn live_replay_restores_screen_cursor_and_the_normal_screen_behind() {
        let body = b"one\r\ntwo\r\nth\x1b[3";
        let mut p = live(body, 20, 4);
        let out = render_live(LiveScreen::capture(&mut p), body, 0, &[mark(0, 20, 4, false)], None);
        let mut c = vt100::Parser::new(4, 20, 100);
        c.process(&out);
        // The client reads on from the unfinished sequence.
        c.process(b"1mree");
        assert_eq!(c.screen().rows(0, 20).take(3).collect::<Vec<_>>(), ["one", "two", "three"]);
        assert_eq!(c.screen().cell(2, 2).unwrap().fgcolor(), vt100::Color::Idx(1));
        // In the alternate screen: the view now, and the shell behind it once the program leaves.
        let body = b"$ app\r\n\x1b[?1049h\x1b[?1004h\x1b[2;1Hview";
        let mut p = live(body, 20, 4);
        let out = render_live(LiveScreen::capture(&mut p), body, 0, &[mark(0, 20, 4, false)], None);
        assert!(String::from_utf8_lossy(&out).contains("\x1b[?1004h"));
        let mut c = vt100::Parser::new(4, 20, 100);
        c.process(&out);
        assert!(c.screen().alternate_screen());
        assert_eq!(c.screen().rows(0, 20).nth(1).unwrap(), "view");
        c.process(b"\x1b[?1049l");
        assert_eq!(c.screen().rows(0, 20).next().unwrap(), "$ app");
    }

    #[test]
    fn live_replay_keeps_a_pending_wrap() {
        let body = b"abcdefghij";
        let mut p = live(body, 10, 3);
        let out = render_live(LiveScreen::capture(&mut p), body, 5, &[mark(0, 10, 3, false)], None);
        let mut c = vt100::Parser::new(3, 10, 100);
        c.process(&out);
        c.process(b"k");
        assert_eq!(c.screen().rows(0, 10).take(2).collect::<Vec<_>>(), ["abcdefghij", "k"]);
    }

    #[test]
    fn live_replay_fits_its_budget() {
        let body: Vec<u8> = (0..300).flat_map(|i| format!("line {i}\r\n").into_bytes()).collect();
        let mut p = live(&body, 20, 5);
        let out = render_live(LiveScreen::capture(&mut p), &body, 0, &[mark(0, 20, 5, false)], Some(300));
        assert!(out.len() <= 300, "{}", out.len());
        assert!(String::from_utf8_lossy(&out).contains("line 299"));
    }

    #[test]
    fn raw_is_faithful_only_from_the_start_at_one_size() {
        assert!(raw_is_faithful(0, &[mark(0, 80, 24, false), mark(9, 80, 24, true)]));
        assert!(!raw_is_faithful(5, &[mark(0, 80, 24, false)]));
        assert!(!raw_is_faithful(0, &[mark(0, 80, 24, false), mark(9, 100, 24, false)]));
    }
}

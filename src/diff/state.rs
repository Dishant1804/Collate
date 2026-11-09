//! What a diff view shows and where: the rows files, hunks and expanded ranges make, the
//! selection, the side text selection copies, and the reading position.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rune::{Key, KeyEvent, Modifier, Tx};

use super::model::{FileDiff, LineKind, Side};
use rune_kit::syntax::{Highlight, Highlighter};

/// How a diff lays out its two sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DiffMode {
    /// One column, removed lines above the added lines that replace them.
    #[default]
    Unified,
    /// Old on the left, new on the right, changed lines side by side. An added or deleted
    /// file shows its one side as a single column.
    Split,
}

/// A line of one side of a file: what selections point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRef {
    pub side: Side,
    /// 1-based.
    pub line: u32,
}

impl LineRef {
    pub fn new(side: Side, line: u32) -> LineRef {
        LineRef { side, line }
    }
}

/// Selected lines of one file, from where the selection started to where it ends (either
/// way round), as a click, shift-click or drag down the line numbers makes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub file: usize,
    pub start: LineRef,
    pub end: LineRef,
}

impl LineRange {
    /// One line.
    pub fn line(file: usize, at: LineRef) -> LineRange {
        LineRange { file, start: at, end: at }
    }
}

/// Which way an unchanged range between hunks opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expand {
    /// Twenty lines up from the hunk below it.
    Up,
    /// Twenty lines down from the hunk above it.
    Down,
    /// All of it.
    All,
}

/// Lines one expand step reveals.
pub const EXPAND_STEP: u32 = 20;

/// The spacer after the last row that lets the last file scroll to the top.
pub(crate) const TAIL_KEY: &str = "diff-tail";

/// Row heights in points. Lines are fixed unless they wrap.
/// How lines look, from the app's settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub font_size: f32,
    /// A line's height; the virtual list estimates rows with it.
    pub line_h: f32,
    /// Unified rows show one line number (the new side's, the old one's for a removal)
    /// instead of both.
    pub one_number: bool,
    /// A `+`/`−` column beside the numbers.
    pub markers: bool,
    /// Tint the changed words inside changed lines.
    pub word_diff: bool,
}

impl Default for Style {
    fn default() -> Style {
        Style { font_size: 12.5, line_h: 20.0, one_number: true, markers: true, word_diff: true }
    }
}

pub(crate) const HEADER_H: f32 = 44.0;
pub(crate) const GAP_H: f32 = 32.0;
pub(crate) const NOTE_H: f32 = 40.0;

/// One row of the virtual list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// A file's header.
    File(u32),
    /// A file with no lines to show: binary, or only renamed or re-moded.
    Note(u32),
    /// The unchanged range before hunk `gap` (the last one: after the last hunk), with its
    /// expand buttons and the hunk's header.
    Gap { f: u32, gap: u32 },
    /// An unchanged line revealed from a range, by its old and new numbers.
    Ctx { f: u32, old: u32, new: u32 },
    /// Line `i` of hunk `h`. In split mode, one without a partner on the other side.
    Line { f: u32, h: u32, i: u32 },
    /// Split mode: removed line `del` beside added line `add`, both of hunk `h`.
    Pair { f: u32, h: u32, del: u32, add: u32 },
}

impl Row {
    pub(crate) fn file(self) -> usize {
        match self {
            Row::File(f) | Row::Note(f) | Row::Gap { f, .. } | Row::Ctx { f, .. } | Row::Line { f, .. } | Row::Pair { f, .. } => f as usize,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FileUi {
    collapsed: bool,
    /// Lines revealed at the top and bottom of each unchanged range (`hunks.len() + 1` of them).
    revealed: Vec<(u32, u32)>,
}

/// An unchanged range before a hunk: its first old and new line numbers and its length
/// (`None` when unknown: after the last hunk without the whole text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Gap {
    pub(crate) old: u32,
    pub(crate) new: u32,
    pub(crate) len: Option<u32>,
}

/// Highlights of 128 lines of one side, by line number.
struct Chunk {
    /// Byte offset of each line in the chunk's text, and one past the last.
    starts: Vec<u32>,
    hl: Rc<[Highlight]>,
}

const CHUNK: u32 = 128;
/// Blocks kept coloured: a few screens either way of where the reader is. Colouring one again
/// takes about a tenth of a millisecond; keeping 512 held 25 MB more on a
/// 60,000-line diff, 64 about 5 MB.
const CHUNKS_KEPT: usize = 64;

/// A diff and everything about how it is shown: the app keeps one in its state and
/// [`DiffView`](super::DiffView) draws it.
pub struct DiffState {
    files: Vec<FileDiff>,
    ui: Vec<FileUi>,
    mode: DiffMode,
    wrap: bool,
    selection: Option<LineRange>,
    selecting: bool,
    selection_at_press: Option<LineRange>,
    /// The press was on the one selected line: it clears the selection unless it drags.
    unselect: bool,
    shift: bool,
    /// The side text selection copies, which follows the pointer.
    copy_side: Side,
    hovered: Vec<(Side, String)>,
    highlighter: Highlighter,
    style: Style,
    search: Option<String>,
    search_rev: u64,
    chunks: RefCell<HashMap<(u32, Side, u32), Rc<Chunk>>>,
    /// Per file, lines added and removed: counted once, read by every frame's headers.
    stats: Vec<(usize, usize)>,
    /// Per file, per hunk, per side: the index in the hunk of each of its lines on that side.
    side_lines: Vec<Vec<[Vec<u32>; 2]>>,
    gaps: Vec<Vec<Gap>>,
    /// Per file, the digits its line numbers take (at least three).
    digits: Vec<u32>,
    /// The widest line of any file, in columns.
    columns: u32,
    pub(crate) rows: Vec<Row>,
    /// The key of the row at the top of the view, taken before the files are replaced, so the
    /// rebuilt list can find its way back to it.
    top_key: Option<String>,
    /// The selection's rows, worked out once per selection and rebuild: every visible row
    /// asks whether it's selected.
    sel_rows: std::cell::Cell<Option<SelRows>>,
    file_rows: Vec<u32>,
    hunk_rows: Vec<u32>,
    /// Bumped whenever the rows change, so built rows rebuild.
    pub(crate) rev: u64,
    /// Changes with `keys`, unique across states so a swapped-in state never matches.
    pub(crate) keys_rev: u64,
    visible_rows: std::ops::Range<usize>,
    view_w: std::cell::Cell<f32>,
    visible: usize,
    /// The visible file last reported to the app.
    reported: Option<usize>,
    nav: Option<u32>,
    /// The visible range first reported after navigating to a row, while it holds.
    nav_range: Option<(u32, std::ops::Range<usize>)>,
}

/// A selection, the revision it was looked up at, and the rows it covers then.
type SelRows = (LineRange, u64, Option<(u32, u32)>);

impl std::fmt::Debug for DiffState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffState").field("files", &self.files.len()).field("rows", &self.rows.len()).field("mode", &self.mode).finish()
    }
}

impl Default for DiffState {
    fn default() -> DiffState {
        DiffState::new(Vec::new())
    }
}

impl DiffState {
    pub fn new(files: Vec<FileDiff>) -> DiffState {
        let mut s = DiffState {
            files: Vec::new(),
            ui: Vec::new(),
            mode: DiffMode::Unified,
            wrap: false,
            selection: None,
            selecting: false,
            selection_at_press: None,
            unselect: false,
            shift: false,
            copy_side: Side::New,
            hovered: Vec::new(),
            highlighter: Highlighter::builtin(),
            style: Style::default(),
            search: None,
            search_rev: 0,
            chunks: RefCell::new(HashMap::new()),
            stats: Vec::new(),
            side_lines: Vec::new(),
            gaps: Vec::new(),
            digits: Vec::new(),
            columns: 0,
            rows: Vec::new(),
            top_key: None,
            sel_rows: std::cell::Cell::new(None),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            rev: 0,
            keys_rev: 0,
            visible_rows: 0..0,
            view_w: std::cell::Cell::new(0.0),
            visible: 0,
            reported: None,
            nav: None,
            nav_range: None,
        };
        s.set_files(files);
        s
    }

    /// Show `files` instead, every range collapsed again and the selection cleared.
    pub fn set_files(&mut self, files: Vec<FileDiff>) {
        let n = files.len();
        self.set_files_folded(files, &vec![false; n]);
    }

    /// [`DiffState::set_files`] with `folded[i]` files collapsed, in one pass: folding them one
    /// by one afterwards rebuilds every row once per file.
    pub fn set_files_folded(&mut self, files: Vec<FileDiff>, folded: &[bool]) {
        // The top row's key reads the old files; take it before they're replaced.
        self.top_key = (!self.visible_rows.is_empty()).then(|| self.rows.get(self.visible_rows.start).map(|&r| self.key_of(r))).flatten();
        self.ui = files.iter().enumerate().map(|(i, f)| FileUi { collapsed: folded.get(i).copied().unwrap_or(false), revealed: vec![(0, 0); f.hunks.len() + 1] }).collect();
        self.side_lines = files
            .iter()
            .map(|f| {
                f.hunks
                    .iter()
                    .map(|h| {
                        let on = |side: Side| (0..h.lines.len() as u32).filter(|&i| h.lines[i as usize].number(side).is_some()).collect();
                        [on(Side::Old), on(Side::New)]
                    })
                    .collect()
            })
            .collect();
        self.gaps = files.iter().map(gaps).collect();
        self.digits = files.iter().map(|f| f.last_line().max(1).ilog10() + 1).map(|d| d.max(3)).collect();
        self.columns = files.iter().map(FileDiff::columns).max().unwrap_or(0);
        self.stats = files.iter().map(|f| (f.additions(), f.deletions())).collect();
        self.files = files;
        self.selection = None;
        self.chunks.borrow_mut().clear();
        self.visible = 0;
        self.reported = None;
        self.visible_rows = 0..0;
        self.rebuild();
    }

    /// Lines added and removed in `file`.
    pub fn stat(&self, file: usize) -> (usize, usize) {
        self.stats.get(file).copied().unwrap_or_default()
    }

    /// Lines added and removed across every file.
    pub fn totals(&self) -> (usize, usize) {
        self.stats.iter().fold((0, 0), |(a, d), &(x, y)| (a + x, d + y))
    }

    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }

    /// The index of the file at `path` (its new path, or old for a deleted file).
    pub fn file_index(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|f| f.path() == path || f.old_path.as_deref() == Some(path))
    }

    pub fn mode(&self) -> DiffMode {
        self.mode
    }

    /// Switch between unified and split. Rows keep their keys where they can (unchanged and
    /// removed lines), so a switch made in a handler that calls `Tx::motion` springs them to
    /// their new places.
    pub fn set_mode(&mut self, mode: DiffMode) {
        if self.mode != mode {
            self.mode = mode;
            self.rebuild();
        }
    }

    /// In split mode, the one side `file` shows across the whole width, as a single column:
    /// an added file's new side or a deleted file's old side ([`FileDiff::sole_side`]).
    /// `None` in unified mode and for a file with both sides.
    pub fn single_column(&self, file: usize) -> Option<Side> {
        if self.mode != DiffMode::Split {
            return None;
        }
        self.files.get(file)?.sole_side()
    }

    /// Whether long lines wrap as drawn: split always wraps, each side half the width.
    pub(crate) fn wraps(&self) -> bool {
        self.wrap || self.mode == DiffMode::Split
    }

    /// Wrap long lines (rows grow), or keep them on one line and scroll sideways (the default).
    /// Split wraps either way, as side-by-side review does, so both sides stay in view.
    pub fn set_wrap(&mut self, wrap: bool) {
        if self.wrap != wrap {
            self.wrap = wrap;
            self.rev += 1;
        }
    }

    /// Highlight code with `h` instead of the built-in scanners.
    pub fn style(&self) -> Style {
        self.style
    }

    /// Restyle the lines; built rows rebuild.
    pub fn set_style(&mut self, style: Style) {
        if self.style != style {
            self.style = style;
            self.rev += 1;
        }
    }

    /// Text to highlight in every line, ignoring case; `None` or empty for none.
    pub fn set_search(&mut self, q: Option<String>) {
        let q = q.filter(|q| !q.is_empty());
        if self.search != q {
            self.search = q;
            self.search_rev += 1;
        }
    }

    pub fn search(&self) -> Option<&str> {
        self.search.as_deref()
    }

    pub(crate) fn search_rev(&self) -> u64 {
        self.search_rev
    }

    /// Every line in the shown hunks that holds the search text, in order, as the line to
    /// scroll to.
    pub fn search_hits(&self) -> Vec<(usize, LineRef)> {
        let Some(q) = self.search.as_deref() else { return Vec::new() };
        let mut out = Vec::new();
        for (f, file) in self.files.iter().enumerate() {
            for l in file.hunks.iter().flat_map(|h| &h.lines) {
                if super::view::find_all(&l.text, q).is_empty() {
                    continue;
                }
                let at = match (l.kind, l.old, l.new) {
                    (LineKind::Removed, Some(n), _) => LineRef::new(Side::Old, n),
                    (_, _, Some(n)) => LineRef::new(Side::New, n),
                    (_, Some(n), None) => LineRef::new(Side::Old, n),
                    _ => continue,
                };
                out.push((f, at));
            }
        }
        out
    }

    /// For the hunk holding the line selection: which of its lines are selected (only added
    /// and removed lines count), as (file, hunk, per line). `None` without a selection on a
    /// change.
    pub fn selected_changes(&self) -> Option<(usize, usize, Vec<bool>)> {
        let sel = self.selection?;
        let f = sel.file;
        for (h, hunk) in self.files.get(f)?.hunks.iter().enumerate() {
            let picked: Vec<bool> = hunk
                .lines
                .iter()
                .map(|l| match (l.kind, l.old, l.new) {
                    (LineKind::Removed, Some(n), _) => self.contains(&sel, LineRef::new(Side::Old, n)),
                    (LineKind::Added, _, Some(n)) => self.contains(&sel, LineRef::new(Side::New, n)),
                    _ => false,
                })
                .collect();
            if picked.iter().any(|&p| p) {
                return Some((f, h, picked));
            }
        }
        None
    }

    pub fn set_highlighter(&mut self, h: Highlighter) {
        self.highlighter = h;
        self.chunks.borrow_mut().clear();
        self.rev += 1;
    }

    pub fn collapsed(&self, file: usize) -> bool {
        self.ui.get(file).is_some_and(|u| u.collapsed)
    }

    /// Show only `file`'s header, or its lines again.
    pub fn set_collapsed(&mut self, file: usize, collapsed: bool) {
        if let Some(u) = self.ui.get_mut(file).filter(|u| u.collapsed != collapsed) {
            u.collapsed = collapsed;
            self.rebuild();
        }
    }

    /// Lines of `file`'s unchanged range before hunk `gap` still hidden, when known.
    pub fn hidden(&self, file: usize, gap: usize) -> Option<u32> {
        let g = self.gaps.get(file)?.get(gap)?;
        let (top, bottom) = self.ui[file].revealed[gap];
        g.len.map(|n| n.saturating_sub(top + bottom))
    }

    /// Reveal more of `file`'s unchanged range before hunk `gap` (`hunks.len()`: after the
    /// last). Does nothing without the file's whole text.
    pub fn expand(&mut self, file: usize, gap: usize, how: Expand) {
        let Some(hidden) = self.hidden(file, gap) else { return };
        if hidden == 0 || !self.files[file].expandable() {
            return;
        }
        let last = gap == self.files[file].hunks.len();
        let r = &mut self.ui[file].revealed[gap];
        match how {
            // Nothing below the first range and nothing above the last to grow from.
            Expand::All => r.0 += hidden,
            Expand::Up if gap > 0 || last => {
                if last { r.0 += hidden.min(EXPAND_STEP) } else { r.1 += hidden.min(EXPAND_STEP) }
            }
            Expand::Up => r.1 += hidden.min(EXPAND_STEP),
            Expand::Down if last => r.0 += hidden.min(EXPAND_STEP),
            Expand::Down => r.0 += hidden.min(EXPAND_STEP),
        }
        self.rebuild();
    }

    pub fn selection(&self) -> Option<LineRange> {
        self.selection
    }

    pub fn set_selection(&mut self, sel: Option<LineRange>) {
        self.selection = sel;
    }

    /// The selected lines' text on the side the selection started on, one per line, without
    /// numbers or markers: what copying the selection gives.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let side = sel.start.side;
        let (a, b) = self.selected_rows(&sel)?;
        let mut out = Vec::new();
        for r in a..=b {
            if let Some(n) = self.row_line(self.rows[r as usize], side) {
                out.push(self.side_line(sel.file, side, n).unwrap_or_default().to_owned());
            }
        }
        Some(out.join("\n"))
    }

    /// The selection's first and last rows.
    fn selected_rows(&self, sel: &LineRange) -> Option<(u32, u32)> {
        if let Some((s, rev, rows)) = self.sel_rows.get()
            && s == *sel
            && rev == self.rev
        {
            return rows;
        }
        let rows = (|| {
            let a = self.row_at(sel.file, sel.start)?;
            let b = self.row_at(sel.file, sel.end)?;
            Some((a.min(b), a.max(b)))
        })();
        self.sel_rows.set(Some((*sel, self.rev, rows)));
        rows
    }

    /// The row showing line `at` of file `file`, if it's shown. A file's rows are contiguous,
    /// so this looks only through that file's, instead of keeping a map of every row.
    fn row_at(&self, file: usize, at: LineRef) -> Option<u32> {
        let start = *self.file_rows.get(file)? as usize;
        let end = self.file_rows.get(file + 1).map_or(self.rows.len(), |&r| r as usize);
        (start..end).find(|&i| self.row_line(self.rows[i], at.side) == Some(at.line)).map(|i| i as u32)
    }

    /// Whether line `at` is in `sel`.
    pub(crate) fn contains(&self, sel: &LineRange, at: LineRef) -> bool {
        let Some((a, b)) = self.selected_rows(sel) else { return false };
        self.row_at(sel.file, at).is_some_and(|r| (a..=b).contains(&r))
    }

    /// Whether row `r`'s line on `side` is selected.
    pub(crate) fn row_selected(&self, r: usize, side: Option<Side>) -> bool {
        let Some(sel) = self.selection else { return false };
        let Some((a, b)) = self.selected_rows(&sel) else { return false };
        if self.rows[r].file() != sel.file || !(a as usize..=b as usize).contains(&r) {
            return false;
        }
        side.is_none_or(|s| s == sel.start.side || s == sel.end.side)
    }

    /// A press on line `at`'s number: a new selection, or with Shift held one extended to it.
    /// Pressing the one selected line clears it on release, unless the press drags.
    pub(crate) fn press_line(&mut self, file: usize, at: LineRef) {
        self.selection_at_press = self.selection;
        let sel = match self.selection {
            Some(s) if self.shift && s.file == file => LineRange { end: at, ..s },
            _ => LineRange::line(file, at),
        };
        self.unselect = !self.shift && self.selection == Some(sel);
        self.selecting = true;
        self.set_selection(Some(sel));
    }

    /// The pointer, dragging from a line number, reached line `at`.
    pub(crate) fn drag_to(&mut self, file: usize, at: LineRef) {
        if let Some(s) = self.selection.filter(|s| self.selecting && s.file == file && s.end != at) {
            self.unselect = false;
            self.set_selection(Some(LineRange { end: at, ..s }));
        }
    }

    /// The press on the line numbers ended: the selection it leaves.
    pub(crate) fn release_line(&mut self) -> Option<LineRange> {
        self.selection_at_press = None;
        if std::mem::take(&mut self.unselect) {
            self.selection = None;
        }
        self.selecting = false;
        self.selection
    }

    pub(crate) fn cancel_line(&mut self) {
        if self.selecting {
            self.selection = self.selection_at_press.take();
        }
        self.selecting = false;
        self.unselect = false;
    }

    pub(crate) fn copy_side(&self) -> Side {
        self.copy_side
    }

    /// The pointer entered or left a line's text on `side`: text selection then copies that
    /// side. A held press keeps the cell it started in entered, so dragging across to the
    /// other side doesn't switch.
    /// Notes the pointer entering or leaving a line's code; returns whether that changed the
    /// side a selection copies, the only part the view reads.
    pub(crate) fn hover_text(&mut self, side: Side, cell: &str, inside: bool) -> bool {
        let was = self.copy_side;
        if inside {
            if !self.hovered.iter().any(|(_, c)| c == cell) {
                self.hovered.push((side, cell.to_owned()));
            }
            if self.hovered.iter().all(|(s, _)| *s == side) {
                self.copy_side = side;
            }
        } else {
            self.hovered.retain(|(_, c)| c != cell);
        }
        self.copy_side != was
    }

    pub fn visible_file(&self) -> usize {
        self.visible
    }

    /// Scroll `file`'s header to the top.
    pub fn scroll_to_file(&mut self, file: usize, tx: &mut Tx) {
        let Some(&r) = self.file_rows.get(file) else { return };
        self.reveal_at_top(r, tx);
        self.visible = file;
        self.nav = Some(r);
    }

    /// Scroll to `file`'s line `at`, opening the range it's in if it's hidden, and put it
    /// below the header.
    pub fn scroll_to_line(&mut self, file: usize, at: LineRef, tx: &mut Tx) {
        if self.collapsed(file) {
            self.set_collapsed(file, false);
        }
        if self.row_at(file, at).is_none() {
            let gap = self.gaps.get(file).and_then(|g| {
                g.iter().position(|g| {
                    let first = if at.side == Side::Old { g.old } else { g.new };
                    at.line >= first && g.len.is_none_or(|n| at.line < first + n)
                })
            });
            if let Some(gap) = gap {
                self.expand(file, gap, Expand::All);
            }
        }
        let Some(r) = self.row_at(file, at) else { return };
        self.reveal_below_header(r, tx);
        self.visible = file;
        self.nav = Some(r);
    }

    /// Bring the next (or previous) hunk to just below the sticky header.
    pub fn next_hunk(&mut self, forward: bool, tx: &mut Tx) -> bool {
        let from = self.position();
        let target = if forward {
            self.hunk_rows.iter().copied().find(|&r| r > from)
        } else {
            self.hunk_rows.iter().rev().copied().find(|&r| r < from)
        };
        let Some(r) = target else { return false };
        self.reveal_below_header(r, tx);
        self.visible = self.rows[r as usize].file();
        self.nav = Some(r);
        true
    }

    /// Scroll to the next (or previous) file's header.
    pub fn next_file(&mut self, forward: bool, tx: &mut Tx) -> bool {
        let Some(&row) = self.rows.get(self.position() as usize) else { return false };
        let at = row.file();
        let target = if forward { at + 1 } else { at.wrapping_sub(1) };
        let Some(f) = (target < self.files.len()).then_some(target) else { return false };
        self.scroll_to_file(f, tx);
        true
    }

    /// The hunk the reader is at (the one `j`/`k` last reached, else the first at or below the
    /// top of the view), as (file, hunk).
    pub fn current_hunk(&self) -> Option<(usize, usize)> {
        let from = self.position() as usize;
        self.rows.get(from..)?.iter().find_map(|r| match *r {
            Row::Line { f, h, .. } | Row::Pair { f, h, .. } => Some((f as usize, h as usize)),
            Row::Gap { f, gap } if (gap as usize) < self.files[f as usize].hunks.len() => Some((f as usize, gap as usize)),
            _ => None,
        })
    }

    /// Jump to the other end of the move at or below the top of the view (or the selection):
    /// where a moved block came from, or went. False when there's no moved line in reach.
    pub fn jump_to_twin(&mut self, tx: &mut Tx) -> bool {
        // From the selection, else where the last jump landed while it's on screen (so a
        // second m goes back even when both ends fit in view), else the top of the view.
        let landed = self.nav.filter(|&n| self.visible_rows.contains(&(n as usize)));
        let from = match self.selection {
            Some(sel) => self.row_at(sel.file, sel.start).unwrap_or(self.position()),
            None => landed.unwrap_or_else(|| self.position()),
        } as usize;
        let found = self.rows.get(from..).unwrap_or(&[]).iter().take(400).find_map(|r| {
            let (f, h, i) = match *r {
                Row::Line { f, h, i } | Row::Pair { f, h, del: i, .. } => (f, h, i),
                _ => return None,
            };
            let file = &self.files[f as usize];
            let l = &file.hunks[h as usize].lines[i as usize];
            file.twin(h as usize, i as usize).map(|t| (t, l.kind))
        });
        let Some(((f, n), kind)) = found else { return false };
        let side = if kind == LineKind::Removed { Side::New } else { Side::Old };
        self.scroll_to_line(f as usize, LineRef::new(side, n), tx);
        true
    }

    /// The row under the sticky header: where navigation last put it, unless the list has
    /// scrolled away since.
    fn position(&self) -> u32 {
        let top = self.row_below_header();
        match (self.nav, top) {
            (Some(n), Some(t)) if t.abs_diff(n) <= 3 => n,
            (_, Some(t)) => t,
            (Some(n), None) => n,
            (None, None) => 0,
        }
    }

    fn row_below_header(&self) -> Option<u32> {
        if self.visible_rows.is_empty() {
            return None;
        }
        let mut r = self.visible_rows.start;
        if self.sticky().is_some() {
            let mut room = 0.0;
            while r + 1 < self.rows.len() && room < HEADER_H {
                room += self.estimate(r);
                r += 1;
            }
        }
        Some(r as u32)
    }

    /// Handle a key for the diff: `j`/`k` next and previous hunk, `]`/`[` next and previous
    /// file, Escape clears the line selection, Cmd-C copies it, and Shift (with
    /// `App::modifier_keys`) makes a click on a line number extend the selection. Call it
    /// from the app's `on_key`; it returns whether it took the key.
    pub fn key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        match &ev.key {
            Key::Modifier { modifier: Modifier::ShiftLeft | Modifier::ShiftRight, .. } => {
                self.shift = ev.mods.shift;
                false
            }
            _ if tx.focused().is_some() && !ev.is_command('c') => false,
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt => match t.as_str() {
                "j" => self.next_hunk(true, tx),
                "k" => self.next_hunk(false, tx),
                "]" => self.next_file(true, tx),
                "[" => self.next_file(false, tx),
                _ => false,
            },
            Key::Escape if self.selection.is_some() => {
                self.set_selection(None);
                true
            }
            _ if ev.is_command('c') => match self.selected_text() {
                Some(text) => {
                    tx.copy(text);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The scroll viewport's rect.
    pub(crate) fn set_viewport(&self, r: [f32; 4]) {
        self.view_w.set(r[2]);
    }

    pub(crate) fn view_width(&self) -> f32 {
        self.view_w.get()
    }

    /// The virtual list's visible row range changed to `rows`: returns `(reported_file, sticky_changed)`.
    pub(crate) fn set_visible_rows(&mut self, rows: std::ops::Range<usize>) -> (Option<usize>, bool) {
        let before_sticky = self.sticky();
        self.visible_rows = rows;
        // Navigation puts its row at the top, but near the end the list reports a full screen
        // of rows ending at the last one, starting above it. While the range stays where
        // navigation left it, the row navigated to is the one in view; a scroll ends that.
        let range = self.visible_rows.clone();
        let landed = match (self.nav, &self.nav_range) {
            (Some(n), Some((m, r))) if *m == n => (*r == range).then_some(n),
            (Some(n), _) => range.contains(&(n as usize)).then_some(n),
            _ => None,
        };
        // Once a scroll ends it, it stays ended (an empty range never matches) until the next jump.
        self.nav_range = self.nav.map(|n| (n, if landed.is_some() { range.clone() } else { 0..0 }));
        if let Some(&r) = self.rows.get(landed.map_or(range.start, |n| n as usize)) {
            self.visible = r.file();
        }
        let reported = (!self.files.is_empty() && self.reported != Some(self.visible)).then(|| {
            self.reported = Some(self.visible);
            self.visible
        });
        (reported, self.sticky() != before_sticky)
    }

    /// The row reached by moving `y` points vertically from the top of row `from`.
    pub(crate) fn row_from(&self, from: u32, mut y: f32) -> Option<u32> {
        let mut r = usize::try_from(from).ok()?.min(self.rows.len().checked_sub(1)?);
        while y < 0.0 && r > 0 {
            r -= 1;
            y += self.estimate(r);
        }
        while r + 1 < self.rows.len() {
            let h = self.estimate(r);
            if y < h {
                break;
            }
            y -= h;
            r += 1;
        }
        Some(r as u32)
    }

    /// The visible file's header has scrolled above the viewport's top, so a copy of it sticks
    /// there.
    pub(crate) fn sticky(&self) -> Option<usize> {
        if self.files.is_empty() || self.visible_rows.is_empty() {
            return None;
        }
        let f = self.visible;
        let header = *self.file_rows.get(f)? as usize;
        (self.visible_rows.start > header).then_some(f)
    }


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

/// Where a comment is anchored: a line by its file, side and number, and the line's text, so
/// it follows the line when edits above move it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub text: String,
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
    /// A strip down the right edge showing where the changes are.
    pub minimap: bool,
}

impl Default for Style {
    fn default() -> Style {
        Style { font_size: 12.5, line_h: 20.0, one_number: true, markers: true, word_diff: true, minimap: true }
    }
}

/// [`DiffState::overview`]'s marks: a removed or added line, a file's start, a search match.
pub(crate) const MAP_DEL: u8 = 1;
pub(crate) const MAP_ADD: u8 = 2;
pub(crate) const MAP_FILE: u8 = 4;
pub(crate) const MAP_FOUND: u8 = 8;
pub(crate) const HEADER_H: f32 = 44.0;
pub(crate) const GAP_H: f32 = 32.0;
pub(crate) const NOTE_H: f32 = 40.0;
/// A comment's row, as estimated before it's laid out.
const COMMENT_H: f32 = 64.0;
/// A changed image's row: both sides drawn.
pub const IMAGE_H: f32 = 280.0;

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
    /// Comment `n` (in the order [`DiffState::set_comments`] took them), under its line.
    Comment { f: u32, n: u32 },
    /// Line `i` of hunk `h`. In split mode, one without a partner on the other side.
    Line { f: u32, h: u32, i: u32 },
    /// Split mode: removed line `del` beside added line `add`, both of hunk `h`.
    Pair { f: u32, h: u32, del: u32, add: u32 },
}

impl Row {
    pub(crate) fn file(self) -> usize {
        match self {
            Row::File(f) | Row::Note(f) | Row::Gap { f, .. } | Row::Ctx { f, .. } | Row::Comment { f, .. } | Row::Line { f, .. } | Row::Pair { f, .. } => f as usize,
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
    /// Which languages the highlighter colours: the rest skip it and draw plain.
    colours: Rc<dyn Fn(&str) -> bool>,
    style: Style,
    search: Option<super::Finder>,
    search_rev: u64,
    chunks: RefCell<HashMap<(u32, Side, u32), Rc<Chunk>>>,
    /// The minimap's marks and what they were worked out for: (rev, search_rev, buckets, tail).
    overview: RefCell<Option<(MapAt, Rc<[u8]>)>>,
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
    /// Each comment's line as (file, hunk, line in hunk); `None` for one whose line isn't in
    /// the diff.
    comments: Vec<Option<(u32, u32, u32)>>,
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

/// What [`DiffState::overview`]'s marks were worked out for.
type MapAt = (u64, u64, u32, u32);

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
            colours: Rc::new(rune_kit::syntax::known),
            style: Style::default(),
            search: None,
            search_rev: 0,
            chunks: RefCell::new(HashMap::new()),
            overview: RefCell::new(None),
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
            comments: Vec::new(),
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

    /// What to highlight in every line; `None` for nothing.
    pub fn set_search(&mut self, f: Option<super::Finder>) {
        if self.search.as_ref().map(|f| f.query()) != f.as_ref().map(|f| f.query()) {
            self.search = f;
            self.search_rev += 1;
        }
    }

    pub fn search(&self) -> Option<&super::Finder> {
        self.search.as_ref()
    }

    pub(crate) fn search_rev(&self) -> u64 {
        self.search_rev
    }

    /// Every line in the shown hunks that holds the search text, in order, as the line to
    /// scroll to.
    pub fn search_hits(&self) -> Vec<(usize, LineRef)> {
        let Some(q) = &self.search else { return Vec::new() };
        let mut out = Vec::new();
        for (f, file) in self.files.iter().enumerate() {
            for l in file.hunks.iter().flat_map(|h| &h.lines) {
                if q.find_all(&l.text).is_empty() {
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

    /// Colour code with `h`, in the languages `colours` says it knows; of those, the built-in
    /// scanners' own languages get the scanners, whatever `h` is.
    pub fn set_highlighter(&mut self, h: Highlighter, colours: impl Fn(&str) -> bool + 'static) {
        self.highlighter = h;
        self.colours = Rc::new(colours);
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

    /// The selection's side (the one it started on) and its first and last line numbers on
    /// that side: what a reference to the selection names.
    pub fn selected_span(&self) -> Option<(Side, u32, u32)> {
        let sel = self.selection?;
        let side = sel.start.side;
        let (a, b) = self.selected_rows(&sel)?;
        let mut lines = (a..=b).filter_map(|r| self.row_line(self.rows[r as usize], side));
        let first = lines.next()?;
        Some((side, first, lines.last().unwrap_or(first)))
    }

    /// The selected rows as a unified diff body: both sides, ` `, `-` and `+` markers, the
    /// removed lines of each change before its added ones (as split mode's pairs aren't), and
    /// `@@` where a hidden range falls between. For handing the lines to an agent.
    pub fn selected_diff(&self) -> Option<String> {
        let sel = self.selection?;
        let (a, b) = self.selected_rows(&sel)?;
        let file = &self.files[sel.file];
        let line = |h: u32, i: u32| &file.hunks[h as usize].lines[i as usize];
        let (mut out, mut added) = (Vec::new(), Vec::new());
        for r in a..=b {
            match self.rows[r as usize] {
                Row::Ctx { old, .. } => {
                    out.append(&mut added);
                    out.push(format!(" {}", self.side_line(sel.file, Side::Old, old).unwrap_or_default()));
                }
                Row::Line { h, i, .. } => {
                    let l = line(h, i);
                    match l.kind {
                        LineKind::Added => added.push(format!("+{}", l.text)),
                        LineKind::Removed => out.push(format!("-{}", l.text)),
                        LineKind::Context => {
                            out.append(&mut added);
                            out.push(format!(" {}", l.text));
                        }
                    }
                }
                Row::Pair { h, del, add, .. } => {
                    out.push(format!("-{}", line(h, del).text));
                    added.push(format!("+{}", line(h, add).text));
                }
                Row::Gap { .. } => {
                    out.append(&mut added);
                    out.push("@@".into());
                }
                Row::File(_) | Row::Note(_) | Row::Comment { .. } => {}
            }
        }
        out.append(&mut added);
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

    /// Scroll so row `r` is at the top: to the end first, so the reveal comes from below.
    fn reveal_at_top(&self, r: u32, tx: &mut Tx) {
        // Jump to the very end (the view's scroll-past-end spacer), so revealing the row then
        // scrolls up until it sits at the top rather than stopping when it first shows.
        if !self.rows.is_empty() {
            tx.scroll_into_view(TAIL_KEY.to_owned());
        }
        tx.scroll_into_view(self.row_key(r as usize));
    }

    /// Scroll so row `r` sits just below the sticky header: the row a header's height above
    /// it goes to the top.
    fn reveal_below_header(&self, r: u32, tx: &mut Tx) {
        let mut top = r as usize;
        let mut room = 0.0;
        while top > 0 && room < HEADER_H {
            if matches!(self.rows[top], Row::File(_)) {
                break;
            }
            top -= 1;
            room += self.estimate(top);
        }
        self.reveal_at_top(top as u32, tx);
    }

    fn estimate(&self, r: usize) -> f32 {
        match self.rows[r] {
            Row::File(_) => HEADER_H,
            Row::Note(f) if self.files[f as usize].images.is_some() => IMAGE_H,
            Row::Note(_) => NOTE_H,
            Row::Gap { .. } => GAP_H,
            Row::Comment { .. } => COMMENT_H,
            _ => self.style.line_h,
        }
    }

    /// The minimap's marks: the list's estimated height plus `tail` cut into `buckets` equal
    /// bands, each a set of [`MAP_DEL`], [`MAP_ADD`], [`MAP_FILE`] and [`MAP_FOUND`] for the
    /// rows that start in it. Worked out once per rows, search and size, as it walks every row.
    pub(crate) fn overview(&self, buckets: u32, tail: f32) -> Rc<[u8]> {
        let at = (self.rev, self.search_rev, buckets, tail as u32);
        if let Some((k, marks)) = &*self.overview.borrow()
            && *k == at
        {
            return marks.clone();
        }
        let total: f32 = (0..self.rows.len()).map(|r| self.estimate(r)).sum::<f32>() + tail;
        let mut marks = vec![0u8; buckets as usize];
        if total > 0.0 && buckets > 0 {
            let per = buckets as f32 / total;
            let found = |text: &str| self.search.as_ref().is_some_and(|q| !q.find_all(text).is_empty());
            let mut y = 0.0;
            for (r, &row) in self.rows.iter().enumerate() {
                let b = ((y * per) as usize).min(marks.len() - 1);
                y += self.estimate(r);
                let line = |f: u32, h: u32, i: u32| &self.files[f as usize].hunks[h as usize].lines[i as usize];
                let mark = match row {
                    Row::File(_) => MAP_FILE,
                    Row::Line { f, h, i } => {
                        let l = line(f, h, i);
                        let kind = match l.kind {
                            LineKind::Added => MAP_ADD,
                            LineKind::Removed => MAP_DEL,
                            LineKind::Context => 0,
                        };
                        kind | if found(&l.text) { MAP_FOUND } else { 0 }
                    }
                    Row::Pair { f, h, del, add } => MAP_DEL | MAP_ADD | if found(&line(f, h, del).text) || found(&line(f, h, add).text) { MAP_FOUND } else { 0 },
                    _ => 0,
                };
                // A tall row (an image) covers every band it spans, not just its first.
                let end = ((y * per).ceil() as usize).clamp(b + 1, marks.len());
                for m in &mut marks[b..if mark & (MAP_ADD | MAP_DEL) != 0 { end } else { b + 1 }] {
                    *m |= mark;
                }
            }
        }
        let marks: Rc<[u8]> = marks.into();
        *self.overview.borrow_mut() = Some((at, marks.clone()));
        marks
    }

    /// Line `n` of `side` of file `f`, from its whole text, its hunks or an unchanged range.
    pub(crate) fn side_line(&self, f: usize, side: Side, n: u32) -> Option<&str> {
        let file = &self.files[f];
        let hunks = &file.hunks;
        let h = hunks.partition_point(|h| h.end(side) <= n);
        if let Some(hunk) = hunks.get(h).filter(|h| h.first(side) <= n) {
            let i = self.side_lines[f][h][side as usize].get((n - hunk.first(side)) as usize)?;
            return Some(&hunk.lines[*i as usize].text);
        }
        let g = self.gaps[f][h];
        let off = n.checked_sub(if side == Side::Old { g.old } else { g.new })?;
        file.context_line(g.old.saturating_add(off), g.new.saturating_add(off))
    }

    /// Colour-only highlights of line `n` of `side` of file `f`, relative to the line.
    pub(crate) fn highlights(&self, f: usize, side: Side, n: u32) -> Vec<(std::ops::Range<usize>, rune_kit::syntax::Paint)> {
        let Some(lang) = self.files[f].language().filter(|l| (self.colours)(l)) else { return Vec::new() };
        let c = n.saturating_sub(1) / CHUNK;
        let key = (f as u32, side, c);
        let chunk = self.chunks.borrow().get(&key).cloned();
        let chunk = chunk.unwrap_or_else(|| {
            let mut text = String::new();
            let mut starts = Vec::with_capacity(CHUNK as usize + 1);
            for m in c * CHUNK + 1..=(c + 1) * CHUNK {
                starts.push(text.len() as u32);
                text.push_str(self.side_line(f, side, m).unwrap_or(""));
                text.push('\n');
            }
            starts.push(text.len() as u32);
            // The built-in scanners skip the highlighter's own cache: this one already keeps
            // the blocks, and that one holds a copy of each block's text besides.
            let hl = if self.highlighter.is_builtin() || rune_kit::syntax::known(lang) {
                rune_kit::syntax::builtin(Some(lang), &text).into()
            } else {
                self.highlighter.highlight(Some(lang), &text)
            };
            let chunk = Rc::new(Chunk { starts, hl });
            let mut cache = self.chunks.borrow_mut();
            if cache.len() > CHUNKS_KEPT {
                cache.clear();
            }
            cache.insert(key, chunk.clone());
            chunk
        });
        let k = (n.saturating_sub(1) % CHUNK) as usize;
        let (a, b) = (chunk.starts[k] as usize, chunk.starts[k + 1] as usize - 1);
        let first = chunk.hl.partition_point(|(r, _)| r.end <= a);
        chunk.hl[first..]
            .iter()
            .take_while(|(r, _)| r.start < b)
            .map(|(r, p)| (r.start.max(a) - a..r.end.min(b) - a, *p))
            .filter(|(r, _)| !r.is_empty())
            .collect()
    }

    /// Row `r`'s line number on `side`.
    pub(crate) fn row_line(&self, r: Row, side: Side) -> Option<u32> {
        match r {
            Row::Ctx { old, new, .. } => Some(if side == Side::Old { old } else { new }),
            Row::Line { f, h, i } => self.files[f as usize].hunks[h as usize].lines[i as usize].number(side),
            Row::Pair { f, h, del, add } => {
                let i = if side == Side::Old { del } else { add };
                self.files[f as usize].hunks[h as usize].lines[i as usize].number(side)
            }
            _ => None,
        }
    }

    /// Puts comments under their lines: each under the line of that file and side with the
    /// same text, at the number given or the nearest one to it, so edits above a line don't
    /// lose its comment. One whose line isn't in the diff (or is folded out of a hunk) isn't
    /// shown. Returns which ones are.
    pub fn set_comments(&mut self, anchors: &[Anchor]) -> Vec<bool> {
        let placed: Vec<Option<(u32, u32, u32)>> = anchors.iter().map(|a| self.anchor(a)).collect();
        let shown = placed.iter().map(Option::is_some).collect();
        if placed != self.comments {
            self.comments = placed;
            self.rebuild();
        }
        shown
    }

    fn anchor(&self, a: &Anchor) -> Option<(u32, u32, u32)> {
        let f = self.file_index(&a.path)?;
        let mut best: Option<(u32, usize, usize)> = None;
        for (h, hunk) in self.files[f].hunks.iter().enumerate() {
            for (i, l) in hunk.lines.iter().enumerate() {
                let Some(n) = l.number(a.side) else { continue };
                if l.text == a.text && best.is_none_or(|(d, ..)| n.abs_diff(a.line) < d) {
                    best = Some((n.abs_diff(a.line), h, i));
                }
            }
        }
        best.map(|(_, h, i)| (f as u32, h as u32, i as u32))
    }

    /// Lay the rows out again from the files, modes and expanded ranges.
    fn rebuild(&mut self) {
        self.rev += 1;
        self.nav = None;
        let mut under: std::collections::HashMap<(u32, u32, u32), Vec<u32>> = Default::default();
        for (n, at) in self.comments.iter().enumerate() {
            if let Some(at) = at {
                under.entry(*at).or_default().push(n as u32);
            }
        }
        let comments = |rows: &mut Vec<Row>, f: u32, h: u32, lines: &[u32]| {
            for i in lines {
                rows.extend(under.get(&(f, h, *i)).into_iter().flatten().map(|&n| Row::Comment { f, n }));
            }
        };
        let mut rows = Vec::new();
        self.file_rows.clear();
        self.hunk_rows.clear();
        for (f, file) in self.files.iter().enumerate() {
            let fu = f as u32;
            self.file_rows.push(rows.len() as u32);
            rows.push(Row::File(fu));
            if self.ui[f].collapsed {
                continue;
            }
            if file.hunks.is_empty() || file.images.is_some() {
                rows.push(Row::Note(fu));
            }
            if file.hunks.is_empty() {
                continue;
            }
            for gap in 0..=file.hunks.len() {
                let g = self.gaps[f][gap];
                let (top, bottom) = self.ui[f].revealed[gap];
                let ctx = |k: u32| Row::Ctx { f: fu, old: g.old + k, new: g.new + k };
                rows.extend((0..top).map(ctx));
                let hidden = g.len.map(|n| n.saturating_sub(top + bottom));
                let first_row = rows.len() as u32;
                // A hunk's header row stands before every hunk, as a review list shows it, so
                // the hunk's actions have a place; after the last hunk only when lines fold.
                if hidden.is_some_and(|n| n > 0) || gap < file.hunks.len() {
                    rows.push(Row::Gap { f: fu, gap: gap as u32 });
                }
                if let Some(n) = g.len {
                    rows.extend((n - bottom.min(n)..n).map(ctx));
                }
                let Some(h) = file.hunks.get(gap) else { break };
                self.hunk_rows.push(first_row);
                let hu = gap as u32;
                let lines = &h.lines;
                let mut i = 0;
                while i < lines.len() {
                    if self.mode == DiffMode::Split && lines[i].kind == LineKind::Removed {
                        let del = i;
                        while i < lines.len() && lines[i].kind == LineKind::Removed {
                            i += 1;
                        }
                        let add = i;
                        while i < lines.len() && lines[i].kind == LineKind::Added {
                            i += 1;
                        }
                        let (nd, na) = (add - del, i - add);
                        for k in 0..nd.max(na) {
                            let (row, lines) = match (k < nd, k < na) {
                                (true, true) => (Row::Pair { f: fu, h: hu, del: (del + k) as u32, add: (add + k) as u32 }, vec![(del + k) as u32, (add + k) as u32]),
                                (true, false) => (Row::Line { f: fu, h: hu, i: (del + k) as u32 }, vec![(del + k) as u32]),
                                _ => (Row::Line { f: fu, h: hu, i: (add + k) as u32 }, vec![(add + k) as u32]),
                            };
                            rows.push(row);
                            comments(&mut rows, fu, hu, &lines);
                        }
                        continue;
                    }
                    rows.push(Row::Line { f: fu, h: hu, i: i as u32 });
                    comments(&mut rows, fu, hu, &[i as u32]);
                    i += 1;
                }
            }
        }
        // Rows make their keys on demand (a few dozen are on screen) rather than keeping one
        // string per row; only the top row's is needed here, to keep the view's place.
        let top = self.top_key.take().or_else(|| (!self.visible_rows.is_empty()).then(|| self.rows.get(self.visible_rows.start).map(|&r| self.key_of(r))).flatten());
        if !self.visible_rows.is_empty() {
            let len = self.visible_rows.len();
            self.visible_rows = match top.and_then(|top| rows.iter().position(|&r| self.key_of(r) == top)) {
                Some(start) => start..(start + len).min(rows.len()),
                None => 0..0,
            };
        }
        static KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.keys_rev = KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.rows = rows;
    }

    /// A row's key: the same for an unchanged or removed line in both modes, so switching
    /// moves it rather than replacing it.
    /// Row `i`'s key: stable for the same line across rebuilds and layouts.
    pub(crate) fn row_key(&self, i: usize) -> String {
        self.key_of(self.rows[i])
    }

    fn key_of(&self, r: Row) -> String {
        // Put together by hand: the list asks for every row's key when the rows change, and
        // `format!` took most of that.
        let key = |f: u32, tag: &str, n: Option<u32>| {
            let mut k = String::with_capacity(2 + 10 + tag.len() + 10);
            k.push_str("df");
            push_number(&mut k, f);
            k.push_str(tag);
            if let Some(n) = n {
                push_number(&mut k, n);
            }
            k
        };
        match r {
            Row::File(f) => key(f, "", None),
            Row::Note(f) => key(f, ".note", None),
            Row::Comment { f, n } => key(f, ".k", Some(n)),
            Row::Gap { f, gap } => key(f, ".g", Some(gap)),
            Row::Ctx { f, old, .. } => key(f, ".c", Some(old)),
            Row::Line { f, h, i } => {
                let l = &self.files[f as usize].hunks[h as usize].lines[i as usize];
                match l.kind {
                    LineKind::Context => key(f, ".c", Some(l.old.unwrap_or(0))),
                    LineKind::Removed => key(f, ".o", Some(l.old.unwrap_or(0))),
                    LineKind::Added => key(f, ".n", Some(l.new.unwrap_or(0))),
                }
            }
            Row::Pair { f, h, del, .. } => key(f, ".o", Some(self.files[f as usize].hunks[h as usize].lines[del as usize].old.unwrap_or(0))),
        }
    }

    /// The digits `file`'s line numbers take.
    pub(crate) fn digits(&self, file: usize) -> u32 {
        self.digits.get(file).copied().unwrap_or(3)
    }

    /// The widest line of any file, in columns.
    pub(crate) fn columns(&self) -> u32 {
        self.columns
    }
}

/// `n` in decimal, onto `s`.
fn push_number(s: &mut String, mut n: u32) {
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    s.extend(digits[i..].iter().map(|&d| d as char));
}

/// The unchanged ranges before each hunk of `f` and after its last.
fn gaps(f: &FileDiff) -> Vec<Gap> {
    let mut out = Vec::with_capacity(f.hunks.len() + 1);
    let (mut old, mut new) = (1, 1);
    for h in f.hunks.iter() {
        let first = h.first(Side::Old);
        out.push(Gap { old, new, len: Some(first.saturating_sub(old)) });
        old = h.end(Side::Old);
        new = h.end(Side::New);
    }
    let len = f.old_len().map(|n| (n + 1).saturating_sub(old));
    out.push(Gap { old, new, len: if f.hunks.is_empty() { Some(0) } else { len } });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn sample() -> DiffState {
        let old = numbered(100);
        let new = old.replace("line 50\n", "line fifty\n");
        DiffState::new(vec![FileDiff::from_texts("a.txt", &old, &new)])
    }

    fn shown(s: &DiffState) -> Vec<String> {
        s.rows
            .iter()
            .map(|&r| match r {
                Row::File(_) => "file".into(),
                Row::Note(_) => "note".into(),
                Row::Comment { n, .. } => format!("comment{n}"),
                Row::Gap { gap, .. } => format!("gap{gap}:{}", s.hidden(0, gap as usize).unwrap()),
                Row::Ctx { old, new, .. } => format!("{old}={new}"),
                Row::Line { .. } | Row::Pair { .. } => {
                    let (o, n) = (s.row_line(r, Side::Old), s.row_line(r, Side::New));
                    format!("{}|{}", o.map_or("-".into(), |v| v.to_string()), n.map_or("-".into(), |v| v.to_string()))
                }
            })
            .collect()
    }

    #[test]
    fn collapsed_ranges_expand_by_twenty_and_all() {
        let mut s = sample();
        assert_eq!(shown(&s)[..3], ["file", "gap0:46", "47|47"]);
        assert_eq!(s.hidden(0, 1), Some(47));
        s.expand(0, 0, Expand::Up);
        assert_eq!(s.hidden(0, 0), Some(26));
        let rows = shown(&s);
        assert_eq!(rows[1..4], ["gap0:26", "27=27", "28=28"]);
        s.expand(0, 1, Expand::Down);
        let rows = shown(&s);
        let after = rows.iter().position(|r| r == "53|53").unwrap();
        assert_eq!(rows[after + 1..after + 3], ["54=54", "55=55"]);
        assert_eq!(rows.last().unwrap(), "gap1:27");
        s.expand(0, 1, Expand::All);
        assert_eq!(s.hidden(0, 1), Some(0));
        assert_eq!(shown(&s).last().unwrap(), "100=100");
        s.expand(0, 0, Expand::All);
        // The file's header, the hunk's header (nothing folds above it now) and every line.
        assert_eq!(shown(&s).len(), 2 + 101);
        assert_eq!(s.side_line(0, Side::Old, 10), Some("line 10"));
        assert_eq!(s.side_line(0, Side::New, 50), Some("line fifty"));
    }

    #[test]
    fn split_pairs_changes_and_keeps_keys() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nC\nX\nd\n";
        let mut s = DiffState::new(vec![FileDiff::from_texts("x", old, new)]);
        let unified = shown(&s);
        assert_eq!(unified, ["file", "gap0:0", "1|1", "2|-", "3|-", "-|2", "-|3", "-|4", "4|5"]);
        let key_b = s.row_key(3);
        s.set_mode(DiffMode::Split);
        assert_eq!(shown(&s), ["file", "gap0:0", "1|1", "2|2", "3|3", "-|4", "4|5"]);
        assert_eq!(s.row_key(3), key_b);
    }

    #[test]
    fn row_keys_spell_the_file_and_line() {
        let mut s = DiffState::new(vec![FileDiff::from_texts("x", "a\nb\nc\nd\n", "a\nB\nC\nX\nd\n"), FileDiff::from_texts("y", "", "")]);
        let keys = |s: &DiffState| (0..s.rows.len()).map(|i| s.row_key(i)).collect::<Vec<_>>();
        assert_eq!(keys(&s), ["df0", "df0.g0", "df0.c1", "df0.o2", "df0.o3", "df0.n2", "df0.n3", "df0.n4", "df0.c4", "df1", "df1.note"]);
        s.set_mode(DiffMode::Split);
        assert_eq!(keys(&s), ["df0", "df0.g0", "df0.c1", "df0.o2", "df0.o3", "df0.n4", "df0.c4", "df1", "df1.note"]);
        for n in [0, 9, 10, 12345, u32::MAX] {
            let mut k = String::new();
            push_number(&mut k, n);
            assert_eq!(k, n.to_string());
        }
        assert_eq!(s.key_of(Row::Ctx { f: 7, old: 120, new: 121 }), "df7.c120");
    }

    #[test]
    fn line_selection_copies_one_side() {
        let old = "keep\nold one\nold two\nend\n";
        let new = "keep\nnew one\nnew two\nend\n";
        let mut s = DiffState::new(vec![FileDiff::from_texts("x", old, new)]);
        s.press_line(0, LineRef::new(Side::New, 1));
        s.release_line();
        s.shift = true;
        s.press_line(0, LineRef::new(Side::New, 4));
        s.release_line();
        assert_eq!(s.selected_text().as_deref(), Some("keep\nnew one\nnew two\nend"));
        s.set_selection(Some(LineRange { file: 0, start: LineRef::new(Side::Old, 3), end: LineRef::new(Side::Old, 2) }));
        assert_eq!(s.selected_text().as_deref(), Some("old one\nold two"));
        // Pressing the one selected line clears it, unless the press drags.
        s.shift = false;
        let one = LineRange::line(0, LineRef::new(Side::New, 1));
        s.press_line(0, one.start);
        s.release_line();
        s.press_line(0, one.start);
        assert_eq!(s.release_line(), None);
        s.press_line(0, one.start);
        s.release_line();
        s.press_line(0, one.start);
        s.drag_to(0, LineRef::new(Side::New, 2));
        assert_eq!(s.release_line(), Some(LineRange { end: LineRef::new(Side::New, 2), ..one }));
    }

    #[test]
    fn cancelling_a_gutter_drag_restores_the_selection_before_the_press() {
        let mut s = sample();
        for original in [None, Some(LineRange::line(0, LineRef::new(Side::Old, 2)))] {
            s.set_selection(original);
            s.press_line(0, LineRef::new(Side::New, 1));
            s.drag_to(0, LineRef::new(Side::New, 3));
            s.cancel_line();
            assert_eq!(s.selection(), original);
            assert!(!s.selecting && !s.unselect);
        }
    }

    #[test]
    fn copy_follows_the_hovered_side_unless_a_press_holds_the_other() {
        let mut s = sample();
        s.hover_text(Side::Old, "a", true);
        assert_eq!(s.copy_side(), Side::Old);
        // A drag that started on the old side enters a new-side cell: the old one stays held.
        s.hover_text(Side::New, "b", true);
        assert_eq!(s.copy_side(), Side::Old);
        s.hover_text(Side::Old, "a", false);
        s.hover_text(Side::New, "b", false);
        s.hover_text(Side::New, "c", true);
        assert_eq!(s.copy_side(), Side::New);
    }

    #[test]
    fn the_visible_file_follows_rows_at_the_top() {
        // Rows: f0 header 0, hunk header 1, lines 2-3; f1 at 4-7; f2 at 8-11.
        let files = (0..3).map(|i| FileDiff::from_texts(format!("f{i}"), "a\n", "b\n")).collect();
        let mut s = DiffState::new(files);
        assert_eq!(s.rows.len(), 12);
        assert_eq!(s.set_visible_rows(0..8), (Some(0), false));
        assert_eq!(s.sticky(), None);
        // Scrolled past file 0's header: file 0 sticks at the top.
        assert_eq!(s.set_visible_rows(1..9), (None, true));
        assert_eq!((s.visible_file(), s.sticky()), (0, Some(0)));
        // At file 1's header: file 1 is visible, its own header row is at the top.
        assert_eq!(s.set_visible_rows(4..12), (Some(1), true));
        assert_eq!((s.visible_file(), s.sticky()), (1, None));
        // Past file 1's header: file 1 sticks.
        assert_eq!(s.set_visible_rows(5..12), (None, true));
        assert_eq!((s.visible_file(), s.sticky()), (1, Some(1)));
        // A jump into file 2's lines.
        assert_eq!(s.set_visible_rows(10..12), (Some(2), true));
        assert_eq!((s.visible_file(), s.sticky()), (2, Some(2)));
        assert_eq!(s.row_from(10, 25.0), Some(11));
    }
}

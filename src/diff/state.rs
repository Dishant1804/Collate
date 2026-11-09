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


//! What a diff is: files of hunks of lines, parsed from a patch or computed from two texts.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use super::myers::{self, Op};

/// The two sides of a diff: the text before the change and after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Old,
    New,
}

/// What a line of a hunk is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Unchanged, on both sides.
    Context,
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
}

/// One line of a hunk.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Line {
    pub kind: LineKind,
    /// Its 1-based number on the old side, unless it was added.
    pub old: Option<u32>,
    /// Its 1-based number on the new side, unless it was removed.
    pub new: Option<u32>,
    /// The line without its line break.
    pub text: String,
    /// Byte ranges of `text` that differ from the line it replaced or was replaced by (word
    /// changes), when it pairs with one and they share enough to say.
    pub changes: Vec<Range<usize>>,
    /// The file ends here without a line break (`\ No newline at end of file`).
    pub no_newline: bool,
}

impl Line {
    fn new(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> Line {
        Line { kind, old, new, text: text.to_owned(), changes: Vec::new(), no_newline: false }
    }

    /// Its number on `side`.
    pub fn number(&self, side: Side) -> Option<u32> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }
}

/// A run of changed lines with the unchanged lines around them.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Hunk {
    /// Where the hunk starts on the old side, as its `@@` header says: the line after which
    /// lines are inserted when `old_count` is 0.
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The text after the header's closing `@@`, often the enclosing function.
    pub section: String,
    pub lines: Vec<Line>,
}

impl Hunk {
    /// The first line number the hunk covers on `side` (its start, or the line after it when
    /// it covers none there).
    pub(crate) fn first(&self, side: Side) -> u32 {
        let (start, count) = match side {
            Side::Old => (self.old_start, self.old_count),
            Side::New => (self.new_start, self.new_count),
        };
        if count == 0 { start.saturating_add(1) } else { start.max(1) }
    }

    /// One past the last line number the hunk covers on `side`.
    pub(crate) fn end(&self, side: Side) -> u32 {
        self.first(side).saturating_add(if side == Side::Old { self.old_count } else { self.new_count })
    }

    /// The header line as a patch writes it, `@@ -1,4 +1,5 @@ section`.
    pub fn header(&self) -> String {
        let range = |start: u32, count: u32| if count == 1 { format!("{start}") } else { format!("{start},{count}") };
        let mut h = format!("@@ -{} +{} @@", range(self.old_start, self.old_count), range(self.new_start, self.new_count));
        if !self.section.is_empty() {
            h.push(' ');
            h.push_str(&self.section);
        }
        h
    }
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

/// One file's changes.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FileDiff {
    /// The path before the change; `None` for an added file.
    pub old_path: Option<String>,
    /// The path after the change; `None` for a deleted file.
    pub new_path: Option<String>,
    pub status: FileStatus,
    /// A rename's or copy's similarity, in percent.
    pub similarity: Option<u8>,
    /// File modes, as `100644`, when the patch gives them.
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// The patch says the file is binary, so it has no lines.
    pub binary: bool,
    /// Shared, not copied, between the watcher's cache and the view: built once, then only
    /// read.
    pub hunks: Arc<Vec<Hunk>>,
    /// Lines that moved, by (hunk, line): where the other end is, as (file index in the
    /// diff, line number on the other side). Set by [`mark_moved`].
    pub moves: std::collections::HashMap<(u32, u32), (u32, u32)>,
    old_text: Option<Arc<Source>>,
    new_text: Option<Arc<Source>>,
    /// The widest line, worked out once: it scans both whole texts, and a cached diff that's
    /// shown again on every refresh shouldn't pay for that again.
    widest: std::cell::Cell<Option<u32>>,
}

/// A whole text, split into lines.
#[derive(Debug)]
pub(crate) struct Source {
    text: String,
    /// Each line's byte range, without its line break.
    lines: Vec<(u32, u32)>,
}

impl Source {
    pub(crate) fn new(text: &str) -> Source {
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, _) in text.match_indices('\n') {
            let end = if text[..i].ends_with('\r') { i - 1 } else { i };
            lines.push((start as u32, end as u32));
            start = i + 1;
        }
        if start < text.len() {
            let end = if text.ends_with('\r') { text.len() - 1 } else { text.len() };
            lines.push((start as u32, end as u32));
        }
        Source { text: text.to_owned(), lines }
    }

    /// Line `n` (1-based).
    pub(crate) fn line(&self, n: u32) -> Option<&str> {
        let &(a, b) = self.lines.get((n as usize).checked_sub(1)?)?;
        Some(&self.text[a as usize..b as usize])
    }

    pub(crate) fn len(&self) -> u32 {
        self.lines.len() as u32
    }

    fn ends_without_newline(&self) -> bool {
        !self.text.is_empty() && !self.text.ends_with('\n')
    }
}

/// Lines of context around each change in a diff computed from texts, as `git diff` keeps.
const CONTEXT: u32 = 3;

impl FileDiff {
    fn empty(old_path: Option<String>, new_path: Option<String>) -> FileDiff {
        FileDiff {
            old_path,
            new_path,
            status: FileStatus::Modified,
            similarity: None,
            old_mode: None,
            new_mode: None,
            binary: false,
            hunks: Arc::new(Vec::new()),
            moves: std::collections::HashMap::new(),
            old_text: None,
            new_text: None,
            widest: std::cell::Cell::new(None),
        }
    }

    /// The diff of `old` to `new`, a file at `path`, with three lines of context around each
    /// change and word changes on paired lines. Both texts stay with it, so every unchanged
    /// range between hunks can be expanded.
    #[cfg(test)]
    pub fn from_texts(path: impl Into<String>, old: &str, new: &str) -> FileDiff {
        let path = path.into();
        FileDiff::between_with_context(Some(path.clone()), Some(path), old, new, CONTEXT)
    }

    /// [`FileDiff::from_texts`] for a file whose path changed (a rename), was added (`old_path`
    /// `None`, `old` empty) or deleted (`new_path` `None`, `new` empty).
    pub fn between(old_path: Option<String>, new_path: Option<String>, old: &str, new: &str) -> FileDiff {
        FileDiff::between_with_context(old_path, new_path, old, new, CONTEXT)
    }

    /// [`FileDiff::between`] with `context` unchanged lines around each change.
    pub fn between_with_context(old_path: Option<String>, new_path: Option<String>, old: &str, new: &str, context: u32) -> FileDiff {
        FileDiff::between_opts(old_path, new_path, old, new, context, false)
    }

    /// [`FileDiff::between_with_context`], comparing lines without their whitespace when
    /// `ignore_whitespace` (as `git diff -w`): a line that only changed indentation or spacing
    /// shows as unchanged, in its new form.
    pub fn between_opts(old_path: Option<String>, new_path: Option<String>, old: &str, new: &str, context: u32, ignore_whitespace: bool) -> FileDiff {
        let mut f = FileDiff::empty(old_path, new_path);
        f.status = match (&f.old_path, &f.new_path) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Deleted,
            (Some(a), Some(b)) if a != b => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        if old.contains('\0') || new.contains('\0') {
            f.binary = true;
            return f;
        }
        let (a, b) = (Arc::new(Source::new(old)), Arc::new(Source::new(new)));
        f.hunks = Arc::new(hunks(&a, &b, context, ignore_whitespace));
        f.old_text = Some(a);
        f.new_text = Some(b);
        f.pair_words();
        f
    }

    /// The path to show: the new one, or the old one for a deleted file.
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("")
    }

    /// Lines added.
    pub fn additions(&self) -> usize {
        self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Added).count()
    }

    /// Lines removed.
    pub fn deletions(&self) -> usize {
        self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Removed).count()
    }

    /// The highlighter language for the path's extension (or name, for a `Makefile`), as
    /// written: `"rs"`, `"tsx"`.
    pub fn language(&self) -> Option<&str> {
        let name = self.path().rsplit('/').next()?;
        match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => Some(ext),
            _ => Some(name).filter(|n| matches!(*n, "Makefile" | "Dockerfile")),
        }
    }

    /// Whether the unchanged lines between hunks can be shown.
    pub fn expandable(&self) -> bool {
        self.old_text.is_some() || self.new_text.is_some()
    }

    /// The only side the file has: `New` for an added file, `Old` for a deleted one, `None`
    /// when it has both. A file diffed from texts counts as added when its old text is empty
    /// and its new one isn't (and the other way round as deleted), unless it moved.
    pub fn sole_side(&self) -> Option<Side> {
        match self.status {
            FileStatus::Added => Some(Side::New),
            FileStatus::Deleted => Some(Side::Old),
            FileStatus::Modified => {
                let empty = |t: &Option<Arc<Source>>| t.as_ref().map(|t| t.text.is_empty());
                match (empty(&self.old_text), empty(&self.new_text)) {
                    (Some(true), Some(false)) => Some(Side::New),
                    (Some(false), Some(true)) => Some(Side::Old),
                    _ => None,
                }
            }
            FileStatus::Renamed | FileStatus::Copied => None,
        }
    }

    /// The whole text of `side`, when the diff has it ([`FileDiff::from_texts`],
    /// [`FileDiff::fold`]).
    pub fn text(&self, side: Side) -> Option<&str> {
        let t = match side {
            Side::Old => &self.old_text,
            Side::New => &self.new_text,
        };
        t.as_deref().map(|t| t.text.as_str())
    }

    /// Unchanged line `n` of the old side (with `new` its number on the new side), from
    /// whichever whole text the diff has.
    pub(crate) fn context_line(&self, old: u32, new: u32) -> Option<&str> {
        self.old_text.as_ref().and_then(|t| t.line(old)).or_else(|| self.new_text.as_ref().and_then(|t| t.line(new)))
    }

    /// How many lines the old side has, when the diff knows: from the old text, or the new
    /// text and the hunks' line counts.
    pub(crate) fn old_len(&self) -> Option<u32> {
        if let Some(t) = &self.old_text {
            return Some(t.len());
        }
        let t = self.new_text.as_ref()?;
        let (added, removed) = (self.additions() as i64, self.deletions() as i64);
        Some((i64::from(t.len()) - added + removed).max(0) as u32)
    }

    /// The widest line it can show, in columns with tabs stopping every four.
    pub(crate) fn columns(&self) -> u32 {
        if let Some(w) = self.widest.get() {
            return w;
        }
        let w = self.scan_columns();
        self.widest.set(Some(w));
        w
    }


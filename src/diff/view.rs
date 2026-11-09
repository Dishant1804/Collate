//! Drawing a [`DiffState`]: a virtual list of file headers, hunk separators and lines, a
//! sticky header for the file being read, and the handlers that feed selection, expansion and
//! the reading position back into the state.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use rune::{Color, Cursor, Drag, El, GrowthSpec, Icon, Phase, ScrollSpec, Span, TextStyle, Tx, col, icon, memo, rich, row, text};

use super::model::{FileStatus, LineKind, Side};
use super::state::{DiffMode, DiffState, EXPAND_STEP, Expand, GAP_H, HEADER_H, LineRef, NOTE_H, Row, Style};
use rune_kit::Kit;
use rune_kit::syntax::SyntaxTheme;

static EXPAND_UP: Icon = Icon {
    name: "kit-diff-expand-up",
    view: 16.0,
    body: r#"<path d="M4.5 9.5L8 6L11.5 9.5" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};
static EXPAND_DOWN: Icon = Icon {
    name: "kit-diff-expand-down",
    view: 16.0,
    body: r#"<path d="M4.5 6.5L8 10L11.5 6.5" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};
static EXPAND_ALL: Icon = Icon {
    name: "kit-diff-expand-all",
    view: 16.0,
    body: r#"<path d="M5 5.5L8 2.5L11 5.5M5 10.5L8 13.5L11 10.5" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};
/// The diff's text edges, shared with the summary above it: a file header's chevron glyph
/// and the last visible control of a header or hunk row sit this far in from the sides.
pub const EDGE: f32 = 16.0;
/// Where a file header's triangle ink starts in its 24 pt toggle box: the 16 pt icon sits 4 pt
/// in and the triangle (x 4–12 of 16) 4 pt further.
const GLYPH_INSET: f32 = 8.0;
/// A small ghost button pads its label this much on each side.
pub const GHOST_PAD: f32 = 8.0;

/// Width of the coloured edge between the gutter and the code of an added or removed line:
/// one continuous stripe down a run of changes, the shape the eye picks up.
const BAR: f32 = 2.0;
/// Space between the line numbers and the code, and after the code.
const CODE_PAD: (f32, f32) = (12.0, 24.0);

type Get<S> = Rc<dyn Fn(&S) -> &DiffState>;
type GetMut<S> = Rc<dyn Fn(&mut S) -> &mut DiffState>;

/// Draws the [`DiffState`] an app keeps in its state: give it the state's accessors and build
/// it in the view. It fills the height it's given and scrolls; Cmd-C, `j`/`k` and `]`/`[` work
/// once the app routes keys to [`DiffState::key`].
///
/// ```ignore
/// DiffView::new(|s: &App| &s.diff, |s: &mut App| &mut s.diff)
///     .on_visible_file(|s: &mut App, f| s.tree.reveal(f))
///     .build(s, &k)
/// ```
#[allow(clippy::type_complexity)]
pub struct DiffView<S> {
    get: Get<S>,
    get_mut: GetMut<S>,
    key: String,
    header: Option<Rc<dyn Fn(&S, usize) -> El>>,
    dim: Option<Rc<dyn Fn(&S, usize) -> bool>>,
    hunk: Option<Rc<dyn Fn(&S, usize, usize) -> El>>,
    on_visible_file: Option<Rc<dyn Fn(&mut S, usize)>>,
}

impl<S: 'static> DiffView<S> {
    pub fn new(get: impl Fn(&S) -> &DiffState + 'static, get_mut: impl Fn(&mut S) -> &mut DiffState + 'static) -> DiffView<S> {
        DiffView {
            get: Rc::new(get),
            get_mut: Rc::new(get_mut),
            key: "diff".into(),
            header: None,
            dim: None,
            hunk: None,
            on_visible_file: None,
        }
    }

    /// Extra controls at the end of each file's header (a "viewed" toggle, a menu), by file.
    pub fn header_actions(mut self, f: impl Fn(&S, usize) -> El + 'static) -> Self {
        self.header = Some(Rc::new(f));
        self
    }

    /// Controls at the end of each hunk's header row (stage, discard), by file and hunk.
    pub fn hunk_actions(mut self, f: impl Fn(&S, usize, usize) -> El + 'static) -> Self {
        self.hunk = Some(Rc::new(f));
        self
    }

    /// Whether a file's header reads as done (a file marked viewed): its path and counts fade.
    pub fn dim(mut self, f: impl Fn(&S, usize) -> bool + 'static) -> Self {
        self.dim = Some(Rc::new(f));
        self
    }

    /// Called when the file at the top of the view changes as it scrolls, to follow it in a
    /// file tree.
    pub fn on_visible_file(mut self, f: impl Fn(&mut S, usize) + 'static) -> Self {
        self.on_visible_file = Some(Rc::new(f));
        self
    }

    pub fn build(self, s: &S, k: &Kit) -> El {
        let st = (self.get)(s);
        let look = Look::new(k, st.style());
        let wrap = st.wraps();
        let key = self.key.clone();
        if !wrap && let Some(r) = rune::memo::layout(&format!("{key}/scroll")) {
            st.set_viewport(r);
        }
        let v = Rc::new(View { look, d: self });
        let count = st.rows.len();
        let mut list = col().key("rows").w_full().growth(GrowthSpec::default()).selection_group(true).role("list").label("Diff");
        if !wrap {
            list = list.min_w(v.content_width(st).max(st.view_width()));
        }
        // Room to scroll past the end, as an editor has, so the last file's header can reach
        // the top when it's picked: otherwise the file above stays "in view" and takes the
        // selection. A spacer after the list, since a virtual list sizes itself from its rows.
        let tail = rune::memo::layout(&format!("{key}/scroll")).map_or(0.0, |r| (r[3] - HEADER_H).max(0.0));
        let build = v.clone();
        let vis_mut = v.d.get_mut.clone();
        let on_file = v.d.on_visible_file.clone();
        let list = list
            .virtual_rows(count, v.look.line_h, keys(v.d.get.clone()), move |s: &S, i| build.row(s, i))
            .virtual_rows_version(st.keys_rev)
            .on_visible_rows(move |s: &mut S, rows| {
                let (reported, sticky_changed) = vis_mut(s).set_visible_rows(rows);
                if let (Some(f), Some(cb)) = (reported, &on_file) {
                    cb(s, f);
                    return true;
                }
                sticky_changed
            });
        let scroll = col().key("scroll").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false });
        let scroll = if wrap { scroll } else { scroll.scroll_xy() };
        let scroll = scroll.child(list).child(col().key(super::state::TAIL_KEY).w_full().h(tail).a11y_hidden());
        let mut root = col().key(key).w_full().grow(1.0).min_h(0.0).fill(v.look.bg).child(scroll);
        if let Some(f) = st.sticky() {
            root = root.child(v.file_header(s, st, f, true).key("sticky").absolute(0.0, 0.0).w_full());
        }
        root
    }
}

/// The row-key closure for [`El::virtual_rows`]: made on demand, for the rows on screen.
fn keys<S: 'static>(get: Get<S>) -> impl for<'a> Fn(&'a S, usize) -> Cow<'a, str> + 'static {
    move |s, i| Cow::Owned(get(s).row_key(i))
}

/// Colours and type, from the kit's palette so themes restyle them.
struct Look {
    line_h: f32,
    one_number: bool,
    markers: bool,
    word_diff: bool,
    font: TextStyle,
    label: TextStyle,
    small: TextStyle,
    /// The advance of a mono column.
    adv: f32,
    bg: Color,
    text: Color,
    muted: Color,
    faint: Color,
    border: Color,
    header_bg: Color,
    hover: Color,
    add: Tints,
    del: Tints,
    /// Lines that moved, added or removed: one hue for both ends of a move.
    moved: Tints,
    /// A search match's background.
    found: Color,
    gap_bg: Color,
    empty_bg: Color,
    selected: Color,
    selected_gutter: Color,
    selection: Color,
    accent: Color,
    triangle_down: &'static Icon,
    triangle_right: &'static Icon,
    syntax: SyntaxTheme,
    /// Changes when anything above does, for memoised rows.
    version: u64,
}

#[derive(Clone, Copy)]
struct Tints {
    line: Color,
    gutter: Color,
    word: Color,
    number: Color,
    bar: Color,
    count: Color,
}

impl Look {
    fn new(k: &Kit, style: Style) -> Look {
        let c = &k.c;
        let bg = c.bg_base;
        let dark = k.dark;
        let tints = |hue: Color, fg: Color| Tints {
            line: tint(bg, hue, if dark { 0.13 } else { 0.1 }),
            gutter: tint(bg, hue, if dark { 0.2 } else { 0.17 }),
            word: tint(bg, hue, if dark { 0.32 } else { 0.27 }),
            number: tint(c.text_muted, hue, 0.7),
            bar: hue,
            count: fg,
        };
        // Code is monospaced: no kerning to look up, and no ligatures turning `!=` into `≠`.
        let font = TextStyle { size: style.font_size, line_height: style.line_h, features: "'kern' 0, 'liga' 0, 'calt' 0", ..k.ty.mono };
        let info = c.info.icon;
        let mut look = Look {
            line_h: style.line_h,
            one_number: style.one_number,
            markers: style.markers,
            word_diff: style.word_diff,
            font,
            label: TextStyle { size: 12.5, line_height: 20.0, ..k.ty.mono },
            small: TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono },
            adv: 0.6 * font.size,
            bg,
            text: c.text_base,
            muted: c.text_muted,
            faint: c.text_faint,
            border: c.border_weak,
            header_bg: c.layer_01,
            hover: k.alpha(c.text_base, if dark { 0.06 } else { 0.035 }),
            add: tints(c.success.icon, c.success.fg),
            del: tints(c.danger.icon, c.danger.fg),
            moved: tints(c.info.icon, c.info.fg),
            found: tint(bg, c.warning.icon, if dark { 0.45 } else { 0.4 }),
            gap_bg: tint(bg, info, if dark { 0.1 } else { 0.06 }),
            empty_bg: tint(bg, c.text_base, if dark { 0.03 } else { 0.025 }),
            selected: tint(bg, info, if dark { 0.2 } else { 0.14 }),
            selected_gutter: tint(bg, info, if dark { 0.32 } else { 0.26 }),
            selection: k.selection(),
            accent: info,
            triangle_down: k.glyph.triangle_down,
            triangle_right: k.glyph.triangle_right,
            syntax: SyntaxTheme::from_kit(k),
            version: 0,
        };
        let mut h = std::hash::DefaultHasher::new();
        for c in [look.bg, look.text, look.add.bar, look.del.bar, look.accent, look.header_bg] {
            [c.r, c.g, c.b, c.a].map(f32::to_bits).hash(&mut h);
        }
        look.font.size.to_bits().hash(&mut h);
        look.line_h.to_bits().hash(&mut h);
        (style.one_number, style.markers, style.word_diff).hash(&mut h);
        look.version = h.finish();
        look
    }

    fn tints(&self, kind: LineKind, moved: bool) -> Option<&Tints> {
        match kind {
            LineKind::Added | LineKind::Removed if moved => Some(&self.moved),
            LineKind::Added => Some(&self.add),
            LineKind::Removed => Some(&self.del),
            LineKind::Context => None,
        }
    }

    fn gutter_fill(&self, kind: LineKind, moved: bool, selected: bool) -> Color {
        match (selected, self.tints(kind, moved)) {
            (true, _) => self.selected_gutter,
            (false, Some(t)) => t.gutter,
            (false, None) => self.bg,
        }
    }

    /// One line-number column for numbers of `digits` digits.
    fn number_w(&self, digits: u32) -> f32 {
        (digits as f32 * self.adv + 16.0).ceil()
    }

    /// The `+`/`−` column, when shown.
    fn marker_w(&self) -> f32 {
        if self.markers { (self.adv + 8.0).ceil() } else { 0.0 }
    }

    /// Everything left of the code: numbers, marker and edge.
    fn gutter_w(&self, digits: u32, unified: bool) -> f32 {
        let columns = if unified && !self.one_number { 2.0 } else { 1.0 };
        columns * self.number_w(digits) + self.marker_w() + BAR
    }
}

/// One side's line in a row: what a cell draws.
#[derive(Clone, Copy)]
struct Cell<'a> {
    moved: bool,
    twin: Option<(u32, u32)>,
    kind: LineKind,
    old: Option<u32>,
    new: Option<u32>,
    text: &'a str,
    changes: &'a [Range<usize>],
    no_newline: bool,
}

impl Cell<'_> {
    /// The side its text is read from, and its number there.
    fn source(&self) -> (Side, u32) {
        match (self.kind, self.old, self.new) {
            (LineKind::Removed, Some(n), _) => (Side::Old, n),
            (_, _, Some(n)) => (Side::New, n),
            (_, Some(n), None) => (Side::Old, n),
            _ => (Side::New, 1),
        }
    }

    fn label(&self) -> String {
        let (_, n) = self.source();
        let kind = match self.kind {
            LineKind::Added if self.moved => "moved here",
            LineKind::Removed if self.moved => "moved away",
            LineKind::Added => "added",
            LineKind::Removed => "removed",
            LineKind::Context => "unchanged",
        };
        format!("{kind} line {n}: {}", self.text)
    }
}

struct View<S> {
    d: DiffView<S>,
    look: Look,
}

impl<S: 'static> View<S> {
    /// How wide unwrapped (unified) content is, so the list doesn't change width as rows are built.
    fn content_width(&self, st: &DiffState) -> f32 {
        let l = &self.look;
        let digits = (0..st.files().len()).map(|f| st.digits(f)).max().unwrap_or(3);
        l.gutter_w(digits, true) + st.columns() as f32 * l.adv + CODE_PAD.0 + CODE_PAD.1
    }

    /// The most a full-width part of a row (a header) spans: the viewport, not
    /// the unwrapped content.
    fn view_w(&self, st: &DiffState, split_half: bool) -> Option<f32> {
        let w = st.view_width();
        (w > 0.0 && !st.wraps()).then_some(if split_half { (w / 2.0).floor() } else { w })
    }

    fn row(&self, s: &S, i: usize) -> El {
        let st = (self.d.get)(s);
        let r = st.rows[i];
        let key = &st.row_key(i);
        match r {
            Row::File(f) => self.file_header(s, st, f as usize, false),
            Row::Note(f) => memo(key, (self.look.version, st.rev, st.view_width().to_bits()), || self.note(st, f as usize)),
            // A hunk's actions read app state, so those rows build every commit; there are few.
            Row::Gap { f, gap } if self.d.hunk.is_some() => self.gap(s, st, f as usize, gap as usize),
            Row::Gap { f, gap } => memo(key, (self.look.version, st.rev, st.view_width().to_bits()), || self.gap(s, st, f as usize, gap as usize)),
            _ => {
                let version = (self.look.version, st.rev, st.row_selected(i, None), st.copy_side(), st.selection().map(|s| s.start.side), st.search_rev());
                memo(key, version, || self.line(st, i))
            }
        }
    }

    fn cells<'a>(&self, st: &'a DiffState, r: Row) -> (Option<Cell<'a>>, Option<Cell<'a>>) {
        let of = |f: u32, h: u32, i: u32| {
            let file = &st.files()[f as usize];
            let l = &file.hunks[h as usize].lines[i as usize];
            let twin = file.twin(h as usize, i as usize);
            Cell { moved: twin.is_some(), twin, kind: l.kind, old: l.old, new: l.new, text: &l.text, changes: &l.changes, no_newline: l.no_newline }
        };
        match r {
            Row::Ctx { f, old, new } => {
                let text = st.side_line(f as usize, Side::New, new).or_else(|| st.side_line(f as usize, Side::Old, old)).unwrap_or("");
                let c = Cell { moved: false, twin: None, kind: LineKind::Context, old: Some(old), new: Some(new), text, changes: &[], no_newline: false };
                (Some(c), None)
            }
            Row::Line { f, h, i } => (Some(of(f, h, i)), None),
            Row::Pair { f, h, del, add } => (Some(of(f, h, del)), Some(of(f, h, add))),
            _ => (None, None),
        }
    }

    fn line(&self, st: &DiffState, i: usize) -> El {
        let r = st.rows[i];
        let f = r.file();
        let (a, b) = self.cells(st, r);
        let Some(a) = a else { return col() };
        match st.mode() {
            DiffMode::Unified => self.unified(st, i, f, a),
            DiffMode::Split => {
                let (old, new) = match (b, a.kind) {
                    (Some(b), _) => (Some(a), Some(b)),
                    (None, LineKind::Removed) => (Some(a), None),
                    (None, LineKind::Added) => (None, Some(a)),
                    (None, LineKind::Context) => (Some(a), Some(a)),
                };
                self.split(st, i, f, old, new)
            }
        }
    }

    fn unified(&self, st: &DiffState, i: usize, f: usize, c: Cell<'_>) -> El {
        let l = &self.look;
        let selected = st.row_selected(i, None);
        let tints = l.tints(c.kind, c.moved);
        let fill = if selected { l.selected } else { tints.map_or(l.bg, |t| t.line) };
        let digits = st.digits(f);
        let copy = match c.kind {
            LineKind::Context => true,
            LineKind::Added => st.copy_side() == Side::New,
            LineKind::Removed => st.copy_side() == Side::Old,
        };
        let hover_side = if c.kind == LineKind::Removed { Side::Old } else { Side::New };
        // The marker, numbers and "+" hold to the viewport's left edge while the code scrolls
        // sideways under them, on the gutter's own opaque fill.
        let mut gutter = row()
            .key("gutter")
            .self_stretch()
            .items_stretch()
            .fill(l.gutter_fill(c.kind, c.moved, selected));
        gutter = if l.one_number {
            let (n, side) = match (c.kind, c.new) {
                (LineKind::Removed, _) | (_, None) => (c.old, Side::Old),
                (_, n) => (n, Side::New),
            };
            gutter.child(self.number(st, i, f, n, side, c.kind, c.moved, selected, digits).key("nn"))
        } else {
            gutter
                .child(self.number(st, i, f, c.old, Side::Old, c.kind, c.moved, selected, digits).key("no"))
                .child(self.number(st, i, f, c.new, Side::New, c.kind, c.moved, selected, digits).key("nn"))
        };
        gutter = gutter.child(self.twin_help(st, &c, self.marker(c.kind, c.moved))).child(self.bar(c.kind, c.moved));
        if !st.wraps() {
            gutter = gutter.sticky_left(0.0);
        }
        row()
            .w_full()
            .min_h(self.look.line_h)
            .items_start()
            .fill(fill)
            .hover(l.hover)
            .hover_group()
            .role("listitem")
            .label(c.label())
            .child(gutter)
            .child(self.code(st, f, &c, copy, hover_side, &st.row_key(i)).key("code"))
    }

    fn split<'a>(&self, st: &'a DiffState, i: usize, f: usize, old: Option<Cell<'a>>, new: Option<Cell<'a>>) -> El {
        let digits = st.digits(f);
        let single = st.single_column(f);
        let half = |side: Side, c: Option<Cell<'a>>| -> El {
            let l = &self.look;
            let key = if side == Side::Old { "old" } else { "new" };
            let h = row().key(key).w_frac(if single.is_some() { 1.0 } else { 0.5 }).min_w(0.0).min_h(self.look.line_h).items_start().self_stretch();
            let Some(c) = c else {
                return h.fill(l.empty_bg);
            };
            // An unchanged line shows on both sides as context.
            let kind = c.kind;
            let selected = st.row_selected(i, Some(side));
            let fill = if selected { l.selected } else { l.tints(kind, c.moved).map_or(l.bg, |t| t.line) };
            let n = if side == Side::Old { c.old } else { c.new };
            let mut cell = c;
            if kind == LineKind::Context {
                cell.text = st.side_line(f, side, n.unwrap_or(1)).unwrap_or(c.text);
            }
            let copy = st.copy_side() == side;
            h.fill(fill)
                .hover(l.hover)
                .hover_group()
                .role("listitem")
                .label(cell.label())
                .child(self.number(st, i, f, n, side, kind, c.moved, selected, digits).key("n"))
                .child(self.twin_help(st, &c, self.marker(kind, c.moved).fill(l.gutter_fill(kind, c.moved, selected))))
                .child(self.bar(kind, c.moved))
                .child(self.code(st, f, &cell, copy, side, &st.row_key(i)).key("code"))
        };
        let line = row().w_full().min_h(self.look.line_h).items_stretch();
        // An added or deleted file has one side: it takes the whole width, as one column.
        match single {
            Some(Side::Old) => line.child(half(Side::Old, old)),
            Some(Side::New) => line.child(half(Side::New, new)),
            None => {
                let divider = col().key("divider").w(1.0).self_stretch().fill(self.look.border);
                line.child(half(Side::Old, old)).child(divider).child(half(Side::New, new))
            }
        }
    }

    /// For a moved line, the marker says where the other end is.
    fn twin_help(&self, st: &DiffState, c: &Cell<'_>, marker: El) -> El {
        let Some((f, n)) = c.twin else { return marker };
        let path = st.files().get(f as usize).map_or("", |f| f.path());
        let way = if c.kind == LineKind::Removed { "Moved to" } else { "Moved from" };
        marker.help(format!("{way} {path}:{n} (m jumps there)"))
    }

    /// The `+` or `−` beside the numbers, in the line's colour.
    fn marker(&self, kind: LineKind, moved: bool) -> El {
        let l = &self.look;
        let mut m = row().key("mark").w(l.marker_w()).min_h(l.line_h).self_stretch().justify_center().items_start().a11y_hidden();
        if !l.markers {
            return m;
        }
        let glyph = match kind {
            LineKind::Added => "+",
            LineKind::Removed => "\u{2212}",
            LineKind::Context => return m,
        };
        if let Some(t) = l.tints(kind, moved) {
            m = m.child(text(glyph).font(l.font).color(t.count));
        }
        m
    }

    fn bar(&self, kind: LineKind, moved: bool) -> El {
        let b = col().key("bar").w(BAR).self_stretch().a11y_hidden();
        match self.look.tints(kind, moved) {
            Some(t) => b.fill(t.bar),
            None => b,
        }
    }

    /// A line-number cell: pressing it selects the line, shift-pressing extends the selection
    /// to it, and dragging down the numbers selects the lines passed.
    #[allow(clippy::too_many_arguments)]
    fn number(&self, st: &DiffState, i: usize, f: usize, n: Option<u32>, side: Side, kind: LineKind, moved: bool, selected: bool, digits: u32) -> El {
        let l = &self.look;
        let unified = st.mode() == DiffMode::Unified;
        let mut c = row().w(l.number_w(digits)).min_h(self.look.line_h).self_stretch().px(8.0).justify_end().items_start().a11y_hidden();
        if !unified {
            c = c.fill(l.gutter_fill(kind, moved, selected));
        }
        if let Some(n) = n {
            let color = if selected { l.text } else { l.tints(kind, moved).map_or(l.faint, |t| t.number) };
            c = c.child(text(n.to_string()).font(l.font).color(color));
        }
        let at = n.map(|n| LineRef::new(side, n)).or_else(|| {
            let other = if side == Side::Old { Side::New } else { Side::Old };
            unified.then(|| st.row_line(st.rows[i], other).map(|n| LineRef::new(other, n))).flatten()
        });
        let Some(at) = at else { return c };
        let get = self.d.get_mut.clone();
        let row_at = i as u32;
        c.cursor(Cursor::Pointer).on_drag(move |s: &mut S, d: &mut Drag, _| {
            let st = get(s);
            match d.phase {
                Phase::Down => st.press_line(f, at),
                Phase::Move => {
                    let Some(r) = st.row_from(row_at, d.y) else { return };
                    let r = st.rows[r as usize];
                    if r.file() != f {
                        return;
                    }
                    // Unified rows hold one side or both: keep to the pressed side where it can.
                    let to = st.row_line(r, side).map(|n| LineRef::new(side, n)).or_else(|| {
                        let other = if side == Side::Old { Side::New } else { Side::Old };
                        unified.then(|| st.row_line(r, other).map(|n| LineRef::new(other, n))).flatten()
                    });
                    if let Some(to) = to {
                        st.drag_to(f, to);
                    }
                }
                Phase::Up => {
                    if d.cancelled() {
                        st.cancel_line();
                        return;
                    }
                    st.release_line();
                }
            }
        })
    }

    /// The code of a line: syntax colours under word changes, tabs padded to four columns.
    fn code(&self, st: &DiffState, f: usize, c: &Cell<'_>, copy: bool, side: Side, row_key: &str) -> El {
        let l = &self.look;
        let (src, n) = c.source();
        let syntax: Vec<(Range<usize>, Color)> = st
            .highlights(f, src, n)
            .into_iter()
            .map(|(r, p)| (r, l.syntax.paint(p)))
            .collect();
        let word = l.tints(c.kind, c.moved).map(|t| t.word).filter(|_| l.word_diff && !c.moved);
        let mut spans = layer(c.text.len(), &syntax, word.map_or(&[][..], |_| c.changes), word);
        if let Some(q) = st.search() {
            spans = mark(c.text, spans, q, l.found);
        }
        let (shown, spans, pads) = tabbed(c.text, spans, st.wraps());
        let mut t = rich(shown, spans).font(l.font).color(l.text).key("text");
        t = if st.wraps() { t.w_full() } else { t.single_line() };
        if copy {
            t = t.selectable(true).selection_color(l.selection);
            t = pads.into_iter().fold(t, |t, r| t.uncopied(r));
        }
        let (get, cell) = (self.d.get_mut.clone(), format!("{row_key}.{}", side as u8));
        let mut code = col().grow(1.0).min_w(0.0).pl(CODE_PAD.0).pr(CODE_PAD.1).child(t).on_hover_change(move |s: &mut S, inside, tx: &mut Tx| {
            if !get(s).hover_text(side, &cell, inside) { tx.live_only(); }
        });
        if c.no_newline {
            code = code.child(text("No newline at end of file").font(l.small).color(l.muted).opacity(0.6).key("eof").pb(2.0));
        }
        code
    }

    fn gutter_w(&self, st: &DiffState, f: usize) -> f32 {
        self.look.gutter_w(st.digits(f), st.mode() == DiffMode::Unified)
    }


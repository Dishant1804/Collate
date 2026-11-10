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
    images: Option<(u64, Rc<dyn Fn(&S, usize) -> El>)>,
    comments: Option<(u64, Rc<dyn Fn(&S, usize) -> El>)>,
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
            images: None,
            comments: None,
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
    /// Draws a changed image's row (a file with [`FileDiff::images`](super::FileDiff)),
    /// [`IMAGE_H`](super::IMAGE_H) high; `version` changes whenever what it draws does.
    pub fn images(mut self, version: u64, f: impl Fn(&S, usize) -> El + 'static) -> Self {
        self.images = Some((version, Rc::new(f)));
        self
    }

    /// Draws comment `n` (see [`DiffState::set_comments`]) under its line; `version` changes
    /// whenever what any comment draws does.
    pub fn comments(mut self, version: u64, f: impl Fn(&S, usize) -> El + 'static) -> Self {
        self.comments = Some((version, Rc::new(f)));
        self
    }

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
        let mut root = col().key(key.clone()).w_full().grow(1.0).min_h(0.0).fill(v.look.bg).child(scroll);
        if let Some(f) = st.sticky() {
            root = root.child(v.file_header(s, st, f, true).key("sticky").absolute(0.0, 0.0).w_full());
        }
        if let Some(r) = rune::memo::layout(&format!("{key}/scroll")) {
            root = root.child(v.overview(st, k, r[2], r[3]));
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

/// The overview's width at the right edge, and its marks' width inside it.
const OVERVIEW_W: f32 = 10.0;
const MARK_W: f32 = 4.0;

impl<S: 'static> View<S> {
    /// Where the changes are in the whole diff, as marks down the right edge beside the scroll
    /// bar, green for added lines, red for removed and the accent for both; one scrolls there.
    fn overview(&self, st: &DiffState, k: &Kit, w: f32, h: f32) -> El {
        let mut ruler = col().key("overview").w(OVERVIEW_W).h(h).absolute(w - OVERVIEW_W, 0.0).a11y_hidden();
        // Runs closer than a point or two draw as one mark: a long diff has thousands.
        let mut marks: Vec<super::state::Mark> = Vec::new();
        for m in &st.overview {
            match marks.last_mut() {
                Some(last) if m.top * h <= last.bottom * h + 2.0 => {
                    last.bottom = m.bottom;
                    last.adds |= m.adds;
                    last.dels |= m.dels;
                }
                _ => marks.push(*m),
            }
        }
        for (n, m) in marks.iter().enumerate() {
            let color = match (m.adds, m.dels) {
                (true, false) => k.c.success.fg,
                (false, true) => k.c.danger.fg,
                _ => k.c.text_accent,
            };
            let get_mut = self.d.get_mut.clone();
            let row_at = m.row;
            ruler = ruler.child(
                row()
                    .key(format!("m{n}"))
                    .absolute((OVERVIEW_W - MARK_W) / 2.0, (m.top * h).floor())
                    .w(MARK_W)
                    .h(((m.bottom - m.top) * h).max(2.0))
                    .radius(1.0)
                    .fill(k.alpha(color, 0.8))
                    .cursor(Cursor::Pointer)
                    .on_press_tx(move |s: &mut S, tx: &mut Tx| get_mut(s).reveal_below_header(row_at, tx)),
            );
        }
        ruler
    }

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
            Row::Note(f) => {
                let images = self.d.images.as_ref().filter(|_| st.files()[f as usize].images.is_some());
                match images {
                    Some((version, draw)) => memo(key, (self.look.version, st.rev, st.view_width().to_bits(), *version), || draw(s, f as usize)),
                    None => memo(key, (self.look.version, st.rev, st.view_width().to_bits(), 0u64), || self.note(st, f as usize)),
                }
            }
            Row::Comment { n, .. } => match &self.d.comments {
                Some((version, draw)) => memo(key, (self.look.version, st.rev, st.view_width().to_bits(), *version), || draw(s, n as usize)),
                None => row(),
            },
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

    /// The unchanged lines between two hunks, folded: buttons to show more, and the next
    /// hunk's header.
    fn gap(&self, s: &S, st: &DiffState, f: usize, gap: usize) -> El {
        let l = &self.look;
        let file = &st.files()[f];
        let hidden = st.hidden(f, gap).unwrap_or(0);
        let last = gap == file.hunks.len();
        let expandable = file.expandable();
        let mut buttons = Vec::new();
        // Nothing folded (a hunk's own header row): nothing to show more of.
        if expandable && hidden > 0 {
            let mut add = |how: Expand, glyph: &'static Icon, label: String| {
                let get = self.d.get_mut.clone();
                buttons.push(
                    row()
                        .key(format!("{how:?}"))
                        .size(24.0, 24.0)
                        .center()
                        .radius(6.0)
                        .hover(l.hover)
                        .cursor(Cursor::Pointer)
                        .role("button")
                        .label(label)
                        .child(icon(glyph, 16.0).color(l.muted))
                        .on_press_tx(move |s: &mut S, _| get(s).expand(f, gap, how)),
                );
            };
            let step = hidden.min(EXPAND_STEP);
            if hidden <= EXPAND_STEP {
                add(Expand::All, &EXPAND_ALL, format!("Show {hidden} hidden lines"));
            } else {
                if gap > 0 {
                    add(Expand::Down, &EXPAND_DOWN, format!("Show {step} more lines below"));
                }
                if !last {
                    add(Expand::Up, &EXPAND_UP, format!("Show {step} more lines above"));
                }
            }
        }
        let buttons = row().key("expand").w(self.gutter_w(st, f)).h(GAP_H).center().gap(2.0).children(buttons);
        let mut inner = row().key("inner").h(GAP_H).items_center().gap(12.0).child(buttons);
        inner = match self.view_w(st, false) {
            Some(w) => inner.w(w).sticky_left(0.0),
            None => inner.w_full(),
        };
        if hidden > 0 {
            let count = text(if hidden == 1 { "1 unchanged line".to_owned() } else { format!("{hidden} unchanged lines") }).font(l.small).color(l.muted).key("count");
            let count = if expandable && hidden > EXPAND_STEP {
                let get = self.d.get_mut.clone();
                row().key("all").px(6.0).h(24.0).items_center().radius(6.0).hover(l.hover).cursor(Cursor::Pointer).role("button").label(format!("Show all {hidden} lines")).on_press_tx(move |s: &mut S, _| get(s).expand(f, gap, Expand::All)).child(count)
            } else {
                row().key("all").px(6.0).child(count)
            };
            inner = inner.child(count);
        }
        if let Some(h) = file.hunks.get(gap) {
            inner = inner.child(text(h.header()).font(l.small).color(l.faint).single_line().min_w(0.0).shrink().key("header"));
            if let Some(actions) = &self.d.hunk {
                inner = inner.child(row().key("space").grow(1.0)).child(actions(s, f, gap).key("actions").pr(EDGE - GHOST_PAD));
            }
        }
        let label = match file.hunks.get(gap) {
            Some(h) => format!("{hidden} unchanged lines, then {}", h.header()),
            None => format!("{hidden} unchanged lines"),
        };
        row().w_full().h(GAP_H).items_center().fill(l.gap_bg).role("group").label(label).child(inner)
    }

    /// A file with no lines to show.
    fn note(&self, st: &DiffState, f: usize) -> El {
        let l = &self.look;
        let file = &st.files()[f];
        let msg = if file.binary {
            "Binary file not shown"
        } else if file.status == FileStatus::Renamed || file.status == FileStatus::Copied {
            "File moved without changes"
        } else if file.old_mode != file.new_mode {
            "File mode changed"
        } else if file.status == FileStatus::Added || file.status == FileStatus::Deleted {
            "Empty file"
        } else {
            "No changes"
        };
        let mut inner = row().key("inner").h(NOTE_H).items_center().pl(self.gutter_w(st, f) + CODE_PAD.0).child(text(msg).font(l.small).color(l.muted));
        if let Some(w) = self.view_w(st, false) {
            inner = inner.max_w(w).sticky_left(0.0);
        }
        row().w_full().h(NOTE_H).fill(l.bg).role("note").label(msg).child(inner)
    }

    fn file_header(&self, s: &S, st: &DiffState, f: usize, sticky: bool) -> El {
        let l = &self.look;
        let file = &st.files()[f];
        let collapsed = st.collapsed(f);
        let get = self.d.get_mut.clone();
        let toggle = row()
            .key("toggle")
            .size(24.0, 24.0)
            .center()
            .radius(6.0)
            .hover(l.hover)
            .cursor(Cursor::Pointer)
            .role("button")
            .label(if collapsed { "Expand file" } else { "Collapse file" }).expanded(!collapsed)
            .child(icon(if collapsed { l.triangle_right } else { l.triangle_down }, 16.0).color(l.muted))
            .on_press_tx(move |s: &mut S, tx| {
                let st = get(s);
                st.set_collapsed(f, !st.collapsed(f));
                if sticky {
                    st.scroll_to_file(f, tx);
                }
            });
        let dim = self.d.dim.as_ref().is_some_and(|d| d(s, f));
        let faded = if dim { 0.55 } else { 1.0 };
        // The folder reads muted and the file name bright, so a column of paths scans by name.
        let (old, new) = (file.old_path.as_deref(), file.new_path.as_deref());
        let split_path = |p: &str| -> El {
            let cut = p.rfind('/').map_or(0, |i| i + 1);
            let mut r = row().items_center().min_w(0.0).shrink();
            if cut > 0 {
                r = r.child(text(p[..cut].to_owned()).font(l.label).color(l.muted).truncate_head().min_w(0.0).shrink().key("dir"));
            }
            r.child(text(p[cut..].to_owned()).font(l.label).color(l.text).key("base"))
        };
        let mut name = row().key("path").items_center().gap(6.0).min_w(0.0).shrink().opacity(faded);
        name = match (file.status, old, new) {
            (FileStatus::Renamed | FileStatus::Copied, Some(a), Some(b)) => name
                .child(text(a.to_owned()).font(l.label).color(l.muted).truncate_head().min_w(0.0).shrink().key("old"))
                .child(text("→").font(l.label).color(l.faint).key("arrow"))
                .child(split_path(b).key("new")),
            _ => name.child(split_path(file.path()).key("new")),
        };
        let status = match file.status {
            FileStatus::Added => Some(("added", l.add.count)),
            FileStatus::Deleted => Some(("deleted", l.del.count)),
            FileStatus::Renamed => Some(("renamed", l.muted)),
            FileStatus::Copied => Some(("copied", l.muted)),
            FileStatus::Modified => None,
        };
        // The chevron's glyph, not its box, starts on the edge; the viewed box (bordered, so
        // its edge shows) ends on it.
        let mut inner = row().key("inner").w_full().grow(1.0).pl(EDGE - GLYPH_INSET).pr(EDGE).gap(8.0).items_center().child(toggle).child(name);
        if let Some((word, color)) = status {
            let badge = row().key("status").h(18.0).px(6.0).items_center().radius(4.0).border(1.0, l.border).child(text(word).font(l.small).color(color));
            inner = inner.child(badge.opacity(faded));
        }
        if let (Some(a), Some(b)) = (&file.old_mode, &file.new_mode)
            && a != b
        {
            inner = inner.child(text(format!("{a} → {b}")).font(l.small).color(l.faint).key("mode"));
        }
        inner = inner.child(row().key("space").grow(1.0).a11y_hidden());
        let (adds, dels) = st.stat(f);
        let mut counts = row().key("counts").gap(6.0).items_center().opacity(faded);
        if adds > 0 {
            counts = counts.child(text(format!("+{adds}")).font(l.small).color(l.add.count).key("adds"));
        }
        if dels > 0 {
            counts = counts.child(text(format!("−{dels}")).font(l.small).color(l.del.count).key("dels"));
        }
        inner = inner.child(counts.child(stat_bar(adds, dels, 40.0, l)));
        if let Some(h) = &self.d.header {
            inner = inner.child(h(s, f).key("actions"));
        }
        let mut label = file.path().to_owned();
        if let (FileStatus::Renamed, Some(a)) = (file.status, old) {
            label.push_str(&format!(", renamed from {a}"));
        }
        label.push_str(&format!(", {adds} added, {dels} removed"));
        let line = |key: &'static str, c: Color| col().key(key).w_full().h(1.0).fill(c).a11y_hidden();
        let mut h = col().w_full().h(HEADER_H).fill(l.header_bg).role("heading").label(label);
        // In the list, as wide as the view and held to its left edge while the code scrolls
        // sideways, so the counts and actions stay in sight.
        if let (false, Some(w)) = (sticky, self.view_w(st, false)) {
            h = h.w(w).sticky_left(0.0);
        }
        h = h.child(line("top", if sticky { l.header_bg } else { l.border }));
        h.child(inner).child(line("bottom", l.border))
    }
}

/// A proportional added/removed bar, as a review list shows beside each file; empty when
/// nothing changed line by line.
fn stat_bar(adds: usize, dels: usize, w: f32, l: &Look) -> El {
    let bar = row().key("stat").w(w).h(4.0).radius(2.0).fill(l.border).a11y_hidden();
    let total = (adds + dels) as f32;
    if total == 0.0 {
        return bar.opacity(0.0);
    }
    let add_w = (w * adds as f32 / total).round();
    bar.child(col().key("a").w(add_w).h_full().radius(2.0).fill(l.add.bar)).child(col().key("d").w(w - add_w).h_full().radius(2.0).fill(l.del.bar))
}

/// Spans for a line from its syntax colours and the byte ranges given `bg` (word changes):
/// cut where either starts or ends, so each piece has one colour and one background.
fn layer(len: usize, syntax: &[(Range<usize>, Color)], words: &[Range<usize>], bg: Option<Color>) -> Vec<Span> {
    let mut cuts: Vec<usize> = syntax.iter().flat_map(|(r, _)| [r.start, r.end]).chain(words.iter().flat_map(|r| [r.start, r.end])).collect();
    cuts.retain(|&c| c <= len);
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<Span> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let color = syntax.get(syntax.partition_point(|(r, _)| r.end <= a)).filter(|(r, _)| r.start <= a).map(|(_, c)| *c);
        let on = words.get(words.partition_point(|r| r.end <= a)).is_some_and(|r| r.start <= a);
        let bg = bg.filter(|_| on);
        if color.is_none() && bg.is_none() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.end as usize == a && last.color == color && last.bg == bg => last.end = b as u32,
            _ => {
                let mut sp = Span::new(a, b);
                sp.color = color;
                sp.bg = bg;
                out.push(sp);
            }
        }
    }
    out
}

/// `spans` with every match of `q` in `text` given background `bg`, over whatever colours and
/// word changes are there.
fn mark(text: &str, spans: Vec<Span>, q: &super::Finder, bg: Color) -> Vec<Span> {
    let hits = q.find_all(text);
    if hits.is_empty() {
        return spans;
    }
    let mut cuts: Vec<usize> = spans.iter().flat_map(|s| [s.start as usize, s.end as usize]).chain(hits.iter().flat_map(|r| [r.start, r.end])).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<Span> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let base = spans.iter().find(|s| (s.start as usize) <= a && a < s.end as usize);
        let hit = hits.iter().any(|r| r.start <= a && a < r.end);
        if base.is_none() && !hit {
            continue;
        }
        let mut sp = Span::new(a, b);
        sp.color = base.and_then(|s| s.color);
        sp.bg = if hit { Some(bg) } else { base.and_then(|s| s.bg) };
        out.push(sp);
    }
    out
}

/// `code` as drawn: text layout draws a tab as one space, so each tab is followed by spaces up
/// to the next four-column stop; and when `wrap`, a zero-width space after `.`, `(`, `[` and
/// `/` lets a long call chain or path break there. Returns the text, `spans` moved to match,
/// and the inserted ranges (drawn, never copied).
fn tabbed(code: &str, mut spans: Vec<Span>, wrap: bool) -> (String, Vec<Span>, Vec<Range<usize>>) {
    if !code.contains('\t') && !(wrap && code.contains(['.', '(', '[', '/'])) {
        return (code.to_owned(), spans, Vec::new());
    }
    let mut shown = String::with_capacity(code.len() + 8);
    // (byte in `code` after the insertion, bytes added up to and including it)
    let mut shifts: Vec<(usize, usize)> = Vec::new();
    let mut pads = Vec::new();
    let mut col = 0;
    let mut chars = code.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        shown.push(ch);
        col += 1;
        let start = shown.len();
        if ch == '\t' {
            let pad = 3 - (col - 1) % 4;
            shown.extend(std::iter::repeat_n(' ', pad));
            col += pad;
        } else if wrap && matches!(ch, '.' | '(' | '[' | '/') && chars.peek().is_some_and(|(_, c)| !c.is_whitespace()) {
            shown.push('\u{200B}');
        }
        if shown.len() > start {
            pads.push(start..shown.len());
            shifts.push((i + ch.len_utf8(), shifts.last().map_or(0, |s| s.1) + shown.len() - start));
        }
    }
    let shift = |b: u32| match shifts.partition_point(|s| s.0 <= b as usize) {
        0 => b,
        n => b + shifts[n - 1].1 as u32,
    };
    for sp in &mut spans {
        (sp.start, sp.end) = (shift(sp.start), shift(sp.end));
    }
    (shown, spans, pads)
}

/// `t` of the way from `base` toward `hue` in OKLab, at `base`'s alpha: a quiet wash that keeps
/// `base`'s lightness close in both themes.
fn tint(base: Color, hue: Color, t: f32) -> Color {
    let [a, b] = [base.to_oklab(), hue.to_oklab()];
    Color::from_oklab([0, 1, 2, 3].map(|i| if i == 3 { a[3] } else { a[i] + (b[i] - a[i]) * t }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn word_backgrounds_cut_syntax_colours() {
        let red = Color::WHITE;
        let green = Color::BLACK;
        // "let total = 1;" with `let` and `1` coloured, `total` changed.
        let spans = layer(14, &[(0..3, red), (12..13, red)], &[4..9], Some(green));
        let got: Vec<(u32, u32, Option<Color>, Option<Color>)> = spans.iter().map(|s| (s.start, s.end, s.color, s.bg)).collect();
        assert_eq!(got, vec![(0, 3, Some(red), None), (4, 9, None, Some(green)), (12, 13, Some(red), None)]);
        // A change across a coloured token splits it.
        let spans = layer(10, &[(0..6, red)], &[3..8], Some(green));
        let got: Vec<(u32, u32, Option<Color>, Option<Color>)> = spans.iter().map(|s| (s.start, s.end, s.color, s.bg)).collect();
        assert_eq!(got, vec![(0, 3, Some(red), None), (3, 6, Some(red), Some(green)), (6, 8, None, Some(green))]);
    }

    #[test]
    fn tabs_pad_to_four_columns_without_copying_the_padding() {
        let (shown, spans, pads) = tabbed("a\tb\t\tc", vec![Span::new(2, 3)], false);
        assert_eq!(shown, "a\t  b\t  \t   c");
        assert_eq!(pads, vec![2..4, 6..8, 9..12]);
        assert_eq!(&shown[spans[0].start as usize..spans[0].end as usize], "b");
        let copied: String = shown.char_indices().filter(|(i, _)| !pads.iter().any(|r| r.contains(i))).map(|(_, c)| c).collect();
        assert_eq!(copied, "a\tb\t\tc");
    }

    #[test]
    fn wrapped_lines_can_break_inside_call_chains() {
        let (shown, spans, pads) = tabbed("a.b(c) x/ y", vec![Span::new(2, 3)], true);
        assert_eq!(shown, "a.\u{200B}b(\u{200B}c) x/ y");
        assert_eq!(&shown[spans[0].start as usize..spans[0].end as usize], "b");
        let copied: String = shown.char_indices().filter(|(i, _)| !pads.iter().any(|r| r.contains(i))).map(|(_, c)| c).collect();
        assert_eq!(copied, "a.b(c) x/ y");
    }
}

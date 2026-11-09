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


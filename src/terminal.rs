//! The terminal drawer under the diff: shells started in the project's folder, for the git
//! commands Collate has no button for.
//!
//! `alacritty_terminal` runs each shell: its thread reads the PTY and parses the output into a
//! grid behind a lock. The UI thread hears of new output at most once per frame (a flag, not a
//! message per read) and draws the visible grid, each row memoised on what it holds, so a
//! screen of output repaints only the rows that changed.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as Ansi, CursorShape, NamedColor, Rgb};
use rune::{Color, Cursor, Drag, El, Icon, Key, KeyEvent, Phase, Span, TextEdit, TextStyle, Tx, WheelEvent, col, el, icon, rich, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit};

/// The drawer's height when first opened, and its bounds: at least 180 points, so a few
/// lines always show, and at most three quarters of the window, so the diff stays in view.
pub const DEFAULT_HEIGHT: f32 = 280.0;
const MIN_HEIGHT: f32 = 180.0;
const MAX_SHARE: f32 = 0.75;
/// Space between the drawer's edge and the grid.
const PAD: f32 = 8.0;
/// The list of terminals beside the grid, shown once there are two.
const LIST_W: f32 = 144.0;

/// Where a shell's events go: the app moves them onto its UI thread.
pub type Sink = Arc<dyn Fn(u64, Event) + Send + Sync>;

/// What runs on the PTY thread for one shell. Output marks it dirty and wakes the UI only
/// when it wasn't already, so a flood of output costs one redraw a frame.
#[derive(Clone)]
pub struct Proxy {
    id: u64,
    dirty: Arc<AtomicBool>,
    pty: Arc<OnceLock<EventLoopSender>>,
    sink: Sink,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => {
                if !self.dirty.swap(true, Ordering::AcqRel) {
                    (self.sink)(self.id, Event::Wakeup);
                }
            }
            // Replies the shell asked for (cursor position, device attributes) go straight back.
            Event::PtyWrite(s) => {
                if let Some(pty) = self.pty.get() {
                    let _ = pty.send(Msg::Input(Cow::Owned(s.into_bytes())));
                }
            }
            Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::Bell => {}
            e => (self.sink)(self.id, e),
        }
    }
}

pub struct Session {
    pub id: u64,
    pub title: String,
    term: Arc<FairMutex<Term<Proxy>>>,
    pty: EventLoopSender,
    dirty: Arc<AtomicBool>,
    /// Columns and rows the PTY was last told.
    size: (usize, usize),
    /// Takes the keyboard: Rune focuses text fields, so an invisible one stands in for the grid.
    pub keys: TextEdit,
    /// Wheel points not yet a whole line.
    wheel: f32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The event loop drops the PTY on its way out, which hangs up the shell.
        let _ = self.pty.send(Msg::Shutdown);
    }
}

impl Session {
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.pty.send(Msg::Input(bytes.into()));
    }

    /// Typing goes to the shell and brings the view back to the prompt.
    fn input(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        {
            let mut t = self.term.lock();
            t.selection = None;
            t.scroll_display(Scroll::Bottom);
        }
        self.write(bytes);
    }

    fn paste(&self, s: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        // A pasted line break is a carriage return to a shell, as Return is.
        let body = s.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            // The end marker in the text would end the paste early and run the rest.
            self.input(format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", "")).into_bytes());
        } else {
            self.input(body.into_bytes());
        }
    }

    /// The visible text, a line per row.
    #[cfg(test)]
    pub fn shown(&self) -> String {
        let term = self.term.lock();
        (0..term.screen_lines())
            .map(|l| (0..term.columns()).map(|c| term.grid()[Point::new(Line(l as i32), Column(c))].c).collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn copy_selection(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }
}

/// The drawer's state: open or not, its height, and the shells.
pub struct Terminals {
    pub open: bool,
    pub height: f32,
    pub list: Vec<Session>,
    pub active: usize,
    next: u64,
    sink: Option<Sink>,
    /// The appearance last drawn in, for programs that ask what a colour is.
    dark: std::cell::Cell<bool>,
}

impl Default for Terminals {
    fn default() -> Self {
        Terminals { open: false, height: DEFAULT_HEIGHT, list: Vec::new(), active: 0, next: 1, sink: None, dark: std::cell::Cell::new(true) }
    }
}

impl Terminals {
    /// Where shells send their events; set once the app has a handle to its UI thread.
    pub fn connect(&mut self, sink: Sink) {
        self.sink = Some(sink);
    }

    pub fn active(&self) -> Option<&Session> {
        self.list.get(self.active)
    }

    /// Whether shells can start: the app has connected their events.
    pub fn ready(&self) -> bool {
        self.sink.is_some()
    }

    /// The id the next shell will have: unique for this run of the app.
    pub fn next_id(&self) -> u64 {
        self.next
    }

    fn by_id(&self, id: u64) -> Option<&Session> {
        self.list.iter().find(|t| t.id == id)
    }

    /// Starts the login shell in `dir` and makes it the active terminal, focused.
    pub fn spawn(&mut self, dir: &Path) -> Result<(), String> {
        let sink = self.sink.clone().ok_or("the terminal isn't ready yet")?;
        let id = self.next;
        self.next += 1;
        let (cols, rows) = self.active().map_or((80, 24), |t| t.size);
        let options = tty::Options {
            working_directory: Some(PathBuf::from(dir)),
            env: [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "Collate")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            ..Default::default()
        };
        let pty = tty::new(&options, window_size(cols, rows), id).map_err(|e| format!("Couldn't start a shell: {e}"))?;
        let dirty = Arc::new(AtomicBool::new(false));
        let sender = Arc::new(OnceLock::new());
        let proxy = Proxy { id, dirty: dirty.clone(), pty: sender.clone(), sink };
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(cols, rows), proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false).map_err(|e| format!("Couldn't start a shell: {e}"))?;
        let pty = event_loop.channel();
        let _ = sender.set(pty.clone());
        event_loop.spawn();
        let mut keys = TextEdit::new("");
        keys.focused = true;
        for t in &mut self.list {
            t.keys.focused = false;
        }
        let title = dir.file_name().map_or_else(|| "Terminal".to_owned(), |n| n.to_string_lossy().into_owned());
        self.list.push(Session { id, title, term, pty, dirty, size: (cols, rows), keys, wheel: 0.0 });
        self.active = self.list.len() - 1;
        self.open = true;
        Ok(())
    }

    /// Starts a shell in `dir` and types `command` into it, as the reader would.
    pub fn run(&mut self, dir: &Path, command: &str) -> Result<(), String> {
        self.spawn(dir)?;
        if let Some(t) = self.active() {
            t.input(format!("{command}\r").into_bytes());
        }
        Ok(())
    }

    pub fn close(&mut self, i: usize) {
        if i >= self.list.len() {
            return;
        }
        self.list.remove(i);
        self.active = self.active.min(self.list.len().saturating_sub(1));
        match self.list.get_mut(self.active) {
            Some(t) => t.keys.focused = true,
            None => self.open = false,
        }
    }

    pub fn pick(&mut self, i: usize) {
        if i < self.list.len() {
            self.active = i;
            for (n, t) in self.list.iter_mut().enumerate() {
                t.keys.focused = n == i;
            }
        }
    }

    /// Shows or hides the drawer; showing it focuses the active shell.
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
        let active = self.active;
        if let Some(t) = self.list.get_mut(active) {
            t.keys.focused = open;
        }
    }

    /// An event from a shell's thread, now on the UI thread. True when the view should run.
    pub fn event(&mut self, id: u64, event: Event, tx: &mut Tx) -> bool {
        let dark = self.dark.get();
        let Some(i) = self.list.iter().position(|t| t.id == id) else { return false };
        let t = &mut self.list[i];
        match event {
            Event::Wakeup => {
                t.dirty.store(false, Ordering::Release);
                return true;
            }
            Event::Title(s) => t.title = s,
            Event::ResetTitle => t.title = "Terminal".to_owned(),
            // A program copying (OSC 52) is allowed; reading the clipboard isn't.
            Event::ClipboardStore(_, s) => tx.copy(s),
            Event::ColorRequest(n, reply) => {
                let rgb = t.term.lock().colors()[n].unwrap_or_else(|| to_rgb(palette(n, dark)));
                t.write(reply(rgb).into_bytes());
                return false;
            }
            Event::TextAreaSizeRequest(reply) => {
                t.write(reply(window_size(t.size.0, t.size.1)).into_bytes());
                return false;
            }
            Event::ChildExit(_) | Event::Exit => self.close(i),
            _ => return false,
        }
        true
    }

    /// The grid laid out `w` by `h` points: tell the shell when that changes its rows or columns.
    fn fit(&mut self, id: u64, w: f32, h: f32, cell: (f32, f32)) {
        let Some(t) = self.list.iter_mut().find(|t| t.id == id) else { return };
        let cols = (((w - 2.0 * PAD) / cell.0).floor() as usize).max(2);
        let rows = (((h - 2.0 * PAD) / cell.1).floor() as usize).max(1);
        if (cols, rows) != t.size {
            t.size = (cols, rows);
            t.term.lock().resize(TermSize::new(cols, rows));
            let _ = t.pty.send(Msg::Resize(window_size(cols, rows)));
        }
    }
}

fn window_size(cols: usize, rows: usize) -> WindowSize {
    WindowSize { num_cols: cols as u16, num_lines: rows as u16, cell_width: 8, cell_height: 16 }
}

/// The bytes a key sends, as xterm (and Terminal.app's defaults) send them: `None` for keys
/// the shell doesn't get (⌘ shortcuts are the app's).
fn encode(ev: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let m = ev.mods;
    // xterm's modifier parameter: 1 + Shift + 2·Option + 4·Control.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let app = mode.contains(TermMode::APP_CURSOR);
    let cursor = |c: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{c}").into_bytes()
        } else if app {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> { if param > 1 { format!("\x1b[{n};{param}~") } else { format!("\x1b[{n}~") }.into_bytes() };
    let bytes = match &ev.key {
        // ⌘ arrows and ⌘⌫ edit the line as they do in Terminal: start, end, erase to start.
        Key::Left if m.cmd => vec![0x01],
        Key::Right if m.cmd => vec![0x05],
        Key::Backspace if m.cmd => vec![0x15],
        _ if m.cmd => return None,
        // Option moves and deletes by word.
        Key::Left if m.alt && !m.shift && !m.ctrl => b"\x1bb".to_vec(),
        Key::Right if m.alt && !m.shift && !m.ctrl => b"\x1bf".to_vec(),
        Key::Backspace if m.alt => b"\x1b\x7f".to_vec(),
        Key::Backspace if m.ctrl => vec![0x08],
        Key::Backspace => vec![0x7f],
        Key::Enter => vec![b'\r'],
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => vec![b'\t'],
        Key::Escape => vec![0x1b],
        Key::Up => cursor('A'),
        Key::Down => cursor('B'),
        Key::Right => cursor('C'),
        Key::Left => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Function(n @ 1..=4) => {
            let c = (b'P' + n - 1) as char;
            if param > 1 { format!("\x1b[1;{param}{c}").into_bytes() } else { format!("\x1bO{c}").into_bytes() }
        }
        Key::Function(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][(*n - 5) as usize]),
        Key::Text(_) | Key::Command(_) if m.ctrl => {
            let c = match &ev.key {
                Key::Command(c) => *c,
                Key::Text(s) => s.chars().next()?,
                _ => return None,
            };
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '/' | '7' => 0x1f,
                '8' | '?' => 0x7f,
                _ => return Some(c.to_string().into_bytes()),
            };
            if m.alt { vec![0x1b, b] } else { vec![b] }
        }
        Key::Text(s) => s.as_bytes().to_vec(),
        _ => return None,
    };
    Some(bytes)
}

/// The 16 ANSI colours for each appearance (VS Code's terminal palette, which most CLI tools
/// are tuned against); the rest of the 256 are xterm's cube and greys. Literal on purpose:
/// the kit has no ANSI palette, and programs expect these hues.
const ANSI_DARK: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5,
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];
const ANSI_LIGHT: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555,
    0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn palette(n: usize, dark: bool) -> u32 {
    match n {
        0..=15 => if dark { ANSI_DARK[n] } else { ANSI_LIGHT[n] },
        16..=231 => {
            let i = n - 16;
            let level = |v: usize| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            (level(i / 36) << 16) | (level(i / 6 % 6) << 8) | level(i % 6)
        }
        232..=255 => {
            let g = 8 + 10 * (n as u32 - 232);
            (g << 16) | (g << 8) | g
        }
        _ => 0,
    }
}

fn to_rgb(c: u32) -> Rgb {
    Rgb { r: (c >> 16) as u8, g: (c >> 8) as u8, b: c as u8 }
}

fn color(rgb: Rgb) -> Color {
    let d = |v: u8| rune::color::srgb_decode(v as f32 / 255.0);
    Color { r: d(rgb.r), g: d(rgb.g), b: d(rgb.b), a: 1.0 }
}

/// What a cell's colour is on screen: the program's own setting (OSC 4), else the palette;
/// the default foreground and background are the app's.
struct Ink<'a> {
    colors: &'a alacritty_terminal::term::color::Colors,
    dark: bool,
    fg: Color,
    bg: Color,
}

impl Ink<'_> {
    fn indexed(&self, n: usize) -> Color {
        color(self.colors[n].unwrap_or_else(|| to_rgb(palette(n, self.dark))))
    }

    fn get(&self, c: Ansi, bold: bool) -> Color {
        match c {
            Ansi::Spec(rgb) => color(rgb),
            // Bold text in one of the first eight colours shows bright, as xterm draws it.
            Ansi::Indexed(n) if bold && n < 8 => self.indexed(n as usize + 8),
            Ansi::Indexed(n) => self.indexed(n as usize),
            Ansi::Named(NamedColor::Foreground | NamedColor::BrightForeground) => self.fg,
            Ansi::Named(NamedColor::DimForeground) => self.fg.alpha(0.66),
            Ansi::Named(NamedColor::Background) => self.bg,
            Ansi::Named(NamedColor::Cursor) => self.fg,
            Ansi::Named(n) => {
                let n = n as usize;
                match n {
                    0..=7 if bold => self.indexed(n + 8),
                    0..=15 => self.indexed(n),
                    // The dim colours follow `Cursor` in order: dim black is the 259th.
                    259..=266 => self.indexed(n - 259).alpha(0.66),
                    _ => self.fg,
                }
            }
        }
    }
}

/// One run of cells that look alike, drawn as one text at its first column.
#[derive(Hash)]
struct Run {
    col: usize,
    cells: usize,
    text: String,
    fg: [u32; 4],
    bg: Option<[u32; 4]>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    wide: bool,
}

fn bits(c: Color) -> [u32; 4] {
    [c.r, c.g, c.b, c.a].map(f32::to_bits)
}

fn unbits(b: [u32; 4]) -> Color {
    let [r, g, b, a] = b.map(f32::from_bits);
    Color { r, g, b, a }
}

/// The visible rows, as runs; and where the cursor is and what's under it.
struct Screen {
    rows: Vec<Vec<Run>>,
    cursor: Option<(usize, usize, CursorShape, String)>,
}

fn screen(t: &Term<Proxy>, k: &Kit, bg: Color) -> Screen {
    let content = t.renderable_content();
    let ink = Ink { colors: content.colors, dark: k.dark, fg: k.c.text_base, bg };
    let selection = content.selection;
    let offset = content.display_offset;
    let (cols, lines) = (t.columns(), t.screen_lines());
    let sel_bg = k.selection();
    let mut rows: Vec<Vec<Run>> = (0..lines).map(|_| Vec::new()).collect();
    for cell in content.display_iter {
        let Some(view_line) = usize::try_from(cell.point.line.0 + offset as i32).ok().filter(|l| *l < lines) else { continue };
        let flags = cell.flags;
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let bold = flags.contains(Flags::BOLD);
        let (mut fg, mut bg_c) = (ink.get(cell.fg, bold), ink.get(cell.bg, false));
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg_c);
        }
        if flags.contains(Flags::DIM) {
            fg = fg.alpha(0.66);
        }
        if selection.is_some_and(|s| s.contains(cell.point)) {
            bg_c = sel_bg;
        }
        let bg_bits = (bg_c != bg).then(|| bits(bg_c));
        let ch = if flags.contains(Flags::HIDDEN) || cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
        let wide = flags.contains(Flags::WIDE_CHAR);
        let row = &mut rows[view_line];
        let (italic, underline, strike) = (flags.contains(Flags::ITALIC), flags.intersects(Flags::ALL_UNDERLINES), flags.contains(Flags::STRIKEOUT));
        let col = cell.point.column.0;
        // Wide characters stand alone, so the font's own width for them can't shift the
        // columns after them; everything else joins the run before it when it looks the same.
        match row.last_mut() {
            Some(r) if !wide && !r.wide && r.col + r.cells == col && r.fg == bits(fg) && r.bg == bg_bits && r.bold == bold && r.italic == italic && r.underline == underline && r.strike == strike => {
                r.text.push(ch);
                if let Some(extra) = cell.zerowidth() {
                    r.text.extend(extra);
                }
                r.cells += 1;
            }
            _ => {
                let mut text = String::from(ch);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
                row.push(Run { col, cells: if wide { 2 } else { 1 }, text, fg: bits(fg), bg: bg_bits, bold, italic, underline, strike, wide });
            }
        }
    }
    // Trailing blanks on the default background draw nothing.
    for row in &mut rows {
        while row.last().is_some_and(|r| r.bg.is_none() && r.text.trim().is_empty()) {
            row.pop();
        }
    }
    let c = content.cursor;
    let cursor = usize::try_from(c.point.line.0 + offset as i32).ok().filter(|l| *l < lines && c.shape != CursorShape::Hidden && c.point.column.0 < cols).map(|l| {
        let cell = &t.grid()[c.point];
        let ch = if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
        (l, c.point.column.0, c.shape, ch.to_string())
    });
    Screen { rows, cursor }
}

/// The terminal's type: the app's mono at a terminal's usual size, ligatures off so every
/// character keeps its cell.
pub fn font(k: &Kit) -> TextStyle {
    TextStyle { size: 12.0, line_height: 17.0, features: "'kern' 0, 'liga' 0, 'calt' 0", ..k.ty.mono }
}

/// A cell's width and height in points.
fn cell(k: &Kit) -> (f32, f32) {
    let f = font(k);
    (rune::text::advance("0000000000", f) / 10.0, f.line_height)
}

/// The grid point under `(x, y)` in the grid's own points, and which half of the cell.
fn point_at(t: &Term<Proxy>, x: f32, y: f32, cell: (f32, f32)) -> (Point, Direction) {
    let col = ((x - PAD) / cell.0).max(0.0);
    let line = (((y - PAD) / cell.1).max(0.0) as usize).min(t.screen_lines().saturating_sub(1));
    let c = (col as usize).min(t.columns().saturating_sub(1));
    let side = if col.fract() < 0.5 { Direction::Left } else { Direction::Right };
    let offset = t.grid().display_offset() as i32;
    (Point::new(Line(line as i32 - offset), Column(c)), side)
}

pub static TERMINAL_ICON: Icon = Icon {
    name: "collate-terminal",
    view: 16.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><rect x="1.75" y="2.75" width="12.5" height="10.5" rx="2"/><path d="M4.75 6.25L6.75 8L4.75 9.75M8.5 10H11.25"/></g>"#,
};
static PLUS_ICON: Icon = Icon {
    name: "collate-plus",
    view: 16.0,
    body: r#"<path d="M8 3.5V12.5M3.5 8H12.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};
static TRASH_ICON: Icon = Icon {
    name: "collate-trash",
    view: 16.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M2.75 4.25H13.25M6.25 4.25V2.75H9.75V4.25M4 4.25L4.75 13.25H11.25L12 4.25"/></g>"#,
};

type Access<S> = fn(&mut S) -> &mut Terminals;

/// The drawer: a strip to drag its height by, the active shell's grid, and (with two or more)
/// the list to switch between them. `new` opens another shell.
pub fn drawer<S: 'static>(t: &Terminals, k: &Kit, window_h: f32, access: Access<S>, new: fn(&mut S)) -> El {
    t.dark.set(k.dark);
    let max = (window_h * MAX_SHARE).max(MIN_HEIGHT);
    let height = t.height.clamp(MIN_HEIGHT, max);
    let bg = k.c.bg_base;
    let handle = el()
        .key("terminal-resize")
        .pin(Some(0.0), Some(0.0), Some(0.0), None)
        .h(6.0)
        .cursor(Cursor::ResizeRow)
        .role("separator")
        .label("Resize terminal")
        .on_drag(move |s: &mut S, d: &mut Drag, _| {
            let t = access(s);
            if d.phase == Phase::Down {
                d.data = t.height.clamp(MIN_HEIGHT, max);
            }
            t.height = (d.data - d.dy).clamp(MIN_HEIGHT, max).round();
        });
    let Some(session) = t.active() else {
        return col().key("terminal").w_full().h(height).fill(bg).child(hairline(k, "terminal-edge", true));
    };
    let grid = grid(session, k, bg, access);
    let mut body = row().key("terminal-body").w_full().grow(1.0).min_h(0.0).child(grid);
    if t.list.len() > 1 {
        body = body.child(list(t, k, access));
    }
    let tools = row()
        .key("terminal-tools")
        .pin(None, Some(6.0), Some(if t.list.len() > 1 { LIST_W + 8.0 } else { 8.0 }), None)
        .gap(2.0)
        .items_center()
        .child(
            IconButton::new(&PLUS_ICON).variant(IconButtonVariant::GhostMuted).label("New terminal").help("New terminal").on_press(new).build(k).key("terminal-new"),
        )
        .child({
            let i = t.active;
            IconButton::new(&TRASH_ICON)
                .variant(IconButtonVariant::GhostMuted)
                .label("Close terminal")
                .help("Close terminal (ends its shell)")
                .on_press(move |s: &mut S| access(s).close(i))
                .build(k)
                .key("terminal-close")
        });
    col().key("terminal").w_full().h(height).fill(bg).child(hairline(k, "terminal-edge", true)).child(body).child(tools).child(handle)
}

fn list<S: 'static>(t: &Terminals, k: &Kit, access: Access<S>) -> El {
    let rows = t.list.iter().enumerate().map(|(i, s)| {
        let on = i == t.active;
        row()
            .key(format!("terminal-tab-{}", s.id))
            .w_full()
            .h(24.0)
            .px(8.0)
            .gap(6.0)
            .items_center()
            .radius(k.radius.sm)
            .fill(if on { k.c.hover } else { k.alpha(k.c.hover, 0.0) })
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .role("tab")
            .label(s.title.clone())
            .on_press(move |s: &mut S| access(s).pick(i))
            .child(icon(&TERMINAL_ICON, 12.0).color(if on { k.c.icon_base } else { k.c.icon_faint }))
            .child(text(s.title.clone()).font(k.ty.text_xsmall).color(if on { k.c.text_base } else { k.c.text_muted }).truncate().min_w(0.0).grow(1.0))
    });
    let tabs = col().key("terminal-tabs").grow(1.0).h_full().pt(36.0).px(4.0).gap(2.0).role("tablist").label("Terminals").children(rows);
    row().key("terminal-list").w(LIST_W).h_full().child(hairline(k, "terminal-list-edge", false)).child(tabs)
}

/// A one-point rule: across the top (`wide`) or down the side.
fn hairline(k: &Kit, key: &str, wide: bool) -> El {
    let e = el().key(key.to_owned()).fill(k.c.border_weak).a11y_hidden();
    if wide { e.w_full().h(1.0) } else { e.w(1.0).h_full() }
}

/// The active shell's grid: rows of runs over the background, the cursor, and the invisible
/// field that takes the keys.
fn grid<S: 'static>(t: &Session, k: &Kit, bg: Color, access: Access<S>) -> El {
    let font = font(k);
    let cell = cell(k);
    let id = t.id;
    let (shown, mode) = {
        let term = t.term.lock();
        (screen(&term, k, bg), *term.mode())
    };
    let focused = t.keys.focused;
    let mut lines = col().key("terminal-lines").pin(Some(PAD), Some(PAD), Some(PAD), None);
    for (n, runs) in shown.rows.iter().enumerate() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        runs.hash(&mut h);
        bits(k.c.text_base).hash(&mut h);
        let version = h.finish();
        let runs: Vec<(usize, usize, String, Color, Option<Color>, Span)> = runs
            .iter()
            .map(|r| {
                let mut sp = Span::new(0, r.text.len());
                if r.bold {
                    sp = sp.bold();
                }
                if r.italic {
                    sp = sp.italic();
                }
                if r.underline {
                    sp = sp.underline(unbits(r.fg));
                }
                (r.col, r.cells, r.text.clone(), unbits(r.fg), r.bg.map(unbits), sp)
            })
            .collect();
        let strikes: Vec<bool> = shown.rows[n].iter().map(|r| r.strike).collect();
        lines = lines.child(rune::memo::memo(format!("term-{id}-row-{n}"), version, move || {
            let mut line = el().key(format!("r{n}")).w_full().h(cell.1);
            for (i, (c, cells, s, fg, bgc, sp)) in runs.into_iter().enumerate() {
                let len = s.len();
                let mut run = rich(s, vec![sp]).key(format!("c{c}")).font(font).color(fg).absolute(c as f32 * cell.0, 0.0).h(cell.1);
                if strikes[i] {
                    run = run.strike(0, len, Some(fg));
                }
                if let Some(b) = bgc {
                    run = run.w(cells as f32 * cell.0).fill(b);
                }
                line = line.child(run);
            }
            line
        }));
    }
    let mut surface = el()
        .key(format!("terminal-grid-{id}"))
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .clip_rounded(0.0)
        .cursor(Cursor::Text)
        .role("terminal")
        .label(t.title.clone())
        .on_layout(move |s: &mut S, r: [f32; 4]| access(s).fit(id, r[2], r[3], cell))
        .on_wheel(move |s: &mut S, w: WheelEvent, _| {
            let ts = access(s);
            let Some(t) = ts.list.iter_mut().find(|t| t.id == id) else { return false };
            t.wheel += w.dy;
            let lines = (t.wheel / cell.1).trunc();
            if lines == 0.0 {
                return true;
            }
            t.wheel -= lines * cell.1;
            let mut term = t.term.lock();
            let mode = *term.mode();
            if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
                // A full-screen program (less, vim) scrolls by its own arrow keys.
                drop(term);
                let key = if lines > 0.0 { 'A' } else { 'B' };
                let app = mode.contains(TermMode::APP_CURSOR);
                let seq = if app { format!("\x1bO{key}") } else { format!("\x1b[{key}") };
                t.write(seq.repeat(lines.abs() as usize).into_bytes());
            } else {
                term.scroll_display(Scroll::Delta(lines as i32));
            }
            true
        })
        .on_drag(move |s: &mut S, d: &mut Drag, _| {
            let ts = access(s);
            let Some(i) = ts.list.iter().position(|t| t.id == id) else { return };
            ts.pick(i);
            let t = &ts.list[i];
            let mut term = t.term.lock();
            let (p, side) = point_at(&term, d.x, d.y, cell);
            match d.phase {
                Phase::Down => {
                    let ty = match d.clicks {
                        2 => SelectionType::Semantic,
                        3.. => SelectionType::Lines,
                        _ => SelectionType::Simple,
                    };
                    let mut sel = Selection::new(ty, p, side);
                    if d.clicks >= 2 {
                        sel.update(p, side);
                    }
                    term.selection = Some(sel);
                }
                Phase::Move => {
                    if let Some(sel) = &mut term.selection {
                        sel.update(p, side);
                    }
                }
                Phase::Up => {
                    // A click without a drag leaves nothing selected.
                    if d.clicks < 2 && (d.x - d.start_x).abs() < 3.0 && (d.y - d.start_y).abs() < 3.0 {
                        term.selection = None;
                    }
                }
            }
        })
        .child(lines);
    if let Some((line, c, shape, under)) = shown.cursor {
        let (x, y) = (PAD + c as f32 * cell.0, PAD + line as f32 * cell.1);
        let ink = k.c.text_base;
        let mark = match shape {
            CursorShape::Beam => el().w(1.5).h(cell.1).fill(ink),
            CursorShape::Underline => el().w(cell.0).h(cell.1).child(el().pin(Some(0.0), None, Some(0.0), Some(0.0)).h(1.5).fill(ink)),
            // Unfocused, the block goes hollow, as in every Mac terminal.
            _ if !focused => el().w(cell.0).h(cell.1).border(1.0, ink),
            _ => el().w(cell.0).h(cell.1).fill(ink).child(text(under).font(font).color(bg)),
        };
        surface = surface.child(mark.key("terminal-cursor").absolute(x, y).a11y_hidden());
    }
    // The keyboard: an invisible field over the grid's corner that hands every key to the shell.
    let keys = rune::text_input(&t.keys, font)
        .key(format!("terminal-keys-{id}"))
        .absolute(0.0, 0.0)
        .w(1.0)
        .h(1.0)
        // Not `opacity(0)`: Rune won't focus what doesn't show, so it draws in clear ink instead.
        .color(Color::TRANSPARENT)
        .caret(Color::TRANSPARENT, Color::TRANSPARENT)
        .clip_rounded(0.0)
        .label("Terminal input")
        .on_edit(move |s: &mut S| {
            let ts = access(s);
            let i = ts.list.iter().position(|t| t.id == id).unwrap_or(0);
            &mut ts.list[i].keys
        })
        .on_input_key(move |s: &mut S, ev: &KeyEvent, tx: &mut Tx| {
            let Some(t) = access(s).by_id(id) else { return false };
            // An input method composing: its keys are its own until it commits.
            if t.keys.preedit.is_some() {
                return false;
            }
            if ev.is_command('c') {
                return match t.copy_selection() {
                    Some(sel) => {
                        tx.copy(sel);
                        true
                    }
                    None => false,
                };
            }
            if ev.is_command('k') {
                // Clear as Terminal does: the shell redraws its prompt on a fresh screen.
                t.term.lock().grid_mut().clear_history();
                t.input(&b"\x0c"[..]);
                return true;
            }
            if ev.is_command('v') {
                return false;
            }
            if ev.mods.shift && matches!(ev.key, Key::PageUp | Key::PageDown) && !ev.mods.cmd {
                t.term.lock().scroll_display(if ev.key == Key::PageUp { Scroll::PageUp } else { Scroll::PageDown });
                return true;
            }
            match encode(ev, mode) {
                Some(bytes) => {
                    t.input(bytes);
                    true
                }
                None => false,
            }
        })
        // What an input method commits arrives as an edit: send it and empty the field.
        .on_input_change(move |s: &mut S, _| {
            let ts = access(s);
            let Some(t) = ts.list.iter_mut().find(|t| t.id == id) else { return };
            if t.keys.preedit.is_none() && !t.keys.text.is_empty() {
                let typed = std::mem::take(&mut t.keys.text);
                t.keys = TextEdit::new("");
                t.keys.focused = true;
                t.input(typed.into_bytes());
            }
        })
        .on_input_paste(move |s: &mut S, content, _| {
            if let Some(t) = access(s).by_id(id) {
                t.paste(content.text());
            }
            true
        });
    surface.child(keys).on_press(move |s: &mut S| {
        let ts = access(s);
        if let Some(i) = ts.list.iter().position(|t| t.id == id) {
            ts.pick(i);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn key(key: Key, mods: Mods) -> Option<Vec<u8>> {
        encode(&KeyEvent { key, mods }, TermMode::empty())
    }

    #[test]
    fn keys_send_what_xterm_sends() {
        let none = Mods::default();
        let ctrl = Mods { ctrl: true, ..none };
        let alt = Mods { alt: true, ..none };
        let cmd = Mods { cmd: true, ..none };
        assert_eq!(key(Key::Text("c".into()), ctrl), Some(vec![3]), "⌃C interrupts");
        assert_eq!(key(Key::Enter, none), Some(b"\r".to_vec()));
        assert_eq!(key(Key::Backspace, none), Some(vec![0x7f]));
        assert_eq!(key(Key::Up, none), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode(&KeyEvent { key: Key::Up, mods: none }, TermMode::APP_CURSOR), Some(b"\x1bOA".to_vec()), "application cursor keys");
        assert_eq!(key(Key::Left, alt), Some(b"\x1bb".to_vec()), "⌥← moves a word");
        assert_eq!(key(Key::Left, cmd), Some(vec![1]), "⌘← goes to the line's start");
        assert_eq!(key(Key::Text("j".into()), cmd), None, "⌘ shortcuts stay the app's");
        assert_eq!(key(Key::Function(5), none), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key(Key::Text("é".into()), none), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn a_shell_runs_in_the_folder_and_its_output_lands_in_the_grid() {
        let mut ts = Terminals::default();
        let woke = Arc::new(AtomicBool::new(false));
        let w = woke.clone();
        ts.connect(Arc::new(move |_, e| {
            if matches!(e, Event::Wakeup) {
                w.store(true, Ordering::Release);
            }
        }));
        let dir = std::env::temp_dir().canonicalize().unwrap();
        ts.spawn(&dir).expect("a shell starts");
        assert!(ts.open && ts.list[0].keys.focused, "the drawer opens on the new shell, focused");
        ts.fit(ts.list[0].id, 600.0, 300.0, (7.0, 17.0));
        assert_eq!(ts.list[0].size, (83, 16), "the grid fills the drawer less its padding");
        // The arithmetic is the shell's, so the echoed command itself can't pass the check.
        ts.list[0].input(&b"cd . && echo collate-$((6*7)) && pwd\r"[..]);
        let start = std::time::Instant::now();
        while !ts.list[0].shown().contains("collate-42") {
            assert!(start.elapsed().as_secs() < 20, "the output arrived:\n{}", ts.list[0].shown());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(woke.load(Ordering::Acquire), "output woke the UI");
        assert!(ts.list[0].shown().contains(&dir.display().to_string()), "it started in the folder");
        ts.close(0);
        assert!(!ts.open && ts.list.is_empty(), "closing the last shell closes the drawer");
    }

    #[test]
    fn the_palette_has_xterms_cube_and_greys() {
        assert_eq!(palette(16, true), 0x000000);
        assert_eq!(palette(231, true), 0xffffff);
        assert_eq!(palette(196, true), 0xff0000);
        assert_eq!(palette(232, true), 0x080808);
        assert_eq!(palette(255, true), 0xeeeeee);
    }
}

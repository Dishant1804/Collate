//! The reader's own comments: kept on this Mac per project, placed under their lines, shown
//! as rows in the diff and copied out as Markdown.
use rune::{El, TextEdit, Tx, col, row, text};
use rune_kit::Kit;

use crate::diff::{self, EDGE, GHOST_PAD};
use crate::{Ask, Collate, action, day, project_data, pulls};

/// A note the reader left on a line. Comments stay on this Mac, per project.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub at: diff::Anchor,
    pub body: String,
    /// Seconds since 1970.
    pub time: i64,
}

impl Collate {
    /// Starts a comment under the selected line (the last, when several are).
    pub fn ask_comment(&mut self) {
        let Some(sel) = self.diff.selection() else {
            self.say("Click a line number first: the comment goes under that line", true);
            return;
        };
        let Some(file) = self.diff.files().get(sel.file) else { return };
        let Some(text) = self.diff.side_line(sel.file, sel.end.side, sel.end.line) else { return };
        let at = diff::Anchor { path: file.path().to_owned(), side: sel.end.side, line: sel.end.line, text: text.to_owned() };
        self.ask(Ask::Comment(at, None));
    }

    pub fn edit_comment(&mut self, n: usize) {
        let Some(c) = self.comments.get(n) else { return };
        self.prompt = Some(Ask::Comment(c.at.clone(), Some(n)));
        self.prompt_text = TextEdit::new(c.body.clone());
    }

    pub fn delete_comment(&mut self, n: usize) {
        if n < self.comments.len() {
            self.comments.remove(n);
            self.store_comments();
        }
    }

    /// Saves the comments and puts them under their lines again.
    pub fn store_comments(&mut self) {
        if let Err(e) = save_comments(&self.comments) {
            self.say(format!("Couldn't save the comments: {e}"), true);
        }
        self.place_comments();
    }

    /// Put the reader's comments, then the open pull request's line threads, under their lines.
    pub fn place_comments(&mut self) {
        let mut anchors: Vec<diff::Anchor> = self.comments.iter().map(|c| c.at.clone()).collect();
        anchors.extend(self.pulls.anchors());
        let mut shown = self.diff.set_comments(&anchors);
        shown.truncate(self.comments.len());
        self.comments_shown = shown;
        self.comments_version += 1;
    }

    /// Every comment as Markdown, each under its file, line and the line's text, to paste
    /// into a pull request or a message.
    pub fn copy_comments(&mut self, tx: &mut Tx) {
        let md: String = self.comments.iter().map(|c| format!("**{}:{}**\n\n> {}\n\n{}\n\n", c.at.path, c.at.line, c.at.text.trim(), c.body.trim())).collect();
        tx.copy(md.trim_end().to_owned());
        let n = self.comments.len();
        self.say(format!("Copied {n} comment{} as Markdown", if n == 1 { "" } else { "s" }), false);
    }
}

/// A field of the comments file: tabs, breaks and backslashes written as escapes.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// The comments file: one comment a line, `path, o|n, line, time, line text, comment` split
/// by tabs.
pub fn comments_text(comments: &[Comment]) -> String {
    comments.iter().map(|c| {
        let side = if c.at.side == diff::Side::Old { "o" } else { "n" };
        format!("{}\t{side}\t{}\t{}\t{}\t{}\n", escape(&c.at.path), c.at.line, c.time, escape(&c.at.text), escape(&c.body))
    }).collect()
}

pub fn parse_comments(text: &str) -> Vec<Comment> {
    text.lines().filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        let [path, side, line, time, text, body] = f[..] else { return None };
        let side = if side == "o" { diff::Side::Old } else { diff::Side::New };
        Some(Comment { at: diff::Anchor { path: unescape(path), side, line: line.parse().ok()?, text: unescape(text) }, body: unescape(body), time: time.parse().ok()? })
    }).collect()
}

pub fn load_comments() -> Vec<Comment> {
    project_data("comments").and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_comments(&t)).unwrap_or_default()
}

fn save_comments(comments: &[Comment]) -> Result<(), String> {
    // Tests run against throwaway repositories; they leave the reader's comments alone.
    if cfg!(test) {
        return Ok(());
    }
    let path = project_data("comments").ok_or("no project is open")?;
    path.parent().map(std::fs::create_dir_all).transpose().map_err(|e| e.to_string())?;
    std::fs::write(&path, comments_text(comments)).map_err(|e| format!("{}: {e}", path.display()))
}

/// A comment under its line: what it says, when, and the way to change or delete it.
pub fn comment_row(s: &Collate, k: &Kit, n: usize) -> El {
    // Past the reader's own: the pull request's threads.
    let Some(c) = s.comments.get(n) else { return pulls::thread_row(s, k, n - s.comments.len()) };
    let head = row().w_full().gap(8.0).items_center()
        .child(text("Comment".to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted))
        .child(text(day(c.time)).font(k.ty.text_small).color(k.c.text_faint).grow(1.0))
        .child(action(k, "edit", "Edit", "Change this comment", false, move |s| s.edit_comment(n)))
        .child(action(k, "delete", "Delete", "Delete this comment", true, move |s| s.delete_comment(n)).mr(-GHOST_PAD));
    let bubble = col().key("bubble").w_full().max_w(720.0).px(12.0).py(8.0).gap(4.0).radius(k.radius.md)
        .border(1.0, k.c.border_weak).fill(k.alpha(k.c.text_accent, 0.06))
        .child(head)
        .child(text(c.body.clone()).font(k.ty.text_small).color(k.c.text_base).wrap().w_full());
    // Under the code, past the line numbers.
    row().w_full().pl(96.0).pr(EDGE).py(6.0).role("note").label(format!("Comment: {}", c.body)).child(bubble)
}

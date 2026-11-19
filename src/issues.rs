//! The Issues place: the repository's issues, grouped by what they want of the reader, and one
//! issue's overview, in place of the diff, as the pull requests are.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::{Cursor, El, Icon, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::syntax::{Syntax, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl, TextArea, TextInput};

use crate::Hand;
use crate::github_issues::{self, Detail, Issue};
use crate::github::{self, Gh};
use crate::{COLUMN_TOP, Collate, EDGE, HANDLE, ISSUES_ICON, MORE_ICON, REFRESH_ICON, current_repo, message};

pub struct IssuesState {
    /// The list shows in place of the diff.
    pub open: bool,
    /// The signed-in login and the issues, once read.
    pub list: Option<Result<(String, Vec<Issue>), String>>,
    pub loading: bool,
    /// The list's groups folded to their titles, by title.
    pub folded: HashSet<&'static str>,
    /// Words the list is narrowed to.
    pub filter: TextEdit,
    /// Which issues the list shows by state.
    pub shown: Shown,
    /// The label the list is narrowed to; empty for issues with none.
    pub label: Option<String>,
    /// The login the list is narrowed to the issues of; empty for issues no one has.
    pub assignee: Option<String>,
    /// How each group is ordered.
    pub sort: Sort,
    /// The issue picked with the keys, by number.
    pub pick: Option<u32>,
    /// The issue open in place of the list, by number, and what's read of it.
    pub number: Option<u32>,
    pub detail: Option<Result<Detail, String>>,
    /// The comment being written at the foot of the open issue.
    pub reply: TextEdit,
    /// The dialog up over the open issue, and its text.
    pub write: Option<Write>,
    pub text: TextEdit,
}

/// What the issue dialog does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Write {
    /// The reader's comment, by its place among the comments.
    EditComment(usize),
    DeleteComment(usize),
}

impl IssuesState {
    /// How many open issues are assigned to the reader.
    pub fn assigned(&self) -> usize {
        let Some(Ok((me, issues))) = &self.list else { return 0 };
        issues.iter().filter(|i| i.open && i.assignees.contains(me)).count()
    }
}

/// How each group of the list is ordered, as GitHub's sort menu offers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Sort {
    #[default]
    Updated,
    Newest,
    Oldest,
    Commented,
}

impl Sort {
    const ALL: [(Sort, &'static str); 4] = [(Sort::Updated, "Recently Updated"), (Sort::Newest, "Newest"), (Sort::Oldest, "Oldest"), (Sort::Commented, "Most Commented")];

    fn name(self) -> &'static str {
        Sort::ALL.iter().find(|(s, _)| *s == self).map_or("", |(_, n)| n)
    }

    /// Order the issues; ties keep the order they came in, newest activity first.
    fn apply(self, issues: &mut [Issue]) {
        match self {
            // The times are ISO 8601, so they order as text.
            Sort::Updated => issues.sort_by(|a, b| b.updated.cmp(&a.updated)),
            Sort::Newest => issues.sort_by(|a, b| b.created.cmp(&a.created)),
            Sort::Oldest => issues.sort_by(|a, b| a.created.cmp(&b.created)),
            Sort::Commented => issues.sort_by_key(|i| std::cmp::Reverse(i.comments)),
        }
    }
}

/// The issues the list shows: open ones to act on, closed ones to look back on, or both.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Shown {
    #[default]
    Open,
    Closed,
    All,
}

impl Shown {
    fn wants(self, i: &Issue) -> bool {
        match self {
            Shown::Open => i.open,
            Shown::Closed => !i.open,
            Shown::All => true,
        }
    }
}

impl Default for IssuesState {
    fn default() -> IssuesState {
        IssuesState { open: false, list: None, loading: false, folded: HashSet::new(), filter: TextEdit::new(""), shown: Shown::Open, label: None, assignee: None, sort: Sort::Updated, pick: None, number: None, detail: None, reply: TextEdit::new(""), write: None, text: TextEdit::new("") }
    }
}

// Lucide's circle-check and circle-slash (ISC licence): an issue closed as done, and one closed
// as not planned or a duplicate, as GitHub marks them.
crate::lucide!(DONE_ICON, "collate-issue-done", r#"<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>"#);
crate::lucide!(SKIPPED_ICON, "collate-issue-skipped", r#"<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/>"#);

/// The issue's state as GitHub marks it: its icon, colour and name.
pub fn state_mark(k: &Kit, i: &Issue) -> (&'static Icon, rune::Color, &'static str) {
    match i.reason.as_str() {
        _ if i.open => (&ISSUES_ICON, k.c.success.icon, "Open"),
        // The kit has no violet of its own; the syntax colours' keyword violet follows the theme.
        "COMPLETED" => (&DONE_ICON, SyntaxTheme::from_kit(k).color(Syntax::Keyword), "Closed"),
        "DUPLICATE" => (&SKIPPED_ICON, k.c.text_faint, "Duplicate"),
        _ => (&SKIPPED_ICON, k.c.text_faint, "Not planned"),
    }
}

/// The signed-in login and the issues.
type List = (String, Vec<Issue>);

/// Each project's last list, shown at once when it's opened again while a new one is read.
static LISTS: std::sync::Mutex<Option<HashMap<PathBuf, List>>> = std::sync::Mutex::new(None);

/// The last list read for the project at `root`.
pub fn cached(root: &Path) -> Option<List> {
    LISTS.lock().ok()?.as_ref()?.get(root).cloned()
}

impl Collate {
    /// Show the issues as last read, and read them again.
    pub fn open_issues(&mut self) {
        self.map.open = false;
        self.pulls.open = false;
        self.close_pull();
        self.issues.open = true;
        self.issues.number = None;
        self.load_issues();
    }

    /// Open issue `n` in place of the list, shown at once from the list while the rest is read.
    pub fn open_issue(&mut self, n: u32) {
        self.issues.number = Some(n);
        self.issues.pick = Some(n);
        self.issues.detail = None;
        self.load_issue();
    }

    /// Back from an issue to the list, where it was.
    pub fn close_issue(&mut self, tx: &mut Tx) {
        if let Some(n) = self.issues.number.take() {
            self.issues.detail = None;
            tx.scroll_into_view(format!("issue-{n}"));
        }
    }

    /// Read the open issue again, off the UI thread.
    fn load_issue(&mut self) {
        let (Some(n), Some(root), Some(app)) = (self.issues.number, current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let got = Gh::new(&root).issue(n);
            app.update(move |s, _| {
                // The reader went back, or on to another issue, meanwhile.
                if s.issues.number == Some(n) && current_repo().is_some_and(|r| r.root == root) {
                    s.issues.detail = Some(got);
                }
            });
        });
    }

    /// Run a change to an issue off the UI thread, report it, and reread the issue and the
    /// list. One at a time, with the app's other slow changes.
    fn issue_act(&mut self, doing: &str, f: impl FnOnce(&Gh) -> Result<String, String> + Send + 'static, then: impl FnOnce(&mut Collate) + Send + 'static) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        if self.busy.is_some() {
            self.say(format!("Wait for {} to finish", self.busy.as_deref().unwrap_or_default().trim_end_matches('…').to_lowercase()), true);
            return;
        }
        self.busy = Some(doing.to_owned());
        std::thread::spawn(move || {
            let result = f(&Gh::new(&root));
            app.update(move |s, _| {
                s.busy = None;
                match result {
                    Ok(done) => {
                        s.say(done, false);
                        then(s);
                    }
                    Err(e) => s.say(e, true),
                }
                s.load_issue();
                s.load_issues();
            });
        });
    }

    /// Send the comment written at the foot of the open issue.
    pub fn send_issue_comment(&mut self) {
        let (Some(n), body) = (self.issues.number, self.issues.reply.text.trim().to_owned()) else { return };
        if body.is_empty() {
            return;
        }
        // Kept until it's sent, so a failure doesn't lose it.
        self.issue_act("Commenting…", move |gh| gh.issue_comment(n, &body).map(|_| format!("Commented on #{n}")), |s| s.issues.reply = TextEdit::new(""));
    }

    /// Close the open issue for `reason` (`completed` or `not planned`), or reopen it with
    /// `None`, with the comment written, if any, said as it goes.
    pub fn set_issue_open(&mut self, reason: Option<&'static str>) {
        let Some(n) = self.issues.number else { return };
        let body = self.issues.reply.text.trim().to_owned();
        let (doing, done) = match reason {
            Some(_) => ("Closing…", format!("Closed #{n}")),
            None => ("Reopening…", format!("Reopened #{n}")),
        };
        self.issue_act(doing, move |gh| gh.set_issue_open(n, reason, &body).map(|_| done), |s| s.issues.reply = TextEdit::new(""));
    }

    fn react_to_issue_post(&mut self, id: String, content: &'static str) {
        self.issue_act("Reacting…", move |gh| gh.react(&id, content).map(|_| "Reacted".to_owned()), |_| {});
    }

    /// Comment `at`'s menu: its link, a quoted reply and a reaction, and for the reader's own,
    /// editing and deleting it.
    fn issue_post_menu(&mut self, at: usize, tx: &mut Tx) {
        let Some(Ok(d)) = &self.issues.detail else { return };
        let Some(c) = d.comments.get(at).cloned() else { return };
        let react = PopupMenu::new().items(github::REACTIONS.iter().map(|(name, emoji)| PopupItem::new(*name, *emoji)));
        let mine = c.mine && !c.id.is_empty();
        let menu = PopupMenu::new()
            .item(PopupItem::new("copy", "Copy Link"))
            .item(PopupItem::new("quote", "Quote Reply"))
            .submenu_enabled("React", react, !c.id.is_empty())
            .separator()
            .item(PopupItem::new("edit", "Edit").enabled(mine))
            .item(PopupItem::new("delete", "Delete…").enabled(mine));
        tx.popup_menu(menu, format!("issue-comment-{at}-more"), move |s: &mut Collate, picked, tx| match picked {
            Some("copy") => {
                tx.copy(c.url.clone());
                s.say("Copied the comment's link", false);
            }
            Some("quote") => {
                let quoted: String = c.body.trim().lines().map(|l| format!("> {l}\n")).collect();
                s.issues.reply = TextEdit::new(format!("{quoted}\n"));
                s.issues.reply.focused = true;
                tx.scroll_into_view("issue-composer");
            }
            Some("edit") => s.start_issue_write(Write::EditComment(at)),
            Some("delete") => s.start_issue_write(Write::DeleteComment(at)),
            Some(name) => {
                if let Some((content, _)) = github::REACTIONS.iter().find(|(n, _)| *n == name) {
                    s.react_to_issue_post(c.id.clone(), content);
                }
            }
            None => {}
        });
    }

    pub fn start_issue_write(&mut self, w: Write) {
        let Some(Ok(d)) = &self.issues.detail else { return };
        self.issues.text = TextEdit::new(match w {
            Write::EditComment(at) => d.comments.get(at).map_or("", |c| c.body.as_str()),
            Write::DeleteComment(_) => "",
        });
        self.issues.text.focused = true;
        self.issues.write = Some(w);
    }

    fn send_issue_write(&mut self) {
        let (Some(w), Some(Ok(d))) = (self.issues.write.take(), &self.issues.detail) else { return };
        let body = self.issues.text.text.trim().to_owned();
        match w {
            Write::EditComment(at) => {
                let Some(id) = d.comments.get(at).map(|c| c.id.clone()) else { return };
                self.issue_act("Saving…", move |gh| gh.edit_post(&id, false, &body).map(|_| "Saved the comment".to_owned()), |_| {});
            }
            Write::DeleteComment(at) => {
                let Some(id) = d.comments.get(at).map(|c| c.id.clone()) else { return };
                self.issue_act("Deleting…", move |gh| gh.delete_post(&id).map(|_| "Deleted the comment".to_owned()), |_| {});
            }
        }
    }

    fn pick_close_reason(&mut self, tx: &mut Tx) {
        let menu = PopupMenu::new().items([PopupItem::new("completed", "Close as Completed"), PopupItem::new("not planned", "Close as Not Planned")]);
        tx.popup_menu(menu, "issue-close-reason", |s: &mut Collate, picked: Option<&str>, _| match picked {
            Some("completed") => s.set_issue_open(Some("completed")),
            Some("not planned") => s.set_issue_open(Some("not planned")),
            _ => {}
        });
    }

    /// Asks for the pictures of everyone the issues show: the reader, the authors and the assignees.
    fn request_issue_faces(&mut self) {
        let Some(Ok((me, issues))) = &self.issues.list else { return };
        let logins = std::iter::once(me.as_str()).chain(issues.iter().flat_map(|i| std::iter::once(i.author.as_str()).chain(i.assignees.iter().map(String::as_str))));
        self.avatars.request_logins(logins.collect::<Vec<_>>());
    }

    /// Read the list again, off the UI thread. Whatever list shows stays until it lands.
    pub fn load_issues(&mut self) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        if std::mem::replace(&mut self.issues.loading, true) {
            return;
        }
        std::thread::spawn(move || {
            let got = Gh::new(&root).issues();
            if let (Ok(list), Ok(mut lists)) = (&got, LISTS.lock()) {
                lists.get_or_insert_default().insert(root.clone(), list.clone());
            }
            app.update(move |s, _| {
                // The reader switched projects meanwhile: this list was the last one's.
                if current_repo().is_none_or(|r| r.root != root) {
                    return;
                }
                s.issues.loading = false;
                s.issues.list = Some(got);
                s.request_issue_faces();
            });
        });
    }
}

/// The list of issues, or the issue open, in place of the diff.
pub fn issues_view(s: &Collate, k: &Kit) -> El {
    if let Some(n) = s.issues.number {
        return overview(s, k, n);
    }
    let refresh = IconButton::new(&REFRESH_ICON).variant(IconButtonVariant::GhostMuted).label("Reload issues").disabled(s.issues.loading).on_press(|s: &mut Collate| {
        s.issues.list = None;
        s.load_issues();
    });
    let mut filter = TextInput::new(&s.issues.filter).placeholder("Filter by title, number, author, assignee or label").leading_icon(k.glyph.search);
    if !s.issues.filter.text.is_empty() {
        filter = filter.clear_button();
    }
    let filter = filter.on_edit(|s: &mut Collate| &mut s.issues.filter).width(None).build(k).key("issues-filter").w(280.0);
    let mut head = row().key("issues-head").w_full().h(28.0).px(EDGE).gap(8.0).items_center()
        .child(text("Issues".to_owned()).font(k.ty.ui_medium).color(k.c.text_base).grow(1.0));
    // Filtering needs the issues, so it waits until GitHub is connected.
    if !matches!(&s.issues.list, Some(Err(e)) if github::needs_connect(e)) {
        head = head.child(filter);
    }
    let head = head.child(refresh.build(k).hand(!s.issues.loading).key("issues-reload"));
    let body = match &s.issues.list {
        None => message(k, "Reading issues…", ""),
        Some(Err(e)) if github::needs_connect(e) => crate::pulls::connect_view(s, k, "See this repository's issues, comment on them, and open new ones without leaving Collate."),
        Some(Err(e)) => message(k, "Can't read issues", e),
        Some(Ok((_, issues))) if issues.is_empty() => message(k, "No issues", "This repository has no open or recently closed issues."),
        Some(Ok(_)) => {
            let sections = sections(s);
            if sections.is_empty() {
                let narrowed = !s.issues.filter.text.trim().is_empty() || s.issues.label.is_some() || s.issues.assignee.is_some();
                let (title, hint) = match (s.issues.shown, narrowed) {
                    (Shown::Open, false) => ("No open issues", "Closed shows the ones done with."),
                    (Shown::Closed, false) => ("No closed issues", "None were closed lately."),
                    _ => ("No issues match", "Try fewer words, or another label or assignee."),
                };
                return col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(filters(s, k)).child(message(k, title, hint));
            }
            let mut list = col().key("issues-list").w_full().pb(16.0);
            for (title, group) in sections {
                let folded = s.issues.folded.contains(title);
                list = list.child(group_head(k, title, group.len(), folded));
                if !folded {
                    for i in group {
                        list = list.child(issue_row(s, k, &i));
                    }
                }
            }
            return col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(filters(s, k))
                .child(col().key("issues-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(list));
        }
    };
    col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(body)
}

/// The groups the list shows, as filtered and sorted, leaving out the empty ones.
fn sections(s: &Collate) -> Vec<(&'static str, Vec<Issue>)> {
    let Some(Ok((me, issues))) = &s.issues.list else { return Vec::new() };
    let words: Vec<String> = s.issues.filter.text.split_whitespace().map(str::to_lowercase).collect();
    let mut shown: Vec<Issue> = issues.iter().filter(|i| s.issues.shown.wants(i) && has_label(i, s.issues.label.as_deref()) && has_assignee(i, s.issues.assignee.as_deref()) && matches(i, &words)).cloned().collect();
    s.issues.sort.apply(&mut shown);
    github_issues::group(&shown, me).into_iter().filter(|(_, g)| !g.is_empty()).map(|(t, g)| (t, g.into_iter().cloned().collect())).collect()
}

impl Collate {
    /// Move the list's pick to the next issue shown, or the one before, and bring it into view.
    pub fn step_issue(&mut self, down: bool, tx: &mut Tx) {
        let shown: Vec<u32> = sections(self).into_iter().filter(|(t, _)| !self.issues.folded.contains(t)).flat_map(|(_, g)| g.into_iter().map(|i| i.number)).collect();
        let at = self.issues.pick.and_then(|n| shown.iter().position(|m| *m == n));
        let next = match (at, down) {
            (None, true) => shown.first(),
            (None, false) => shown.last(),
            (Some(a), true) => shown.get(a + 1).or(shown.get(a)),
            (Some(a), false) => shown.get(a.saturating_sub(1)),
        };
        if let Some(&n) = next {
            self.issues.pick = Some(n);
            tx.scroll_into_view(format!("issue-{n}"));
        }
    }
}

/// The row under the head that narrows the list: by state, then the menus.
/// Not keyed as a filter: a focused control under a key with "filter" in it keeps the letters.
fn filters(s: &Collate, k: &Kit) -> El {
    let at = [Shown::Open, Shown::Closed, Shown::All];
    let state = SegmentedControl::new(&["Open", "Closed", "All"], at.iter().position(|w| *w == s.issues.shown))
        .label("Issues to show")
        .fit()
        .on_pick(move |s: &mut Collate, i, _: &mut Tx| s.issues.shown = at[i])
        .build(k).hand(true)
        .key("issues-state");
    let label = match s.issues.label.as_deref() {
        None => "Label".to_owned(),
        Some("") => "No label".to_owned(),
        Some(l) => format!("Label: {l}"),
    };
    let label = menu_button(k, "issues-label", label, s.issues.label.is_some(), "Show only issues with a label", |s, tx| s.pick_issue_label(tx));
    let assignee = match s.issues.assignee.as_deref() {
        None => "Assignee".to_owned(),
        Some("") => "No assignee".to_owned(),
        Some(a) => format!("Assignee: {a}"),
    };
    let assignee = menu_button(k, "issues-assignee", assignee, s.issues.assignee.is_some(), "Show only issues someone has", |s, tx| s.pick_issue_assignee(tx));
    let sort = menu_button(k, "issues-sort", s.issues.sort.name().to_owned(), false, "Order the issues", |s, tx| {
        let menu = PopupMenu::picker(Sort::ALL.iter().map(|(_, n)| (*n, *n)), s.issues.sort.name());
        tx.popup_menu(menu, "issues-sort", |s: &mut Collate, picked, _| {
            if let Some(&(sort, _)) = Sort::ALL.iter().find(|(_, n)| Some(*n) == picked) {
                s.issues.sort = sort;
            }
        });
    });
    row().key("issues-bar").w_full().px(EDGE).pt(8.0).pb(4.0).gap(8.0).items_center().child(state).child(label).child(assignee)
        .child(row().grow(1.0)).child(sort)
}

/// A button that opens a menu, with the chevron that says so; it stands out while it narrows.
fn menu_button(k: &Kit, key: &'static str, title: String, set: bool, help: &'static str, open: fn(&mut Collate, &mut Tx)) -> El {
    let chevron = k.glyph.chevron_down;
    Button::new(title.clone())
        .variant(if set { ButtonVariant::Outline } else { ButtonVariant::GhostMuted })
        .label_with(move |font, ink| row().gap(4.0).items_center().child(text(title).font(font).color(ink)).child(icon(chevron, 14.0).color(ink).a11y_hidden()))
        .help(help)
        .on_press_tx(open)
        .build(k).hand(true)
        .key(key)
}

impl Collate {
    /// Narrow the list to a label the issues have, or to those with none.
    fn pick_issue_label(&mut self, tx: &mut Tx) {
        let Some(Ok((_, issues))) = &self.issues.list else { return };
        let mut names: Vec<&str> = issues.iter().flat_map(|i| i.labels.iter().map(|(l, _)| l.as_str())).collect();
        names.sort_by_key(|l| l.to_lowercase());
        names.dedup();
        let now = match self.issues.label.as_deref() {
            None => "any".to_owned(),
            Some("") => "none".to_owned(),
            Some(l) => format!("label:{l}"),
        };
        let menu = PopupMenu::new().item(PopupItem::new("any", "Any Label")).item(PopupItem::new("none", "No Label")).separator()
            .items(names.iter().map(|l| PopupItem::new(format!("label:{l}"), *l)))
            .selected(now);
        tx.popup_menu(menu, "issues-label", |s: &mut Collate, picked, _| match picked {
            Some("any") => s.issues.label = None,
            Some("none") => s.issues.label = Some(String::new()),
            Some(p) => s.issues.label = p.strip_prefix("label:").map(str::to_owned),
            None => {}
        });
    }
}

impl Collate {
    /// Narrow the list to someone's issues, the reader first, or to those no one has.
    fn pick_issue_assignee(&mut self, tx: &mut Tx) {
        let Some(Ok((me, issues))) = &self.issues.list else { return };
        let mut logins: Vec<&str> = issues.iter().flat_map(|i| i.assignees.iter().map(String::as_str)).filter(|a| a != me).collect();
        logins.sort_by_key(|a| a.to_lowercase());
        logins.dedup();
        let now = match self.issues.assignee.as_deref() {
            None => "any".to_owned(),
            Some("") => "none".to_owned(),
            Some(a) => format!("login:{a}"),
        };
        let menu = PopupMenu::new().item(PopupItem::new("any", "Anyone")).item(PopupItem::new("none", "No One"))
            .item(PopupItem::new(format!("login:{me}"), format!("{me} (you)"))).separator()
            .items(logins.iter().map(|a| PopupItem::new(format!("login:{a}"), *a)))
            .selected(now);
        tx.popup_menu(menu, "issues-assignee", |s: &mut Collate, picked, _| match picked {
            Some("any") => s.issues.assignee = None,
            Some("none") => s.issues.assignee = Some(String::new()),
            Some(p) => s.issues.assignee = p.strip_prefix("login:").map(str::to_owned),
            None => {}
        });
    }
}

/// Whether `login` has the issue, or no one when it's empty; any issue when there's no login.
fn has_assignee(i: &Issue, login: Option<&str>) -> bool {
    match login {
        None => true,
        Some("") => i.assignees.is_empty(),
        Some(a) => i.assignees.iter().any(|n| n == a),
    }
}

/// Whether the issue has the label, or none when it's empty; any issue when there's no label.
fn has_label(i: &Issue, label: Option<&str>) -> bool {
    match label {
        None => true,
        Some("") => i.labels.is_empty(),
        Some(l) => i.labels.iter().any(|(n, _)| n == l),
    }
}

/// Whether every word is in the issue's title, number, author, assignees or labels.
fn matches(i: &Issue, words: &[String]) -> bool {
    let labels: Vec<&str> = i.labels.iter().map(|(l, _)| l.as_str()).collect();
    let hay = format!("{} #{} {} {} {}", i.title, i.number, i.author, i.assignees.join(" "), labels.join(" ")).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// A group's title and count; a click anywhere on it folds the group to it or opens it again.
fn group_head(k: &Kit, title: &'static str, count: usize, folded: bool) -> El {
    row().key(format!("issue-group-{title}")).w_full().h(28.0).px(EDGE).gap(6.0).items_center().mt(8.0)
        .cursor(Cursor::Pointer).role("button").label(title.to_owned()).description(format!("{count} issues")).expanded(!folded)
        .help(if folded { "Show the group" } else { "Fold the group to its title" })
        .on_press(move |s: &mut Collate| {
            if !s.issues.folded.remove(title) {
                s.issues.folded.insert(title);
            }
        })
        .child(text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
        .child(text(count.to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint).grow(1.0).a11y_hidden())
        // The staging panel's chevron, turned as it turns; its glyph, not its box, on the edge.
        .child(icon(k.glyph.chevron_down, 16.0).mr(-4.0).color(k.c.icon_faint)
            .icon_rotation(if folded { -std::f32::consts::FRAC_PI_2 } else { 0.0 }).motion(k.motion.control).a11y_hidden())
}

/// One issue in the list: its state, title and labels, who opened it and when, who has it,
/// and how much it's been talked about.
fn issue_row(s: &Collate, k: &Kit, i: &Issue) -> El {
    let n = i.number;
    let (mark, hue, state) = state_mark(k, i);
    let mut title = row().w_full().min_w(0.0).gap(6.0).items_center()
        .child(text(i.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate().shrink());
    for (name, color) in i.labels.iter().take(3) {
        title = title.child(crate::pulls::label_pill(k, name, crate::pulls::label_color(k, color)));
    }
    if i.labels.len() > 3 {
        title = title.child(text(format!("+{}", i.labels.len() - 3)).font(k.ty.text_xsmall_medium).color(k.c.text_faint));
    }
    let words = col().grow(1.0).min_w(0.0).gap(2.0)
        .child(title)
        .child(text(format!("#{n} - opened by {} - {}", i.author, i.created.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).truncate());
    let mut line = row().key(format!("issue-{n}")).w_full().px(EDGE).py(8.0).gap(12.0).items_center()
        .fill(if s.issues.pick == Some(n) { k.c.hover } else { rune::Color::TRANSPARENT })
        .hover(k.c.hover).cursor(Cursor::Pointer).role("button").label(format!("#{n} {}", i.title)).description(state).help("Open this issue")
        .on_press(move |s: &mut Collate| s.open_issue(n))
        .child(icon(mark, 16.0).color(hue).a11y_hidden())
        .child(words);
    // Who has it, overlapping as GitHub stacks them.
    let mut faces = row().items_center();
    for (at, a) in i.assignees.iter().take(3).enumerate() {
        faces = faces.child(crate::history_view::face(k, s, a, 20.0).ml(if at == 0 { 0.0 } else { -6.0 }));
    }
    line = line.child(faces);
    let comments = row().w(40.0).gap(4.0).items_center().justify_end();
    let comments = if i.comments > 0 {
        comments.child(icon(&crate::pulls::MESSAGE_ICON, 14.0).color(k.c.icon_muted).a11y_hidden())
            .child(text(i.comments.to_string()).font(k.ty.text_small).color(k.c.text_muted))
    } else {
        comments
    };
    line.child(comments)
}

/// The issue open, as GitHub lays it out: its title and state, who opened it, and what it says.
fn overview(s: &Collate, k: &Kit, n: u32) -> El {
    let listed = s.issues.list.as_ref().and_then(|l| l.as_ref().ok()).and_then(|(_, all)| all.iter().find(|i| i.number == n));
    let detail = s.issues.detail.as_ref().and_then(|d| d.as_ref().ok());
    let back = row().key("issue-back").h(28.0).px(EDGE).gap(4.0).items_center().cursor(Cursor::Pointer).role("button").label("Back to Issues").help("Back to the list (Esc)")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.close_issue(tx))
        .child(icon(k.glyph.chevron_down, 16.0).color(k.c.icon_muted).icon_rotation(std::f32::consts::FRAC_PI_2).a11y_hidden())
        .child(text("Issues".to_owned()).font(k.ty.text_small_medium).color(k.c.text_muted));
    let head = row().key("issue-head").w_full().pt(COLUMN_TOP).child(back);
    let Some(i) = detail.map(|d| &d.issue).or(listed) else {
        let body = match &s.issues.detail {
            Some(Err(e)) => message(k, "Can't read the issue", e),
            _ => message(k, "Reading the issue…", ""),
        };
        return col().key("issues").w_full().grow(1.0).min_h(0.0).child(head).child(body);
    };
    let (mark, hue, state) = state_mark(k, i);
    let title_font = rune::TextStyle { size: 23.0, line_height: 32.0, weight: 600.0, ..k.ty.ui_medium };
    let pill = row().key("issue-state").h(22.0).px(8.0).gap(5.0).items_center().radius(11.0).fill(k.alpha(hue, 0.16))
        .child(icon(mark, 12.0).color(hue))
        .child(text(state.to_owned()).font(k.ty.text_small_medium).color(hue));
    let muted = |t: String| text(t).font(k.ty.text_small).color(k.c.text_muted);
    let byline = row().key("issue-byline").w_full().gap(6.0).items_center().flex_wrap()
        .child(pill)
        .child(crate::history_view::face(k, s, &i.author, 20.0).ml(2.0))
        .child(text(i.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(muted(format!("opened this issue on {}", i.created.get(..10).unwrap_or(""))))
        .child(text(format!("#{n}")).font(k.ty.text_small).color(k.c.text_faint));
    let body = match &s.issues.detail {
        None => text("Reading…".to_owned()).key("issue-body").font(k.ty.ui).color(k.c.text_faint),
        Some(Err(e)) => message(k, "Can't read the issue", e),
        Some(Ok(d)) if d.body.trim().is_empty() => text("No description provided.".to_owned()).key("issue-body").font(k.ty.ui).color(k.c.text_faint),
        Some(Ok(d)) => crate::pulls::rich(s, k, "issue-body", &d.body, k.ty.ui),
    };
    let mut main = col().key("issue-main").grow(1.0).min_w(0.0).max_w(780.0).gap(28.0)
        .child(col().key("issue-title").w_full().gap(10.0)
            .child(text(i.title.clone()).font(title_font).color(k.c.text_base).wrap().w_full())
            .child(byline))
        .child(body);
    if let Some(d) = detail {
        let n = d.comments.len();
        let mut activity = col().key("issue-activity").w_full().gap(10.0)
            .child(text(format!("Activity - {n} comment{}", if n == 1 { "" } else { "s" })).font(k.ty.text_small_medium).color(k.c.text_muted));
        if d.comments.is_empty() {
            activity = activity.child(text("No comments yet.".to_owned()).font(k.ty.text_small).color(k.c.text_faint));
        }
        // Comments and events as they happened.
        let mut items: Vec<(&str, El)> = d.comments.iter().enumerate().map(|(at, c)| (c.time.as_str(), comment_card(s, k, at, c)))
            .chain(d.events.iter().enumerate().map(|(at, e)| (e.time.as_str(), event_row(s, k, at, e)))).collect();
        items.sort_by_key(|(time, _)| *time);
        activity = activity.children(items.into_iter().map(|(_, el)| el));
        main = main.child(activity).child(composer(s, k));
    }
    col().key("issues").w_full().grow(1.0).min_h(0.0).child(head)
        .child(col().key("issue-overview").w_full().grow(1.0).min_h(0.0).items_center().scroll(rune::ScrollSpec { stick_bottom: false })
            .child(row().key("issue-page").w_full().max_w(1100.0).px(EDGE * 2.0).pt(16.0).pb(48.0).gap(40.0).items_start().child(main)))
}

/// The dialog over the open issue: editing the reader's comment, or asking before deleting it.
pub fn write_dialog(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let Some(w) = s.issues.write else { return };
    let (title, what, button) = match w {
        Write::EditComment(_) => ("Edit comment", "Saved to GitHub.", "Save"),
        Write::DeleteComment(_) => ("Delete this comment?", "It's removed from the issue for everyone, and can't be brought back.", "Delete"),
    };
    let delete = matches!(w, Write::DeleteComment(_));
    let blocked = s.busy.is_some() || (!delete && s.issues.text.text.trim().is_empty());
    let mut body = col().key("issue-write-body").w_full().gap(12.0);
    if !delete {
        body = body.child(TextArea::new(&s.issues.text).placeholder("Write a comment").label("Comment").rows(8).max_height(320.0)
            .on_edit(|s: &mut Collate| &mut s.issues.text).width(None).build(k).key("issue-write-message").w_full());
    }
    let buttons = row().key("issue-write-buttons").w_full().gap(8.0).items_center().child(row().grow(1.0))
        .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.issues.write = None).build(k).hand(true).key("issue-write-cancel"))
        .child(Button::new(button).variant(if delete { ButtonVariant::Danger } else { ButtonVariant::Contrast }).disabled(blocked)
            .on_press(|s: &mut Collate| s.send_issue_write()).build(k).hand(!blocked).key("issue-write-ok"));
    crate::described_dialog(k, "issue-write", title, what, body.child(buttons)).width(560.0).on_close(|s: &mut Collate| s.issues.write = None).build(k, layer);
}

/// The box at the foot of the activity for the reader's comment, beside their face.
fn composer(s: &Collate, k: &Kit) -> El {
    let me = s.issues.list.as_ref().and_then(|l| l.as_ref().ok()).map_or("", |(me, _)| me.as_str());
    let empty = s.issues.reply.text.trim().is_empty();
    let blocked = empty || s.busy.is_some();
    // Keyed as a message so typing in it isn't taken for the list's keys.
    let area = TextArea::new(&s.issues.reply).placeholder("Add a comment").label("Add a comment").rows(4).max_height(240.0)
        .on_edit(|s: &mut Collate| &mut s.issues.reply).width(None).build(k).key("issue-message").w_full();
    let send = Button::new("Comment").variant(ButtonVariant::Contrast).disabled(blocked).help("Add it to the issue (⌘↩)")
        .on_press(|s: &mut Collate| s.send_issue_comment()).build(k).hand(!blocked).key("issue-comment-send");
    // As GitHub words it: what's written goes with the closing or reopening.
    let open = s.issues.detail.as_ref().and_then(|d| d.as_ref().ok()).is_none_or(|d| d.issue.open);
    let busy = s.busy.is_some();
    let with = if empty { "" } else { " with Comment" };
    let state = if open {
        row().gap(2.0).items_center()
            .child(Button::new(format!("Close{with}")).icon(&DONE_ICON).variant(ButtonVariant::Outline).disabled(busy).help("Close it as completed")
                .on_press(|s: &mut Collate| s.set_issue_open(Some("completed"))).build(k).hand(!busy).key("issue-close"))
            .child(IconButton::new(k.glyph.chevron_down).variant(IconButtonVariant::GhostMuted).label("Close as…").disabled(busy)
                .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.pick_close_reason(tx)).build(k).hand(!busy).key("issue-close-reason"))
    } else {
        row().child(Button::new(format!("Reopen{with}")).icon(&ISSUES_ICON).variant(ButtonVariant::Outline).disabled(busy).help("Open it again")
            .on_press(|s: &mut Collate| s.set_issue_open(None)).build(k).hand(!busy).key("issue-reopen"))
    };
    row().key("issue-composer").w_full().gap(12.0).items_start()
        .child(crate::history_view::face(k, s, me, 32.0).mt(2.0))
        .child(col().grow(1.0).min_w(0.0).gap(8.0).child(area).child(row().w_full().gap(8.0).items_center().child(row().grow(1.0)).child(state).child(send)))
}

/// An event in the activity, set between the comments as GitHub sets it: a mark in the faces'
/// column, then who did what and when.
fn event_row(s: &Collate, k: &Kit, at: usize, e: &github_issues::Event) -> El {
    let (glyph, tint): (&'static Icon, rune::Color) = match (e.kind.as_str(), e.what.as_str()) {
        ("ClosedEvent", w) if w.ends_with("completed") => (&DONE_ICON, SyntaxTheme::from_kit(k).color(Syntax::Keyword)),
        ("ClosedEvent", _) => (&SKIPPED_ICON, k.c.text_faint),
        ("ReopenedEvent", _) => (&ISSUES_ICON, k.c.success.icon),
        ("RenamedTitleEvent", _) => (&crate::pulls::PENCIL_ICON, k.c.icon_muted),
        ("CrossReferencedEvent", _) => (&crate::pulls::LINK_ICON, k.c.icon_muted),
        _ => (&crate::pulls::CLOCK_ICON, k.c.icon_faint),
    };
    let mut line = row().key(format!("issue-event-{at}")).w_full().gap(12.0).items_center().min_h(28.0)
        .child(row().w(32.0).center().child(icon(glyph, 14.0).color(tint).a11y_hidden()))
        .child(crate::history_view::face(k, s, &e.who, 16.0))
        .child(text(e.who.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(text(e.what.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).shrink());
    if let Some((name, color)) = &e.label {
        line = line.child(crate::pulls::label_pill(k, name, crate::pulls::label_color(k, color)));
    }
    line = line.child(text(e.time.get(..10).unwrap_or("").to_owned()).font(k.ty.text_small).color(k.c.text_faint));
    if !e.url.is_empty() {
        let url = e.url.clone();
        line = line.cursor(Cursor::Pointer).role("link").help("Open it on GitHub").on_press(move |_: &mut Collate| crate::open_url(&url));
    }
    line
}

/// A comment in the activity: who wrote it beside it, as GitHub's timeline sets it, then when,
/// what they wrote, and its reactions.
fn comment_card(s: &Collate, k: &Kit, at: usize, c: &github::Post) -> El {
    let key = format!("issue-comment-{at}");
    let head = row().w_full().px(12.0).py(8.0).gap(6.0).items_center().fill(k.alpha(k.c.text_base, 0.03))
        .child(text(c.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(text(format!("commented on {}", c.time.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).grow(1.0))
        .child(IconButton::new(&MORE_ICON).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Small).label(format!("Actions for {}'s comment", c.author))
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.issue_post_menu(at, tx)).build(k).hand(true).key(format!("{key}-more")).mr(-6.0));
    let mut card = col().key(key.clone()).w_full().radius(k.radius.md).border(1.0, k.c.border_weak).child(head)
        .child(col().w_full().h(1.0).fill(k.c.border_weak))
        .child(col().w_full().px(12.0).py(10.0).child(crate::pulls::rich(s, k, &format!("{key}-body"), &c.body, k.ty.text_small)));
    if !c.reactions.is_empty() {
        card = card.child(row().key(format!("{key}-reactions")).w_full().px(12.0).pb(10.0).gap(6.0).flex_wrap().children(c.reactions.iter().filter_map(|(name, n)| {
            let (content, emoji) = github::REACTIONS.iter().find(|(c, _)| c == name)?;
            let id = c.id.clone();
            // A press adds the reader's own, as on GitHub.
            Some(row().key(format!("{key}-react-{content}")).h(24.0).px(8.0).gap(4.0).items_center().radius(12.0).border(1.0, k.c.border_weak)
                .hover(k.c.hover).cursor(Cursor::Pointer).role("button").label(format!("{n} {content}, add yours"))
                .on_press(move |s: &mut Collate| s.react_to_issue_post(id.clone(), content))
                .child(text((*emoji).to_owned()).font(k.ty.text_small).a11y_hidden())
                .child(text(n.to_string()).font(k.ty.text_small).color(k.c.text_muted).a11y_hidden()))
        })));
    }
    row().w_full().gap(12.0).items_start()
        .child(crate::history_view::face(k, s, &c.author, 32.0).mt(2.0))
        .child(card.grow(1.0).min_w(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorting_orders_by_time_or_talk() {
        let i = |number: u32, created: &str, updated: &str, comments: u32| Issue { number, created: created.into(), updated: updated.into(), comments, ..Issue::default() };
        let mut all = [i(1, "2026-01-01", "2026-03-01", 0), i(2, "2026-02-01", "2026-02-02", 5), i(3, "2026-01-15", "2026-04-01", 2)];
        let order = |all: &[Issue]| all.iter().map(|i| i.number).collect::<Vec<_>>();
        for (sort, want) in [(Sort::Updated, [3, 1, 2]), (Sort::Newest, [2, 3, 1]), (Sort::Oldest, [1, 3, 2]), (Sort::Commented, [2, 3, 1])] {
            sort.apply(&mut all);
            assert_eq!(order(&all), want, "{sort:?}");
        }
    }

    #[test]
    fn the_filter_wants_every_word_somewhere() {
        let i = Issue { number: 12, title: "Crash on launch".into(), author: "ana".into(), assignees: vec!["bo".into()], labels: vec![("bug".into(), String::new())], ..Issue::default() };
        let w = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert!(matches(&i, &w("crash bug")) && matches(&i, &w("#12")) && matches(&i, &w("bo ana")) && matches(&i, &[]));
        assert!(!matches(&i, &w("crash feature")), "every word, not any");
    }
}

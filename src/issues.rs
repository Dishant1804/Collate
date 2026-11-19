//! The Issues place: the repository's issues, grouped by what they want of the reader, and one
//! issue's overview, in place of the diff, as the pull requests are.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::{Cursor, El, Icon, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::syntax::{Syntax, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, IconButton, IconButtonVariant, Kit, SegmentedControl, TextInput};

use crate::Hand;
use crate::github_issues::{self, Issue};
use crate::github::{self, Gh};
use crate::{COLUMN_TOP, Collate, EDGE, HANDLE, ISSUES_ICON, REFRESH_ICON, current_repo, message};

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
        IssuesState { open: false, list: None, loading: false, folded: HashSet::new(), filter: TextEdit::new(""), shown: Shown::Open, label: None, assignee: None, sort: Sort::Updated }
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
        self.load_issues();
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

/// The list of issues, in place of the diff.
pub fn issues_view(s: &Collate, k: &Kit) -> El {
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
        Some(Ok((me, issues))) => {
            let words: Vec<String> = s.issues.filter.text.split_whitespace().map(str::to_lowercase).collect();
            let mut shown: Vec<Issue> = issues.iter().filter(|i| s.issues.shown.wants(i) && has_label(i, s.issues.label.as_deref()) && has_assignee(i, s.issues.assignee.as_deref()) && matches(i, &words)).cloned().collect();
            if shown.is_empty() {
                let (title, hint) = match (s.issues.shown, words.is_empty() && s.issues.label.is_none() && s.issues.assignee.is_none()) {
                    (Shown::Open, true) => ("No open issues", "Closed shows the ones done with."),
                    (Shown::Closed, true) => ("No closed issues", "None were closed lately."),
                    _ => ("No issues match", "Try fewer words, or another label or assignee."),
                };
                return col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(filters(s, k)).child(message(k, title, hint));
            }
            s.issues.sort.apply(&mut shown);
            let mut list = col().key("issues-list").w_full().pb(16.0);
            for (title, group) in github_issues::group(&shown, me) {
                if group.is_empty() {
                    continue;
                }
                let folded = s.issues.folded.contains(title);
                list = list.child(group_head(k, title, group.len(), folded));
                if !folded {
                    for i in group {
                        list = list.child(issue_row(s, k, i));
                    }
                }
            }
            return col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(filters(s, k))
                .child(col().key("issues-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(list));
        }
    };
    col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(body)
}

/// The row under the head that narrows the list: by state, then the menus.
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
    row().key("issues-filters").w_full().px(EDGE).pt(8.0).pb(4.0).gap(8.0).items_center().child(state).child(label).child(assignee)
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
        .label(format!("#{n} {}", i.title)).description(state)
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

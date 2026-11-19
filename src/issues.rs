//! The Issues place: the repository's issues, grouped by what they want of the reader, and one
//! issue's overview, in place of the diff, as the pull requests are.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::{Cursor, El, Icon, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::syntax::{Syntax, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl, TextArea, TextInput};

use crate::Hand;
use crate::github_issues::{self, Detail, Issue, Options};
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
    /// Comments begun on other issues, by number, back in the box when their issue opens again.
    pub drafts: HashMap<u32, String>,
    /// The dialog up over the open issue, and its text.
    pub write: Option<Write>,
    pub text: TextEdit,
    /// The title, while the issue is edited.
    pub title: TextEdit,
    /// The template the new issue started from, by name.
    pub template: Option<String>,
    /// The labels and assignees a new issue opens with.
    pub new_labels: Vec<String>,
    pub new_assignees: Vec<String>,
    /// Who can be assigned and the labels there are, read once an issue opens.
    pub options: Option<Result<Options, String>>,
}

/// What the issue dialog does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Write {
    /// The reader's comment, by its place among the comments.
    EditComment(usize),
    DeleteComment(usize),
    /// The issue's title and description.
    Edit,
    /// A new issue.
    New,
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
        IssuesState { open: false, list: None, loading: false, folded: HashSet::new(), filter: TextEdit::new(""), shown: Shown::Open, label: None, assignee: None, sort: Sort::Updated, pick: None, number: None, detail: None, reply: TextEdit::new(""), drafts: HashMap::new(), write: None, text: TextEdit::new(""), title: TextEdit::new(""), template: None, new_labels: Vec::new(), new_assignees: Vec::new(), options: None }
    }
}

// Lucide's smile-plus (ISC licence): adding a reaction, as GitHub's button draws it.
crate::lucide!(SMILE_PLUS_ICON, "collate-smile-plus", r#"<path d="M22 11v1a10 10 0 1 1-9-10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><line x1="9" x2="9.01" y1="9" y2="9"/><line x1="15" x2="15.01" y1="9" y2="9"/><path d="M16 5h6"/><path d="M19 2v6"/>"#);

// Lucide's circle-check and circle-slash (ISC licence): an issue closed as done, and one closed
// as not planned or a duplicate, as GitHub marks them.
crate::lucide!(DONE_ICON, "collate-issue-done", r#"<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>"#);
crate::lucide!(SKIPPED_ICON, "collate-issue-skipped", r#"<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/>"#);
crate::lucide!(LOCK_ICON, "collate-lock", r#"<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#);

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
        self.put_issue_draft_away();
        self.issues.reply = TextEdit::new(self.issues.drafts.remove(&n).unwrap_or_default());
        self.issues.number = Some(n);
        self.issues.pick = Some(n);
        self.issues.detail = None;
        self.load_issue();
        if self.issues.options.is_none() {
            self.load_issue_options();
        }
    }

    /// Read who can be assigned and the labels there are, off the UI thread.
    fn load_issue_options(&mut self) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let got = Gh::new(&root).issue_options();
            app.update(move |s, _| {
                if current_repo().is_some_and(|r| r.root == root) {
                    s.issues.options = Some(got);
                }
            });
        });
    }

    /// Back from an issue to the list, where it was.
    pub fn close_issue(&mut self, tx: &mut Tx) {
        self.put_issue_draft_away();
        if let Some(n) = self.issues.number.take() {
            self.issues.detail = None;
            tx.scroll_into_view(format!("issue-{n}"));
        }
    }

    /// Keep the comment half written on the open issue for when it opens again.
    fn put_issue_draft_away(&mut self) {
        let reply = std::mem::replace(&mut self.issues.reply, TextEdit::new(""));
        if let Some(n) = self.issues.number && !reply.text.trim().is_empty() {
            self.issues.drafts.insert(n, reply.text.clone());
        }
    }

    /// Clear issue `n`'s comment once it's sent, wherever the reader has gone since.
    fn sent_issue_draft(&mut self, n: u32) {
        if self.issues.number == Some(n) {
            self.issues.reply = TextEdit::new("");
        }
        self.issues.drafts.remove(&n);
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
        if body.is_empty() || self.issues.detail.as_ref().and_then(|d| d.as_ref().ok()).is_some_and(|d| !d.can_comment()) {
            return;
        }
        // Kept until it's sent, so a failure doesn't lose it.
        self.issue_act("Commenting…", move |gh| gh.issue_comment(n, &body).map(|_| format!("Commented on #{n}")), move |s| s.sent_issue_draft(n));
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
        self.issue_act(doing, move |gh| gh.set_issue_open(n, reason, &body).map(|_| done), move |s| s.sent_issue_draft(n));
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
        let d = self.issues.detail.as_ref().and_then(|d| d.as_ref().ok());
        if d.is_none() && w != Write::New {
            return;
        }
        self.issues.text = TextEdit::new(match (w, d) {
            (Write::EditComment(at), Some(d)) => d.comments.get(at).map_or("", |c| c.body.as_str()),
            (Write::Edit, Some(d)) => &d.body,
            _ => "",
        });
        self.issues.title = TextEdit::new(d.filter(|_| w == Write::Edit).map_or("", |d| d.issue.title.as_str()));
        if w == Write::New {
            self.issues.template = None;
            self.issues.new_labels.clear();
            self.issues.new_assignees.clear();
            if self.issues.options.is_none() {
                self.load_issue_options();
            }
        }
        let titled = matches!(w, Write::Edit | Write::New);
        self.issues.text.focused = !titled;
        self.issues.title.focused = titled;
        self.issues.write = Some(w);
    }

    fn send_issue_write(&mut self) {
        let body = self.issues.text.text.trim().to_owned();
        if self.issues.write == Some(Write::New) {
            // The dialog stays up until the issue is made, so a failure loses nothing.
            let title = self.issues.title.text.trim().to_owned();
            let (labels, assignees) = (self.issues.new_labels.clone(), self.issues.new_assignees.clone());
            let made = std::sync::Arc::new(std::sync::Mutex::new(None));
            let got = made.clone();
            self.issue_act("Opening the issue…", move |gh| {
                let n = gh.create_issue(&title, &body, &labels, &assignees)?;
                *made.lock().map_err(|e| e.to_string())? = Some(n);
                Ok(format!("Opened #{n}"))
            }, move |s| {
                s.issues.write = None;
                if let Some(n) = got.lock().ok().and_then(|n| *n) {
                    s.open_issue(n);
                }
            });
            return;
        }
        let (Some(w), Some(Ok(d))) = (self.issues.write.take(), &self.issues.detail) else { return };
        match w {
            Write::New => {}
            Write::EditComment(at) => {
                let Some(id) = d.comments.get(at).map(|c| c.id.clone()) else { return };
                self.issue_act("Saving…", move |gh| gh.edit_post(&id, false, &body).map(|_| "Saved the comment".to_owned()), |_| {});
            }
            Write::DeleteComment(at) => {
                let Some(id) = d.comments.get(at).map(|c| c.id.clone()) else { return };
                self.issue_act("Deleting…", move |gh| gh.delete_post(&id).map(|_| "Deleted the comment".to_owned()), |_| {});
            }
            Write::Edit => {
                let (n, title) = (d.issue.number, self.issues.title.text.trim().to_owned());
                self.issue_act("Saving…", move |gh| gh.edit_issue(n, &title, &body).map(|_| format!("Saved #{n}")), |_| {});
            }
        }
    }

    /// The assignees' menu: everyone who can be, those on it ticked; a pick adds or takes them off.
    fn pick_issue_assignees(&mut self, tx: &mut Tx) {
        let (Some(n), Some(Ok(d))) = (self.issues.number, &self.issues.detail) else { return };
        let on = d.issue.assignees.clone();
        let menu = match &self.issues.options {
            Some(Ok(o)) => {
                // Those on it first, then the rest, as GitHub lists them.
                let rest = o.people.iter().filter(|p| !on.contains(p));
                PopupMenu::new().items(on.iter().chain(rest).map(|p| PopupItem::new(p.clone(), p.clone()).checked(on.contains(p))))
            }
            Some(Err(e)) => PopupMenu::new().item(PopupItem::new("", format!("Couldn't read who can be assigned: {e}")).enabled(false)),
            None => PopupMenu::new().item(PopupItem::new("", "Reading who can be assigned…").enabled(false)),
        };
        tx.popup_menu(menu, "issue-edit-assignees", move |s: &mut Collate, picked: Option<&str>, _| {
            let Some(who) = picked.filter(|p| !p.is_empty()).map(str::to_owned) else { return };
            if on.contains(&who) {
                s.issue_act("Unassigning…", move |gh| gh.issue_assignees(n, &[], &[&who]).map(|_| format!("Took {who} off #{n}")), |_| {});
            } else {
                s.issue_act("Assigning…", move |gh| gh.issue_assignees(n, &[&who], &[]).map(|_| format!("Assigned {who} to #{n}")), |_| {});
            }
        });
    }

    /// The labels' menu: every label there is, those on it ticked; a pick adds or takes it off.
    fn pick_issue_labels(&mut self, tx: &mut Tx) {
        let (Some(n), Some(Ok(d))) = (self.issues.number, &self.issues.detail) else { return };
        let on: Vec<String> = d.issue.labels.iter().map(|(name, _)| name.clone()).collect();
        let menu = match &self.issues.options {
            Some(Ok(o)) if o.labels.is_empty() => PopupMenu::new().item(PopupItem::new("", "This repository has no labels").enabled(false)),
            Some(Ok(o)) => PopupMenu::new().items(o.labels.iter().map(|(name, _)| PopupItem::new(name.clone(), name.clone()).checked(on.contains(name)))),
            Some(Err(e)) => PopupMenu::new().item(PopupItem::new("", format!("Couldn't read the labels: {e}")).enabled(false)),
            None => PopupMenu::new().item(PopupItem::new("", "Reading the labels…").enabled(false)),
        };
        tx.popup_menu(menu, "issue-edit-labels", move |s: &mut Collate, picked: Option<&str>, _| {
            let Some(name) = picked.filter(|p| !p.is_empty()).map(str::to_owned) else { return };
            if on.contains(&name) {
                s.issue_act("Removing the label…", move |gh| gh.issue_labels(n, &[], &[&name]).map(|_| format!("Took {name} off #{n}")), |_| {});
            } else {
                s.issue_act("Labelling…", move |gh| gh.issue_labels(n, &[&name], &[]).map(|_| format!("Labelled #{n} {name}")), |_| {});
            }
        });
    }

    /// The milestone's menu: the open milestones, the issue's ticked, and taking it out of one.
    fn pick_issue_milestone(&mut self, tx: &mut Tx) {
        let (Some(n), Some(Ok(d))) = (self.issues.number, &self.issues.detail) else { return };
        let now = d.milestone.clone();
        let menu = match &self.issues.options {
            Some(Ok(o)) => {
                let none = PopupItem::new("", "No Milestone").checked(now.is_none());
                PopupMenu::new().item(none).separator().items(o.milestones.iter().map(|m| PopupItem::new(format!("m:{m}"), m.clone()).checked(now.as_ref() == Some(m))))
            }
            Some(Err(e)) => PopupMenu::new().item(PopupItem::new("-", format!("Couldn't read the milestones: {e}")).enabled(false)),
            None => PopupMenu::new().item(PopupItem::new("-", "Reading the milestones…").enabled(false)),
        };
        tx.popup_menu(menu, "issue-edit-milestone", move |s: &mut Collate, picked: Option<&str>, _| {
            let to = match picked {
                Some("") => None,
                Some(m) => Some(m.strip_prefix("m:").unwrap_or(m).to_owned()),
                None => return,
            };
            if to == now {
                return;
            }
            let done = match &to {
                Some(m) => format!("Put #{n} in {m}"),
                None => format!("Took #{n} out of its milestone"),
            };
            s.issue_act("Saving…", move |gh| gh.issue_milestone(n, to.as_deref()).map(|_| done), |_| {});
        });
    }

    /// The open issue's menu: its link, GitHub, and whether the reader hears of it or others
    /// may comment.
    fn issue_menu(&mut self, tx: &mut Tx) {
        let (Some(n), Some(Ok(d))) = (self.issues.number, &self.issues.detail) else { return };
        let (url, id, subscribed, locked) = (d.issue.url.clone(), d.id.clone(), d.subscribed, d.locked);
        let menu = PopupMenu::new()
            .item(PopupItem::new("copy", "Copy Link"))
            .item(PopupItem::new("github", "Open on GitHub"))
            .separator()
            .item(PopupItem::new("subscribe", if subscribed { "Unsubscribe" } else { "Subscribe" }).enabled(!id.is_empty()))
            .item(PopupItem::new("lock", if locked { "Unlock Conversation" } else { "Lock Conversation" }).enabled(d.can_edit));
        tx.popup_menu(menu, "issue-more", move |s: &mut Collate, picked: Option<&str>, tx| match picked {
            Some("copy") => {
                tx.copy(url.clone());
                s.say(format!("Copied the link to #{n}"), false);
            }
            Some("github") => crate::open_url(&url),
            Some("subscribe") => {
                let done = if subscribed { format!("You won't hear of #{n} unless you're in it") } else { format!("You'll hear of everything on #{n}") };
                s.issue_act("Saving…", move |gh| gh.subscribe(&id, !subscribed).map(|_| done), |_| {});
            }
            Some("lock") => {
                let done = if locked { format!("Anyone can comment on #{n} again") } else { format!("Only collaborators can comment on #{n} now") };
                s.issue_act(if locked { "Unlocking…" } else { "Locking…" }, move |gh| gh.lock_issue(n, !locked).map(|_| done), |_| {});
            }
            _ => {}
        });
    }

    /// The new issue's labels or assignees: everything there is, those picked ticked; a pick
    /// adds or takes one off.
    fn pick_new_issue(&mut self, labels: bool, tx: &mut Tx) {
        let picked = if labels { &self.issues.new_labels } else { &self.issues.new_assignees };
        let menu = match &self.issues.options {
            Some(Ok(o)) => {
                let all: Vec<&String> = if labels { o.labels.iter().map(|(name, _)| name).collect() } else { o.people.iter().collect() };
                if all.is_empty() {
                    PopupMenu::new().item(PopupItem::new("", if labels { "This repository has no labels" } else { "No one can be assigned" }).enabled(false))
                } else {
                    PopupMenu::new().items(all.into_iter().map(|x| PopupItem::new(x.clone(), x.clone()).checked(picked.contains(x))))
                }
            }
            Some(Err(e)) => PopupMenu::new().item(PopupItem::new("", format!("Couldn't read them: {e}")).enabled(false)),
            None => PopupMenu::new().item(PopupItem::new("", "Reading…").enabled(false)),
        };
        let anchor = if labels { "issue-new-labels" } else { "issue-new-assignees" };
        tx.popup_menu(menu, anchor, move |s: &mut Collate, picked: Option<&str>, _| {
            let Some(x) = picked.filter(|p| !p.is_empty()).map(str::to_owned) else { return };
            let list = if labels { &mut s.issues.new_labels } else { &mut s.issues.new_assignees };
            match list.iter().position(|y| *y == x) {
                Some(at) => _ = list.remove(at),
                None => list.push(x),
            }
        });
    }

    /// The templates a new issue can start from; one fills in what it says, a blank one clears it.
    fn pick_issue_template(&mut self, tx: &mut Tx) {
        let Some(Ok(o)) = &self.issues.options else { return };
        let templates = o.templates.clone();
        let menu = PopupMenu::picker(std::iter::once(("".to_owned(), "Blank".to_owned())).chain(templates.iter().map(|t| (t.name.clone(), t.name.clone()))),
            self.issues.template.as_deref().unwrap_or(""));
        tx.popup_menu(menu, "issue-new-template", move |s: &mut Collate, picked: Option<&str>, _| {
            let Some(name) = picked else { return };
            let t = templates.iter().find(|t| t.name == name).cloned().unwrap_or_default();
            s.issues.template = (!name.is_empty()).then(|| name.to_owned());
            s.issues.title = TextEdit::new(&t.title);
            s.issues.title.focused = true;
            s.issues.text = TextEdit::new(&t.body);
            s.issues.new_labels = t.labels;
            s.issues.new_assignees = t.assignees;
        });
    }

    fn assign_me_to_issue(&mut self) {
        let Some(n) = self.issues.number else { return };
        self.issue_act("Assigning…", move |gh| gh.issue_assignees(n, &["@me"], &[]).map(|_| format!("Assigned you to #{n}")), |_| {});
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
    let mut head = head.child(refresh.build(k).hand(!s.issues.loading).key("issues-reload"));
    if matches!(&s.issues.list, Some(Ok(_))) {
        head = head.child(Button::new("New Issue").variant(ButtonVariant::Contrast).help("Open a new issue in this repository")
            .on_press(|s: &mut Collate| s.start_issue_write(Write::New)).build(k).hand(true).key("issues-new"));
    }
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
    let mut head = row().key("issue-head").w_full().pt(COLUMN_TOP).pr(EDGE).items_center().child(back).child(row().grow(1.0));
    if detail.is_some() {
        head = head.child(IconButton::new(&MORE_ICON).variant(IconButtonVariant::GhostMuted).label("More actions for this issue").help("Copy its link, open it on GitHub, subscribe, lock")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.issue_menu(tx)).build(k).hand(true).key("issue-more"));
    }
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
    let mut title = row().w_full().gap(12.0).items_start().child(text(i.title.clone()).font(title_font).color(k.c.text_base).wrap().grow(1.0).min_w(0.0));
    if detail.is_some_and(|d| d.can_edit) {
        title = title.child(Button::new("Edit").variant(ButtonVariant::Outline).control_size(ControlSize::Small).help("Change its title and description")
            .on_press(|s: &mut Collate| s.start_issue_write(Write::Edit)).build(k).hand(true).key("issue-edit").mt(4.0));
    }
    let body = match &s.issues.detail {
        None => text("Reading…".to_owned()).key("issue-body").font(k.ty.ui).color(k.c.text_faint),
        Some(Err(e)) => message(k, "Can't read the issue", e),
        Some(Ok(d)) if d.body.trim().is_empty() => text("No description provided.".to_owned()).key("issue-body").font(k.ty.ui).color(k.c.text_faint),
        Some(Ok(d)) => crate::pulls::rich(s, k, "issue-body", &d.body, k.ty.ui),
    };
    let body = match detail.filter(|d| !d.id.is_empty()) {
        Some(d) => col().w_full().gap(12.0).child(body).child(reactions(k, "issue", &d.id, &d.reactions)),
        None => body,
    };
    let mut main = col().key("issue-main").grow(1.0).min_w(0.0).max_w(780.0).gap(28.0)
        .child(col().key("issue-title").w_full().gap(10.0)
            .child(title)
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
    let mut page = row().key("issue-page").w_full().max_w(1100.0).px(EDGE * 2.0).pt(16.0).pb(48.0).gap(40.0).items_start().child(main);
    if let Some(d) = detail {
        // Held at the top while the description and activity scroll past, as on GitHub.
        page = page.child(side(s, k, d).sticky_top(16.0));
    }
    col().key("issues").w_full().grow(1.0).min_h(0.0).child(head)
        .child(col().key("issue-overview").w_full().grow(1.0).min_h(0.0).items_center().scroll(rune::ScrollSpec { stick_bottom: false }).child(page))
}

/// The dialog over the open issue: editing the reader's comment, or asking before deleting it.
pub fn write_dialog(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let Some(w) = s.issues.write else { return };
    let (title, what, button) = match w {
        Write::EditComment(_) => ("Edit comment", "Saved to GitHub.", "Save"),
        Write::DeleteComment(_) => ("Delete this comment?", "It's removed from the issue for everyone, and can't be brought back.", "Delete"),
        Write::Edit => ("Edit issue", "Saved to GitHub.", "Save"),
        Write::New => ("New issue", "Opened on GitHub, then here.", "Open Issue"),
    };
    let delete = matches!(w, Write::DeleteComment(_));
    // An issue needs a title; its description may be empty, a comment may not.
    let blocked = s.busy.is_some()
        || match w {
            Write::Edit | Write::New => s.issues.title.text.trim().is_empty(),
            Write::EditComment(_) => s.issues.text.text.trim().is_empty(),
            Write::DeleteComment(_) => false,
        };
    let mut body = col().key("issue-write-body").w_full().gap(12.0);
    let titled = matches!(w, Write::Edit | Write::New);
    let templates = s.issues.options.as_ref().and_then(|o| o.as_ref().ok()).is_some_and(|o| !o.templates.is_empty());
    if w == Write::New && templates {
        // First, since it fills in the rest.
        let name = s.issues.template.clone().unwrap_or_else(|| "Blank".to_owned());
        body = body.child(row().w_full().ml(-crate::pulls::REGULAR_PAD).child(menu_button(k, "issue-new-template", format!("Template: {name}"), s.issues.template.is_some(),
            "Start from one of the repository's templates; it replaces what's written", Collate::pick_issue_template)));
    }
    if titled {
        body = body.child(TextInput::new(&s.issues.title).placeholder("Title").on_edit(|s: &mut Collate| &mut s.issues.title).width(None).build(k).key("issue-write-title").w_full());
    }
    if w == Write::New {
        let named = |all: &[String], none: &str| if all.is_empty() { none.to_owned() } else { all.join(", ") };
        body = body.child(row().key("issue-new-lists").w_full().gap(4.0).items_center().ml(-crate::pulls::REGULAR_PAD)
            .child(menu_button(k, "issue-new-labels", named(&s.issues.new_labels, "Labels"), !s.issues.new_labels.is_empty(), "Label it", |s, tx| s.pick_new_issue(true, tx)))
            .child(menu_button(k, "issue-new-assignees", named(&s.issues.new_assignees, "Assignees"), !s.issues.new_assignees.is_empty(), "Assign it", |s, tx| s.pick_new_issue(false, tx))));
    }
    let (hint, label) = if titled { ("Description", "Description") } else { ("Write a comment", "Comment") };
    if !delete {
        body = body.child(TextArea::new(&s.issues.text).placeholder(hint).label(label).rows(8).max_height(320.0)
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
    if s.issues.detail.as_ref().and_then(|d| d.as_ref().ok()).is_some_and(|d| !d.can_comment()) {
        // Said in place of the box, so the reader isn't left to find out from a failed send.
        return row().key("issue-composer").w_full().gap(8.0).items_center().px(12.0).py(10.0).radius(k.radius.md).border(1.0, k.c.border_weak)
            .child(icon(&LOCK_ICON, 14.0).color(k.c.icon_muted))
            .child(text("This conversation is locked - only collaborators can comment.".to_owned()).key("issue-locked").font(k.ty.text_small).color(k.c.text_muted));
    }
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

/// Beside the issue: who has it and its labels.
fn side(s: &Collate, k: &Kit, d: &Detail) -> El {
    use crate::pulls::{REGULAR_PAD, heading};
    let i = &d.issue;
    let busy = s.busy.is_some();
    // A gear opens each list's editor, as on GitHub.
    let gear = |key: &'static str, label: &str, open: fn(&mut Collate, &mut Tx)| d.can_edit.then(|| IconButton::new(&crate::pulls::GEAR_ICON).variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small).label(label.to_owned()).disabled(busy).on_press_tx(open).build(k).hand(!busy).key(key).mr(-4.0));
    let mut assignees = col().key("issue-assignees").w_full().gap(2.0)
        .child(heading(k, "issue-assignees-title", "Assignees", gear("issue-edit-assignees", "Change the assignees", Collate::pick_issue_assignees)));
    if i.assignees.is_empty() {
        let mut none = row().w_full().h(24.0).gap(4.0).items_center().child(text("No one -".to_owned()).font(k.ty.text_small).color(k.c.text_faint));
        if d.can_edit {
            none = none.child(Button::new("Assign yourself").variant(ButtonVariant::GhostMuted).disabled(busy)
                .on_press(|s: &mut Collate| s.assign_me_to_issue()).build(k).hand(!busy).key("issue-assign-me").ml(-REGULAR_PAD));
        }
        assignees = assignees.child(none);
    }
    for a in &i.assignees {
        assignees = assignees.child(row().key(format!("issue-assignee-{a}")).w_full().h(28.0).gap(8.0).items_center()
            .child(crate::history_view::face(k, s, a, 20.0))
            .child(text(a.clone()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).truncate().min_w(0.0)));
    }
    let mut labels = col().key("issue-labels").w_full().gap(4.0).child(heading(k, "issue-labels-title", "Labels", gear("issue-edit-labels", "Change the labels", Collate::pick_issue_labels)));
    labels = if i.labels.is_empty() {
        labels.child(text("None yet".to_owned()).font(k.ty.text_small).color(k.c.text_faint))
    } else {
        labels.child(row().w_full().gap(4.0).flex_wrap().children(i.labels.iter().map(|(name, color)| crate::pulls::label_pill(k, name, crate::pulls::label_color(k, color)))))
    };
    let milestone = col().key("issue-milestone").w_full().gap(2.0)
        .child(heading(k, "issue-milestone-title", "Milestone", gear("issue-edit-milestone", "Change the milestone", Collate::pick_issue_milestone)))
        .child(match &d.milestone {
            Some(m) => text(m.clone()).font(k.ty.text_small).color(k.c.text_base),
            None => text("No milestone".to_owned()).font(k.ty.text_small).color(k.c.text_faint),
        });
    let mut side = col().key("issue-side").w(220.0).gap(24.0).child(assignees).child(labels).child(milestone);
    // The pull requests that close it, each opening here as the Pull Requests list opens it.
    if !d.pulls.is_empty() {
        side = side.child(col().key("issue-pulls").w_full().gap(2.0).child(heading(k, "issue-pulls-title", "Development", None))
            .children(d.pulls.iter().map(|(n, title, state)| {
                let (glyph, tint, said): (&'static Icon, rune::Color, &str) = match state.as_str() {
                    "MERGED" => (&crate::MERGE_ICON, SyntaxTheme::from_kit(k).color(Syntax::Keyword), "Merged"),
                    "CLOSED" => (&crate::PULL_CLOSED_ICON, k.c.danger.icon, "Closed"),
                    _ => (&crate::PULLS_ICON, k.c.success.icon, "Open"),
                };
                let n = *n;
                row().key(format!("issue-pull-{n}")).w_full().h(24.0).gap(8.0).items_center().radius(k.radius.sm).hover(k.c.hover)
                    .cursor(Cursor::Pointer).role("button").label(format!("#{n} {title} - {said}")).help("Open the pull request")
                    .on_press(move |s: &mut Collate| {
                        s.open_pulls();
                        s.open_pull(n);
                    })
                    .child(icon(glyph, 14.0).color(tint).a11y_hidden())
                    .child(text(title.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
                    .child(text(format!("#{n}")).font(k.ty.text_small).color(k.c.text_faint))
            })));
    }
    side
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

/// A comment's or the description's reactions, each with its count, and a button to add one.
/// A press on one adds the reader's own, as on GitHub.
fn reactions(k: &Kit, key: &str, id: &str, had: &[(String, u32)]) -> El {
    let chips = had.iter().filter_map(|(name, n)| {
        let (content, emoji) = github::REACTIONS.iter().find(|(c, _)| c == name)?;
        let id = id.to_owned();
        Some(row().key(format!("{key}-react-{content}")).h(24.0).px(8.0).gap(4.0).items_center().radius(12.0).border(1.0, k.c.border_weak)
            .hover(k.c.hover).cursor(Cursor::Pointer).role("button").label(format!("{n} {content}, add yours"))
            .on_press(move |s: &mut Collate| s.react_to_issue_post(id.clone(), content))
            .child(text((*emoji).to_owned()).font(k.ty.text_small).a11y_hidden())
            .child(text(n.to_string()).font(k.ty.text_small).color(k.c.text_muted).a11y_hidden()))
    });
    let (id, anchor) = (id.to_owned(), format!("{key}-react-add"));
    let add = IconButton::new(&SMILE_PLUS_ICON).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Small).label("Add a reaction")
        .on_press_tx({
            let anchor = anchor.clone();
            move |_: &mut Collate, tx: &mut Tx| {
                let id = id.clone();
                let menu = PopupMenu::new().items(github::REACTIONS.iter().map(|(name, emoji)| PopupItem::new(*name, *emoji)));
                tx.popup_menu(menu, anchor.clone(), move |s: &mut Collate, picked: Option<&str>, _| {
                    if let Some((content, _)) = picked.and_then(|p| github::REACTIONS.iter().find(|(n, _)| *n == p)) {
                        s.react_to_issue_post(id.clone(), content);
                    }
                });
            }
        }).build(k).hand(true).key(anchor);
    row().key(format!("{key}-reactions")).w_full().gap(6.0).items_center().flex_wrap().children(chips).child(add)
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
    if !c.id.is_empty() {
        card = card.child(reactions(k, &key, &c.id, &c.reactions).px(12.0).pb(10.0));
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

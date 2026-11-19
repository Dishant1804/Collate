//! The Pull Requests place: the repository's pull requests, the open ones grouped by what they
//! want of the reader and then the merged and closed, and one pull request under review. Its diff is a range in History; its details,
//! checks, conversation and actions take the staging panel's top; its line threads sit under
//! their lines with the reader's own comments, which a review sends as its line comments.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::{Cursor, El, Icon, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, IconButton, IconButtonVariant, Kit, Markdown, MarkdownStyle, SegmentedControl, TextArea, TextInput};

use rune_kit::syntax::{Syntax, SyntaxTheme};

use crate::agent::Settle;
use crate::github::{self, CheckState, Detail, Draft, Gh, Method, PrState, Pull, Verdict};
use crate::{COLUMN_TOP, Collate, EDGE, GHOST_PAD, HANDLE, MERGE_ICON, MORE_ICON, PULLS_ICON, PULL_CLOSED_ICON, PULL_DRAFT_ICON, REFRESH_ICON, current_repo, diff, message, open_url};

/// What's being written in the compose dialog.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Compose {
    Review(Verdict),
    /// A comment in the conversation.
    Comment,
    /// A reply in thread `n` of the open pull request.
    Reply(usize),
    /// A new pull request from the current branch.
    Create,
    /// The open pull request's title and description.
    Edit,
    /// Who's asked to review, as a list.
    Reviewers,
    Labels,
    Assignees,
    /// Comment or review summary `n` of the conversation, rewritten.
    EditPost(usize),
    /// Comment `n` of the conversation, deleted once the reader confirms.
    DeletePost(usize),
}

/// The names in a comma- or space-separated list, without an `@`.
fn names(list: &str) -> Vec<String> {
    list.split([',', ' ']).map(|n| n.trim().trim_start_matches('@').to_owned()).filter(|n| !n.is_empty()).collect()
}

/// What to add to and remove from `was` to make it `now`.
fn changes(was: &[String], now: &[String]) -> (Vec<String>, Vec<String>) {
    (now.iter().filter(|n| !was.contains(n)).cloned().collect(), was.iter().filter(|w| !now.contains(w)).cloned().collect())
}

/// The open pull request's pages, as GitHub's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrTab {
    Overview,
    Files,
}

pub struct PullsState {
    /// The list shows in place of the diff.
    pub open: bool,
    /// The signed-in login and the open pull requests, once read.
    pub list: Option<Result<(String, Vec<Pull>), String>>,
    pub loading: bool,
    /// The pull request being reviewed.
    pub pr: Option<Detail>,
    /// Its overview or its files.
    pub tab: PrTab,
    /// The pull request being fetched to open.
    pub fetching: Option<u32>,
    pub compose: Option<Compose>,
    pub text: TextEdit,
    /// A new pull request's title.
    pub title: TextEdit,
    pub draft: bool,
    /// Merging deletes the branch too.
    pub delete_branch: bool,
    /// Long comments shown whole, by their key.
    pub expanded: HashSet<String>,
    /// Words the list is narrowed to.
    pub filter: TextEdit,
    /// The comment being written suggests its text in place of the line's.
    pub suggest: bool,
    /// Pull requests already known to ask for the reader's review: only new ones notify.
    /// `None` until the first list, which notifies of nothing.
    pub asking: Option<HashSet<u32>>,
    /// Connecting GitHub: what the reader is asked to finish, while Collate waits for it.
    pub connecting: Option<&'static str>,
    /// Each press of Connect; a wait for an earlier one ends quietly.
    pub connect_try: u32,
    /// The list's groups folded to their titles, by title.
    pub folded: HashSet<&'static str>,
    /// What a force-push changed since the reader's review, as `git range-diff` tells it.
    pub rewrite: Option<String>,
    /// Who can be asked to review, read when the reviewers editor opens.
    pub people: Option<Result<Vec<String>, String>>,
}

impl Default for PullsState {
    fn default() -> PullsState {
        PullsState {
            open: false,
            list: None,
            loading: false,
            pr: None,
            tab: PrTab::Overview,
            fetching: None,
            compose: None,
            text: TextEdit::new(""),
            title: TextEdit::new(""),
            draft: false,
            delete_branch: true,
            expanded: HashSet::new(),
            filter: TextEdit::new(""),
            suggest: false,
            asking: None,
            connecting: None,
            connect_try: 0,
            // Finished pull requests are there to look back on, not to act on.
            folded: HashSet::from(["Merged", "Closed"]),
            rewrite: None,
            people: None,
        }
    }
}

impl PullsState {
    /// Where each line thread of the open pull request goes in the diff: by its line and that
    /// line's text, as a comment is. An outdated thread has no line, and an anchor no line matches.
    /// Then, when the diff is the whole pull request (`whole`), its checks' annotations: by their
    /// line in the head, whose text `head_line` reads from the diff.
    pub fn anchors(&self, whole: bool, head_line: impl Fn(&str, u32) -> Option<String>) -> Vec<diff::Anchor> {
        let Some(pr) = &self.pr else { return Vec::new() };
        let threads = pr.threads.iter().map(|t| diff::Anchor {
            path: t.path.clone(),
            side: if t.left { diff::Side::Old } else { diff::Side::New },
            line: t.line.unwrap_or(0),
            text: if t.line.is_some() { t.text.clone() } else { "\0outdated".into() },
        });
        let annotations = pr.annotations.iter().flatten().map(|a| diff::Anchor {
            path: a.path.clone(),
            side: diff::Side::New,
            line: a.line,
            text: whole.then(|| head_line(&a.path, a.line)).flatten().unwrap_or_else(|| "\0elsewhere".into()),
        });
        threads.chain(annotations).collect()
    }

    /// The signed-in login, once the list is read.
    pub fn me(&self) -> Option<&str> {
        match &self.list {
            Some(Ok((me, _))) => Some(me),
            _ => None,
        }
    }

    /// Pull requests asking the reader for a review.
    pub fn asked(&self) -> usize {
        match &self.list {
            Some(Ok((me, pulls))) => pulls.iter().filter(|p| github::asks(p, me)).count(),
            _ => 0,
        }
    }
}

/// The pull requests asking `me` for a review, and those of them not in `seen` (none the first
/// time, when nothing has been seen).
fn fresh_requests<'a>(seen: Option<&HashSet<u32>>, pulls: &'a [Pull], me: &str) -> (HashSet<u32>, Vec<&'a Pull>) {
    let now: HashSet<u32> = pulls.iter().filter(|p| github::asks(p, me)).map(|p| p.number).collect();
    let fresh = match seen {
        Some(seen) => pulls.iter().filter(|p| now.contains(&p.number) && !seen.contains(&p.number)).collect(),
        None => Vec::new(),
    };
    (now, fresh)
}

/// How often the review requests are read again.
const REVIEWS_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// Read the pull requests at launch, so the list is there when it's opened, then again every
/// few minutes, so a new review request notifies and the sidebar's count stays true.
pub fn watch_reviews() {
    let Some(app) = HANDLE.get().cloned() else { return };
    app.update(|s, _| {
        if s.has_project {
            s.load_pulls();
        }
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(REVIEWS_EVERY);
        app.update(|s, _| {
            if s.settings.notify_reviews && s.has_project {
                s.load_pulls();
            }
        });
    });
}

/// The signed-in login and the pull requests.
type List = (String, Vec<Pull>);

/// Each project's last list, shown at once when it's opened again while a new one is read.
static LISTS: std::sync::Mutex<Option<HashMap<PathBuf, List>>> = std::sync::Mutex::new(None);

/// The last list read for the project at `root`.
pub fn cached(root: &Path) -> Option<List> {
    LISTS.lock().ok()?.as_ref()?.get(root).cloned()
}

/// How long Collate waits for the reader to finish connecting GitHub, looking every few seconds.
const CONNECT_FOR: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const CONNECT_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Lucide's GitHub mark (ISC licence).
pub static GITHUB_ICON: Icon = Icon {
    name: "collate-github",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.08-1.25-.27-2.48-1-3.5.28-1.15.28-2.35 0-3.5 0 0-1 0-3 1.5-2.64-.5-5.36-.5-8 0C6 2 5 2 5 2c-.3 1.15-.3 2.35 0 3.5A5.403 5.403 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.39.49-.68 1.05-.85 1.65-.17.6-.22 1.23-.15 1.85v4"/><path d="M9 18c-4.51 2-5-2-7-2"/></g>"#,
};

// Lucide's pencil, link, gear, check, message and clock (ISC licence): the overview's edit and
// copy link, the sidebar's settings, how a reviewer stands, and when it last changed.
crate::lucide!(PENCIL_ICON, "collate-pencil", r#"<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>"#);
crate::lucide!(LINK_ICON, "collate-link", r#"<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>"#);
crate::lucide!(GEAR_ICON, "collate-gear", r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#);
crate::lucide!(CHECK_ICON, "collate-check", r#"<path d="M20 6 9 17l-5-5"/>"#);
crate::lucide!(MESSAGE_ICON, "collate-message", r#"<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>"#);
crate::lucide!(CLOCK_ICON, "collate-clock", r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>"#);

/// A macOS notification, through AppleScript: no permission to ask for, and nothing to bundle.
fn notify(title: &str, body: &str) {
    let quote = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
    let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
}

fn gh() -> Option<Gh> {
    current_repo().map(|r| Gh::new(&r.root))
}

impl Collate {
    /// Asks for the pictures of everyone the pull requests show: the reader, the authors, and
    /// the open one's reviewers, assignees and commenters.
    fn request_faces(&mut self) {
        let mut logins: Vec<&str> = self.pulls.me().into_iter().collect();
        if let Some(Ok((_, pulls))) = &self.pulls.list {
            logins.extend(pulls.iter().map(|p| p.author.as_str()));
        }
        if let Some(pr) = &self.pulls.pr {
            logins.push(&pr.pull.author);
            logins.extend(pr.reviewers.iter().map(|(w, _)| w.as_str()));
            logins.extend(pr.assignees.iter().map(String::as_str));
            logins.extend(pr.conversation.iter().chain(pr.threads.iter().flat_map(|t| &t.comments)).map(|p| p.author.as_str()));
        }
        self.avatars.request_logins(logins);
    }

    /// Sign in to GitHub through the GitHub CLI, in a terminal under the diff, installing the
    /// CLI with Homebrew first if it's missing; without Homebrew, its download page opens.
    /// Then wait for the sign-in and read the pull requests.
    pub fn connect_github(&mut self) {
        let login = "auth login --hostname github.com --git-protocol https --web";
        let quote = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let command = match (github::find("gh"), github::find("brew")) {
            (Some(gh), _) => Some(format!("{} {login}", quote(&gh))),
            (None, Some(brew)) => Some(format!("{} install gh && {} {login}", quote(&brew), quote(&brew.with_file_name("gh")))),
            (None, None) => None,
        };
        self.pulls.connecting = Some(match command {
            Some(command) => {
                let dir = current_repo().map(|r| r.root.clone()).or_else(|| std::env::var_os("HOME").map(Into::into)).unwrap_or_else(|| "/".into());
                if let Err(e) = self.terminals.run(&dir, &command) {
                    self.say(e, true);
                    return;
                }
                "Finish signing in in the terminal below; this updates once you're connected."
            }
            None => {
                open_url("https://cli.github.com");
                "Install the GitHub CLI from the page that opened, then connect again."
            }
        });
        self.pulls.connect_try += 1;
        let (mine, Some(gh), Some(app)) = (self.pulls.connect_try, gh(), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while start.elapsed() < CONNECT_FOR {
                std::thread::sleep(CONNECT_EVERY);
                if let Ok(me) = gh.me() {
                    app.update(move |s, _| {
                        if s.pulls.connect_try == mine {
                            s.pulls.connecting = None;
                            s.say(format!("Connected to GitHub as {me}"), false);
                            s.pulls.list = None;
                            s.load_pulls();
                        }
                    });
                    return;
                }
            }
        });
    }

    /// A failed call to GitHub: one that needs GitHub connected shows the Pull Requests
    /// place asking for that, anything else is said.
    fn github_failed(&mut self, e: String) {
        if github::needs_connect(&e) {
            self.pulls.list = Some(Err(e));
            self.pulls.open = true;
        } else {
            self.say(e, true);
        }
    }

    /// Show the list as last read, and read it again.
    pub fn open_pulls(&mut self) {
        self.map.open = false;
        self.pulls.open = true;
        self.load_pulls();
    }

    /// Read the list again, off the UI thread. Whatever list shows stays until it lands.
    pub fn load_pulls(&mut self) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        if std::mem::replace(&mut self.pulls.loading, true) {
            return;
        }
        std::thread::spawn(move || {
            let got = Gh::new(&root).list();
            if let (Ok(list), Ok(mut lists)) = (&got, LISTS.lock()) {
                lists.get_or_insert_default().insert(root.clone(), list.clone());
            }
            app.update(move |s, _| {
                // The reader switched projects meanwhile: this list was the last one's.
                if current_repo().is_none_or(|r| r.root != root) {
                    return;
                }
                s.pulls.loading = false;
                s.pulls.list = Some(got);
                s.request_faces();
                s.note_review_requests();
            });
        });
    }

    /// Notify of pull requests that newly ask for the reader's review.
    fn note_review_requests(&mut self) {
        let Some(Ok((me, pulls))) = &self.pulls.list else { return };
        let (now, fresh) = fresh_requests(self.pulls.asking.as_ref(), pulls, me);
        if self.settings.notify_reviews {
            for p in fresh {
                notify(&format!("Review requested on {}", self.name), &format!("#{} {} - by {}", p.number, p.title, p.author));
            }
        }
        self.pulls.asking = Some(now);
    }

    /// Fetch pull request `n` and show its diff, details and threads.
    pub fn open_pull(&mut self, n: u32) {
        let (Some(gh), Some(app)) = (gh(), HANDLE.get().cloned()) else { return };
        if self.pulls.fetching.is_some() {
            return;
        }
        self.pulls.fetching = Some(n);
        let known = self.pulls.pr.as_ref().map(|d| &d.pull).filter(|p| p.number == n)
            .or_else(|| self.pulls.list.as_ref()?.as_ref().ok()?.1.iter().find(|p| p.number == n)).cloned();
        std::thread::spawn(move || {
            let got = gh.open(n, known.as_ref());
            app.update(move |s, _| {
                s.pulls.fetching = None;
                match got {
                    Ok((range, d)) => {
                        if let Err(e) = &d.annotations {
                            s.say(format!("Couldn't read the checks' annotations: {e}"), true);
                        }
                        s.pulls.open = false;
                        s.open_commit(&range);
                        s.compared = Some((d.pull.base.clone(), d.pull.head.clone()));
                        s.pulls.pr = Some(d);
                        s.request_faces();
                        s.pulls.tab = PrTab::Overview;
                        s.place_comments();
                    }
                    Err(e) => s.github_failed(e),
                }
            });
        });
    }

    /// Whether the diff shows the open pull request's whole change, the one GitHub's line
    /// comments are numbered against.
    pub fn pr_whole(&self) -> bool {
        self.pulls.pr.as_ref().is_some_and(|d| self.commit.as_deref() == Some(d.range().as_str()))
    }

    /// Shows part of the open pull request: `all` of it, what changed `since` the reader's last
    /// review, or one commit by its id.
    pub fn show_pr_part(&mut self, part: &str) {
        let Some(d) = &self.pulls.pr else { return };
        let (range, compared) = match part {
            "all" => (d.range(), (d.pull.base.clone(), d.pull.head.clone())),
            "since" => {
                let Some(at) = self.pulls.me().and_then(|me| d.last_reviewed(me)) else { return };
                // A force-push can leave the reviewed commit out of what was fetched.
                if current_repo().is_some_and(|r| r.resolve(at).is_err()) {
                    let at = crate::abbrev(at);
                    self.say(format!("Your last review was on {at}, which this pull request no longer has: its branch was rewritten since."), true);
                    return;
                }
                (format!("{at}..{}", d.head_oid), (format!("your review ({})", crate::abbrev(at)), d.pull.head.clone()))
            }
            id => {
                let Some((_, subject)) = d.commits.iter().find(|(c, _)| c == id) else { return };
                (format!("{id}^..{id}"), (format!("{}^", crate::abbrev(id)), subject.clone()))
            }
        };
        // Opening a range leaves the pull request; this is still it.
        let pr = self.pulls.pr.take();
        self.open_commit(&range);
        self.compared = Some(compared);
        self.pulls.pr = pr;
        self.place_comments();
    }

    /// Picks which part of the open pull request the diff shows.
    pub fn pr_part_menu(&mut self, tx: &mut Tx) {
        let Some(d) = &self.pulls.pr else { return };
        let mut menu = PopupMenu::new().item(PopupItem::new("all", "All Changes").checked(self.pr_whole()));
        let me = self.pulls.me().unwrap_or("");
        if d.force_pushed(me) {
            menu = menu.item(PopupItem::new("rewrite", "What the Force-Push Changed…"));
        } else if let Some(at) = d.last_reviewed(me) {
            let since = format!("{at}..{}", d.head_oid);
            menu = menu.item(PopupItem::new("since", "Changes Since Your Last Review").checked(self.commit.as_deref() == Some(since.as_str())));
        }
        if !d.commits.is_empty() {
            menu = menu.separator();
        }
        for (i, (id, subject)) in d.commits.iter().enumerate() {
            let shown = self.commit.as_deref() == Some(format!("{id}^..{id}").as_str());
            menu = menu.item(PopupItem::new(id.clone(), format!("{} of {} - {subject}", i + 1, d.commits.len())).checked(shown));
        }
        tx.popup_menu(menu, "pr-part", |s: &mut Collate, picked, _| match picked {
            Some("rewrite") => s.read_force_push(),
            Some(p) => s.show_pr_part(p),
            None => {}
        });
    }

    /// Reads what a force-push changed since the reader's last review, off the UI thread.
    fn read_force_push(&mut self) {
        let (Some(gh), Some(app), Some(d)) = (gh(), HANDLE.get().cloned(), self.pulls.pr.clone()) else { return };
        let Some(at) = self.pulls.me().and_then(|me| d.last_reviewed(me)).map(str::to_owned) else { return };
        self.say("Reading what the force-push changed…", false);
        std::thread::spawn(move || {
            let got = gh.rewritten(&d, &at);
            app.update(move |s, _| match got {
                Ok(out) if out.trim().is_empty() => s.say("The force-push changed no commits", false),
                Ok(out) => s.pulls.rewrite = Some(out),
                Err(e) => s.github_failed(e),
            });
        });
    }

    /// Leave the pull request: its threads go from the diff.
    pub fn close_pull(&mut self) {
        if self.pulls.pr.take().is_some() {
            self.place_comments();
        }
    }

    /// Run a change to a pull request off the UI thread, report it, and reread the pull request
    /// and the list. One at a time, with the app's other slow changes.
    fn pr_act(&mut self, doing: &str, f: impl FnOnce(&Gh) -> Result<String, String> + Send + 'static, then: impl FnOnce(&mut Collate) + Send + 'static) {
        let (Some(gh), Some(app)) = (gh(), HANDLE.get().cloned()) else { return };
        if self.busy.is_some() {
            self.say(format!("Wait for {} to finish", self.busy.as_deref().unwrap_or_default().trim_end_matches('…').to_lowercase()), true);
            return;
        }
        self.busy = Some(doing.to_owned());
        std::thread::spawn(move || {
            let result = f(&gh);
            app.update(move |s, _| {
                s.busy = None;
                let ok = result.is_ok();
                match result {
                    Ok(done) => s.say(done, false),
                    Err(e) => s.github_failed(e),
                }
                if ok {
                    then(s);
                }
                if let Some(n) = s.pulls.pr.as_ref().map(|p| p.pull.number) {
                    s.open_pull(n);
                }
                s.pulls.list = None;
                s.load_pulls();
            });
        });
    }

    /// After a merge or a checkout moved branches: read them, and the working tree, again.
    fn reread(&mut self) {
        self.load_branches();
        crate::refresh();
    }

    /// The reader's comments on lines of the open pull request's diff that GitHub takes: a
    /// review sends them.
    pub fn pending(&self) -> Vec<(usize, Draft)> {
        let Some(pr) = &self.pulls.pr else { return Vec::new() };
        // Lines of one commit, or of the change since a review, aren't the lines GitHub numbers.
        if !self.pr_whole() {
            return Vec::new();
        }
        self.comments
            .iter()
            .enumerate()
            .filter(|(i, c)| self.comments_shown.get(*i).copied().unwrap_or(false) && pr.commentable(&c.at.path, c.at.side == diff::Side::Old, c.at.line))
            .map(|(i, c)| (i, Draft { path: c.at.path.clone(), line: c.at.line, left: c.at.side == diff::Side::Old, body: c.body.clone() }))
            .collect()
    }

    /// The reader's comments in the diff that GitHub wouldn't take (outside its hunks): they
    /// stay on this Mac rather than failing the whole review.
    pub fn held(&self) -> usize {
        let shown = self.comments_shown.iter().filter(|&&on| on).count();
        if self.pulls.pr.is_none() { 0 } else { shown - self.pending().len() }
    }

    pub fn start_compose(&mut self, c: Compose) {
        self.pulls.compose = Some(c);
        self.pulls.text = TextEdit::new("");
        let pr = self.pulls.pr.as_ref();
        match c {
            Compose::Edit => {
                self.pulls.title = TextEdit::new(pr.map_or("", |p| p.pull.title.as_str()));
                self.pulls.text = TextEdit::new(pr.map_or("", |p| p.body.as_str()));
            }
            // A list to edit in place: the names as they are now.
            Compose::Reviewers => {
                self.pulls.title = TextEdit::new(pr.map(|p| p.reviewers.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default());
                self.load_people();
            }
            Compose::Labels => self.pulls.title = TextEdit::new(pr.map(|p| p.pull.labels.join(", ")).unwrap_or_default()),
            Compose::Assignees => {
                self.pulls.title = TextEdit::new(pr.map(|p| p.assignees.join(", ")).unwrap_or_default());
                self.load_people();
            }
            Compose::EditPost(i) => self.pulls.text = TextEdit::new(pr.and_then(|p| p.conversation.get(i)).map_or("", |p| p.body.as_str())),
            _ => {}
        }
        if c == Compose::Create {
            // The branch's last commit is the likeliest title.
            let subject = current_repo().and_then(|r| r.last_message()).and_then(|m| m.lines().next().map(str::to_owned)).unwrap_or_default();
            self.pulls.title = TextEdit::new(&subject);
            self.pulls.draft = false;
        }
    }

    /// Reads who can review, for the reviewers editor to offer.
    fn load_people(&mut self) {
        self.pulls.people = None;
        let (Some(gh), Some(app)) = (gh(), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let people = gh.assignable();
            app.update(move |s, _| s.pulls.people = Some(people));
        });
    }

    /// Ticks or unticks `who` in the reviewers or assignees editor's list.
    fn toggle_person(&mut self, who: &str) {
        let mut list = names(&self.pulls.title.text);
        match list.iter().position(|n| n == who) {
            Some(i) => {
                list.remove(i);
            }
            None => list.push(who.to_owned()),
        }
        self.pulls.title = TextEdit::new(list.join(", "));
    }

    fn send_compose(&mut self) {
        let Some(c) = self.pulls.compose.take() else { return };
        let body = self.pulls.text.text.trim().to_owned();
        let (n, head) = self.pulls.pr.as_ref().map_or((0, String::new()), |p| (p.pull.number, p.head_oid.clone()));
        match c {
            Compose::Review(v) => {
                let pending = self.pending();
                let drafts: Vec<Draft> = pending.iter().map(|(_, d)| d.clone()).collect();
                let sent: Vec<usize> = pending.iter().map(|(i, _)| *i).collect();
                let count = drafts.len();
                let what = match v {
                    Verdict::Approve => "Approved",
                    Verdict::RequestChanges => "Requested changes on",
                    Verdict::Comment => "Reviewed",
                };
                let note = if count == 0 { String::new() } else { format!(" with {count} line comment{}", if count == 1 { "" } else { "s" }) };
                self.pr_act("Sending the review…", move |gh| gh.review(n, &head, v, &body, &drafts).map(|_| format!("{what} #{n}{note}")), move |s| {
                    // The comments are on GitHub now; keep them here only once.
                    let mut i = 0;
                    s.comments.retain(|_| {
                        i += 1;
                        !sent.contains(&(i - 1))
                    });
                    s.store_comments();
                });
            }
            Compose::Comment => self.pr_act("Commenting…", move |gh| gh.comment(n, &body).map(|_| format!("Commented on #{n}")), |_| {}),
            Compose::Reply(t) => {
                let Some(to) = self.pulls.pr.as_ref().and_then(|p| p.threads.get(t)).map(|t| t.reply_to) else { return };
                self.pr_act("Replying…", move |gh| gh.reply(n, to, &body).map(|_| "Replied".to_owned()), |_| {});
            }
            Compose::Edit => {
                let title = self.pulls.title.text.trim().to_owned();
                self.pr_act("Saving…", move |gh| gh.retitle(n, &title, &body).map(|_| format!("Saved #{n}")), |_| {});
            }
            Compose::Reviewers | Compose::Labels | Compose::Assignees => {
                let Some(pr) = &self.pulls.pr else { return };
                let (what, was, done) = match c {
                    Compose::Reviewers => ("reviewer", pr.reviewers.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>(), "Updated the reviewers"),
                    Compose::Labels => ("label", pr.pull.labels.clone(), "Updated the labels"),
                    _ => ("assignee", pr.assignees.clone(), "Updated the assignees"),
                };
                let (add, remove) = changes(&was, &names(&self.pulls.title.text));
                self.pr_act("Saving…", move |gh| gh.people(n, what, &add, &remove).map(|_| done.to_owned()), |_| {});
            }
            Compose::EditPost(i) => {
                let Some(p) = self.pulls.pr.as_ref().and_then(|p| p.conversation.get(i)) else { return };
                let (id, review) = (p.id.clone(), !p.state.is_empty());
                self.pr_act("Saving…", move |gh| gh.edit_post(&id, review, &body).map(|_| "Saved the comment".to_owned()), |_| {});
            }
            Compose::DeletePost(i) => {
                let Some(id) = self.pulls.pr.as_ref().and_then(|p| p.conversation.get(i)).map(|p| p.id.clone()) else { return };
                self.pr_act("Deleting…", move |gh| gh.delete_post(&id).map(|_| "Deleted the comment".to_owned()), |_| {});
            }
            Compose::Create => {
                let (title, draft) = (self.pulls.title.text.trim().to_owned(), self.pulls.draft);
                let Some(base) = self.base.clone().map(|b| b.strip_prefix("origin/").map(str::to_owned).unwrap_or(b)) else {
                    self.say("Choose a base branch first, from the branch menu", true);
                    return;
                };
                self.pr_act("Opening the pull request…", move |gh| gh.create(&base, &title, &body, draft).map(|url| format!("Opened {url}")), |s| s.open_pulls());
            }
        }
    }

    /// Bring the base's new commits into the open pull request's branch: merged, or rebased.
    fn update_branch(&mut self, rebase: bool) {
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        self.pr_act("Updating the branch…", move |gh| gh.update_branch(n, rebase).map(|_| format!("Updated #{n} with its base")), |_| {});
    }

    /// Put thread `t`'s suggestion in place of the lines it's on, in the working tree: the
    /// pull request's branch has to be the one checked out. The change is left to commit.
    pub fn apply_suggestion(&mut self, t: usize) {
        let (Some(pr), Some(repo)) = (self.pulls.pr.as_ref(), current_repo()) else { return };
        let Some(th) = pr.threads.get(t) else { return };
        let (Some(last), Some(with)) = (th.line, th.comments.iter().find_map(|c| github::suggestion(&c.body))) else { return };
        let path = repo.root.join(&th.path);
        let applied = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", th.path))
            .and_then(|text| github::replace_lines(&text, th.start.unwrap_or(last), last, &with).ok_or_else(|| format!("{} no longer has those lines", th.path)))
            .and_then(|text| std::fs::write(&path, text).map_err(|e| format!("{}: {e}", th.path)));
        match applied {
            Ok(()) => self.say(format!("Applied the suggestion to {}; commit it from All Changes", th.path), false),
            Err(e) => self.say(format!("Couldn't apply the suggestion: {e}"), true),
        }
        crate::refresh();
    }

    pub fn toggle_thread(&mut self, t: usize) {
        let Some(th) = self.pulls.pr.as_ref().and_then(|p| p.threads.get(t)) else { return };
        let (id, resolve) = (th.id.clone(), !th.resolved);
        self.pr_act(if resolve { "Resolving…" } else { "Reopening the thread…" }, move |gh| gh.resolve(&id, resolve).map(|_| if resolve { "Resolved".to_owned() } else { "Reopened the thread".to_owned() }), |_| {});
    }

    fn merge_menu(&mut self, tx: &mut Tx) {
        let Some((n, head)) = self.pulls.pr.as_ref().map(|p| (p.pull.number, p.head_oid.clone())) else { return };
        let mut menu = PopupMenu::new();
        for m in Method::ALL {
            menu = menu.item(PopupItem::new(m.title(), m.title()));
        }
        tx.popup_menu(menu, "pr-merge", move |s: &mut Collate, picked, _| {
            let Some(m) = Method::ALL.into_iter().find(|m| Some(m.title()) == picked) else { return };
            let (delete, head) = (s.pulls.delete_branch, head.clone());
            s.pr_act("Merging…", move |gh| gh.merge(n, &head, m, delete).map(|_| format!("Merged #{n}")), Collate::reread);
        });
    }

    /// Turns on auto-merge by a method picked from a menu.
    fn auto_merge_menu(&mut self, tx: &mut Tx) {
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        let menu = PopupMenu::new().items(Method::ALL.into_iter().map(|m| PopupItem::new(m.title(), m.title())));
        tx.popup_menu(menu, "pr-auto-merge", move |s: &mut Collate, picked, _| {
            let Some(m) = Method::ALL.into_iter().find(|m| Some(m.title()) == picked) else { return };
            s.pr_act("Turning on auto-merge…", move |gh| gh.auto_merge(n, Some(m)).map(|_| format!("#{n} merges once its requirements pass")), |_| {});
        });
    }

    fn disable_auto_merge(&mut self) {
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        self.pr_act("Turning off auto-merge…", move |gh| gh.auto_merge(n, None).map(|_| format!("Auto-merge is off for #{n}")), |_| {});
    }

    /// Asks `who`, who has reviewed already, to review again.
    fn rerequest(&mut self, who: String) {
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        self.pr_act("Asking…", move |gh| gh.people(n, "reviewer", std::slice::from_ref(&who), &[]).map(|_| format!("Asked {who} to review again")), |_| {});
    }

    fn assign_me(&mut self) {
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        self.pr_act("Assigning…", move |gh| gh.people(n, "assignee", &["@me".to_owned()], &[]).map(|_| format!("Assigned you to #{n}")), |_| {});
    }

    fn react(&mut self, id: String, content: &'static str) {
        self.pr_act("Reacting…", move |gh| gh.react(&id, content).map(|_| "Reacted".to_owned()), |_| {});
    }

    /// Conversation post `i`'s menu: its link, a quoted reply, a reaction, and for the
    /// reader's own, editing and deleting it. A review's summary can be edited, not deleted.
    fn post_menu(&mut self, i: usize, tx: &mut Tx) {
        let me = self.pulls.me().map(str::to_owned);
        let Some(pr) = &self.pulls.pr else { return };
        let Some(p) = pr.conversation.get(i).cloned() else { return };
        let mine = p.mine || me.as_deref() == Some(p.author.as_str());
        let review = !p.state.is_empty();
        let link = if p.url.is_empty() { pr.pull.url.clone() } else { p.url.clone() };
        let react = PopupMenu::new().items(github::REACTIONS.iter().map(|(name, emoji)| PopupItem::new(*name, *emoji)));
        let menu = PopupMenu::new()
            .item(PopupItem::new("copy", "Copy Link"))
            .item(PopupItem::new("quote", "Quote Reply"))
            .submenu_enabled("React", react, !p.id.is_empty())
            .separator()
            .item(PopupItem::new("edit", "Edit").enabled(mine && !p.id.is_empty()))
            .item(PopupItem::new("delete", "Delete").enabled(mine && !review && !p.id.is_empty()));
        tx.popup_menu(menu, "pr-post", move |s: &mut Collate, picked, tx| match picked {
            Some("copy") => {
                tx.copy(link.clone());
                s.say("Copied the comment's link", false);
            }
            Some("quote") => {
                s.start_compose(Compose::Comment);
                let quoted: String = p.body.trim().lines().map(|l| format!("> {l}\n")).collect();
                s.pulls.text = TextEdit::new(format!("{quoted}\n"));
            }
            Some("edit") => s.start_compose(Compose::EditPost(i)),
            Some("delete") => s.start_compose(Compose::DeletePost(i)),
            Some(name) => {
                if let Some((content, _)) = github::REACTIONS.iter().find(|(n, _)| *n == name) {
                    s.react(p.id.clone(), content);
                }
            }
            None => {}
        });
    }

    /// Marks the open pull request ready for review, or turns it back into a draft.
    fn toggle_draft(&mut self) {
        let Some((n, draft)) = self.pulls.pr.as_ref().map(|p| (p.pull.number, p.pull.draft)) else { return };
        if draft {
            self.pr_act("Marking ready…", move |gh| gh.set(n, &["ready"]).map(|_| format!("#{n} is ready for review")), |_| {});
        } else {
            self.pr_act("Converting…", move |gh| gh.set(n, &["ready", "--undo"]).map(|_| format!("#{n} is a draft")), |_| {});
        }
    }

    /// Closes the open pull request, or reopens a closed one.
    fn toggle_closed(&mut self) {
        let Some((n, open)) = self.pulls.pr.as_ref().map(|p| (p.pull.number, p.state == "OPEN")) else { return };
        if open {
            self.pr_act("Closing…", move |gh| gh.set(n, &["close"]).map(|_| format!("Closed #{n}")), |_| {});
        } else {
            self.pr_act("Reopening…", move |gh| gh.set(n, &["reopen"]).map(|_| format!("Reopened #{n}")), |_| {});
        }
    }

    /// What has no button of its own on the overview: the branch, here and on GitHub.
    fn pr_menu(&mut self, tx: &mut Tx) {
        let Some(pr) = &self.pulls.pr else { return };
        let (n, open, id, locked, subscribed) = (pr.pull.number, pr.state == "OPEN", pr.meta.id.clone(), pr.meta.locked, pr.meta.subscribed);
        let (head, base) = (pr.pull.head.clone(), pr.pull.base.clone());
        // The remote's branches it could merge into instead, by their names there.
        let mut bases: Vec<String> = self.branches.iter().filter_map(|b| b.strip_prefix("origin/")).filter(|b| *b != head).map(str::to_owned).collect();
        bases.dedup();
        let base_menu = PopupMenu::new().items(bases.iter().map(|b| PopupItem::new(format!("base:{b}"), b.clone()).checked(*b == base)));
        let menu = PopupMenu::new()
            .item(PopupItem::new("checkout", "Check Out Branch"))
            .item(PopupItem::new("reload", "Reload"))
            .separator()
            .item(PopupItem::new("update", "Update Branch with Base").enabled(open))
            .item(PopupItem::new("update-rebase", "Update Branch by Rebasing").enabled(open))
            .submenu_enabled("Change Base Branch", base_menu, open && !bases.is_empty())
            .separator()
            .item(PopupItem::new("subscribe", if subscribed { "Unsubscribe" } else { "Subscribe" }).enabled(!id.is_empty()))
            .item(PopupItem::new("lock", if locked { "Unlock Conversation" } else { "Lock Conversation" }));
        tx.popup_menu(menu, "pr-more", move |s: &mut Collate, picked, _| match picked {
            Some("checkout") => s.pr_act("Checking out…", move |gh| gh.checkout(n).map(|_| format!("Checked out #{n}")), Collate::reread),
            Some("reload") => s.open_pull(n),
            Some("update") => s.update_branch(false),
            Some("update-rebase") => s.update_branch(true),
            Some("subscribe") => {
                let id = id.clone();
                let done = if subscribed { "You won't get its notifications" } else { "You'll get its notifications" };
                s.pr_act("Saving…", move |gh| gh.subscribe(&id, !subscribed).map(|_| done.to_owned()), |_| {});
            }
            Some("lock") => {
                let (verb, done) = if locked { ("unlock", format!("Unlocked #{n}")) } else { ("lock", format!("Locked #{n}: only collaborators can comment")) };
                s.pr_act("Saving…", move |gh| gh.set(n, &[verb]).map(|_| done), |_| {});
            }
            Some(b) => {
                if let Some(b) = b.strip_prefix("base:").map(str::to_owned) {
                    s.pr_act("Changing the base…", move |gh| gh.set_base(n, &b).map(|_| format!("#{n} merges into {b} now")), |_| {});
                }
            }
            None => {}
        });
    }
}

fn check_color(k: &Kit, s: Option<CheckState>) -> rune::Color {
    match s {
        Some(CheckState::Passed) => k.c.success.icon,
        Some(CheckState::Failed) => k.c.danger.icon,
        Some(CheckState::Pending) => k.c.warning.icon,
        _ => k.c.text_faint,
    }
}

fn dot(k: &Kit, s: Option<CheckState>) -> El {
    row().w(8.0).h(8.0).radius(4.0).fill(check_color(k, s)).a11y_hidden()
}

/// The pull request's state as GitHub marks it: its icon, colour and name.
fn state_mark(k: &Kit, p: &Pull) -> (&'static Icon, rune::Color, &'static str) {
    match p.state {
        PrState::Open if p.draft => (&PULL_DRAFT_ICON, k.c.icon_faint, "Draft"),
        PrState::Open => (&PULLS_ICON, k.c.success.icon, "Open"),
        // The kit has no violet of its own; the syntax colours' keyword violet follows the theme.
        PrState::Merged => (&MERGE_ICON, SyntaxTheme::from_kit(k).color(Syntax::Keyword), "Merged"),
        PrState::Closed => (&PULL_CLOSED_ICON, k.c.danger.icon, "Closed"),
    }
}

fn decision(k: &Kit, p: &Pull) -> (&'static str, rune::Color) {
    match p.decision.as_str() {
        _ if p.state != PrState::Open => ("", k.c.text_muted),
        _ if p.draft => ("Draft", k.c.text_faint),
        "APPROVED" => ("Approved", k.c.success.icon),
        "CHANGES_REQUESTED" => ("Changes requested", k.c.danger.icon),
        "REVIEW_REQUIRED" => ("Review required", k.c.text_muted),
        _ => ("", k.c.text_muted),
    }
}

/// In place of the list until GitHub is connected: what connecting gives, and the button.
fn connect_view(s: &Collate, k: &Kit) -> El {
    let detail = s.pulls.connecting.unwrap_or("See this repository's pull requests, review them, and get a notification when someone asks for your review.");
    let button = Button::new("Connect to GitHub").icon(&GITHUB_ICON).variant(ButtonVariant::Contrast)
        .help("Sign in with the GitHub CLI, installing it first if it's missing").on_press(|s: &mut Collate| s.connect_github()).build(k).key("connect-github");
    col().key("connect").grow(1.0).w_full().center().gap(6.0).fill(k.c.bg_base)
        .child(icon(&GITHUB_ICON, 32.0).color(k.c.icon_faint).mb(8.0).a11y_hidden())
        .child(text("Connect to GitHub".to_owned()).font(k.ty.ui_medium).color(k.c.text_base))
        .child(text(detail.to_owned()).font(k.ty.ui).color(k.c.text_muted).wrap().max_w(420.0).align(rune::Align::Center))
        .child(button.mt(10.0))
}

/// The list of pull requests, in place of the diff.
pub fn pulls_view(s: &Collate, k: &Kit) -> El {
    let refresh = IconButton::new(&REFRESH_ICON).variant(IconButtonVariant::GhostMuted).label("Reload pull requests").disabled(s.pulls.loading).on_press(|s: &mut Collate| {
        s.pulls.list = None;
        s.load_pulls();
    });
    let create = Button::new("New Pull Request").variant(ButtonVariant::Outline).control_size(ControlSize::Small).help("Open a pull request from this branch")
        .disabled(s.branch.is_empty() || s.branch == "HEAD").on_press(|s: &mut Collate| s.start_compose(Compose::Create)).build(k).key("pr-new");
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let mut filter = TextInput::new(&s.pulls.filter).placeholder("Filter by title, number, author, branch or label").leading_icon(k.glyph.search);
    if !s.pulls.filter.text.is_empty() {
        filter = filter.clear_button();
    }
    let filter = filter.on_edit(|s: &mut Collate| &mut s.pulls.filter).width(None).build(k).key("pulls-filter").w(280.0);
    let mut head = row().key("pulls-head").w_full().h(28.0).px(EDGE).gap(8.0).items_center()
        .child(text("Pull Requests".to_owned()).font(k.ty.ui_medium).color(k.c.text_base).grow(1.0));
    // Filtering and opening one need GitHub, so they wait until it's connected.
    if !matches!(&s.pulls.list, Some(Err(e)) if github::needs_connect(e)) {
        head = head.child(filter).child(create);
    }
    let head = head.child(refresh.build(k).key("pulls-reload"));
    let body = match &s.pulls.list {
        None => message(k, "Reading pull requests…", ""),
        Some(Err(e)) if github::needs_connect(e) => connect_view(s, k),
        Some(Err(e)) => message(k, "Can't read pull requests", e),
        Some(Ok((_, pulls))) if pulls.is_empty() => message(k, "No pull requests", "Open one from this branch with New Pull Request."),
        Some(Ok((me, pulls))) => {
            let mut list = col().key("pulls-list").w_full().pb(16.0);
            let words: Vec<String> = s.pulls.filter.text.split_whitespace().map(str::to_lowercase).collect();
            let shown: Vec<Pull> = pulls.iter().filter(|p| matches(p, &words)).cloned().collect();
            if shown.is_empty() {
                return col().key("pulls").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(message(k, "No pull requests match", "Try fewer or other words."));
            }
            for (title, group) in github::group(&shown, me) {
                if group.is_empty() {
                    continue;
                }
                // A click anywhere on the title folds the group to it or opens it again.
                let folded = s.pulls.folded.contains(title);
                list = list.child(row().key(format!("group-{title}")).w_full().h(28.0).px(EDGE).gap(6.0).items_center().mt(8.0)
                    .cursor(Cursor::Pointer).role("button").label(title.to_owned()).description(format!("{} pull requests", group.len())).expanded(!folded)
                    .help(if folded { "Show the group" } else { "Fold the group to its title" })
                    .on_press(move |s: &mut Collate| {
                        if !s.pulls.folded.remove(title) {
                            s.pulls.folded.insert(title);
                        }
                    })
                    .child(text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
                    .child(text(group.len().to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint).grow(1.0).a11y_hidden())
                    // The staging panel's chevron, turned as it turns; its glyph, not its box, on the edge.
                    .child(icon(k.glyph.chevron_down, 16.0).key("disclosure").mr(-4.0).color(k.c.icon_faint)
                        .icon_rotation(if folded { -std::f32::consts::FRAC_PI_2 } else { 0.0 }).motion(k.motion.control).a11y_hidden()));
                if folded {
                    continue;
                }
                for p in group {
                    let n = p.number;
                    let fetching = s.pulls.fetching == Some(n);
                    let (verdict, tint) = decision(k, p);
                    let (mark, hue, state) = state_mark(k, p);
                    let mut words = col().grow(1.0).min_w(0.0).gap(2.0)
                        .child(text(p.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate())
                        .child(text(format!("#{n} - {} - {} → {} - {}", p.author, p.head, p.base, p.updated.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).truncate());
                    if !p.labels.is_empty() {
                        words = words.child(text(p.labels.join(", ")).font(k.ty.text_small).color(k.c.text_faint).truncate());
                    }
                    let mut line = row().key(format!("pull-{n}")).w_full().px(EDGE).py(8.0).gap(12.0).items_center()
                        .hover(k.c.hover).cursor(Cursor::Pointer).role("button").label(format!("#{n} {}", p.title)).description(state).help("Review this pull request")
                        .on_press(move |s: &mut Collate| s.open_pull(n))
                        .child(icon(mark, 16.0).color(hue).a11y_hidden())
                        .child(words)
                        .child(crate::history_view::face(k, s, &p.author, 20.0));
                    if p.ci.is_some() {
                        line = line.child(dot(k, p.ci));
                    }
                    list = list.child(line
                        .child(text(if fetching { "Opening…" } else { verdict }.to_owned()).font(k.ty.text_xsmall_medium).color(tint))
                        .child(text(format!("+{}", p.additions)).font(mono).color(k.c.success.fg))
                        .child(text(format!("−{}", p.deletions)).font(mono).color(k.c.danger.fg)));
                }
            }
            col().key("pulls-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(list)
        }
    };
    col().key("pulls").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(body)
}

/// Whether every word is in the pull request's title, number, author, branches or labels.
fn matches(p: &Pull, words: &[String]) -> bool {
    let hay = format!("{} #{} {} {} {} {}", p.title, p.number, p.author, p.head, p.base, p.labels.join(" ")).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// What stands between the pull request and merging, in words.
fn readiness(pr: &Detail) -> (&'static str, bool) {
    match (pr.state.as_str(), pr.mergeable.as_str(), pr.merge_state.as_str()) {
        ("MERGED", ..) => ("Merged", false),
        ("CLOSED", ..) => ("Closed", false),
        (_, "CONFLICTING", _) | (_, _, "DIRTY") => ("Has conflicts with the base", false),
        (_, _, "BEHIND") => ("Behind the base branch", true),
        (_, _, "BLOCKED") => ("Blocked: needs approval or passing checks", false),
        (_, _, "UNSTABLE") => ("Some checks failed", true),
        (_, _, "DRAFT") => ("Draft: not ready to merge", false),
        (_, _, "CLEAN" | "HAS_HOOKS") => ("Ready to merge", true),
        _ => ("Checking mergeability…", false),
    }
}

/// Where the merge stands, in a line under its title: what to do next, or why it can't.
fn readiness_why(pr: &Detail) -> String {
    let p = &pr.pull;
    match (pr.state.as_str(), pr.mergeable.as_str(), pr.merge_state.as_str()) {
        ("MERGED", ..) => format!("{} was merged into {}", p.head, p.base),
        ("CLOSED", ..) => "Closed without merging".to_owned(),
        (_, "CONFLICTING", _) | (_, _, "DIRTY") => "Settle the conflicts before merging".to_owned(),
        (_, _, "BEHIND") => format!("Update it to take in {}'s new commits", p.base),
        (_, _, "BLOCKED") => "Merging waits on an approval or passing checks".to_owned(),
        (_, _, "UNSTABLE") => "It can merge, but a check is failing".to_owned(),
        (_, _, "DRAFT") => "Mark it ready for review to merge it".to_owned(),
        (_, _, "CLEAN" | "HAS_HOOKS") => format!("No conflicts with {}", p.base),
        _ => "GitHub is still working it out".to_owned(),
    }
}

/// The open pull request's two pages, as GitHub and DiffKit split them: what it is and where it
/// stands, and the files it changes. They stand in the middle column's header.
pub fn pr_tabs(s: &Collate, k: &Kit) -> Option<El> {
    let pr = s.pulls.pr.as_ref()?;
    let files = if s.loaded { format!("Files Changed {}", s.diff.files().len()) } else { "Files Changed".to_owned() };
    let tabs = SegmentedControl::new(&["Overview", files.as_str()], Some((s.pulls.tab == PrTab::Files) as usize))
        .label("Pull request views")
        .fit()
        .on_pick(|s: &mut Collate, i, _: &mut Tx| s.pulls.tab = if i == 1 { PrTab::Files } else { PrTab::Overview })
        .build(k)
        .key("pr-tabs");
    let mono = rune::TextStyle { size: 12.0, line_height: 18.0, ..k.ty.mono };
    Some(row().key("pr-tabs-row").grow(1.0).min_w(0.0).gap(10.0).items_center().child(tabs)
        .child(text(format!("+{}", pr.pull.additions)).font(mono).color(k.c.success.fg))
        .child(text(format!("−{}", pr.pull.deletions)).font(mono).color(k.c.danger.fg)))
}

/// A section's small title on the overview, with an optional action at its end.
fn heading(k: &Kit, key: &str, title: &str, action: Option<El>) -> El {
    let mut r = row().key(key.to_owned()).w_full().h(24.0).items_center()
        .child(text(title.to_owned()).font(k.ty.text_small_medium).color(k.c.text_muted).grow(1.0));
    if let Some(a) = action {
        r = r.child(a);
    }
    r
}

/// A branch name as a chip, the way GitHub sets them in a sentence.
fn branch_chip(k: &Kit, name: &str) -> El {
    let mono = rune::TextStyle { size: 12.0, line_height: 18.0, ..k.ty.mono };
    row().px(6.0).radius(k.radius.sm).fill(k.alpha(k.c.text_base, 0.07)).min_w(0.0)
        .child(text(name.to_owned()).font(mono).color(k.c.text_base).truncate().min_w(0.0))
}

/// One line of the merge box: a mark, what stands, why, and what can be done about it.
fn status_row(k: &Kit, key: &str, mark: El, title: &str, why: &str, actions: Vec<El>) -> El {
    let mut r = row().key(key.to_owned()).w_full().px(14.0).py(12.0).gap(12.0).items_start()
        .child(row().pt(5.0).child(mark))
        .child(col().grow(1.0).min_w(0.0).gap(2.0)
            .child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base).wrap().w_full())
            .child(text(why.to_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full()));
    for a in actions {
        r = r.child(a);
    }
    r
}

/// A comment or review in the activity: who, what they did and when, over what they wrote.
fn activity_post(s: &Collate, k: &Kit, i: usize, p: &github::Post) -> El {
    let key = &format!("post-{i}");
    let (verdict, tint) = match p.state.as_str() {
        "APPROVED" => ("approved these changes", k.c.success.icon),
        "CHANGES_REQUESTED" => ("requested changes", k.c.danger.icon),
        "COMMENTED" => ("reviewed", k.c.text_muted),
        "DISMISSED" => ("had a review dismissed", k.c.text_muted),
        _ => ("commented", k.c.text_muted),
    };
    let parts = crate::body::parse(p.body.trim());
    // A long one (a bot's review, mostly) shuts to its gist, so the activity reads at a glance;
    // a press on its head opens it.
    let long = !crate::body::short(&parts);
    let shown = !long || s.pulls.expanded.contains(key);
    let mut head = row().key(format!("{key}-head")).w_full().px(12.0).py(8.0).gap(6.0).items_center().fill(k.alpha(k.c.text_base, 0.03))
        .child(text(p.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(text(verdict.to_owned()).font(k.ty.text_small).color(tint))
        .child(text(p.time.get(..10).unwrap_or("").to_owned()).font(k.ty.text_small).color(k.c.text_faint).grow(1.0));
    if long {
        let flip = key.to_owned();
        head = head.cursor(Cursor::Pointer).role("button").label(format!("{} {}'s {}", if shown { "Hide" } else { "Show" }, p.author, verdict))
            .on_press(move |s: &mut Collate| if !s.pulls.expanded.remove(&flip) { s.pulls.expanded.insert(flip.clone()); })
            .child(icon(k.glyph.chevron_down, 14.0).color(k.c.icon_muted)
                .icon_rotation(if shown { std::f32::consts::PI } else { 0.0 }).motion(k.motion.control).a11y_hidden());
    }
    let more = IconButton::new(&MORE_ICON).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Small).label(format!("Actions for {}'s comment", p.author))
        .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.post_menu(i, tx)).build(k).key(format!("{key}-more")).mr(-6.0);
    let head = head.child(more);
    let mut c = col().key(key.to_owned()).w_full().radius(k.radius.md).border(1.0, k.c.border_weak).child(head);
    if !parts.is_empty() {
        c = c.child(col().w_full().h(1.0).fill(k.c.border_weak));
        c = if shown {
            c.child(col().w_full().px(12.0).py(10.0).child(parts_view(s, k, &format!("{key}-body"), &parts, k.ty.text_small)))
        } else {
            c.child(text(crate::body::gist(&parts)).key(format!("{key}-gist")).font(k.ty.text_small).color(k.c.text_muted).truncate().w_full().px(12.0).py(8.0))
        };
    }
    if !p.reactions.is_empty() {
        // Each reaction and its count; a press adds the reader's own, as on GitHub.
        c = c.child(row().key(format!("{key}-reactions")).w_full().px(12.0).pb(10.0).gap(6.0).flex_wrap().children(p.reactions.iter().filter_map(|(name, n)| {
            let (content, emoji) = github::REACTIONS.iter().find(|(c, _)| c == name)?;
            let id = p.id.clone();
            Some(row().key(format!("{key}-react-{content}")).h(24.0).px(8.0).gap(4.0).items_center().radius(12.0).border(1.0, k.c.border_weak)
                .cursor(Cursor::Pointer).role("button").label(format!("{n} {content}, add yours"))
                .on_press(move |s: &mut Collate| s.react(id.clone(), content))
                .child(text((*emoji).to_owned()).font(k.ty.text_small))
                .child(text(n.to_string()).font(k.ty.text_small).color(k.c.text_muted)))
        })));
    }
    // Who wrote it beside it, as GitHub's timeline sets it.
    row().key(format!("{key}-row")).w_full().gap(12.0).items_start()
        .child(crate::history_view::face(k, s, &p.author, 32.0).mt(2.0))
        .child(c.grow(1.0).min_w(0.0))
}

/// The open pull request's overview, as GitHub and DiffKit lay it out: its title and what it
/// merges where, the description, the activity, the merge box and the reader's review; its
/// properties, reviewers and labels in a column beside them. Its files are the other tab.
pub fn pr_overview(s: &Collate, k: &Kit) -> Option<El> {
    let pr = s.pulls.pr.as_ref()?;
    let p = &pr.pull;
    let open = pr.state == "OPEN";
    let me = s.pulls.list.as_ref().and_then(|l| l.as_ref().ok()).map(|(login, _)| login.as_str());
    let (state_icon, state_color, state_name) = state_mark(k, p);

    // The title, and what merges where.
    let title_font = rune::TextStyle { size: 23.0, line_height: 32.0, weight: 600.0, ..k.ty.ui_medium };
    let tool = |i: &'static Icon, label: &str| IconButton::new(i).variant(IconButtonVariant::GhostMuted).label(label.to_owned());
    let (web, link) = (p.url.clone(), p.url.clone());
    let tools = row().key("pr-tools").pt(4.0).gap(2.0).items_center().mr(-4.0)
        .child(tool(&PENCIL_ICON, "Edit title and description").disabled(!open)
            .on_press(|s: &mut Collate| s.start_compose(Compose::Edit)).build(k).key("pr-edit"))
        .child(tool(&GITHUB_ICON, "Open on GitHub").on_press(move |_: &mut Collate| open_url(&web)).build(k).key("pr-web"))
        .child(tool(&LINK_ICON, "Copy link").help("Copy the pull request's address")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| { tx.copy(link.clone()); s.say("Copied the pull request's link", false); }).build(k).key("pr-copy-link"))
        .child(tool(&MORE_ICON, "More actions for this pull request").help("Check out, reload, update the branch")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.pr_menu(tx)).build(k).key("pr-more"));
    let pill = row().key("pr-state").h(22.0).px(8.0).gap(5.0).items_center().radius(11.0).fill(k.alpha(state_color, 0.16))
        .child(icon(state_icon, 12.0).color(state_color))
        .child(text(state_name.to_owned()).font(k.ty.text_small_medium).color(state_color));
    let merges = match pr.state.as_str() {
        "MERGED" => "merged into",
        "CLOSED" => "wanted to merge into",
        _ => "wants to merge into",
    };
    let muted = |t: &str| text(t.to_owned()).font(k.ty.text_small).color(k.c.text_muted);
    let byline = row().key("pr-byline").w_full().gap(6.0).items_center().flex_wrap()
        .child(pill)
        .child(crate::history_view::face(k, s, &p.author, 20.0).ml(2.0))
        .child(text(p.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(muted(merges)).child(branch_chip(k, &p.base))
        .child(muted("from")).child(branch_chip(k, &p.head))
        .child(text(format!("#{}", p.number)).font(k.ty.text_small).color(k.c.text_faint));
    let mut main = col().key("pr-main").grow(1.0).min_w(0.0).max_w(780.0).gap(28.0)
        .child(col().key("pr-title").w_full().gap(10.0)
            .child(row().w_full().gap(8.0).items_start()
                .child(text(p.title.clone()).font(title_font).color(k.c.text_base).wrap().grow(1.0).min_w(0.0))
                .child(tools))
            .child(byline));

    // Asked for the reader's review: the way to the files.
    if open && me.is_some_and(|me| pr.reviewers.iter().any(|(who, st)| who == me && st == "REQUESTED")) {
        let go = Button::new("Review Changes").variant(ButtonVariant::Contrast).control_size(ControlSize::Small)
            .help("Open the files this pull request changes").on_press(|s: &mut Collate| s.pulls.tab = PrTab::Files).build(k).key("pr-review-changes");
        main = main.child(row().key("pr-asked").w_full().px(14.0).py(10.0).gap(10.0).items_center().radius(k.radius.md).border(1.0, k.c.border_weak)
            .fill(k.alpha(k.c.text_base, 0.03))
            .child(text("Your review has been requested").font(k.ty.ui).color(k.c.text_base).grow(1.0))
            .child(go));
    }

    // The description.
    let body = if pr.body.trim().is_empty() {
        col().key("pr-body").w_full().child(text("No description provided.").font(k.ty.ui).color(k.c.text_faint))
    } else {
        rich(s, k, "pr-body", &pr.body, k.ty.ui)
    };
    main = main.child(body);

    // The activity: comments and reviews, oldest first.
    let open_threads = pr.threads.iter().filter(|t| !t.resolved).count();
    let mut activity = col().key("pr-activity").w_full().gap(10.0)
        .child(heading(k, "pr-talk-title", &format!("Activity - {} comment{}, {open_threads} open thread{}",
            pr.conversation.len(), if pr.conversation.len() == 1 { "" } else { "s" }, if open_threads == 1 { "" } else { "s" }), None));
    if pr.conversation.is_empty() {
        activity = activity.child(text("No comments yet.").font(k.ty.text_small).color(k.c.text_faint));
    }
    for (i, post) in pr.conversation.iter().enumerate() {
        activity = activity.child(activity_post(s, k, i, post));
    }
    main = main.child(activity);

    // The merge box: reviews, checks and mergeability, then merging.
    let (ready, can_merge) = readiness(pr);
    let line = || col().w_full().h(1.0).fill(k.c.border_weak);
    let mut merge_box = col().key("pr-merge-box").w_full().radius(k.radius.md).border(1.0, k.c.border_weak);
    if open {
        let said = |st: &str| pr.reviewers.iter().filter(|(_, x)| x == st).map(|(w, _)| w.as_str()).collect::<Vec<_>>();
        let (approved, changes, asked) = (said("APPROVED"), said("CHANGES_REQUESTED"), said("REQUESTED"));
        let (mark, title, why) = if !changes.is_empty() {
            (dot(k, Some(CheckState::Failed)), "Changes requested", format!("By {}", changes.join(", ")))
        } else if !approved.is_empty() {
            (dot(k, Some(CheckState::Passed)), "Changes approved", format!("By {}", approved.join(", ")))
        } else if !asked.is_empty() {
            (dot(k, Some(CheckState::Pending)), "Review requested", format!("Waiting on {}", asked.join(", ")))
        } else {
            (dot(k, Some(CheckState::Skipped)), "No reviews yet", "Nobody has reviewed it".to_owned())
        };
        merge_box = merge_box.child(status_row(k, "pr-reviews", mark, title, &why, vec![]));
    }
    if !p.checks.is_empty() {
        let count = |st: CheckState| p.checks.iter().filter(|c| c.state == st).count();
        let (failed, pending, passed) = (count(CheckState::Failed), count(CheckState::Pending), count(CheckState::Passed));
        let (state, title) = if failed > 0 {
            (CheckState::Failed, "Some checks failed")
        } else if pending > 0 {
            (CheckState::Pending, "Some checks haven't finished")
        } else {
            (CheckState::Passed, "All checks have passed")
        };
        let mut why = vec![format!("{passed} passed")];
        if failed > 0 { why.push(format!("{failed} failed")); }
        if pending > 0 { why.push(format!("{pending} running")); }
        // Failed GitHub Actions runs, each once: those can be run again from here.
        let mut runs: Vec<u64> = p.checks.iter().filter(|c| c.state == CheckState::Failed).filter_map(github::Check::run_id).collect();
        runs.sort_unstable();
        runs.dedup();
        let mut actions = vec![];
        if open && !runs.is_empty() {
            let n = p.number;
            actions.push(Button::new("Re-run Failed").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(s.busy.is_some())
                .help("Run the failed GitHub Actions jobs again").on_press(move |s: &mut Collate| {
                    let runs = runs.clone();
                    s.pr_act("Re-running…", move |gh| gh.rerun_failed(&runs).map(|_| format!("Re-running the failed checks of #{n}")), |_| {});
                }).build(k).key("pr-rerun").mr(-GHOST_PAD));
        }
        if open {
            merge_box = merge_box.child(line());
        }
        merge_box = merge_box.child(status_row(k, "pr-checks", dot(k, Some(state)), title, &why.join(", "), actions));
        let mut checks: Vec<&github::Check> = p.checks.iter().collect();
        // Failures first: they're what needs looking at.
        checks.sort_by_key(|c| match c.state {
            CheckState::Failed => 0,
            CheckState::Pending => 1,
            CheckState::Passed => 2,
            CheckState::Skipped => 3,
        });
        let mut list = col().key("pr-check-list").w_full().pl(34.0).pr(14.0).pb(10.0).gap(2.0);
        for (i, c) in checks.into_iter().enumerate() {
            let url = c.url.clone();
            let mut r = row().key(format!("check-{i}")).w_full().h(24.0).gap(8.0).items_center().child(dot(k, Some(c.state)))
                .child(text(c.name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0));
            if !url.is_empty() {
                r = r.cursor(Cursor::Pointer).role("link").label(format!("Open {}", c.name)).help("Open the check on GitHub").on_press(move |_: &mut Collate| open_url(&url));
            }
            list = list.child(r);
        }
        merge_box = merge_box.child(list);
    }
    let mut fixes = vec![];
    // Behind its base: the one-click fix GitHub offers, a merge of the base into it.
    if open && pr.merge_state == "BEHIND" {
        fixes.push(Button::new("Update Branch").variant(ButtonVariant::Outline).control_size(ControlSize::Small).disabled(s.busy.is_some())
            .help("Merge the base's new commits into this branch, on GitHub").on_press(|s: &mut Collate| s.update_branch(false)).build(k).key("pr-update"));
    }
    // Conflicts: GitHub can't settle them; the reader's agent can, here.
    if open && (pr.mergeable == "CONFLICTING" || pr.merge_state == "DIRTY") {
        let settle = Settle::Pull(p.number, p.head.clone(), p.base.clone());
        fixes.push(Button::new(format!("Settle with {}", s.settings.agent.title())).variant(ButtonVariant::Outline).control_size(ControlSize::Small)
            .help("Start the coding agent in the Agent tab to merge the base into this branch and settle the conflicts; it doesn't push")
            .on_press(move |s: &mut Collate| s.settle_with_agent(settle.clone())).build(k).key("pr-agent"));
    }
    if open || !p.checks.is_empty() {
        merge_box = merge_box.child(line());
    }
    let ready_mark = if pr.state == "MERGED" {
        icon(&MERGE_ICON, 14.0).color(state_color)
    } else {
        dot(k, Some(if can_merge { CheckState::Passed } else if open { CheckState::Pending } else { CheckState::Skipped }))
    };
    merge_box = merge_box.child(status_row(k, "pr-ready", ready_mark, ready, &readiness_why(pr), fixes));
    if open && let Some(method) = &pr.meta.auto_merge {
        let off = Button::new("Disable Auto-Merge").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(s.busy.is_some())
            .on_press(|s: &mut Collate| s.disable_auto_merge()).build(k).key("pr-auto-off").mr(-GHOST_PAD);
        let how = Method::ALL.into_iter().find(|m| m.name() == method).map_or(method.to_lowercase(), |m| m.title().to_lowercase());
        merge_box = merge_box.child(line())
            .child(status_row(k, "pr-auto", dot(k, Some(CheckState::Pending)), "Auto-merge enabled", &format!("It will {how} once its requirements pass"), vec![off]));
    }
    if open {
        let merge = Button::new("Merge…").variant(ButtonVariant::Contrast).control_size(ControlSize::Small).disabled(!can_merge || s.busy.is_some())
            .help("Squash, merge or rebase into the base").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.merge_menu(tx)).build(k).key("pr-merge");
        let delete = Checkbox::new(s.pulls.delete_branch).label("Delete branch").on_toggle(|s: &mut Collate| s.pulls.delete_branch ^= true).build(k).key("pr-delete-branch");
        let mut merge_row = row().key("pr-merge-row").w_full().px(14.0).py(10.0).gap(12.0).items_center().fill(k.alpha(k.c.text_base, 0.03)).child(merge);
        // Waiting on checks or reviews: GitHub can merge it by itself once they pass.
        if !can_merge && pr.meta.auto_merge.is_none() {
            merge_row = merge_row.child(Button::new("Enable Auto-Merge…").variant(ButtonVariant::Outline).control_size(ControlSize::Small).disabled(s.busy.is_some())
                .help("Merge it by itself once its checks and reviews pass").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.auto_merge_menu(tx)).build(k).key("pr-auto-merge"));
        }
        merge_box = merge_box.child(line()).child(merge_row.child(delete));
    }
    main = main.child(merge_box);

    // The reader's review, and a comment on the whole of it.
    if open {
        let pending = s.pending().len();
        let verdict = |key: &str, label: &str, v: Verdict| Button::new(label.to_owned()).variant(ButtonVariant::Outline).control_size(ControlSize::Small)
            .disabled(s.busy.is_some()).on_press(move |s: &mut Collate| s.start_compose(Compose::Review(v))).build(k).key(key.to_owned());
        let comment = Button::new("Add a Comment").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .on_press(|s: &mut Collate| s.start_compose(Compose::Comment)).build(k).key("pr-add-comment");
        let mut review = col().key("pr-your-review").w_full().gap(8.0)
            .child(heading(k, "pr-review-title", "Your Review", None))
            .child(row().key("pr-review").w_full().gap(6.0).items_center().flex_wrap()
                .child(verdict("pr-approve", "Approve", Verdict::Approve))
                .child(verdict("pr-changes", "Request Changes", Verdict::RequestChanges))
                .child(verdict("pr-comment-review", if pending > 0 { "Send Comments" } else { "Comment" }, Verdict::Comment))
                .child(row().grow(1.0))
                .child(comment.mr(-GHOST_PAD)));
        if pending > 0 {
            review = review.child(text(format!("{pending} line comment{} go with your review", if pending == 1 { "" } else { "s" })).font(k.ty.text_small).color(k.c.text_muted));
        }
        let held = s.held();
        if held > 0 {
            let why = if s.pr_whole() { "outside GitHub's diff" } else { "on part of the pull request" };
            review = review.child(text(format!("{held} comment{} {why} stay on this Mac", if held == 1 { "" } else { "s" })).font(k.ty.text_small).color(k.c.warning.icon).wrap().w_full());
        }
        main = main.child(review);
    }

    // Beside it: what it is, who reviews it, its labels.
    let side_row = |key: String, lead: El, label: String, tint: rune::Color| row().key(key).w_full().h(24.0).gap(8.0).items_center().child(lead)
        .child(text(label).font(k.ty.text_small).color(tint).truncate().min_w(0.0).grow(1.0));
    // A gear opens each list's editor, as on GitHub.
    let gear = |key: &str, what: Compose, label: &str| IconButton::new(&GEAR_ICON).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Small)
        .label(label.to_owned()).disabled(!open).on_press(move |s: &mut Collate| s.start_compose(what)).build(k).key(key.to_owned()).mr(-4.0);
    let mut props = col().key("pr-properties").w_full().gap(2.0).child(heading(k, "pr-properties-title", "Properties", None))
        .child(side_row("prop-state".into(), icon(state_icon, 14.0).color(state_color), state_name.to_owned(), k.c.text_base))
        .child(side_row("prop-commits".into(), icon(&MERGE_ICON, 14.0).color(k.c.icon_muted), format!("{} commit{}", pr.commits.len(), if pr.commits.len() == 1 { "" } else { "s" }), k.c.text_muted));
    if let Some(day) = p.updated.get(..10) {
        props = props.child(side_row("prop-updated".into(), icon(&CLOCK_ICON, 14.0).color(k.c.icon_muted), format!("Updated {day}"), k.c.text_muted));
    }
    // Under what it is, what changes that: a draft or not, open or closed. A merged one stays.
    if pr.state != "MERGED" {
        let act = |key: &str, glyph: &'static Icon, label: &str, help: &str, f: fn(&mut Collate)| Button::new(label.to_owned()).icon(glyph).variant(ButtonVariant::GhostMuted).control_size(ControlSize::Small)
            .help(help.to_owned()).disabled(s.busy.is_some()).on_press(f).build(k).key(key.to_owned()).ml(-GHOST_PAD);
        let mut acts = col().key("pr-state-actions").w_full().pt(6.0).gap(2.0).items_start();
        if open {
            acts = acts.child(if p.draft {
                act("pr-draft", &PULLS_ICON, "Ready for Review", "Mark it ready, which asks its reviewers", Collate::toggle_draft)
            } else {
                act("pr-draft", &PULL_DRAFT_ICON, "Convert to Draft", "Mark it as still in progress", Collate::toggle_draft)
            });
        }
        acts = acts.child(if open {
            act("pr-close", &PULL_CLOSED_ICON, "Close Pull Request", "Close it without merging", Collate::toggle_closed)
        } else {
            act("pr-close", &PULLS_ICON, "Reopen Pull Request", "Open it again", Collate::toggle_closed)
        });
        props = props.child(acts);
    }
    let mut reviewers = col().key("pr-reviewers").w_full().gap(2.0).child(heading(k, "pr-reviewers-title", "Reviewers", Some(gear("pr-edit-reviewers", Compose::Reviewers, "Ask for reviews"))));
    if pr.reviewers.is_empty() {
        reviewers = reviewers.child(text("No reviews").font(k.ty.text_small).color(k.c.text_faint));
    }
    // Who, and where they stand, as GitHub marks it: approved, changes asked, commented, waited on.
    for (who, state) in &pr.reviewers {
        let (mark, said) = match state.as_str() {
            "APPROVED" => (icon(&CHECK_ICON, 14.0).color(k.c.success.icon), "Approved"),
            "CHANGES_REQUESTED" => (icon(&crate::ALL_ICON, 14.0).color(k.c.danger.icon), "Requested changes"),
            "REQUESTED" => (row().size(14.0, 14.0).center().child(dot(k, Some(CheckState::Pending))), "Awaiting review"),
            _ => (icon(&MESSAGE_ICON, 14.0).color(k.c.icon_muted), "Commented"),
        };
        let mut r = row().key(format!("reviewer-{who}")).w_full().h(28.0).gap(8.0).items_center().label(format!("{who} - {said}")).help(said.to_owned())
            .child(crate::history_view::face(k, s, who, 20.0))
            .child(text(who.clone()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).truncate().min_w(0.0));
        // Reviewed already: ask again, as GitHub's re-request does.
        if open && state != "REQUESTED" && Some(who.as_str()) != me {
            let again = who.clone();
            r = r.child(IconButton::new(&REFRESH_ICON).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Small).label(format!("Ask {who} to review again"))
                .disabled(s.busy.is_some()).on_press(move |s: &mut Collate| s.rerequest(again.clone())).build(k).key(format!("rerequest-{who}")));
        }
        reviewers = reviewers.child(r.child(mark.a11y_hidden()));
    }
    let mut labels = col().key("pr-labels").w_full().gap(4.0).child(heading(k, "pr-labels-title", "Labels", Some(gear("pr-edit-labels", Compose::Labels, "Change the labels"))));
    if p.labels.is_empty() {
        labels = labels.child(text("None yet").font(k.ty.text_small).color(k.c.text_faint));
    } else {
        // Each in its own colour, as on GitHub: a tint of it, so the name reads in both themes.
        let tint = |name: &str| pr.label_colors.iter().find(|(n, _)| n == name).and_then(|(_, c)| rune::color::hex(&format!("#{c}"))).unwrap_or(k.c.text_muted);
        labels = labels.child(row().w_full().gap(4.0).flex_wrap().children(p.labels.iter().map(|l| {
            let c = tint(l);
            row().key(format!("label-{l}")).h(20.0).px(8.0).items_center().radius(10.0).fill(k.alpha(c, 0.22)).border(1.0, k.alpha(c, 0.55))
                .child(text(l.clone()).font(k.ty.text_xsmall_medium).color(k.c.text_base))
        })));
    }
    let mut assignees = col().key("pr-assignees").w_full().gap(2.0).child(heading(k, "pr-assignees-title", "Assignees", Some(gear("pr-edit-assignees", Compose::Assignees, "Change the assignees"))));
    if pr.assignees.is_empty() {
        let mut none = row().w_full().gap(4.0).items_center().child(text("No one -").font(k.ty.text_small).color(k.c.text_faint));
        if open {
            none = none.child(Button::new("Assign yourself").variant(ButtonVariant::GhostMuted).control_size(ControlSize::Small).disabled(s.busy.is_some())
                .on_press(|s: &mut Collate| s.assign_me()).build(k).key("pr-assign-me").ml(-4.0));
        }
        assignees = assignees.child(none);
    }
    for a in &pr.assignees {
        assignees = assignees.child(row().key(format!("assignee-{a}")).w_full().h(28.0).gap(8.0).items_center()
            .child(crate::history_view::face(k, s, a, 20.0))
            .child(text(a.clone()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).truncate().min_w(0.0)));
    }
    let mut side = col().key("pr-side").w(220.0).gap(24.0).child(props).child(reviewers).child(assignees).child(labels);
    if let Some(m) = &pr.meta.milestone {
        side = side.child(col().key("pr-milestone").w_full().gap(2.0).child(heading(k, "pr-milestone-title", "Milestone", None))
            .child(text(m.clone()).font(k.ty.text_small).color(k.c.text_base)));
    }
    // Issues it closes when it merges, each opening on GitHub.
    if !pr.meta.issues.is_empty() {
        side = side.child(col().key("pr-issues").w_full().gap(2.0).child(heading(k, "pr-issues-title", "Development", None))
            .children(pr.meta.issues.iter().map(|(n, title, url)| {
                let url = url.clone();
                side_row(format!("issue-{n}"), text(format!("#{n}")).font(k.ty.text_small).color(k.c.text_muted), title.clone(), k.c.text_base)
                    .cursor(Cursor::Pointer).role("link").help("Open the issue on GitHub").on_press(move |_: &mut Collate| open_url(&url))
            })));
    }

    Some(col().key("pr-overview").w_full().grow(1.0).min_h(0.0).items_center().scroll(rune::ScrollSpec { stick_bottom: false })
        .child(row().key("pr-page").w_full().max_w(1100.0).px(EDGE * 2.0).pt(24.0).pb(48.0).gap(40.0).items_start().child(main)
            // Held at the top while the description and activity scroll past, as on GitHub.
            .child(side.sticky_top(24.0))))
}

/// A comment's body as GitHub renders it: its Markdown, the HTML GitHub draws as the same, and
/// each `<details>` a fold that opens on a press, shut unless written open.
fn body_view(s: &Collate, k: &Kit, key: &str, body: &str) -> El {
    rich(s, k, key, body, k.ty.text_small)
}

/// [`body_view`] at a given body size: the description reads at the page's size.
fn rich(s: &Collate, k: &Kit, key: &str, body: &str, font: rune::TextStyle) -> El {
    parts_view(s, k, key, &crate::body::parse(body.trim()), font)
}

fn parts_view(s: &Collate, k: &Kit, key: &str, parts: &[crate::body::Part], font: rune::TextStyle) -> El {
    use crate::body::Part;
    let mut c = col().key(key.to_owned()).w_full().gap(8.0);
    for (i, part) in parts.iter().enumerate() {
        let key = format!("{key}-{i}");
        c = c.child(match part {
            Part::Text(md) => {
                let style = MarkdownStyle::from_kit(k).body_font(font);
                Markdown::new(md.as_str()).style(style).on_link(|_: &mut Collate, url: &str| open_url(url)).build(k).key(key).w_full()
            }
            Part::Fold { summary, open, inner } => {
                // A press flips it from how it was written.
                let shown = *open != s.pulls.expanded.contains(&key);
                let flip = key.clone();
                let head = row().key("summary").w_full().gap(4.0).items_start().py(2.0).cursor(Cursor::Pointer).role("button")
                    .label(format!("{} {summary}", if shown { "Hide" } else { "Show" }))
                    .on_press(move |s: &mut Collate| if !s.pulls.expanded.remove(&flip) { s.pulls.expanded.insert(flip.clone()); })
                    .child(row().h(font.line_height.max(16.0)).items_center().child(icon(k.glyph.chevron_down, 14.0).color(k.c.icon_muted)
                        .icon_rotation(if shown { 0.0 } else { -std::f32::consts::FRAC_PI_2 }).motion(k.motion.control).a11y_hidden()))
                    .child(text(summary.clone()).font(rune::TextStyle { weight: 500.0, ..font }).color(k.c.text_base).wrap().grow(1.0).min_w(0.0));
                let mut fold = col().key(key.clone()).w_full().gap(6.0).child(head);
                if shown && !inner.is_empty() {
                    fold = fold.child(row().w_full().pl(6.0).gap(12.0).items_stretch()
                        .child(col().w(1.0).fill(k.c.border_weak))
                        .child(col().grow(1.0).min_w(0.0).child(parts_view(s, k, &format!("{key}-in"), inner, font))));
                }
                fold
            }
        });
    }
    c
}

fn post_view(s: &Collate, k: &Kit, key: &str, p: &github::Post) -> El {
    let verdict = match p.state.as_str() {
        "APPROVED" => " approved",
        "CHANGES_REQUESTED" => " requested changes",
        "COMMENTED" => " reviewed",
        "DISMISSED" => "'s review was dismissed",
        _ => "",
    };
    let mut c = col().key(key.to_owned()).w_full().gap(2.0)
        .child(row().w_full().gap(6.0).items_center()
            .child(crate::history_view::face(k, s, &p.author, 16.0))
            .child(text(format!("{}{verdict}", p.author)).font(k.ty.text_xsmall_medium).color(k.c.text_base))
            .child(text(p.time.get(..10).unwrap_or("").to_owned()).font(k.ty.text_small).color(k.c.text_faint)));
    if !p.body.trim().is_empty() {
        c = c.child(body_view(s, k, &format!("{key}-body"), &p.body));
    }
    c
}

/// A line thread of the open pull request, under its line: its comments, a reply, resolve.
pub fn thread_row(s: &Collate, k: &Kit, t: usize) -> El {
    let Some(pr) = &s.pulls.pr else { return row() };
    // Past the threads: the checks' annotations.
    let Some(th) = pr.threads.get(t) else { return annotation_row(k, pr, t - pr.threads.len()) };
    let resolve = Button::new(if th.resolved { "Unresolve" } else { "Resolve" }).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .disabled(s.busy.is_some()).on_press(move |s: &mut Collate| s.toggle_thread(t)).build(k).key("resolve");
    let reply = Button::new("Reply").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .on_press(move |s: &mut Collate| s.start_compose(Compose::Reply(t))).build(k).key("reply");
    let mut head = row().w_full().gap(8.0).items_center()
        .child(text(if th.resolved { "Resolved thread" } else { "Thread" }.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).grow(1.0));
    // A suggestion applies to the working tree, so only with the pull request's branch checked out.
    let pr_branch = s.pulls.pr.as_ref().is_some_and(|p| p.pull.head == s.branch);
    if !th.resolved && !th.left && th.line.is_some() && th.comments.iter().any(|c| github::suggestion(&c.body).is_some()) {
        let apply = Button::new("Apply Suggestion").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(!pr_branch)
            .help(if pr_branch { "Put the suggested lines in the file, to commit" } else { "Check out the pull request's branch first, from the ⋯ menu" })
            .on_press(move |s: &mut Collate| s.apply_suggestion(t)).build(k).key("apply");
        head = head.child(apply);
    }
    let head = head
        .child(reply)
        .child(resolve.mr(-GHOST_PAD));
    let mut bubble = col().key("bubble").w_full().max_w(720.0).px(12.0).py(8.0).gap(8.0).radius(k.radius.md)
        .border(1.0, k.c.border_weak).fill(k.alpha(k.c.text_base, if th.resolved { 0.02 } else { 0.05 })).child(head);
    // A resolved thread shows only where it started.
    let shown = if th.resolved { 1 } else { th.comments.len() };
    for (i, c) in th.comments.iter().take(shown).enumerate() {
        bubble = bubble.child(post_view(s, k, &format!("t{t}-c{i}"), c));
    }
    row().w_full().pl(96.0).pr(EDGE).py(6.0).role("note").label(format!("Review thread by {}", th.comments.first().map_or("", |c| c.author.as_str()))).child(bubble)
}

/// A check's annotation under its line, coloured by how much it matters.
fn annotation_row(k: &Kit, pr: &github::Detail, a: usize) -> El {
    let Some(a) = pr.annotations.as_ref().ok().and_then(|l| l.get(a)) else { return row() };
    let (what, hue) = match a.level {
        github::Level::Failure => ("Failure", k.c.danger.fg),
        github::Level::Warning => ("Warning", k.c.warning.icon),
        github::Level::Notice => ("Notice", k.c.info.icon),
    };
    let head = row().w_full().gap(8.0).items_center()
        .child(text(what.to_owned()).font(k.ty.text_xsmall_medium).color(hue))
        .child(text(a.check.clone()).font(k.ty.text_small).color(k.c.text_faint).truncate().min_w(0.0))
        .child(text(a.title.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0));
    let bubble = col().key("annotation").w_full().max_w(720.0).px(12.0).py(8.0).gap(4.0).radius(k.radius.md)
        .border(1.0, k.alpha(hue, 0.5)).fill(k.alpha(hue, 0.06))
        .child(head)
        .child(text(a.message.trim().to_owned()).font(k.ty.text_small).color(k.c.text_base).wrap().w_full());
    row().w_full().pl(96.0).pr(EDGE).py(6.0).role("note").label(format!("{what} from {}: {}", a.check, a.message)).child(bubble)
}

/// The dialog for writing a review, a comment, a reply or a new pull request.
pub fn compose_dialog(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let Some(c) = s.pulls.compose else { return };
    let pending = s.pending().len();
    let n = s.pulls.pr.as_ref().map_or(0, |p| p.pull.number);
    let (title, what, button, placeholder) = match c {
        Compose::Review(Verdict::Approve) => (format!("Approve #{n}"), "A summary is optional.".to_owned(), "Approve", "Leave a summary (optional)"),
        Compose::Review(Verdict::RequestChanges) => (format!("Request changes on #{n}"), "Say what needs to change.".to_owned(), "Request Changes", "What needs to change"),
        Compose::Review(Verdict::Comment) => (format!("Review #{n}"), "Comments without approving or blocking.".to_owned(), "Send Review", "Leave a summary"),
        Compose::Comment => (format!("Comment on #{n}"), "Added to the conversation.".to_owned(), "Comment", "Write a comment"),
        Compose::Reply(_) => ("Reply".to_owned(), "Added to the thread.".to_owned(), "Reply", "Write a reply"),
        Compose::Create => (
            format!("New pull request into {}", s.base.as_deref().unwrap_or("the base")),
            format!("From {}. A branch that isn't pushed yet is pushed first.", s.branch),
            "Open Pull Request",
            "Describe the change (optional)",
        ),
        Compose::Edit => (format!("Edit #{n}"), "Saved to GitHub.".to_owned(), "Save", "Description"),
        Compose::Reviewers => (format!("Reviewers of #{n}"), "Tick people to ask, or type logins and teams (org/team), separated by commas. Removing one takes back the request.".to_owned(), "Save", ""),
        Compose::Labels => (format!("Labels of #{n}"), "Names of the repository's labels, separated by commas.".to_owned(), "Save", ""),
        Compose::Assignees => (format!("Assignees of #{n}"), "Tick people, or type logins separated by commas.".to_owned(), "Save", ""),
        Compose::EditPost(_) => ("Edit comment".to_owned(), "Saved to GitHub.".to_owned(), "Save", "Write a comment"),
        Compose::DeletePost(_) => ("Delete this comment?".to_owned(), "It's removed from the pull request for everyone, and can't be brought back.".to_owned(), "Delete", ""),
    };
    // A list is one line, edited in the title field.
    let list = matches!(c, Compose::Reviewers | Compose::Labels | Compose::Assignees);
    let held = s.held();
    let what = match c {
        Compose::Review(_) if pending > 0 => format!("{what} Your {pending} line comment{} go with it.", if pending == 1 { "" } else { "s" }),
        _ => what,
    };
    let what = match c {
        Compose::Review(_) if held > 0 && !s.pr_whole() => format!("{what} {held} on part of the pull request stay on this Mac; show all its changes to send them."),
        Compose::Review(_) if held > 0 => format!("{what} {held} outside GitHub's diff stay on this Mac."),
        _ => what,
    };
    let area = TextArea::new(&s.pulls.text).placeholder(placeholder.to_owned()).label(placeholder.to_owned()).rows(6).max_height(180.0)
        .on_edit(|s: &mut Collate| &mut s.pulls.text).width(None).build(k).key("compose-text").w_full();
    let empty = s.pulls.text.text.trim().is_empty();
    // An approval needs no words, a review with line comments has them already, and a new
    // pull request needs only its title.
    let blocked = s.busy.is_some()
        || match c {
            Compose::Review(Verdict::Approve) => false,
            Compose::Review(Verdict::Comment) => pending == 0 && empty,
            Compose::Create | Compose::Edit => s.pulls.title.text.trim().is_empty(),
            Compose::Reviewers | Compose::Labels | Compose::Assignees | Compose::DeletePost(_) => false,
            _ => empty,
        };
    let mut body = col().key("compose-body").w_full().gap(12.0);
    if matches!(c, Compose::Create | Compose::Edit) || list {
        let hint = match c {
            Compose::Reviewers => "ana, core/web",
            Compose::Assignees => "ana, bo",
            Compose::Labels => "bug, needs review",
            _ => "Title",
        };
        body = body.child(TextInput::new(&s.pulls.title).placeholder(hint).on_edit(|s: &mut Collate| &mut s.pulls.title).width(None).build(k).key("compose-title").w_full());
    }
    if matches!(c, Compose::Reviewers | Compose::Assignees) {
        body = body.child(people_picker(s, k, c == Compose::Reviewers));
    }
    if !list && !matches!(c, Compose::DeletePost(_)) {
        body = body.child(area);
    }
    let mut buttons = row().key("compose-buttons").w_full().gap(8.0).items_center();
    if c == Compose::Create {
        buttons = buttons.child(Checkbox::new(s.pulls.draft).label("Draft").on_toggle(|s: &mut Collate| s.pulls.draft ^= true).build(k).key("compose-draft"));
    }
    buttons = buttons.child(row().grow(1.0))
        .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.pulls.compose = None).build(k))
        .child(Button::new(button).variant(if matches!(c, Compose::DeletePost(_)) { ButtonVariant::Danger } else { ButtonVariant::Contrast }).disabled(blocked).on_press(|s: &mut Collate| s.send_compose()).build(k).key("compose-ok"));
    crate::described_dialog(k, "compose", title, what, body.child(buttons)).width(520.0).on_close(|s: &mut Collate| s.pulls.compose = None).build(k, layer);
}

/// The reviewers or assignees editor's people to tick: those on it already, then everyone else
/// in the repository. The author can't review their own pull request, so isn't offered then.
fn people_picker(s: &Collate, k: &Kit, reviewers: bool) -> El {
    let author = if reviewers { s.pulls.pr.as_ref().map_or("", |p| p.pull.author.as_str()) } else { "" };
    let picked = names(&s.pulls.title.text);
    let mut people: Vec<String> = s.pulls.pr.as_ref().map(|p| if reviewers { p.reviewers.iter().map(|(w, _)| w.clone()).collect() } else { p.assignees.clone() }).unwrap_or_default();
    let note = match &s.pulls.people {
        None => Some("Reading who's in the repository…".to_owned()),
        Some(Err(e)) => Some(format!("Couldn't read who's in the repository: {e}")),
        Some(Ok(all)) => {
            people.extend(all.iter().filter(|p| !people.contains(p)).cloned().collect::<Vec<_>>());
            None
        }
    };
    people.retain(|p| p != author);
    let note = match note {
        None if people.is_empty() => Some("No one else can be picked here: GitHub only takes the repository's collaborators, added in its settings on GitHub.".to_owned()),
        note => note,
    };
    let rows = col().key("reviewer-people").w_full().max_h(220.0).scroll(rune::ScrollSpec { stick_bottom: false }).gap(2.0).children(people.into_iter().map(|who| {
        let on = picked.contains(&who);
        let key = format!("pick-{who}");
        Checkbox::new(on).label(who.clone()).on_toggle(move |s: &mut Collate| s.toggle_person(&who)).build(k).key(key)
    }));
    let mut out = col().key("reviewer-picker").w_full().gap(8.0).child(rows);
    if let Some(note) = note {
        out = out.child(text(note).key("reviewer-note").font(k.ty.text_small).color(k.c.text_muted).wrap());
    }
    out
}

/// A line of `git range-diff`: a pair of commits and whether the second kept the first's
/// change, or a line of how a changed one differs (by its outer `+` or `-`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rd {
    Same,
    Changed,
    Dropped,
    Added,
    Plus,
    Minus,
    Context,
}

pub fn range_diff_lines(out: &str) -> Vec<(Rd, &str)> {
    out.lines()
        .map(|l| {
            let kind = match l.strip_prefix("    ") {
                Some(inner) if inner.starts_with('+') => Rd::Plus,
                Some(inner) if inner.starts_with('-') => Rd::Minus,
                Some(_) => Rd::Context,
                // `1:  a1b2c3d ! 1:  d4e5f6a Subject`: the third word says what happened.
                None => match l.split_whitespace().nth(2) {
                    Some("=") => Rd::Same,
                    Some("!") => Rd::Changed,
                    Some("<") => Rd::Dropped,
                    Some(">") => Rd::Added,
                    _ => Rd::Context,
                },
            };
            (kind, l)
        })
        .collect()
}

/// How many commits a range-diff kept, changed, dropped and added, in words.
fn range_diff_summary(lines: &[(Rd, &str)]) -> String {
    let n = |k: Rd| lines.iter().filter(|(x, _)| *x == k).count();
    let parts: Vec<String> = [(Rd::Same, "unchanged"), (Rd::Changed, "changed"), (Rd::Dropped, "dropped"), (Rd::Added, "added")]
        .iter()
        .filter(|(k, _)| n(*k) > 0)
        .map(|(k, what)| format!("{} {what}", n(*k)))
        .collect();
    format!("Since your review: {}.", parts.join(", "))
}

/// What a force-push changed since the reader's review: each commit then against now, and
/// how a changed one's change differs.
pub fn rewrite_dialog(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let Some(out) = &s.pulls.rewrite else { return };
    // A long rewrite would be thousands of elements; past this it's read in a terminal.
    const MAX: usize = 1500;
    let lines = range_diff_lines(out);
    let mono = rune::TextStyle { size: 12.0, line_height: 18.0, ..k.ty.mono };
    let mut list = col().key("rewrite-lines").w_full();
    for (i, (kind, l)) in lines.iter().take(MAX).enumerate() {
        let color = match kind {
            Rd::Same => k.c.text_faint,
            Rd::Changed => k.c.text_base,
            Rd::Dropped | Rd::Minus => k.c.danger.fg,
            Rd::Added | Rd::Plus => k.c.success.fg,
            Rd::Context => k.c.text_muted,
        };
        list = list.child(text((*l).to_owned()).key(format!("rewrite-{i}")).font(mono).color(color).truncate().min_w(0.0));
    }
    let mut body = col().key("rewrite-body").w_full().gap(8.0).child(col().w_full().max_h(480.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(list));
    if lines.len() > MAX {
        body = body.child(text(format!("The first {MAX} of {} lines.", lines.len())).font(k.ty.text_small).color(k.c.text_muted));
    }
    body = body.child(row().w_full().child(row().grow(1.0))
        .child(Button::new("Done").variant(ButtonVariant::Contrast).on_press(|s: &mut Collate| s.pulls.rewrite = None).build(k).key("rewrite-done")));
    Dialog::new("rewrite", "What the force-push changed").open(true).width(760.0).fit().description(range_diff_summary(&lines))
        .on_close(|s: &mut Collate| s.pulls.rewrite = None).child(body).build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::{Rd, names, range_diff_lines, range_diff_summary};

    #[test]
    fn ticking_a_reviewer_adds_or_removes_their_login() {
        let mut s = crate::init();
        s.pulls.title = rune::TextEdit::new("ana, core/web");
        s.toggle_person("bo");
        assert_eq!(names(&s.pulls.title.text), ["ana", "core/web", "bo"]);
        s.toggle_person("ana");
        assert_eq!(s.pulls.title.text, "core/web, bo");
    }

    #[test]
    fn a_range_diff_reads_as_kept_changed_dropped_and_added_commits() {
        let out = "1:  a1b2c3d = 1:  d4e5f6a First\n2:  1111111 ! 2:  2222222 Second\n    @@ src/a.rs\n     fn a() {\n    -    old();\n    +    new();\n3:  3333333 < -:  ------- Dropped\n-:  ------- > 3:  4444444 Added\n";
        let kinds: Vec<Rd> = range_diff_lines(out).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, [Rd::Same, Rd::Changed, Rd::Context, Rd::Context, Rd::Minus, Rd::Plus, Rd::Dropped, Rd::Added]);
        assert_eq!(range_diff_summary(&range_diff_lines(out)), "Since your review: 1 unchanged, 1 changed, 1 dropped, 1 added.");
    }

    #[test]
    fn only_new_review_requests_notify_and_never_the_first_time() {
        let pull = |number: u32, requested: &[&str]| crate::github::Pull {
            number, title: String::new(), author: "ana".into(), head: String::new(), base: String::new(), state: crate::github::PrState::Open, draft: false, decision: String::new(),
            requested: requested.iter().map(|r| (*r).to_owned()).collect(), additions: 0, deletions: 0, updated: String::new(), labels: vec![], checks: vec![], ci: None, url: String::new(),
        };
        let first = [pull(1, &["me"]), pull(2, &["bo"])];
        let (seen, fresh) = super::fresh_requests(None, &first, "me");
        assert!(fresh.is_empty() && seen.contains(&1));
        let later = [pull(1, &["me"]), pull(2, &["bo"]), pull(3, &["me", "bo"])];
        let (_, fresh) = super::fresh_requests(Some(&seen), &later, "me");
        assert_eq!(fresh.iter().map(|p| p.number).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn a_list_edit_adds_and_removes_only_what_changed() {
        let was = super::names("ana, @bo");
        assert_eq!(was, ["ana", "bo"]);
        let (add, remove) = super::changes(&was, &super::names("bo core/web"));
        assert_eq!((add, remove), (vec!["core/web".to_owned()], vec!["ana".to_owned()]));
    }

    #[test]
    fn the_filter_wants_every_word_somewhere() {
        let p = crate::github::Pull {
            number: 42, title: "Add billing".into(), author: "ana".into(), head: "billing".into(), base: "main".into(), state: crate::github::PrState::Open, draft: false,
            decision: String::new(), requested: vec![], additions: 0, deletions: 0, updated: String::new(), labels: vec!["db".into()], checks: vec![], ci: None, url: String::new(),
        };
        let w = |s: &str| s.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        assert!(super::matches(&p, &w("")));
        assert!(super::matches(&p, &w("BILL ana")));
        assert!(super::matches(&p, &w("#42 db")));
        assert!(!super::matches(&p, &w("billing bo")));
    }
}

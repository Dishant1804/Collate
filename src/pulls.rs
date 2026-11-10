//! The Pull Requests place: the repository's pull requests, the open ones grouped by what they
//! want of the reader and then the merged and closed, and one pull request under review. Its diff is a range in History; its details,
//! checks, conversation and actions take the staging panel's top; its line threads sit under
//! their lines with the reader's own comments, which a review sends as its line comments.

use std::collections::HashSet;

use rune::{Cursor, El, Icon, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, IconButton, IconButtonVariant, Kit, Markdown, MarkdownStyle, TextArea, TextInput};

use crate::github::{self, CheckState, Detail, Draft, Gh, Method, Pull, Verdict};
use crate::{COLUMN_TOP, Collate, EDGE, GHOST_PAD, HANDLE, MORE_ICON, PANEL_INSET, REFRESH_ICON, current_repo, diff, message, open_url};

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
}

/// The names in a comma- or space-separated list, without an `@`.
fn names(list: &str) -> Vec<String> {
    list.split([',', ' ']).map(|n| n.trim().trim_start_matches('@').to_owned()).filter(|n| !n.is_empty()).collect()
}

/// What to add to and remove from `was` to make it `now`.
fn changes(was: &[String], now: &[String]) -> (Vec<String>, Vec<String>) {
    (now.iter().filter(|n| !was.contains(n)).cloned().collect(), was.iter().filter(|w| !now.contains(w)).cloned().collect())
}

pub struct PullsState {
    /// The list shows in place of the diff.
    pub open: bool,
    /// The signed-in login and the open pull requests, once read.
    pub list: Option<Result<(String, Vec<Pull>), String>>,
    pub loading: bool,
    /// The pull request being reviewed.
    pub pr: Option<Detail>,
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
}

impl Default for PullsState {
    fn default() -> PullsState {
        PullsState {
            open: false,
            list: None,
            loading: false,
            pr: None,
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
        }
    }
}

impl PullsState {
    /// Where each line thread of the open pull request goes in the diff: by its line and that
    /// line's text, as a comment is. An outdated thread has no line, and an anchor no line matches.
    pub fn anchors(&self) -> Vec<diff::Anchor> {
        let Some(pr) = &self.pr else { return Vec::new() };
        pr.threads
            .iter()
            .map(|t| diff::Anchor {
                path: t.path.clone(),
                side: if t.left { diff::Side::Old } else { diff::Side::New },
                line: t.line.unwrap_or(0),
                text: if t.line.is_some() { t.text.clone() } else { "\0outdated".into() },
            })
            .collect()
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

/// Read the open pull requests again every few minutes, so a new review request notifies
/// and the sidebar's count stays true.
pub fn watch_reviews() {
    let Some(app) = HANDLE.get().cloned() else { return };
    std::thread::spawn(move || loop {
        std::thread::sleep(REVIEWS_EVERY);
        app.update(|s, _| {
            if s.settings.notify_reviews && s.has_project {
                s.load_pulls();
            }
        });
    });
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

    /// Show the list, reading it if it isn't yet.
    pub fn open_pulls(&mut self) {
        self.map.open = false;
        self.pulls.open = true;
        if self.pulls.list.is_none() {
            self.load_pulls();
        }
    }

    pub fn load_pulls(&mut self) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        if std::mem::replace(&mut self.pulls.loading, true) {
            return;
        }
        std::thread::spawn(move || {
            let got = Gh::new(&root).list();
            app.update(move |s, _| {
                // The reader switched projects meanwhile: this list was the last one's.
                if current_repo().is_none_or(|r| r.root != root) {
                    return;
                }
                s.pulls.loading = false;
                s.pulls.list = Some(got);
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
        std::thread::spawn(move || {
            let got = gh.detail(n).and_then(|mut d| Ok((gh.fetch(&mut d)?, d)));
            app.update(move |s, _| {
                s.pulls.fetching = None;
                match got {
                    Ok((range, d)) => {
                        s.pulls.open = false;
                        s.open_commit(&range);
                        s.compared = Some((d.pull.base.clone(), d.pull.head.clone()));
                        s.pulls.pr = Some(d);
                        s.place_comments();
                    }
                    Err(e) => s.github_failed(e),
                }
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
        if let Some(r) = current_repo() {
            self.branches = r.branches();
        }
        crate::refresh();
    }

    /// The reader's comments on lines of the open pull request's diff that GitHub takes: a
    /// review sends them.
    pub fn pending(&self) -> Vec<(usize, Draft)> {
        let Some(pr) = &self.pulls.pr else { return Vec::new() };
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
            Compose::Reviewers => self.pulls.title = TextEdit::new(pr.map(|p| p.reviewers.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()),
            Compose::Labels => self.pulls.title = TextEdit::new(pr.map(|p| p.pull.labels.join(", ")).unwrap_or_default()),
            _ => {}
        }
        if c == Compose::Create {
            // The branch's last commit is the likeliest title.
            let subject = current_repo().and_then(|r| r.last_message()).and_then(|m| m.lines().next().map(str::to_owned)).unwrap_or_default();
            self.pulls.title = TextEdit::new(&subject);
            self.pulls.draft = false;
        }
    }

    fn send_compose(&mut self) {
        let Some(c) = self.pulls.compose.take() else { return };
        let body = self.pulls.text.text.trim().to_owned();
        let n = self.pulls.pr.as_ref().map_or(0, |p| p.pull.number);
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
                self.pr_act("Sending the review…", move |gh| gh.review(n, v, &body, &drafts).map(|_| format!("{what} #{n}{note}")), move |s| {
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
            Compose::Reviewers | Compose::Labels => {
                let Some(pr) = &self.pulls.pr else { return };
                let (what, was) = if c == Compose::Reviewers {
                    ("reviewer", pr.reviewers.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>())
                } else {
                    ("label", pr.pull.labels.clone())
                };
                let (add, remove) = changes(&was, &names(&self.pulls.title.text));
                let done = if c == Compose::Reviewers { "Updated the reviewers" } else { "Updated the labels" };
                self.pr_act("Saving…", move |gh| gh.people(n, what, &add, &remove).map(|_| done.to_owned()), |_| {});
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
        let Some(n) = self.pulls.pr.as_ref().map(|p| p.pull.number) else { return };
        let mut menu = PopupMenu::new();
        for m in Method::ALL {
            menu = menu.item(PopupItem::new(m.title(), m.title()));
        }
        tx.popup_menu(menu, "pr-merge", move |s: &mut Collate, picked, _| {
            let Some(m) = Method::ALL.into_iter().find(|m| Some(m.title()) == picked) else { return };
            let delete = s.pulls.delete_branch;
            s.pr_act("Merging…", move |gh| gh.merge(n, m, delete).map(|_| format!("Merged #{n}")), Collate::reread);
        });
    }

    fn pr_menu(&mut self, tx: &mut Tx) {
        let Some(pr) = &self.pulls.pr else { return };
        let (n, url, open, draft) = (pr.pull.number, pr.pull.url.clone(), pr.state == "OPEN", pr.pull.draft);
        let menu = PopupMenu::new()
            .item(PopupItem::new("checkout", "Check Out Branch"))
            .item(PopupItem::new("web", "Open on GitHub"))
            .item(PopupItem::new("reload", "Reload"))
            .separator()
            .item(PopupItem::new("edit", "Edit Title and Description…").enabled(open))
            .item(PopupItem::new("reviewers", "Reviewers…").enabled(open))
            .item(PopupItem::new("labels", "Labels…").enabled(open))
            .separator()
            .item(PopupItem::new("update", "Update Branch with Base").enabled(open))
            .item(PopupItem::new("update-rebase", "Update Branch by Rebasing").enabled(open))
            .separator()
            .item(PopupItem::new("ready", if draft { "Ready for Review" } else { "Convert to Draft" }).enabled(open))
            .item(PopupItem::new("state", if open { "Close Pull Request" } else { "Reopen Pull Request" }));
        tx.popup_menu(menu, "pr-more", move |s: &mut Collate, picked, _| match picked {
            Some("checkout") => s.pr_act("Checking out…", move |gh| gh.checkout(n).map(|_| format!("Checked out #{n}")), Collate::reread),
            Some("web") => open_url(&url),
            Some("reload") => s.open_pull(n),
            Some("edit") => s.start_compose(Compose::Edit),
            Some("reviewers") => s.start_compose(Compose::Reviewers),
            Some("labels") => s.start_compose(Compose::Labels),
            Some("update") => s.update_branch(false),
            Some("update-rebase") => s.update_branch(true),
            Some("ready") if draft => s.pr_act("Marking ready…", move |gh| gh.set(n, &["ready"]).map(|_| format!("#{n} is ready for review")), |_| {}),
            Some("ready") => s.pr_act("Converting…", move |gh| gh.set(n, &["ready", "--undo"]).map(|_| format!("#{n} is a draft")), |_| {}),
            Some("state") if open => s.pr_act("Closing…", move |gh| gh.set(n, &["close"]).map(|_| format!("Closed #{n}")), |_| {}),
            Some("state") => s.pr_act("Reopening…", move |gh| gh.set(n, &["reopen"]).map(|_| format!("Reopened #{n}")), |_| {}),
            _ => {}
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

fn decision(k: &Kit, p: &Pull) -> (&'static str, rune::Color) {
    match p.decision.as_str() {
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
                list = list.child(row().key(format!("group-{title}")).w_full().h(28.0).px(EDGE).gap(6.0).items_center().mt(8.0)
                    .child(text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted))
                    .child(text(group.len().to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint)));
                for p in group {
                    let n = p.number;
                    let fetching = s.pulls.fetching == Some(n);
                    let (verdict, tint) = decision(k, p);
                    let mut words = col().grow(1.0).min_w(0.0).gap(2.0)
                        .child(text(p.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate())
                        .child(text(format!("#{n} - {} - {} → {} - {}", p.author, p.head, p.base, p.updated.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).truncate());
                    if !p.labels.is_empty() {
                        words = words.child(text(p.labels.join(", ")).font(k.ty.text_small).color(k.c.text_faint).truncate());
                    }
                    list = list.child(row().key(format!("pull-{n}")).w_full().px(EDGE).py(8.0).gap(12.0).items_center()
                        .hover(k.c.hover).cursor(Cursor::Pointer).role("button").label(format!("#{n} {}", p.title)).help("Review this pull request")
                        .on_press(move |s: &mut Collate| s.open_pull(n))
                        .child(dot(k, github::overall(&p.checks)))
                        .child(words)
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

/// The open pull request at the top of the staging panel: what it is, where it stands, its
/// checks, reviewers and conversation, and what can be done with it.
pub fn pr_card(s: &Collate, k: &Kit) -> Option<El> {
    let pr = s.pulls.pr.as_ref()?;
    let p = &pr.pull;
    let open = pr.state == "OPEN";
    let section = |key: &str, title: &str| text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_faint).key(key.to_owned()).pt(4.0);
    let (ready, can_merge) = readiness(pr);
    let more = IconButton::new(&MORE_ICON).variant(IconButtonVariant::GhostMuted).label("More actions for this pull request").help("Check out, open on GitHub, draft, close")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.pr_menu(tx)).build(k).key("pr-more");
    let mut card = col().key("pr-card").w_full().px(PANEL_INSET).pt(4.0).pb(10.0).gap(6.0)
        .child(row().w_full().gap(4.0).items_start()
            .child(text(p.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).wrap().grow(1.0).min_w(0.0))
            .child(more.mr(-4.0)))
        .child(text(format!("#{} by {} - {} → {}", p.number, p.author, p.head, p.base)).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full())
        .child({
            let mut r = row().key("pr-ready").w_full().gap(6.0).items_center()
                .child(dot(k, Some(if can_merge { CheckState::Passed } else if open { CheckState::Pending } else { CheckState::Skipped })))
                .child(text(ready.to_owned()).font(k.ty.text_small).color(k.c.text_base).grow(1.0));
            // Behind its base: the one-click fix GitHub offers, a merge of the base into it.
            if open && pr.merge_state == "BEHIND" {
                r = r.child(Button::new("Update Branch").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(s.busy.is_some())
                    .help("Merge the base's new commits into this branch, on GitHub").on_press(|s: &mut Collate| s.update_branch(false)).build(k).key("pr-update").mr(-GHOST_PAD));
            }
            r
        });
    if open {
        let pending = s.pending().len();
        let verdict = |key: &str, label: &str, v: Verdict| Button::new(label.to_owned()).variant(ButtonVariant::Outline).control_size(ControlSize::Small)
            .disabled(s.busy.is_some()).on_press(move |s: &mut Collate| s.start_compose(Compose::Review(v))).build(k).key(key.to_owned());
        card = card.child(row().key("pr-review").w_full().gap(6.0).flex_wrap()
            .child(verdict("pr-approve", "Approve", Verdict::Approve))
            .child(verdict("pr-changes", "Request Changes", Verdict::RequestChanges))
            .child(verdict("pr-comment-review", if pending > 0 { "Send Comments" } else { "Comment" }, Verdict::Comment)));
        if pending > 0 {
            card = card.child(text(format!("{pending} line comment{} go with your review", if pending == 1 { "" } else { "s" })).font(k.ty.text_small).color(k.c.text_muted));
        }
        let held = s.held();
        if held > 0 {
            card = card.child(text(format!("{held} comment{} outside GitHub's diff stay on this Mac", if held == 1 { "" } else { "s" })).font(k.ty.text_small).color(k.c.warning.icon).wrap().w_full());
        }
        let merge = Button::new("Merge…").variant(ButtonVariant::Contrast).control_size(ControlSize::Small).disabled(!can_merge || s.busy.is_some())
            .help("Squash, merge or rebase into the base").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.merge_menu(tx)).build(k).key("pr-merge");
        let delete = Checkbox::new(s.pulls.delete_branch).label("Delete branch").on_toggle(|s: &mut Collate| s.pulls.delete_branch ^= true).build(k).key("pr-delete-branch");
        card = card.child(row().key("pr-merge-row").w_full().gap(10.0).items_center().child(merge).child(delete));
    }
    if !p.checks.is_empty() {
        let failed = p.checks.iter().filter(|c| c.state == CheckState::Failed).count();
        let passed = p.checks.iter().filter(|c| c.state == CheckState::Passed).count();
        let title = section("pr-checks-title", &format!("Checks - {passed} passed{}", if failed > 0 { format!(", {failed} failed") } else { String::new() }));
        // Failed GitHub Actions runs, each once: those can be run again from here.
        let mut runs: Vec<u64> = p.checks.iter().filter(|c| c.state == CheckState::Failed).filter_map(github::Check::run_id).collect();
        runs.sort_unstable();
        runs.dedup();
        if runs.is_empty() || !open {
            card = card.child(title);
        } else {
            let n = p.number;
            let rerun = Button::new("Re-run Failed").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(s.busy.is_some())
                .help("Run the failed GitHub Actions jobs again").on_press(move |s: &mut Collate| {
                    let runs = runs.clone();
                    s.pr_act("Re-running…", move |gh| gh.rerun_failed(&runs).map(|_| format!("Re-running the failed checks of #{n}")), |_| {});
                }).build(k).key("pr-rerun");
            card = card.child(row().w_full().items_center().child(title.grow(1.0)).child(rerun.mr(-GHOST_PAD)));
        }
        let mut checks: Vec<&github::Check> = p.checks.iter().collect();
        // Failures first: they're what needs looking at.
        checks.sort_by_key(|c| match c.state {
            CheckState::Failed => 0,
            CheckState::Pending => 1,
            CheckState::Passed => 2,
            CheckState::Skipped => 3,
        });
        for (i, c) in checks.into_iter().enumerate() {
            let url = c.url.clone();
            let mut r = row().key(format!("check-{i}")).w_full().gap(6.0).items_center().child(dot(k, Some(c.state)))
                .child(text(c.name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0));
            if !url.is_empty() {
                r = r.cursor(Cursor::Pointer).role("link").label(format!("Open {}", c.name)).on_press(move |_: &mut Collate| open_url(&url));
            }
            card = card.child(r);
        }
    }
    if !pr.reviewers.is_empty() {
        card = card.child(section("pr-reviewers-title", "Reviewers"));
        for (who, state) in &pr.reviewers {
            let (label, tint) = match state.as_str() {
                "APPROVED" => ("Approved", k.c.success.icon),
                "CHANGES_REQUESTED" => ("Changes requested", k.c.danger.icon),
                "REQUESTED" => ("Asked", k.c.text_faint),
                _ => ("Commented", k.c.text_muted),
            };
            card = card.child(row().key(format!("reviewer-{who}")).w_full().gap(6.0)
                .child(text(who.clone()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).truncate().min_w(0.0))
                .child(text(label.to_owned()).font(k.ty.text_small).color(tint)));
        }
    }
    if !p.labels.is_empty() {
        card = card.child(section("pr-labels-title", "Labels")).child(text(p.labels.join(", ")).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full());
    }
    if !pr.body.trim().is_empty() {
        card = card.child(section("pr-body-title", "Description")).child(body_view(s, k, "pr-body", &pr.body));
    }
    let open_threads = pr.threads.iter().filter(|t| !t.resolved).count();
    card = card.child(section("pr-talk-title", &format!("Conversation - {} comment{}, {open_threads} open thread{}",
        pr.conversation.len(), if pr.conversation.len() == 1 { "" } else { "s" }, if open_threads == 1 { "" } else { "s" })));
    for (i, post) in pr.conversation.iter().enumerate() {
        card = card.child(post_view(s, k, &format!("post-{i}"), post));
    }
    if open {
        card = card.child(row().w_full().child(Button::new("Add a Comment").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .on_press(|s: &mut Collate| s.start_compose(Compose::Comment)).build(k).key("pr-add-comment").ml(-GHOST_PAD)));
    }
    // The panel's file list follows; a long conversation scrolls here rather than pushing it away.
    Some(col().key("pr-scroll").w_full().max_h(440.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(card))
}

/// The part of a long comment shown before "Show all": up to its first `<details>` (where bots
/// fold their extras) or about 600 characters, cut at a line end. `None` when it's short.
fn preview(body: &str) -> Option<&str> {
    const SHORT: usize = 600;
    let cut = body.find("<details>").unwrap_or(body.len());
    if cut >= body.len() && body.len() <= SHORT {
        return None;
    }
    let mut end = cut.min(SHORT);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let end = body[..end].rfind('\n').filter(|&n| n > 0).unwrap_or(end);
    Some(&body[..end])
}

/// A comment's body as GitHub renders it: Markdown, with HTML (comments, images, folds) left
/// out, and a long one folded to its start until asked.
fn body_view(s: &Collate, k: &Kit, key: &str, body: &str) -> El {
    let body = body.trim();
    let open = s.pulls.expanded.contains(key);
    let short = if open { None } else { preview(body) };
    let style = MarkdownStyle::from_kit(k).body_font(k.ty.text_small);
    let md = Markdown::new(short.unwrap_or(body)).style(style).on_link(|_: &mut Collate, url: &str| open_url(url)).build(k).key(format!("{key}-md")).w_full();
    let mut c = col().key(key.to_owned()).w_full().gap(2.0).child(md);
    if short.is_some() || (open && preview(body).is_some()) {
        let key = key.to_owned();
        c = c.child(row().w_full().child(Button::new(if open { "Show Less" } else { "Show All" }).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .on_press(move |s: &mut Collate| if !s.pulls.expanded.remove(&key) { s.pulls.expanded.insert(key.clone()); }).build(k).key("more").ml(-GHOST_PAD)));
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
        .child(row().w_full().gap(6.0)
            .child(text(format!("{}{verdict}", p.author)).font(k.ty.text_xsmall_medium).color(k.c.text_base))
            .child(text(p.time.get(..10).unwrap_or("").to_owned()).font(k.ty.text_small).color(k.c.text_faint)));
    if !p.body.trim().is_empty() {
        c = c.child(body_view(s, k, &format!("{key}-body"), &p.body));
    }
    c
}

/// A line thread of the open pull request, under its line: its comments, a reply, resolve.
pub fn thread_row(s: &Collate, k: &Kit, t: usize) -> El {
    let Some(th) = s.pulls.pr.as_ref().and_then(|p| p.threads.get(t)) else { return row() };
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
        Compose::Reviewers => (format!("Reviewers of #{n}"), "Logins or teams (org/team), separated by commas. Removing one takes back the request.".to_owned(), "Save", ""),
        Compose::Labels => (format!("Labels of #{n}"), "Names of the repository's labels, separated by commas.".to_owned(), "Save", ""),
    };
    // A list is one line, edited in the title field.
    let list = matches!(c, Compose::Reviewers | Compose::Labels);
    let held = s.held();
    let what = match c {
        Compose::Review(_) if pending > 0 => format!("{what} Your {pending} line comment{} go with it.", if pending == 1 { "" } else { "s" }),
        _ => what,
    };
    let what = match c {
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
            Compose::Reviewers | Compose::Labels => false,
            _ => empty,
        };
    let mut body = col().key("compose-body").w_full().gap(12.0);
    if matches!(c, Compose::Create | Compose::Edit) || list {
        let hint = match c {
            Compose::Reviewers => "ana, core/web",
            Compose::Labels => "bug, needs review",
            _ => "Title",
        };
        body = body.child(TextInput::new(&s.pulls.title).placeholder(hint).on_edit(|s: &mut Collate| &mut s.pulls.title).width(None).build(k).key("compose-title").w_full());
    }
    if !list {
        body = body.child(area);
    }
    let mut buttons = row().key("compose-buttons").w_full().gap(8.0).items_center();
    if c == Compose::Create {
        buttons = buttons.child(Checkbox::new(s.pulls.draft).label("Draft").on_toggle(|s: &mut Collate| s.pulls.draft ^= true).build(k).key("compose-draft"));
    }
    buttons = buttons.child(row().grow(1.0))
        .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.pulls.compose = None).build(k))
        .child(Button::new(button).variant(ButtonVariant::Contrast).disabled(blocked).on_press(|s: &mut Collate| s.send_compose()).build(k).key("compose-ok"));
    Dialog::new("compose", title).open(true).width(520.0).fit().description(what).on_close(|s: &mut Collate| s.pulls.compose = None)
        .child(body.child(buttons)).build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::preview;

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

    #[test]
    fn long_comments_fold_before_their_details_or_at_a_line_end() {
        assert_eq!(preview("Looks good"), None);
        assert_eq!(preview("Rename this.\n\n<details>\n<summary>Prompt</summary>\nlong\n</details>"), Some("Rename this.\n"));
        let long = "word ".repeat(100) + "\n" + &"more ".repeat(100);
        let cut = preview(&long).expect("folded");
        assert!(cut.len() <= 600 && !cut.contains("more"));
    }
}

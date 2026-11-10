//! The Pull Requests place: the repository's open pull requests, grouped by what they want of
//! the reader, and one pull request under review. Its diff is a range in History; its details,
//! checks, conversation and actions take the staging panel's top; its line threads sit under
//! their lines with the reader's own comments, which a review sends as its line comments.

use std::collections::HashSet;

use rune::{Cursor, El, PopupItem, PopupMenu, TextEdit, Tx, col, row, text};
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
            Some(Ok((me, pulls))) => pulls.iter().filter(|p| p.requested.iter().any(|r| r == me)).count(),
            _ => 0,
        }
    }
}

fn gh() -> Option<Gh> {
    current_repo().map(|r| Gh::new(&r.root))
}

impl Collate {
    /// Show the list, reading it if it isn't yet.
    pub fn open_pulls(&mut self) {
        self.map.open = false;
        self.pulls.open = true;
        if self.pulls.list.is_none() {
            self.load_pulls();
        }
    }

    pub fn load_pulls(&mut self) {
        let (Some(gh), Some(app)) = (gh(), HANDLE.get().cloned()) else { return };
        if std::mem::replace(&mut self.pulls.loading, true) {
            return;
        }
        std::thread::spawn(move || {
            let got = gh.me().and_then(|me| Ok((me, gh.pulls()?)));
            app.update(move |s, _| {
                s.pulls.loading = false;
                s.pulls.list = Some(got);
            });
        });
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
                    Err(e) => s.say(e, true),
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
                    Err(e) => s.say(e, true),
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
            .item(PopupItem::new("ready", if draft { "Ready for Review" } else { "Convert to Draft" }).enabled(open))
            .item(PopupItem::new("state", if open { "Close Pull Request" } else { "Reopen Pull Request" }));
        tx.popup_menu(menu, "pr-more", move |s: &mut Collate, picked, _| match picked {
            Some("checkout") => s.pr_act("Checking out…", move |gh| gh.checkout(n).map(|_| format!("Checked out #{n}")), Collate::reread),
            Some("web") => open_url(&url),
            Some("reload") => s.open_pull(n),
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

/// The list of open pull requests, in place of the diff.
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
    let head = row().key("pulls-head").w_full().h(28.0).px(EDGE).gap(8.0).items_center()
        .child(text("Pull Requests".to_owned()).font(k.ty.ui_medium).color(k.c.text_base).grow(1.0))
        .child(filter)
        .child(create)
        .child(refresh.build(k).key("pulls-reload"));
    let body = match &s.pulls.list {
        None => message(k, "Reading pull requests…", ""),
        Some(Err(e)) => message(k, "Can't read pull requests", e),
        Some(Ok((_, pulls))) if pulls.is_empty() => message(k, "No open pull requests", "Open one from this branch with New Pull Request."),
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
        .child(row().key("pr-ready").w_full().gap(6.0).items_center()
            .child(dot(k, Some(if can_merge { CheckState::Passed } else if open { CheckState::Pending } else { CheckState::Skipped })))
            .child(text(ready.to_owned()).font(k.ty.text_small).color(k.c.text_base)));
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
        card = card.child(section("pr-checks-title", &format!("Checks - {passed} passed{}", if failed > 0 { format!(", {failed} failed") } else { String::new() })));
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
    let head = row().w_full().gap(8.0).items_center()
        .child(text(if th.resolved { "Resolved thread" } else { "Thread" }.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).grow(1.0))
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
    };
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
            Compose::Create => s.pulls.title.text.trim().is_empty(),
            _ => empty,
        };
    let mut body = col().key("compose-body").w_full().gap(12.0);
    if c == Compose::Create {
        body = body.child(TextInput::new(&s.pulls.title).placeholder("Title").on_edit(|s: &mut Collate| &mut s.pulls.title).width(None).build(k).key("compose-title").w_full());
    }
    body = body.child(area);
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
    fn the_filter_wants_every_word_somewhere() {
        let p = crate::github::Pull {
            number: 42, title: "Add billing".into(), author: "ana".into(), head: "billing".into(), base: "main".into(), draft: false,
            decision: String::new(), requested: vec![], additions: 0, deletions: 0, updated: String::new(), labels: vec!["db".into()], checks: vec![], url: String::new(),
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

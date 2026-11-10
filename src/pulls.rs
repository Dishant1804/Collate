//! The Pull Requests place: the repository's open pull requests, grouped by what they want of
//! the reader, and one pull request under review. Its diff is a range in History; its details,
//! checks, conversation and actions take the staging panel's top; its line threads sit under
//! their lines with the reader's own comments, which a review sends as its line comments.

use rune::{Cursor, El, PopupItem, PopupMenu, TextEdit, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, IconButton, IconButtonVariant, Kit, TextArea, TextInput};

use crate::github::{self, CheckState, Detail, Draft, Gh, Method, Pull, Verdict};
use crate::{COLUMN_TOP, Collate, EDGE, GHOST_PAD, HANDLE, MORE_ICON, PANEL_INSET, REFRESH_ICON, current_repo, diff, message, open_url};

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
}

impl Default for PullsState {
    fn default() -> PullsState {
        PullsState {
            open: false,
            list: None,
            loading: false,
            pr: None,
            fetching: None,
        }
    }
}

impl PullsState {
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
            let got = gh.detail(n).and_then(|d| Ok((gh.fetch(&d)?, d)));
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
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let head = row().key("pulls-head").w_full().h(28.0).px(EDGE).gap(8.0).items_center()
        .child(text("Pull Requests".to_owned()).font(k.ty.ui_medium).color(k.c.text_base).grow(1.0))
        .child(refresh.build(k).key("pulls-reload"));
    let body = match &s.pulls.list {
        None => message(k, "Reading pull requests…", ""),
        Some(Err(e)) => message(k, "Can't read pull requests", e),
        Some(Ok((_, pulls))) if pulls.is_empty() => message(k, "No open pull requests", "Open one from this branch with New Pull Request."),
        Some(Ok((me, pulls))) => {
            let mut list = col().key("pulls-list").w_full().pb(16.0);
            for (title, group) in github::group(pulls, me) {
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
    let mut card = col().key("pr-card").w_full().px(PANEL_INSET).pt(4.0).pb(10.0).gap(6.0)
        .child(row().w_full().gap(4.0).items_start()
            .child(text(p.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).wrap().grow(1.0).min_w(0.0))
            )
        .child(text(format!("#{} by {} - {} → {}", p.number, p.author, p.head, p.base)).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full())
        .child(row().key("pr-ready").w_full().gap(6.0).items_center()
            .child(dot(k, Some(if can_merge { CheckState::Passed } else if open { CheckState::Pending } else { CheckState::Skipped })))
            .child(text(ready.to_owned()).font(k.ty.text_small).color(k.c.text_base)));
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
        card = card.child(section("pr-body-title", "Description")).child(text(pr.body.trim().to_owned()).font(k.ty.text_small).color(k.c.text_base).wrap().w_full().max_lines(12));
    }
    let open_threads = pr.threads.iter().filter(|t| !t.resolved).count();
    card = card.child(section("pr-talk-title", &format!("Conversation - {} comment{}, {open_threads} open thread{}",
        pr.conversation.len(), if pr.conversation.len() == 1 { "" } else { "s" }, if open_threads == 1 { "" } else { "s" })));
    for (i, post) in pr.conversation.iter().enumerate() {
        card = card.child(post_view(k, &format!("post-{i}"), post));
    }
    // The panel's file list follows; a long conversation scrolls here rather than pushing it away.
    Some(col().key("pr-scroll").w_full().max_h(440.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(card))
}

fn post_view(k: &Kit, key: &str, p: &github::Post) -> El {
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
        c = c.child(text(p.body.trim().to_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().max_lines(8));
    }
    c
}


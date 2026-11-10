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
}

impl Default for PullsState {
    fn default() -> PullsState {
        PullsState {
            open: false,
            list: None,
            loading: false,
            pr: None,
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
                    let fetching = false;
                    let (verdict, tint) = decision(k, p);
                    let mut words = col().grow(1.0).min_w(0.0).gap(2.0)
                        .child(text(p.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate())
                        .child(text(format!("#{n} - {} - {} → {} - {}", p.author, p.head, p.base, p.updated.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).truncate());
                    if !p.labels.is_empty() {
                        words = words.child(text(p.labels.join(", ")).font(k.ty.text_small).color(k.c.text_faint).truncate());
                    }
                    list = list.child(row().key(format!("pull-{n}")).w_full().px(EDGE).py(8.0).gap(12.0).items_center()
                        .role("listitem").label(format!("#{n} {}", p.title))
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


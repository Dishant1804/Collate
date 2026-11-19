//! The Issues place: the repository's issues, grouped by what they want of the reader, and one
//! issue's overview, in place of the diff, as the pull requests are.

use rune::{El, col, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit};

use crate::Hand;
use crate::github_issues::Issue;
use crate::github::Gh;
use crate::{COLUMN_TOP, Collate, EDGE, HANDLE, ISSUES_ICON, REFRESH_ICON, current_repo, message};

#[derive(Default)]
pub struct IssuesState {
    /// The list shows in place of the diff.
    pub open: bool,
    /// The signed-in login and the issues, once read.
    pub list: Option<Result<(String, Vec<Issue>), String>>,
    pub loading: bool,
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

    /// Read the list again, off the UI thread. Whatever list shows stays until it lands.
    pub fn load_issues(&mut self) {
        let (Some(root), Some(app)) = (current_repo().map(|r| r.root.clone()), HANDLE.get().cloned()) else { return };
        if std::mem::replace(&mut self.issues.loading, true) {
            return;
        }
        std::thread::spawn(move || {
            let got = Gh::new(&root).issues();
            app.update(move |s, _| {
                // The reader switched projects meanwhile: this list was the last one's.
                if current_repo().is_none_or(|r| r.root != root) {
                    return;
                }
                s.issues.loading = false;
                s.issues.list = Some(got);
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
    let head = row().key("issues-head").w_full().h(28.0).px(EDGE).gap(8.0).items_center()
        .child(text("Issues".to_owned()).font(k.ty.ui_medium).color(k.c.text_base).grow(1.0))
        .child(refresh.build(k).hand(!s.issues.loading).key("issues-reload"));
    let body = match &s.issues.list {
        None => message(k, "Reading issues…", ""),
        Some(Err(e)) => message(k, "Can't read issues", e),
        Some(Ok((_, issues))) if issues.is_empty() => message(k, "No issues", "This repository has no open or recently closed issues."),
        Some(Ok((_, issues))) => {
            let mut list = col().key("issues-list").w_full().pb(16.0);
            for i in issues {
                list = list.child(issue_row(s, k, i));
            }
            col().key("issues-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(list)
        }
    };
    col().key("issues").w_full().grow(1.0).min_h(0.0).pt(COLUMN_TOP).child(head).child(body)
}

/// One issue in the list: its state, title, who opened it and when, and its labels.
fn issue_row(s: &Collate, k: &Kit, i: &Issue) -> El {
    let n = i.number;
    let (hue, state) = if i.open { (k.c.success.icon, "Open") } else { (k.c.text_faint, "Closed") };
    let mut words = col().grow(1.0).min_w(0.0).gap(2.0)
        .child(text(i.title.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate())
        .child(text(format!("#{n} - {} - {}", i.author, i.updated.get(..10).unwrap_or(""))).font(k.ty.text_small).color(k.c.text_muted).truncate());
    if !i.labels.is_empty() {
        words = words.child(text(i.labels.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(", ")).font(k.ty.text_small).color(k.c.text_faint).truncate());
    }
    row().key(format!("issue-{n}")).w_full().px(EDGE).py(8.0).gap(12.0).items_center()
        .label(format!("#{n} {}", i.title)).description(state)
        .child(icon(&ISSUES_ICON, 16.0).color(hue).a11y_hidden())
        .child(words)
        .child(crate::history_view::face(k, s, &i.author, 20.0))
}

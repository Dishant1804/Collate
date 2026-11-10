//! History: the commit graph with its lanes and labels, the chosen commit's card, and the
//! header above a commit's diff.
use rune::{Cursor, El, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::diff::{EDGE, GHOST_PAD};
use crate::git::Scope;
use crate::{Collate, EXTERNAL_ICON, MORE_ICON, PANEL_INSET, REFRESH_ICON, abbrev, graph, open_url, pulls, refresh};

/// The graph's geometry: a row's height and the spacing of its lanes.
pub const GRAPH_ROW: f32 = 28.0;
const LANE: f32 = 16.0;
/// The fixed columns after the description: author, date and short hash.
pub const AUTHOR_W: f32 = 112.0;
pub const DATE_W: f32 = 126.0;
pub const HASH_W: f32 = 64.0;

/// A lane's colour, as Git Graph cycles them: each branch line keeps its lane's colour.
fn lane_color(lane: usize) -> rune::Color {
    const LANES: [&str; 8] = ["#4c9df0", "#e0609e", "#3fbf6a", "#e09a3c", "#a678e8", "#e5584f", "#36bfb5", "#b8c440"];
    rune::color::hex(LANES[lane % LANES.len()]).unwrap()
}

fn graph_width(lanes: usize) -> f32 {
    lanes.max(1) as f32 * LANE + 8.0
}

// Keep topology order: sorting by author or date independently would break graph edges.
pub fn history_graph(s: &Collate, k: &Kit) -> El {
    let width = graph_width(s.graph_lanes);
    let label = |t: &str| text(t.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_faint);
    // Outside the scroll, so the column names stay put while the rows move under them.
    let header = row().key("graph-header").w_full().h(24.0).px(EDGE).gap(12.0).items_center()
        .child(label("Graph").w(width))
        .child(label("Description").grow(1.0).min_w(0.0))
        .child(label("Author").w(AUTHOR_W))
        .child(label("Date").w(DATE_W))
        .child(label("Commit").w(HASH_W));
    let kit = k.clone();
    let rows = col().key("graph-rows").growth(rune::GrowthSpec::default()).w_full()
        .virtual_rows(s.graph_commits.len(), GRAPH_ROW,
            |s: &Collate, i| format!("graph-{}", s.graph_commits[i].id).into(),
            move |s: &Collate, i| graph_row(s, &kit, i, width))
        .virtual_rows_version(s.graph_version)
        // Within a hundred rows of the end, the next page starts loading.
        .on_visible_rows(|s: &mut Collate, rows| {
            if rows.end + 100 >= s.graph_commits.len() && !s.graph_loading && s.commit_ids.len() >= s.graph_limit {
                s.load_more_history();
                return true;
            }
            false
        });
    let refresh = IconButton::new(&REFRESH_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .label("Refresh history")
        .help("Read the history again (⌘R)")
        .on_press(|s: &mut Collate| { s.load_history(); refresh(); })
        .build(k)
        .key("graph-refresh");
    let shown = s.commit_ids.len();
    let title = row().key("graph-title").w_full().h(36.0).px(EDGE).gap(8.0).items_center()
        .child(text("Commits").font(k.ty.text_small_medium).color(k.c.text_base))
        .child(text(format!("{shown}{} on all branches{}", if shown >= s.graph_limit { "+" } else { "" }, if s.graph_loading { " - loading more…" } else { "" })).font(k.ty.text_small).color(k.c.text_muted).grow(1.0).truncate().min_w(0.0))
        .child(refresh.mr(-4.0));
    col().key("history-graph").w_full().grow(1.0).min_h(0.0)
        .child(title)
        .child(header)
        .child(col().w_full().h(1.0).fill(k.c.border_weak))
        .child(col().key("graph-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(rows))
}

/// A round badge with someone's initials, tinted by their name so each author keeps a colour.
/// An author's GitHub picture, else their initials on a colour of their own.
fn avatar(k: &Kit, s: &Collate, c: &graph::Commit, size: f32) -> El {
    if let Some(img) = s.avatars.get(&c.email) {
        return row().size(size, size).radius(size / 2.0).border(1.0, k.c.border_weak).pixels_fit(img.pixels(), rune::SurfaceFit::Fill).a11y_hidden();
    }
    let name = c.author.as_str();
    let initials: String = name.split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-')
        .filter_map(|w| w.chars().next()).take(if size < 24.0 { 1 } else { 2 }).collect::<String>().to_uppercase();
    let hue = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    let tint = lane_color(hue);
    row().size(size, size).center().radius(size / 2.0).fill(k.alpha(tint, 0.22)).border(1.0, k.alpha(tint, 0.6))
        .child(text(if initials.is_empty() { "?".to_owned() } else { initials }).font(if size >= 40.0 { k.ty.text_small_medium } else if size >= 24.0 { k.ty.text_xsmall_medium } else { rune::TextStyle { size: 9.0, line_height: 12.0, ..k.ty.text_xsmall_medium } }).color(k.c.text_base))
}

/// The host's name for a web link: "GitHub" for github.com, else the host.
fn web_name(url: &str) -> String {
    let host = url.trim_start_matches("https://").split('/').next().unwrap_or_default();
    match host {
        "github.com" => "GitHub".to_owned(),
        "gitlab.com" => "GitLab".to_owned(),
        "bitbucket.org" => "Bitbucket".to_owned(),
        h => h.to_owned(),
    }
}

/// The chosen commit at the head of History's side panel, as Zed shows it: who and when, its
/// branches, email, full hash, a link to the host, and the whole message.
pub fn commit_card(s: &Collate, k: &Kit) -> Option<El> {
    let id = s.commit.as_deref()?;
    let c = s.graph_commits.iter().find(|c| c.id == id)?;
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    // Named fields on one column edge, so the values line up.
    let line = |key: &str, name: &str, body: El| row().key(key.to_owned()).w_full().gap(8.0).items_center()
        .child(text(name.to_owned()).w(52.0).font(k.ty.text_small).color(k.c.text_faint)).child(body);
    let mut card = col().key("commit-card").w_full().px(PANEL_INSET).pt(4.0).pb(10.0).gap(8.0)
        .child(col().w_full().items_center().gap(4.0)
            .child(avatar(k, s, c, 44.0))
            .child(text(c.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().min_w(0.0))
            .child(text(c.date.clone()).font(k.ty.text_small).color(k.c.text_muted)));
    let labels = ref_labels(&c.refs);
    if !labels.is_empty() {
        // Wrapped onto as many lines as the names need, each name whole unless it alone is too wide.
        let mut refs = row().key("commit-refs").w_full().gap(4.0).justify_center().flex_wrap();
        for (label, kind, _) in labels {
            let tag = kind == graph::RefKind::Tag;
            refs = refs.child(text(label).font(mono).color(k.c.text_base).truncate().min_w(0.0).px(6.0).h(20.0).radius(4.0)
                .border(1.0, if tag { k.c.border_weak } else { k.alpha(lane_color(s.graph_rows.get(s.graph_commits.iter().position(|x| x.id == c.id).unwrap_or(0)).map_or(0, |r| r.col)), 0.8) }));
        }
        card = card.child(refs);
    }
    let sha = c.id.clone();
    if c.id != graph::UNCOMMITTED {
        let more = IconButton::new(&MORE_ICON)
            .variant(IconButtonVariant::GhostMuted)
            .label("More actions for this commit")
            .help("Branch, cherry-pick, revert, reset")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.commit_menu("commit-more", tx))
            .build(k)
            .key("commit-more");
        card = card.child(row().key("commit-actions").w_full().justify_end().child(more.mr(-4.0)));
    }
    card = card
        .child(line("commit-email", "Email", text(c.email.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0)))
        .child(line("commit-sha", "Commit", text(c.id.clone()).font(mono).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0)
            .cursor(Cursor::Pointer).role("button").label("Copy the commit's full hash").help("Copy hash")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| { tx.copy(sha.clone()); s.say("Copied the commit hash", false); })));
    if let Some(web) = &s.web {
        let url = format!("{web}/commit/{}", c.id);
        let name = web_name(web);
        // In the card's grey, with the host's mark when it's GitHub and an arrow that says it opens the browser.
        let mut link = row().gap(5.0).items_center().cursor(Cursor::Pointer).role("link").label(format!("Open {url}")).help(url.clone());
        if name == "GitHub" {
            link = link.child(icon(&pulls::GITHUB_ICON, 13.0).color(k.c.icon_muted));
        }
        link = link.child(text(format!("View on {name}")).font(k.ty.text_small).color(k.c.text_muted)).child(icon(&EXTERNAL_ICON, 11.0).color(k.c.icon_faint));
        card = card.child(line("commit-web", "", link.on_press(move |_: &mut Collate| open_url(&url))));
    }
    if c.parents.len() > 1 {
        let parents = c.parents.iter().map(|p| p.chars().take(8).collect::<String>()).collect::<Vec<_>>().join("  ");
        card = card.child(line("commit-parents", "Merge", text(parents).font(mono).color(k.c.text_muted)));
    }
    let message = if s.commit_body.is_empty() { c.subject.clone() } else { format!("{}\n\n{}", c.subject, s.commit_body) };
    let (adds, dels) = s.diff.totals();
    card = card
        .child(col().w_full().h(1.0).fill(k.c.border_weak))
        .child(text("Message").font(k.ty.text_xsmall_medium).color(k.c.text_faint))
        .child(text(message).font(k.ty.text_small).color(k.c.text_base).wrap().w_full().max_lines(14))
        .child(row().key("commit-stat").w_full().gap(6.0).items_center().pt(4.0)
            .child(text(if s.loaded { "Lines changed" } else { "Reading files…" }.to_owned()).font(k.ty.text_small).color(k.c.text_muted).grow(1.0))
            .child(text(format!("+{adds}")).font(mono).color(k.c.success.fg))
            .child(text(format!("−{dels}")).font(mono).color(k.c.danger.fg)));
    Some(card)
}

/// Above a commit's diff: the way back to the list, who made it and when, its subject, and
/// the hash to copy.
pub fn commit_header(s: &Collate, k: &Kit) -> El {
    let back = Button::new("‹ All Commits").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .help("Back to the commit list (Esc)")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.close_commit_diff(tx)).build(k).key("history-back");
    let mut r = row().key("commit-header").w_full().px(EDGE).py(10.0).gap(12.0).items_center().child(back.ml(-GHOST_PAD));
    if let Some(range) = s.commit.as_deref().filter(|c| c.contains("..")) {
        let (from, to) = s.compared.clone().unwrap_or_else(|| {
            let (a, b) = range.split_once("..").unwrap_or_default();
            (abbrev(a), abbrev(b))
        });
        let mono = rune::TextStyle { size: 12.0, line_height: 18.0, ..k.ty.mono };
        r = r.child(row().key("compared").grow(1.0).min_w(0.0).gap(8.0).items_center()
            .child(text("Comparing").font(k.ty.text_small).color(k.c.text_muted))
            .child(text(from).font(mono).color(k.c.text_base).truncate().min_w(0.0))
            .child(text("→").font(k.ty.text_small).color(k.c.text_faint))
            .child(text(to).font(mono).color(k.c.text_base).truncate().min_w(0.0)));
    } else if let Some(c) = s.commit.as_deref().and_then(|id| s.graph_commits.iter().find(|c| c.id == id)) {
        let sha = c.id.clone();
        let copy = Button::new("Copy SHA").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .help("Copy the commit's full hash")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| { tx.copy(sha.clone()); s.say("Copied the commit hash", false); }).build(k).key("copy-sha");
        let more = IconButton::new(&MORE_ICON)
            .variant(IconButtonVariant::GhostMuted)
            .label("More actions for this commit")
            .help("Branch, cherry-pick, revert, reset")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.commit_menu("commit-header-more", tx))
            .build(k)
            .key("commit-header-more");
        r = r.child(avatar(k, s, c, 32.0))
            .child(col().grow(1.0).min_w(0.0).gap(2.0)
                .child(row().gap(8.0).items_center()
                    .child(text(c.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
                    .child(text(format!("{} - {}", c.date, c.email)).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0)))
                .child(text(c.subject.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0)))
            .child(copy)
            .child(more.mr(-4.0));
    }
    col().w_full().child(r).child(col().w_full().h(1.0).fill(k.c.border_weak))
}

/// Points on the S-curve from lane `a` in one row's middle to lane `b` in the next row's, cut
/// at the row boundary: the upper half (`upper`) in the first row's coordinates, else the lower
/// half in the second's. Vertical where it meets each dot, as Git Graph draws its branches.
fn edge_half(xa: f32, xb: f32, upper: bool) -> rune::Path {
    let (h, c) = (GRAPH_ROW, GRAPH_ROW / 2.0);
    let mid = [(xa + xb) / 2.0, if upper { h } else { 0.0 }];
    if upper {
        rune::Path::new().move_to([xa, c]).cubic_to([xa, c + h / 4.0], [(3.0 * xa + xb) / 4.0, c + 3.0 * h / 8.0], mid)
    } else {
        rune::Path::new().move_to(mid).cubic_to([(xa + 3.0 * xb) / 4.0, h / 8.0], [xb, h / 4.0], [xb, c])
    }
}

fn graph_row(s: &Collate, k: &Kit, i: usize, width: f32) -> El {
    use rune::{Shape, LineCap, canvas};
    let x = |lane: usize| 4.0 + LANE / 2.0 + lane as f32 * LANE;
    let r = &s.graph_rows[i];
    let c = &s.graph_commits[i];
    let uncommitted = c.id == graph::UNCOMMITTED;
    let head = s.head.as_deref() == Some(c.id.as_str());
    let faint = k.c.text_faint;
    // A line belongs to the branch that opens or closes on it: the lane further right.
    let edge_color = |from_uncommitted: bool, a: usize, b: usize| if from_uncommitted { faint } else { lane_color(a.max(b)) };
    let mut shape = Shape::new();
    if i > 0 {
        let above = s.graph_commits[i - 1].id == graph::UNCOMMITTED;
        for &(a, b) in &s.graph_rows[i - 1].down {
            let color = edge_color(above, a, b);
            shape = if a == b {
                shape.line([x(a), 0.0], [x(a), GRAPH_ROW / 2.0], 2.0, LineCap::Butt, color)
            } else {
                shape.stroke_path(&edge_half(x(a), x(b), false), 2.0, LineCap::Butt, color)
            };
        }
    }
    for &(a, b) in &r.down {
        let color = edge_color(uncommitted, a, b);
        shape = if a == b {
            shape.line([x(a), GRAPH_ROW / 2.0], [x(a), GRAPH_ROW], 2.0, LineCap::Butt, color)
        } else {
            shape.stroke_path(&edge_half(x(a), x(b), true), 2.0, LineCap::Butt, color)
        };
    }
    let ring = |radius: f32| -> Vec<[f32; 2]> {
        (0..=32).map(|n| { let t = n as f32 * std::f32::consts::TAU / 32.0; [x(r.col) + t.cos() * radius, GRAPH_ROW / 2.0 + t.sin() * radius] }).collect()
    };
    let dot = if uncommitted { faint } else { lane_color(r.col) };
    // HEAD and the uncommitted changes are hollow, so where you are reads at a glance.
    shape = if head || uncommitted {
        shape.fill(ring(5.0), k.c.bg_base).stroke(ring(4.0), 2.0, dot)
    } else {
        shape.fill(ring(4.0), dot)
    };
    let selected = s.commit.as_deref() == Some(c.id.as_str());
    let mut description = row().gap(6.0).items_center().grow(1.0).min_w(0.0);
    // Two labels at most, each cut short if long, so the subject keeps its room; the side
    // panel lists them all.
    let labels = ref_labels(&c.refs);
    let more = labels.len().saturating_sub(2);
    for (label, kind, at_head) in labels.into_iter().take(2) {
        let tint = lane_color(r.col);
        let pill = text(label).font(if at_head { k.ty.text_xsmall_medium } else { k.ty.text_xsmall }).truncate().max_w(220.0).px(6.0).h(18.0).radius(4.0);
        description = description.child(match kind {
            graph::RefKind::Tag => pill.color(k.c.text_base).border(1.0, k.c.border_weak).fill(k.alpha(k.c.text_base, 0.06)),
            graph::RefKind::Remote => pill.color(k.c.text_muted).border(1.0, k.alpha(tint, 0.5)),
            graph::RefKind::Branch => pill.color(k.c.text_base).border(1.0, k.alpha(tint, if at_head { 1.0 } else { 0.6 })).fill(k.alpha(tint, 0.18)),
        });
    }
    if more > 0 {
        description = description.child(text(format!("+{more}")).font(k.ty.text_xsmall).color(k.c.text_muted).px(6.0).h(18.0).radius(4.0).border(1.0, k.c.border_weak)
            .help(format!("{more} more on this commit: listed in the side panel")));
    }
    let subject = if uncommitted { format!("Uncommitted Changes ({})", s.total) } else { c.subject.clone() };
    description = description.child(text(subject).font(if head { k.ty.text_small_medium } else { k.ty.text_small })
        .color(if uncommitted { k.c.text_muted } else { k.c.text_base }).truncate().min_w(0.0).grow(1.0));
    let muted = |t: String, w: f32| text(t).w(w).truncate().font(k.ty.text_small).color(k.c.text_muted);
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let fill = if selected { k.alpha(k.c.border_focus, 0.22) } else { k.alpha(k.c.text_base, 0.0) };
    let id = c.id.clone();
    row().key(format!("graph-{}", c.id)).w_full().h(GRAPH_ROW).px(EDGE).gap(12.0).items_center()
        .fill(fill).hover(if selected { fill } else { k.alpha(k.c.text_base, 0.06) }).cursor(Cursor::Pointer).role("button")
        .label(if uncommitted { "Uncommitted changes: open All Changes".to_owned() } else { format!("{}{}, {}, {}", if selected { "Selected: " } else { "" }, c.subject, c.author, c.id) })
        .on_press(move |s: &mut Collate| if id == graph::UNCOMMITTED { s.set_scope(Scope::Uncommitted) } else { s.set_commit(&id) })
        .on_double_press(move |s: &mut Collate| if !uncommitted { s.history_diff = true })
        .child(canvas(shape).w(width).h(GRAPH_ROW)).child(description)
        .child(if uncommitted { muted(String::new(), AUTHOR_W) } else {
            row().w(AUTHOR_W).gap(6.0).items_center().child(avatar(k, s, c, 16.0)).child(muted(c.author.clone(), AUTHOR_W - 22.0).min_w(0.0).shrink())
        })
        .child(muted(c.date.clone(), DATE_W))
        .child(text(if uncommitted { String::new() } else { c.id.chars().take(8).collect() }).w(HASH_W).font(mono).color(k.c.text_muted))
}

/// A commit's labels, a local branch and its remote counterparts shown as one
/// (`main - origin`), as Git Graph does: (text, kind, whether HEAD is on it).
pub fn ref_labels(refs: &[graph::Ref]) -> Vec<(String, graph::RefKind, bool)> {
    let mut out = Vec::new();
    let mut merged = vec![false; refs.len()];
    for (i, r) in refs.iter().enumerate() {
        match r.kind {
            graph::RefKind::Branch => {
                let mut label = r.name.clone();
                for (j, o) in refs.iter().enumerate() {
                    if o.kind == graph::RefKind::Remote
                        && let Some((remote, name)) = o.name.split_once('/')
                        && name == r.name
                    {
                        label.push_str(&format!(" - {remote}"));
                        merged[j] = true;
                    }
                }
                out.push((label, r.kind, r.head));
            }
            graph::RefKind::Remote if !merged[i] => out.push((r.name.clone(), r.kind, false)),
            graph::RefKind::Remote => {}
            graph::RefKind::Tag => out.push((r.name.clone(), r.kind, false)),
        }
    }
    out
}

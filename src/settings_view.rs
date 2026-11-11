//! The settings panel: sections down the left, each section's settings and their controls on
//! the right.
use rune::{Cursor, El, Icon, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit, SegmentedControl, Switch};

use crate::diff::DiffMode;
use crate::git::Scope;
use crate::settings::{Agent, CONTEXT_LINES, Channel, Density, Editor, FOLD_LARGE, FONT_SIZES, Motion, REFRESH_MS, Settings, Theme};
use crate::{ADD_PROJECT, AGENT_ICON, ALL_ICON, Collate, Screen, current_repo, grammars, lucide, project_labels, tilde, update};

/// The width every settings control is right-aligned in, so each description wraps at the
/// same measure and rows of the same kind come out the same height.
const CONTROL_W: f32 = 272.0;

/// One setting: what it is (with its shortcut as keycaps, if it has one) and why, then its
/// control at the right edge.
fn setting(k: &Kit, key: &str, title: &str, keys: &[&str], detail: &str, control: El) -> El {
    let mut name = row().gap(8.0).items_center().child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base));
    if !keys.is_empty() {
        name = name.child(rune_kit::Keybind::new(keys).build(k));
    }
    let mut words = col().grow(1.0).min_w(0.0).gap(4.0).child(name);
    if !detail.is_empty() {
        words = words.child(text(detail.to_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap());
    }
    row().key(key.to_owned()).w_full().py(14.0).gap(24.0).items_center().child(words).child(row().min_w(CONTROL_W).justify_end().items_center().child(control))
}

/// The Languages section: whether to download grammars, what's built in, and every grammar on
/// offer with how it stands and what to do with it.
fn languages_settings(s: &Collate, k: &Kit) -> Vec<El> {
    let mut rows = vec![
        setting(k, "download", "Download languages", &[], "Fetch a language's grammar from GitHub the first time a diff shows it; building it needs Xcode's command line tools", switch(k, "Download languages", s.settings.download_languages, |c, v| c.download_languages = v)),
        setting(k, "built-in", "Built in", &[], grammars::BUILT_IN, row()),
    ];
    for g in grammars::CATALOGUE {
        let status = s.languages.status(g);
        let files = g.files.iter().map(|f| if f.contains('.') || f.chars().next().is_some_and(char::is_uppercase) { (*f).to_owned() } else { format!(".{f}") }).collect::<Vec<_>>().join(", ");
        let mut name = row().gap(8.0).items_center().child(text(g.title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base));
        let state = match status {
            grammars::Status::Installed if s.languages.loaded.contains_key(g.id) => "In use",
            grammars::Status::Installed => "Installed",
            grammars::Status::Outdated => "Update available",
            _ => "",
        };
        if !state.is_empty() {
            name = name.child(text(state.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_faint));
        }
        let mut words = col().grow(1.0).min_w(0.0).gap(4.0).child(name).child(text(files).font(k.ty.text_small).color(k.c.text_muted).truncate());
        if let grammars::Status::Failed(e) = &status {
            words = words.child(text(e.clone()).font(k.ty.text_small).color(k.c.danger.icon).wrap());
        }
        let (label, help) = match status {
            grammars::Status::Available => ("Install", "Fetch and build this grammar"),
            grammars::Status::Installed => ("Remove", "Delete the grammar; its files draw plain until it's installed again"),
            grammars::Status::Outdated => ("Update", "Build the version this Collate expects"),
            grammars::Status::Working => ("Installing…", ""),
            grammars::Status::Failed(_) => ("Retry", "Try fetching and building it again"),
        };
        let installed = status == grammars::Status::Installed;
        let mut button = Button::new(label).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(status == grammars::Status::Working);
        if !help.is_empty() {
            button = button.help(help);
        }
        let button = button.on_press(move |s: &mut Collate| if installed { s.remove_language(g) } else { s.prepare_language(g) }).build(k);
        rows.push(row().key(format!("lang-{}", g.id)).w_full().py(14.0).gap(24.0).items_center().child(words).child(row().min_w(CONTROL_W).justify_end().child(button)));
    }
    rows
}

/// The Agent section: which agent CLI Collate hands work to, how it runs it, and the files
/// that tell it what to ask and how to write.
fn agent_settings(s: &Collate, k: &Kit) -> Vec<El> {
    let st = &s.settings;
    let edit = |key: &'static str, pick: &'static str| Button::new("Edit…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help("Open the file in the editor from Review settings").on_press(move |s: &mut Collate| s.agent_menu_pick(pick)).build(k).key(key);
    let runs = format!("{} in the terminal under the diff (⌘J); {} for what it fills in, like a commit message", st.agent.command(), st.agent.print_command());
    vec![
        setting(k, "agent", "Coding agent", &[], "Gets your comments, review questions, commit messages and conflicts; it never pushes", pick(k, &Agent::ALL.map(Agent::title), &Agent::ALL, st.agent, |c, v| c.agent = v)),
        setting(k, "agent-runs", "Runs as", &[], &runs, row()),
        setting(k, "agent-checkpoint", "Checkpoint first", &[], "The working tree is saved before the agent starts, to restore from Checkpoints in the sidebar", row()),
        setting(k, "agent-questions", "Review questions", &[], "What Review Change asks about the change in view, one a line", edit("edit-questions", "ask:edit")),
        setting(k, "agent-commit", "Commit message instructions", &[], "How the agent writes a message from the staged diff", edit("edit-commit-rules", "ask:commit-rules")),
    ]
}

/// The Updates section: the running version and how it stands, then how updates come.
fn update_settings(s: &Collate, k: &Kit) -> Vec<El> {
    use update::Status;
    static CHANNELS: [Channel; 2] = [Channel::Release, Channel::Source];
    let st = &s.settings;
    let in_app = update::app_bundle().is_some();
    let (state, error) = match &s.update {
        Status::Idle => (String::new(), false),
        Status::Checking => ("Checking…".to_owned(), false),
        Status::Current => ("Up to date".to_owned(), false),
        Status::Available(u) if in_app => (format!("{} is available", u.label()), false),
        Status::Available(u) => (format!("{} is available; run the installed app to update", u.label()), false),
        Status::Installing(u) if matches!(u, update::Update::Source { .. }) => (format!("Building {}…", u.label()), false),
        Status::Installing(u) => (format!("Installing {}…", u.label()), false),
        Status::Ready(u) => (format!("{} is installed; restart to use it", u.label()), false),
        Status::Failed(e) => (e.clone(), true),
    };
    let mut words = col().grow(1.0).min_w(0.0).gap(4.0).child(text(update::running()).font(k.ty.ui_medium).color(k.c.text_base));
    if !state.is_empty() {
        words = words.child(text(state).font(k.ty.text_small).color(if error { k.c.danger.icon } else { k.c.text_muted }).wrap());
    }
    let button = match &s.update {
        Status::Ready(_) => Button::new("Restart").variant(ButtonVariant::Outline).help("Quit and open the new version").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.restart(tx)),
        Status::Available(_) if in_app => Button::new("Install").variant(ButtonVariant::Outline).on_press(|s: &mut Collate| s.install_update()),
        _ => Button::new("Check Now").variant(ButtonVariant::Ghost).disabled(s.update.busy()).on_press(|s: &mut Collate| {
            s.update = update::Status::Idle;
            s.check_update();
        }),
    };
    vec![
        row().key("version").w_full().py(14.0).gap(24.0).items_center().child(words).child(row().min_w(CONTROL_W).justify_end().child(button.control_size(ControlSize::Small).build(k))),
        setting(k, "check", "Check for updates", &[], "Look for a new version when Collate opens and every few hours", switch(k, "Check for updates", st.check_updates, |c, v| c.check_updates = v)),
        setting(k, "auto-update", "Install automatically", &[], "Install a new version in the background; it opens when Collate next starts", switch(k, "Install automatically", st.auto_update, |c, v| c.auto_update = v)),
        setting(k, "from", "Update from", &[], "Latest source builds the newest commit on main, which needs Rust", pick(k, &["Releases", "Latest source"], &CHANNELS, st.update_from, |c, v| c.update_from = v)),
    ]
}

fn pick<T: Copy + PartialEq + 'static>(k: &Kit, labels: &[&str], values: &'static [T], current: T, set: fn(&mut Settings, T)) -> El {
    SegmentedControl::new(labels, values.iter().position(|v| *v == current))
        .fit()
        .on_pick(move |s: &mut Collate, i, _: &mut Tx| s.configure(|c| set(c, values[i])))
        .build(k)
}

fn switch(k: &Kit, title: &str, on: bool, set: fn(&mut Settings, bool)) -> El {
    // Off, the kit's standard switch is a dark knob on a surface-coloured track, which nearly
    // vanishes on the panel's own surface in the dark theme: draw it as a hairline control
    // with a muted knob, so off reads as clearly as on.
    let mut kk = k.clone();
    if !on {
        kk.c.surface_base = k.alpha(k.c.text_base, 0.08);
        kk.c.border_weak = k.alpha(k.c.text_base, 0.22);
        kk.c.std_border = k.alpha(k.c.text_base, 0.28);
        if k.dark {
            kk.c.std_icon_invert = k.c.text_muted;
        }
    }
    Switch::new(on).standard().label(title.to_owned()).hide_label().on_toggle(move |s: &mut Collate| s.configure(|c| set(c, !on))).build(&kk)
}

pub const SETTINGS_TABS: [&str; 8] = ["Appearance", "Diff", "Languages", "Review", "Agent", "Projects", "Refresh", "Updates"];
lucide!(APPEARANCE_ICON, "collate-palette", r#"<path d="M12 22a1 1 0 0 1 0-20 10 9 0 0 1 10 9 5 5 0 0 1-5 5h-2.25a1.75 1.75 0 0 0-1.4 2.8l.3.4a1.75 1.75 0 0 1-1.4 2.8z"/><circle cx="13.5" cy="6.5" r=".5" fill="currentColor"/><circle cx="17.5" cy="10.5" r=".5" fill="currentColor"/><circle cx="6.5" cy="12.5" r=".5" fill="currentColor"/><circle cx="8.5" cy="7.5" r=".5" fill="currentColor"/>"#);
lucide!(LANGUAGES_ICON, "collate-languages", r#"<path d="m5 8 6 6"/><path d="m4 14 6-6 2-3"/><path d="M2 5h12"/><path d="M7 2h1"/><path d="m22 22-5-10-5 10"/><path d="M14 18h6"/>"#);
lucide!(REVIEW_ICON, "collate-eye", r#"<path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"/><circle cx="12" cy="12" r="3"/>"#);
lucide!(PROJECTS_ICON, "collate-folder-git", r#"<circle cx="12" cy="13" r="2"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/><path d="M14 13h3"/><path d="M7 13h3"/>"#);
lucide!(REFRESH_TAB_ICON, "collate-refresh-cw", r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#);
lucide!(UPDATES_ICON, "collate-download", r#"<path d="M12 15V3"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/>"#);
/// Each settings section's Lucide icon, in `SETTINGS_TABS` order.
static SETTINGS_ICONS: [&Icon; 8] = [&APPEARANCE_ICON, &ALL_ICON, &LANGUAGES_ICON, &REVIEW_ICON, &AGENT_ICON, &PROJECTS_ICON, &REFRESH_TAB_ICON, &UPDATES_ICON];
/// The Agent section, which the sidebar's agent menu opens.
pub const AGENT_TAB: usize = 4;
/// The Projects section, which has its own Add button.
const PROJECTS_TAB: usize = 5;

/// Settings as a floating panel over the review: sections down the left, the chosen one's
/// settings on the right, on one surface. Esc, the close button or a click outside put it
/// away; every change applies and saves at once, so there's nothing to confirm.
pub fn settings_panel(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let st = &s.settings;
    static THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
    static DENSITIES: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];
    static MOTIONS: [Motion; 2] = [Motion::Default, Motion::Reduced];
    static LAYOUTS: [DiffMode; 2] = [DiffMode::Unified, DiffMode::Split];
    static NUMBERS: [bool; 2] = [true, false];
    let tab = s.settings_tab.min(SETTINGS_TABS.len() - 1);
    let rows: Vec<El> = match tab {
        0 => vec![
            setting(k, "theme", "Theme", &[], "", pick(k, &["System", "Light", "Dark"], &THEMES, st.theme, |c, v| c.theme = v)),
            setting(k, "font", "Code size", &[], "In points, for code and line numbers", pick(k, &["11", "12", "13", "14", "16"], &FONT_SIZES, st.font_size, |c, v| c.font_size = v)),
            setting(k, "density", "Line spacing", &[], "", pick(k, &["Compact", "Comfortable", "Spacious"], &DENSITIES, st.density, |c, v| c.density = v)),
            setting(k, "motion", "Animations", &[], "Reduced plays every animation in half the time", pick(k, &["Default", "Reduced"], &MOTIONS, st.motion, |c, v| c.motion = v)),
        ],
        1 => vec![
            setting(k, "layout", "Layout", &["S"], "Split shows old and new side by side", pick(k, &["Unified", "Split"], &LAYOUTS, st.layout, |c, v| c.layout = v)),
            setting(k, "numbers", "Line numbers", &[], "Unified view: new numbers only, or both", pick(k, &["One column", "Old and new"], &NUMBERS, st.one_number, |c, v| c.one_number = v)),
            setting(k, "context", "Context lines", &[], "Unchanged lines kept around each change", pick(k, &["3", "10", "25", "Full file"], &CONTEXT_LINES, st.context, |c, v| c.context = v)),
            setting(k, "ws", "Ignore whitespace", &["⇧", "W"], "Hides indent-only changes; stage whole files while on", switch(k, "Ignore whitespace", st.ignore_whitespace, |c, v| c.ignore_whitespace = v)),
            setting(k, "moved", "Highlight moved code", &["M"], "Moved blocks show in blue, M jumps between their ends", switch(k, "Highlight moved code", st.show_moved, |c, v| c.show_moved = v)),
            setting(k, "wrap", "Wrap long lines", &["W"], "Off scrolls them sideways; split view always wraps", switch(k, "Wrap long lines", st.wrap, |c, v| c.wrap = v)),
            setting(k, "markers", "+ and − markers", &[], "A sign beside each changed line", switch(k, "Markers", st.markers, |c, v| c.markers = v)),
            setting(k, "words", "Word highlights", &[], "Tint the words that changed within a line", switch(k, "Word highlights", st.word_diff, |c, v| c.word_diff = v)),
            setting(k, "syntax", "Syntax highlighting", &[], "", switch(k, "Syntax highlighting", st.syntax, |c, v| c.syntax = v)),
            setting(k, "minimap", "Minimap", &[], "A strip on the right showing where the changes are; click it to go there", switch(k, "Minimap", st.minimap, |c, v| c.minimap = v)),
        ],
        2 => languages_settings(s, k),
        3 => vec![
            setting(k, "scope", "Open to", &[], "The view a new window starts on", pick(k, &["All", "Staged", "Unstaged", "PR Preview", "Git Graph"], &Scope::ALL, st.default_scope, |c, v| c.default_scope = v)),
            setting(k, "editor", "Open files in", &["O"], "Where Open in Editor sends a file", pick(k, &["Default", "Zed", "VS Code", "Cursor"], &Editor::ALL, st.editor, |c, v| c.editor = v)),
            setting(k, "collapse", "Fold viewed files", &["V"], "Marking a file viewed folds it away", switch(k, "Fold viewed files", st.collapse_viewed, |c, v| c.collapse_viewed = v)),
            setting(k, "large", "Fold large files", &[], "Files above this many changed lines start folded", pick(k, &["Never", "300", "1000", "3000"], &FOLD_LARGE, st.fold_large, |c, v| c.fold_large = v)),
            setting(k, "notify", "Review requests", &[], "A notification when someone asks for your review, checked every few minutes", switch(k, "Review requests", st.notify_reviews, |c, v| c.notify_reviews = v)),
            setting(k, "low", "Hide low-signal files", &[], "Lockfiles, bundles, snapshots and generated files", switch(k, "Hide low-signal files", st.hide_low_signal, |c, v| c.hide_low_signal = v)),
            setting(k, "order", "Source files first", &[], "Then tests, then lockfiles and generated files", switch(k, "Source files first", st.source_first, |c, v| c.source_first = v)),
        ],
        AGENT_TAB => agent_settings(s, k),
        PROJECTS_TAB => {
            let labels = project_labels(&s.projects);
            let mut rows: Vec<El> = s
                .projects
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let open = current_repo().is_some_and(|r| r.root == *p);
                    let mut name = row().gap(8.0).items_center().child(text(labels[i].clone()).font(k.ty.ui_medium).color(if p.exists() { k.c.text_base } else { k.c.text_faint }));
                    if open {
                        name = name.child(text("Open".to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_faint));
                    }
                    let words = col().grow(1.0).min_w(0.0).gap(4.0).child(name).child(text(tilde(p)).font(k.ty.text_small).color(k.c.text_muted).truncate());
                    let remove = Button::new("Remove").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(open).help(if open { "The open project stays in the list" } else { "Remove from the list; the folder isn't touched" }).on_press(move |s: &mut Collate| s.forget_project(i)).build(k);
                    row().key(format!("proj{i}")).w_full().py(14.0).gap(24.0).items_center().child(words).child(row().min_w(CONTROL_W).justify_end().child(remove))
                })
                .collect();
            if rows.is_empty() {
                rows.push(setting(k, "none", "No projects yet", &[], "Add a folder inside a git repository", row()));
            }
            rows
        }
        6 => vec![
            setting(k, "auto", "Watch for changes", &[], "Reread when files change on disk", switch(k, "Watch for changes", st.auto_refresh, |c, v| c.auto_refresh = v)),
            setting(k, "every", "Check every", &[], "Only when file events aren't available", pick(k, &["0.5 s", "1 s", "2 s", "5 s"], &REFRESH_MS, st.refresh_ms, |c, v| c.refresh_ms = v)),
        ],
        _ => update_settings(s, k),
    };
    let hairline = |key: String| col().key(key).w_full().h(1.0).fill(k.c.border_weak).a11y_hidden();
    // Left: the sections, on the panel's own surface, the chosen one a quiet step brighter.
    let nav = col().key("settings-nav").w(176.0).h_full().px(12.0).py(12.0).gap(2.0).children(SETTINGS_TABS.iter().enumerate().map(|(i, name)| {
        let on = i == tab;
        row()
            .key(format!("tab-{}", name.to_lowercase()))
            .w_full()
            .h(32.0)
            .px(10.0)
            .items_center()
            .radius(6.0)
            .fill(if on { k.alpha(k.c.text_base, 0.08) } else { k.alpha(k.c.text_base, 0.0) })
            .hover(k.alpha(k.c.text_base, 0.05))
            .cursor(Cursor::Pointer)
            .role("tab")
            .label(name.to_string())
            .checked(on)
            .on_press(move |s: &mut Collate| s.settings_tab = i)
            .gap(6.0)
            .child(icon(SETTINGS_ICONS[i], 16.0).color(if on { k.c.icon_base } else { k.c.icon_muted }).a11y_hidden())
            .child(text(name.to_string()).font(if on { k.ty.ui_medium } else { k.ty.ui }).color(if on { k.c.text_base } else { k.c.text_muted }))
    }));
    // Right: the section's rows straight on the surface, divided by hairlines (no card in a
    // panel), scrolling when long, with what applies to every section in a footer.
    let n = rows.len();
    let mut list = col().key("settings-rows").w_full();
    for (i, r) in rows.into_iter().enumerate() {
        list = list.child(r);
        if i + 1 < n {
            list = list.child(hairline(format!("rule{i}")));
        }
    }
    let mut content = col().key("settings-content").w_full().px(24.0).pb(16.0);
    if tab == PROJECTS_TAB {
        let add = Button::new(ADD_PROJECT).variant(ButtonVariant::Outline).control_size(ControlSize::Small).on_press(|s: &mut Collate| s.add_project()).build(k).key("settings-add");
        content = content.child(row().key("projects-head").w_full().pt(12.0).pb(4.0).justify_end().child(add));
    }
    let content = content.child(list);
    let scroll = col().key("settings-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(content);
    let mut foot = row()
        .key("settings-foot")
        .w_full()
        .h(48.0)
        .px(24.0)
        .gap(12.0)
        .items_center()
        .child(text("Changes save as you make them".to_owned()).font(k.ty.text_small).color(k.c.text_faint).grow(1.0).min_w(0.0));
    if let Some(e) = &s.save_error {
        foot = foot.child(text(format!("Couldn't save: {e}")).font(k.ty.text_small).color(k.c.danger.icon));
    }
    foot = foot.child(Button::new("Reset to Defaults").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).on_press(|s: &mut Collate| s.configure(|c| *c = Settings::default())).build(k).key("settings-reset"));
    let right = col().key("settings-right").grow(1.0).min_w(0.0).h_full().child(scroll).child(hairline("foot-rule".into())).child(foot);
    let body = col()
        .key("settings-frame")
        .w_full()
        .child(hairline("head-rule".into()))
        .child(row().key("settings-body").w_full().h(560.0).child(nav).child(col().key("settings-split").w(1.0).h_full().fill(k.c.border_weak)).child(right));
    Dialog::new("settings", "Settings").open(true).fit().width(880.0).on_close(|s: &mut Collate| s.screen = Screen::Review).child(body).build(k, layer);
}

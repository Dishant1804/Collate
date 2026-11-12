//! The settings panel: sections down the left, each section's settings and their controls on
//! the right.
use rune::{Color, Cursor, El, Icon, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit, SegmentedControl, Select, SelectAppearance, Switch};

use crate::diff::DiffMode;
use crate::git::Scope;
use crate::settings::{Agent, CONTEXT_LINES, Channel, Density, Editor, FOLD_LARGE, FONT_SIZES, Motion, REFRESH_MS, Settings, Theme};
use crate::{ADD_PROJECT, AGENT_ICON, ALL_ICON, Collate, Screen, current_repo, grammars, lucide, project_labels, themes, tilde, update};

/// The width every settings control is right-aligned in, so each description wraps at the
/// same measure and rows of the same kind come out the same height.
const CONTROL_W: f32 = 272.0;
/// The theme dropdown and its preview, as wide as each other.
const THEME_W: f32 = 200.0;

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

/// The Languages section: whether to download grammars, what's built in, then every grammar on
/// offer as a pill. Installed ones are solid, with an × to remove them; the rest are dashed,
/// with a + that installs them on a click.
fn languages_settings(s: &Collate, k: &Kit) -> Vec<El> {
    let mut installed = row().key("lang-installed").w_full().gap(8.0).flex_wrap();
    let mut offered = row().key("lang-offered").w_full().gap(8.0).flex_wrap();
    let (mut any_installed, mut errors) = (false, col().key("lang-errors").w_full().gap(4.0));
    for g in grammars::CATALOGUE {
        let status = s.languages.status(g);
        let files = g.files.iter().map(|f| if f.contains('.') || f.chars().next().is_some_and(char::is_uppercase) { (*f).to_owned() } else { format!(".{f}") }).collect::<Vec<_>>().join(", ");
        let pill = row().key(format!("lang-{}", g.id)).h(28.0).px(10.0).gap(6.0).items_center().radius(14.0);
        let name = |color| text(g.title.to_owned()).font(k.ty.text_small).color(color);
        match status {
            grammars::Status::Installed | grammars::Status::Outdated => {
                any_installed = true;
                let outdated = status == grammars::Status::Outdated;
                let state = if outdated { "Update available" } else if s.languages.loaded.contains_key(g.id) { "In use" } else { "Installed" };
                let mut pill = pill.pr(4.0).border(1.0, if outdated { k.c.warning.icon } else { k.c.border_weak }).fill(k.alpha(k.c.text_base, 0.04))
                    .role("group").label(format!("{} - {state}", g.title)).help(format!("{files} - {state}")).child(name(k.c.text_base));
                if outdated {
                    pill = pill.child(icon_hit(k, &REFRESH_TAB_ICON, format!("Update {}", g.title), "Build the version this Collate expects", move |s: &mut Collate| s.prepare_language(g)));
                }
                installed = installed.child(pill.child(icon_hit(k, k.glyph.close, format!("Remove {}", g.title), "Delete the grammar; its files draw plain until it's installed again", move |s: &mut Collate| s.remove_language(g))));
            }
            grammars::Status::Working => {
                offered = offered.child(pill.border(1.0, k.c.border_weak).border_dash(4.0, 3.0).opacity(0.6)
                    .role("button").label(format!("Installing {}", g.title)).help(format!("{files} - installing…"))
                    .child(icon(&PLUS_ICON, 12.0).color(k.c.icon_muted).a11y_hidden()).child(name(k.c.text_muted)).child(text("…".to_owned()).font(k.ty.text_small).color(k.c.text_faint)));
            }
            grammars::Status::Available | grammars::Status::Failed(_) => {
                let failed = matches!(status, grammars::Status::Failed(_));
                if let grammars::Status::Failed(e) = &status {
                    errors = errors.child(text(format!("{} - {e}", g.title)).font(k.ty.text_small).color(k.c.danger.icon).wrap());
                }
                let edge = if failed { k.c.danger.icon } else { k.c.border_weak };
                offered = offered.child(pill.border(1.0, edge).border_dash(4.0, 3.0).hover(k.alpha(k.c.text_base, 0.06)).cursor(Cursor::Pointer)
                    .role("button").label(format!("{} {}", if failed { "Retry" } else { "Install" }, g.title))
                    .help(format!("{files} - {}", if failed { "try fetching and building it again" } else { "fetch and build this grammar" }))
                    .on_press(move |s: &mut Collate| s.prepare_language(g))
                    .child(icon(&PLUS_ICON, 12.0).color(k.c.icon_base).a11y_hidden()).child(name(k.c.text_muted)));
            }
        }
    }
    let section = |key: &str, title: &str, detail: &str, pills: El| col().key(key.to_owned()).w_full().py(14.0).gap(12.0)
        .child(col().gap(4.0).child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base)).child(text(detail.to_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap()))
        .child(pills);
    let mut rows = vec![
        setting(k, "download", "Download languages", &[], "Fetch a language's grammar from GitHub the first time a diff shows it; building it needs Xcode's command line tools", switch(k, "Download languages", s.settings.download_languages, |c, v| c.download_languages = v)),
        setting(k, "built-in", "Built in", &[], grammars::BUILT_IN, row()),
    ];
    if any_installed {
        rows.push(section("lang-installed-section", "Installed", "Hover one for the files it colours; × removes it", installed));
    }
    rows.push(section("lang-offered-section", "More languages", "Click one to fetch and build its grammar", offered.child(errors)));
    rows
}

/// A small icon that acts, inside a pill.
fn icon_hit<S: 'static>(k: &Kit, glyph: &'static Icon, label: String, help: &str, act: impl Fn(&mut S) + 'static) -> El {
    rune::el().size(20.0, 20.0).center().radius(10.0).hover(k.alpha(k.c.text_base, 0.08)).cursor(Cursor::Pointer)
        .role("button").label(label).help(help.to_owned()).on_press(act)
        .child(icon(glyph, 12.0).color(k.c.icon_muted).a11y_hidden())
}

/// The Agent section: which agent CLI Collate hands work to, how it runs it, and the files
/// that tell it what to ask and how to write.
fn agent_settings(s: &Collate, k: &Kit, menus: &mut Vec<El>) -> Vec<El> {
    let st = &s.settings;
    let edit = |key: &'static str, pick: &'static str| Button::new("Edit…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help("Open the file in the editor from Review settings").on_press(move |s: &mut Collate| s.agent_menu_pick(pick)).build(k).key(key);
    let runs = format!("{} in the terminal under the diff (⌘J); {} for what it fills in, like a commit message", st.agent.command(), st.agent.print_command());
    vec![
        setting(k, "agent", "Coding agent", &[], "Gets your comments, review questions, commit messages and conflicts; it never pushes", pick(s, k, menus, "agent", "Coding agent", &Agent::ALL.map(Agent::title), &Agent::ALL, st.agent, |c, v| c.agent = v)),
        setting(k, "agent-runs", "Runs as", &[], &runs, row()),
        setting(k, "agent-checkpoint", "Checkpoint first", &[], "The working tree is saved before the agent starts, to restore from Checkpoints in the sidebar", row()),
        setting(k, "agent-questions", "Review questions", &[], "What Review Change asks about the change in view, one a line", edit("edit-questions", "ask:edit")),
        setting(k, "agent-commit", "Commit message instructions", &[], "How the agent writes a message from the staged diff", edit("edit-commit-rules", "ask:commit-rules")),
    ]
}

/// The Updates section: the running version and how it stands, then how updates come.
fn update_settings(s: &Collate, k: &Kit, menus: &mut Vec<El>) -> Vec<El> {
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
        setting(k, "from", "Update from", &[], "Latest source builds the newest commit on main, which needs Rust", pick(s, k, menus, "from", "Update from", &["Releases", "Latest source"], &CHANNELS, st.update_from, |c, v| c.update_from = v)),
    ]
}

#[allow(clippy::too_many_arguments)]
fn pick<T: Copy + PartialEq + 'static>(s: &Collate, k: &Kit, menus: &mut Vec<El>, key: &'static str, title: &str, labels: &[&str], values: &'static [T], current: T, set: fn(&mut Settings, T)) -> El {
    let at = values.iter().position(|v| *v == current);
    // More than three segments crowd the row and read as tabs: past that, a dropdown.
    if labels.len() > 3 {
        let current = at.map(|i| labels[i]);
        return dropdown(s, k, menus, key, title, labels, current, move |s: &mut Collate, i| s.configure(|c| set(c, values[i])));
    }
    SegmentedControl::new(labels, at)
        .fit()
        .on_pick(move |s: &mut Collate, i, _: &mut Tx| s.configure(|c| set(c, values[i])))
        .build(k)
}

/// A settings dropdown, `THEME_W` wide, its menu as wide and opening under it. `key` names it
/// in `Collate::settings_menu` while it's open; `choose` gets the picked option's index.
#[allow(clippy::too_many_arguments)]
fn dropdown(s: &Collate, k: &Kit, menus: &mut Vec<El>, key: &'static str, title: &str, labels: &[&str], current: Option<&str>, choose: impl Fn(&mut Collate, usize) + Clone + 'static) -> El {
    let open = s.settings_menu == Some(key);
    let owned: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
    let select = Select::new(key, labels)
        .appearance(SelectAppearance::Field)
        .width(THEME_W)
        .current(current)
        .label(title.to_owned())
        .open(open)
        .on_toggle(move |s: &mut Collate| s.settings_menu = if s.settings_menu == Some(key) { None } else { Some(key) })
        .on_close(|s: &mut Collate| s.settings_menu = None)
        .on_choose(move |s: &mut Collate, v: &str, _: &mut Tx| {
            s.settings_menu = None;
            if let Some(i) = owned.iter().position(|l| l == v) {
                choose(s, i);
            }
        })
        .build(k, menus);
    // The kit's menu is at least 160 wide and ends on the trigger's right edge; as wide as the
    // trigger, it reads as the dropdown opening. The menu is the last thing the select layers.
    if open && let Some(menu) = menus.pop() {
        menus.push(menu.w(THEME_W));
    }
    select
}

/// The Theme setting: a dropdown of every theme, and under it a preview of the chosen one.
/// The preview paints from colours worked out once, so an open panel costs no more per frame.
fn theme_picker(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> El {
    let current = s.settings.theme;
    let swatches = themes::swatches();
    // A few lines of code in the theme's colours: accent and text, an added line, a removed one.
    let page = |[bg, fg, accent, added, removed]: [Color; 5]| {
        let line = |w: f32, c: Color| row().w(w).h(4.0).radius(2.0).fill(c);
        col()
            .grow(1.0)
            .h_full()
            .px(10.0)
            .py(12.0)
            .gap(6.0)
            .fill(bg)
            .child(row().gap(4.0).child(line(12.0, accent)).child(line(28.0, fg)))
            .child(row().gap(4.0).child(line(8.0, fg)).child(line(32.0, added)))
            .child(row().gap(4.0).child(line(8.0, fg)).child(line(22.0, removed)))
    };
    let i = Theme::ALL.iter().position(|t| *t == current).unwrap_or(0);
    let mut preview = row().h(64.0).clip_rounded(k.radius.md - 1.0).child(page(swatches[i]));
    if current == Theme::System {
        // System is Light or Dark as macOS is: show both.
        preview = preview.child(page(swatches[2]));
    }
    let preview = row().key("theme-preview").w(THEME_W).padding(1.0).radius(k.radius.md).fill(k.c.border_weak).a11y_hidden().child(preview.grow(1.0));
    let names = Theme::ALL.map(Theme::name);
    let select = dropdown(s, k, layer, "theme", "Theme", &names, Some(current.name()), |s: &mut Collate, i| s.configure(|c| c.theme = Theme::ALL[i]));
    // Top-aligned rather than centred like the other rows, so the name sits by the dropdown
    // and the preview hangs below.
    let words = col().grow(1.0).min_w(0.0).gap(4.0).child(text("Theme".to_owned()).font(k.ty.ui_medium).color(k.c.text_base)).child(text("System follows macOS between Light and Dark".to_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap());
    row().key("theme").w_full().py(14.0).gap(24.0).items_start().child(words).child(col().min_w(CONTROL_W).gap(8.0).items_end().child(select).child(preview))
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
lucide!(PLUS_ICON, "collate-plus", r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#);
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
    static DENSITIES: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];
    static MOTIONS: [Motion; 2] = [Motion::Default, Motion::Reduced];
    static LAYOUTS: [DiffMode; 2] = [DiffMode::Unified, DiffMode::Split];
    static NUMBERS: [bool; 2] = [true, false];
    let tab = s.settings_tab.min(SETTINGS_TABS.len() - 1);
    // A dropdown's menu is placed by its trigger, which must be laid out first: menus go on
    // the layer after the dialog, or the menu can't find the trigger and opens at the corner.
    let mut menu_layer = Vec::new();
    let menus = &mut menu_layer;
    let rows: Vec<El> = match tab {
        0 => vec![
            theme_picker(s, k, menus),
            setting(k, "font", "Code size", &[], "In points, for code and line numbers", pick(s, k, menus, "font", "Code size", &["11", "12", "13", "14", "16"], &FONT_SIZES, st.font_size, |c, v| c.font_size = v)),
            setting(k, "density", "Line spacing", &[], "", pick(s, k, menus, "density", "Line spacing", &["Compact", "Comfortable", "Spacious"], &DENSITIES, st.density, |c, v| c.density = v)),
            setting(k, "motion", "Animations", &[], "Reduced plays every animation in half the time", pick(s, k, menus, "motion", "Animations", &["Default", "Reduced"], &MOTIONS, st.motion, |c, v| c.motion = v)),
        ],
        1 => vec![
            setting(k, "layout", "Layout", &["S"], "Split shows old and new side by side", pick(s, k, menus, "layout", "Layout", &["Unified", "Split"], &LAYOUTS, st.layout, |c, v| c.layout = v)),
            setting(k, "numbers", "Line numbers", &[], "Unified view: new numbers only, or both", pick(s, k, menus, "numbers", "Line numbers", &["One column", "Old and new"], &NUMBERS, st.one_number, |c, v| c.one_number = v)),
            setting(k, "context", "Context lines", &[], "Unchanged lines kept around each change", pick(s, k, menus, "context", "Context lines", &["3", "10", "25", "Full file"], &CONTEXT_LINES, st.context, |c, v| c.context = v)),
            setting(k, "ws", "Ignore whitespace", &["⇧", "W"], "Hides indent-only changes; stage whole files while on", switch(k, "Ignore whitespace", st.ignore_whitespace, |c, v| c.ignore_whitespace = v)),
            setting(k, "moved", "Highlight moved code", &["M"], "Moved blocks show in blue, M jumps between their ends", switch(k, "Highlight moved code", st.show_moved, |c, v| c.show_moved = v)),
            setting(k, "wrap", "Wrap long lines", &["W"], "Off scrolls them sideways; split view always wraps", switch(k, "Wrap long lines", st.wrap, |c, v| c.wrap = v)),
            setting(k, "markers", "+ and − markers", &[], "A sign beside each changed line", switch(k, "Markers", st.markers, |c, v| c.markers = v)),
            setting(k, "words", "Word highlights", &[], "Tint the words that changed within a line", switch(k, "Word highlights", st.word_diff, |c, v| c.word_diff = v)),
            setting(k, "syntax", "Syntax highlighting", &[], "", switch(k, "Syntax highlighting", st.syntax, |c, v| c.syntax = v)),
        ],
        2 => languages_settings(s, k),
        3 => vec![
            setting(k, "scope", "Open to", &[], "The view a new window starts on", pick(s, k, menus, "scope", "Open to", &["All", "Staged", "Unstaged", "PR Preview", "Git Graph"], &Scope::ALL, st.default_scope, |c, v| c.default_scope = v)),
            setting(k, "editor", "Open files in", &["O"], "Where Open in Editor sends a file", pick(s, k, menus, "editor", "Open files in", &["Default", "Zed", "VS Code", "Cursor"], &Editor::ALL, st.editor, |c, v| c.editor = v)),
            setting(k, "collapse", "Fold viewed files", &["V"], "Marking a file viewed folds it away", switch(k, "Fold viewed files", st.collapse_viewed, |c, v| c.collapse_viewed = v)),
            setting(k, "large", "Fold large files", &[], "Files above this many changed lines start folded", pick(s, k, menus, "large", "Fold large files", &["Never", "300", "1000", "3000"], &FOLD_LARGE, st.fold_large, |c, v| c.fold_large = v)),
            setting(k, "notify", "Review requests", &[], "A notification when someone asks for your review, checked every few minutes", switch(k, "Review requests", st.notify_reviews, |c, v| c.notify_reviews = v)),
            setting(k, "low", "Hide low-signal files", &[], "Lockfiles, bundles, snapshots and generated files", switch(k, "Hide low-signal files", st.hide_low_signal, |c, v| c.hide_low_signal = v)),
            setting(k, "order", "Source files first", &[], "Then tests, then lockfiles and generated files", switch(k, "Source files first", st.source_first, |c, v| c.source_first = v)),
            setting(k, "git-tool", "Git's diff and merge tool", &[], "git difftool and git mergetool open Collate", Button::new("Use Collate").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
                .help("Sets diff.tool and merge.tool in your global git config").on_press(|s: &mut Collate| s.register_git_tool()).build(k).key("register-tool")),
        ],
        AGENT_TAB => agent_settings(s, k, menus),
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
            setting(k, "every", "Check every", &[], "Only when file events aren't available", pick(s, k, menus, "every", "Check every", &["0.5 s", "1 s", "2 s", "5 s"], &REFRESH_MS, st.refresh_ms, |c, v| c.refresh_ms = v)),
        ],
        _ => update_settings(s, k, menus),
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
    Dialog::new("settings", "Settings").open(true).fit().width(880.0).on_close(|s: &mut Collate| {
        s.screen = Screen::Review;
        s.settings_menu = None;
    }).child(body).build(k, layer);
    layer.append(&mut menu_layer);
}

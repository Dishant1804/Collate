//! Every command the review offers, once: the menu bar is built from this list, so each
//! action can be found by name there (and in Help's search) with its key beside it.
use std::cell::RefCell;

use rune::{MenuError, MenuItem, Tx};

use crate::diff::DiffMode;
use crate::git::Scope;
use crate::{Collate, HANDLE, Screen, keys, refresh};

pub struct Command {
    /// Unique; empty for a separator.
    pub id: &'static str,
    pub title: &'static str,
    /// The menu bar menu it's in; "" is the app menu.
    pub menu: &'static str,
    /// Its key as `rune::Shortcut` parses it (`j`, `shift+a`, `cmd+shift+g`), or "".
    pub key: &'static str,
    pub run: fn(&mut Collate, &mut Tx),
    pub enabled: fn(&Collate) -> bool,
    pub checked: fn(&Collate) -> bool,
}

const fn cmd(id: &'static str, title: &'static str, menu: &'static str, key: &'static str, enabled: fn(&Collate) -> bool, run: fn(&mut Collate, &mut Tx)) -> Command {
    Command { id, title, menu, key, run, enabled, checked: never }
}

const fn check(c: Command, checked: fn(&Collate) -> bool) -> Command {
    Command { checked, ..c }
}

const fn sep(menu: &'static str) -> Command {
    cmd("", "", menu, "", never, |_, _| {})
}

fn never(_: &Collate) -> bool {
    false
}
fn always(_: &Collate) -> bool {
    true
}

/// Nothing modal is up: the review takes keys and commands.
fn ready(s: &Collate) -> bool {
    s.has_project && s.screen != Screen::Settings && !s.modal_open()
}
/// The diff is shown, not the Map or the pull request list.
fn diffing(s: &Collate) -> bool {
    (ready(s) || (s.loose.is_some() && !s.modal_open())) && !s.map.open && !s.pulls.open
}
/// The diff shown can be staged and discarded.
fn editable(s: &Collate) -> bool {
    diffing(s) && s.loose.is_none() && !s.scope.read_only()
}
/// A repository change can start.
fn idle(s: &Collate) -> bool {
    ready(s) && s.loose.is_none() && s.busy.is_none()
}
fn shows(s: &Collate, scope: Scope) -> bool {
    s.scope == scope && !s.map.open && !s.pulls.open && s.pulls.pr.is_none()
}

fn letter(s: &mut Collate, key: &str, tx: &mut Tx) {
    keys::letter(s, key, tx);
}

pub const COMMANDS: &[Command] = &[
    cmd("settings", "Settings…", "", "cmd+comma", always, |s, _| s.screen = Screen::Settings),
    cmd("add-project", "Add Project…", "File", "cmd+o", |s| !s.picking, |s, _| s.add_project()),
    cmd("reread", "Reread Repository", "File", "cmd+r", ready, |_, _| {
        refresh();
    }),
    check(cmd("view-all", "All Changes", "View", "1", ready, |s, _| s.show_scope(Scope::Uncommitted)), |s| shows(s, Scope::Uncommitted)),
    check(cmd("view-staged", "Staged", "View", "2", ready, |s, _| s.show_scope(Scope::Staged)), |s| shows(s, Scope::Staged)),
    check(cmd("view-unstaged", "Unstaged", "View", "3", ready, |s, _| s.show_scope(Scope::Unstaged)), |s| shows(s, Scope::Unstaged)),
    check(cmd("view-branch", "Pull Request Preview", "View", "4", ready, |s, _| s.show_scope(Scope::Branch)), |s| shows(s, Scope::Branch)),
    check(cmd("view-history", "Git Graph", "View", "5", ready, |s, _| s.show_scope(Scope::History)), |s| shows(s, Scope::History)),
    check(cmd("view-pulls", "Pull Requests", "View", "", ready, |s, _| s.open_pulls()), |s| s.pulls.open || s.pulls.pr.is_some()),
    check(cmd("view-map", "Map", "View", "", ready, |s, _| {
        s.pulls.open = false;
        s.open_map();
    }), |s| s.map.open),
    sep("View"),
    check(cmd("split", "Side by Side", "View", "s", diffing, |s, tx| letter(s, "s", tx)), |s| s.settings.layout == DiffMode::Split),
    check(cmd("wrap", "Wrap Long Lines", "View", "w", diffing, |s, tx| letter(s, "w", tx)), |s| s.settings.wrap),
    check(cmd("whitespace", "Hide Whitespace Changes", "View", "shift+w", diffing, |s, tx| letter(s, "W", tx)), |s| s.settings.ignore_whitespace),
    check(cmd("moved", "Highlight Moved Code", "View", "", diffing, |s, _| s.configure(|c| c.show_moved = !c.show_moved)), |s| s.settings.show_moved),
    check(cmd("blame", "Inline Blame", "View", "b", |s| diffing(s) && s.loose.is_none() && s.diff.mode() == DiffMode::Unified, |s, tx| letter(s, "b", tx)), |s| s.diff.blame_shown()),
    sep("View"),
    cmd("collapse", "Collapse All Files", "View", "z", diffing, |s, tx| letter(s, "z", tx)),
    cmd("expand", "Expand All Files", "View", "shift+z", diffing, |s, tx| letter(s, "Z", tx)),
    sep("View"),
    check(cmd("sidebar", "Sidebar", "View", "cmd+shift+b", ready, |s, _| s.sidebar.toggle()), |s| s.sidebar.is_open()),
    check(cmd("panel", "Staging Panel", "View", "cmd+alt+0", ready, |s, _| s.inspector.toggle()), |s| s.inspector.is_open()),
    check(cmd("terminal", "Terminal", "View", "cmd+j", |s| s.has_project && !s.modal_open(), |s, _| s.toggle_terminal()), |s| s.terminals.open),
    cmd("next-change", "Next Change", "Go", "j", diffing, |s, tx| letter(s, "j", tx)),
    cmd("prev-change", "Previous Change", "Go", "k", diffing, |s, tx| letter(s, "k", tx)),
    cmd("next-file", "Next File", "Go", "bracketright", diffing, |s, tx| letter(s, "]", tx)),
    cmd("prev-file", "Previous File", "Go", "bracketleft", diffing, |s, tx| letter(s, "[", tx)),
    cmd("moved-twin", "Other End of the Move", "Go", "m", diffing, |s, tx| letter(s, "m", tx)),
    sep("Go"),
    cmd("go-file", "Go to File…", "Go", "cmd+p", ready, |s, _| s.start_jump("")),
    cmd("go-line", "Go to Line…", "Go", "cmd+l", ready, |s, _| s.start_jump(":")),
    cmd("back", "Back", "Go", "cmd+bracketleft", ready, |s, _| s.go(true)),
    cmd("forward", "Forward", "Go", "cmd+bracketright", ready, |s, _| s.go(false)),
    sep("Go"),
    cmd("older", "Older Commit", "Go", "shift+comma", |s| ready(s) && shows(s, Scope::History), |s, tx| letter(s, "<", tx)),
    cmd("newer", "Newer Commit", "Go", "shift+period", |s| ready(s) && shows(s, Scope::History), |s, tx| letter(s, ">", tx)),
    sep("Go"),
    cmd("find", "Find…", "Go", "cmd+f", ready, |s, _| s.focus_search()),
    cmd("find-next", "Find Next", "Go", "cmd+g", ready, |s, tx| s.next_hit(true, tx)),
    cmd("find-prev", "Find Previous", "Go", "cmd+shift+g", ready, |s, tx| s.next_hit(false, tx)),
    check(cmd("find-case", "Match Case", "Go", "cmd+alt+c", ready, |s, _| s.search_option(|s| s.find_case ^= true)), |s| s.find_case),
    check(cmd("find-word", "Whole Words", "Go", "cmd+alt+w", ready, |s, _| s.search_option(|s| s.find_word ^= true)), |s| s.find_word),
    check(cmd("find-regex", "Regular Expression", "Go", "cmd+alt+r", ready, |s, _| s.search_option(|s| s.find_regex ^= true)), |s| s.find_regex),
    cmd("stage", "Stage Lines or Hunk", "Changes", "a", editable, |s, tx| letter(s, "a", tx)),
    cmd("unstage", "Unstage Lines or Hunk", "Changes", "u", editable, |s, tx| letter(s, "u", tx)),
    cmd("discard", "Discard Lines or Hunk…", "Changes", "x", editable, |s, tx| letter(s, "x", tx)),
    cmd("pick-words", "Pick Word Changes…", "Changes", "p", editable, |s, tx| letter(s, "p", tx)),
    sep("Changes"),
    cmd("stage-file", "Stage File", "Changes", "shift+a", |s| editable(s) && s.scope != Scope::Staged, |s, tx| letter(s, "A", tx)),
    cmd("unstage-file", "Unstage File", "Changes", "shift+u", editable, |s, tx| letter(s, "U", tx)),
    cmd("discard-file", "Discard File…", "Changes", "shift+x", editable, |s, tx| letter(s, "X", tx)),
    sep("Changes"),
    cmd("stage-all", "Stage All", "Changes", "", |s| idle(s) && s.unstaged > 0, |s, _| s.stage_all()),
    cmd("unstage-all", "Unstage All", "Changes", "", |s| idle(s) && s.staged > 0, |s, _| s.unstage_all()),
    cmd("commit", "Commit", "Changes", "cmd+enter", |s| idle(s) && (s.staged > 0 || s.amend), |s, _| s.commit()),
    check(cmd("amend", "Amend Last Commit", "Changes", "", |s| idle(s) && s.head.is_some() && !s.committing, |s, _| s.toggle_amend()), |s| s.amend),
    cmd("undo", "Undo Last Commit, Merge or Switch", "Changes", "", |s| idle(s) && s.last_move.is_some(), |s, _| s.undo_move()),
    sep("Changes"),
    check(cmd("viewed", "Mark File Viewed", "Changes", "v", |s| diffing(s) && s.loose.is_none(), |s, tx| letter(s, "v", tx)), |s| s.is_viewed(s.diff.visible_file())),
    cmd("comment", "Comment on Line…", "Changes", "c", |s| diffing(s) && s.loose.is_none(), |s, tx| letter(s, "c", tx)),
    cmd("copy-ref", "Copy Line Reference", "Changes", "y", diffing, |s, tx| letter(s, "y", tx)),
    cmd("copy-lines", "Copy Lines for an Agent", "Changes", "shift+y", diffing, |s, tx| letter(s, "Y", tx)),
    cmd("copy-comments", "Copy Comments as Markdown", "Changes", "", |s| !s.comments.is_empty(), |s, tx| s.copy_comments(tx)),
    cmd("open", "Open in Editor", "Changes", "o", |s| diffing(s) && s.loose.is_none(), |s, tx| letter(s, "o", tx)),
    cmd("fetch", "Fetch", "Branch", "", idle, |s, _| s.branch_do("fetch")),
    cmd("pull", "Pull", "Branch", "", idle, |s, _| s.branch_do("pull")),
    cmd("push", "Push", "Branch", "", idle, |s, _| s.branch_do("push")),
    cmd("unpushed", "Show Unpushed Commits", "Branch", "", |s| idle(s) && s.ahead_behind.is_some_and(|(a, _)| a > 0), |s, _| s.branch_do("unpushed")),
    sep("Branch"),
    cmd("new-branch", "New Branch…", "Branch", "", idle, |s, _| s.branch_do("new-branch")),
    cmd("stash", "Stash All Changes…", "Branch", "", |s| idle(s) && s.total > 0, |s, _| s.branch_do("stash")),
    cmd("new-tag", "New Tag at HEAD…", "Branch", "", idle, |s, _| s.branch_do("new-tag")),
    sep("Branch"),
    cmd("compare", "Compare Branches or Tags…", "Branch", "", idle, |s, _| s.branch_do("compare")),
    cmd("ask", "Ask the Agent…", "Agent", "i", |s| diffing(s) && s.loose.is_none(), |s, tx| letter(s, "i", tx)),
    cmd("review", "Review the Change", "Agent", "", |s| diffing(s) && s.loose.is_none(), |s, _| s.ask_checklist(None)),
    cmd("send-comments", "Send Comments", "Agent", "", |s| ready(s) && !s.comments.is_empty(), |s, _| s.send_comments()),
    cmd("write-message", "Write the Commit Message", "Agent", "", |s| idle(s) && s.staged > 0 && !s.writing_message, |s, _| s.write_message()),
    cmd("checkpoint", "Save a Checkpoint", "Agent", "", idle, |s, _| s.save_checkpoint()),
    cmd("keys", "Keyboard Shortcuts", "Help", "shift+slash", |s| ready(s) || !s.has_project, |s, _| s.show_help = true),
];

thread_local! {
    /// Each command's enabled and checked state as the menu bar last got it.
    static SHOWN: RefCell<Option<Vec<(bool, bool)>>> = const { RefCell::new(None) };
}

/// Gives the menu bar the commands' current state when it changed. Called each frame.
pub fn sync_menu(s: &mut Collate) {
    let Some(app) = HANDLE.get() else { return };
    let now: Vec<(bool, bool)> = COMMANDS.iter().map(|c| ((c.enabled)(s), (c.checked)(s))).collect();
    if SHOWN.with_borrow(|shown| shown.as_ref() == Some(&now)) {
        return;
    }
    let items = COMMANDS.iter().zip(&now).map(|(c, &(enabled, checked))| {
        let item = if c.id.is_empty() { MenuItem::separator() } else { MenuItem::new(c.title, c.run).id(c.id).enabled(enabled).checked(checked) };
        let item = if c.menu.is_empty() { item } else { item.menu(c.menu) };
        // The window's own keys come first: a field typing the letter, the terminal, a dialog.
        match c.key.parse() {
            Ok(key) if !c.key.is_empty() => item.key_equivalent(key).keys_first(),
            _ => item,
        }
    });
    match app.set_menu(items) {
        // Not up yet: the next frame tries again.
        Err(MenuError::NotRunning) => return,
        Err(e) => s.say(format!("Couldn't update the menu bar: {e}"), true),
        Ok(()) => {}
    }
    SHOWN.set(Some(now));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_unique_id_and_a_key_the_menu_takes() {
        let mut ids = std::collections::HashSet::new();
        for c in COMMANDS.iter().filter(|c| !c.id.is_empty()) {
            assert!(ids.insert(c.id), "{} twice", c.id);
            assert!(c.key.is_empty() || c.key.parse::<rune::Shortcut>().is_ok(), "{}: {}", c.id, c.key);
        }
    }
}

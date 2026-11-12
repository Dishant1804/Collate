//! The keyboard: what each key does in each place, for the keys the focused control leaves
//! alone.
use rune::{Key, KeyEvent, Tx};
use rune_kit::shell;

use crate::diff::DiffMode;
use crate::git::{Scope, Staged};
use crate::merge::Take;
use crate::{Collate, Screen, merge_view, refresh};

pub fn on_key(s: &mut Collate, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command(',') {
        s.screen = if s.screen == Screen::Settings { Screen::Review } else { Screen::Settings };
        return true;
    }
    // The settings panel is modal: it takes its own keys (Escape closes it).
    if s.screen == Screen::Settings {
        return false;
    }
    if s.jumping {
        match &ev.key {
            Key::Escape => s.jumping = false,
            Key::Enter => s.jump_to(None, tx),
            Key::Up => s.jump_pick = s.jump_pick.saturating_sub(1),
            Key::Down => s.jump_pick = (s.jump_pick + 1).min(s.jump_matches().0.len().saturating_sub(1)),
            Key::Backspace => {
                s.jump.pop();
                s.jump_pick = 0;
            }
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl => {
                s.jump.push_str(t);
                s.jump_pick = 0;
            }
            _ => return ev.is_command('p') || ev.is_command('l'),
        }
        return true;
    }
    // The merge editor: n and p move between conflicts, 1, 2 and 3 take ours, theirs or both.
    if s.merge.is_some() {
        if ev.mods.cmd || ev.mods.ctrl || ev.mods.alt {
            return false;
        }
        match &ev.key {
            Key::Text(t) if t == "n" => merge_view::step(s, true, tx),
            Key::Text(t) if t == "p" => merge_view::step(s, false, tx),
            Key::Text(t) if t == "1" => merge_view::take(s, Take::Ours, tx),
            Key::Text(t) if t == "2" => merge_view::take(s, Take::Theirs, tx),
            Key::Text(t) if t == "3" => merge_view::take(s, Take::Both, tx),
            _ => return false,
        }
        return true;
    }
    // A dialog takes its own keys (Escape closes it); nothing else acts behind it.
    if s.dialog_open() {
        return false;
    }
    // The shortcuts sheet: typing searches it. Its dialog takes Escape.
    if s.show_help {
        match &ev.key {
            Key::Backspace => {
                s.help_query.pop();
            }
            Key::Text(t) if t == "?" && s.help_query.is_empty() => s.show_help = false,
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl => s.help_query.push_str(t),
            _ => return false,
        }
        return true;
    }
    if ev.is_command('j') && s.has_project {
        s.toggle_terminal();
        return true;
    }
    // ⌘W ends the open drawer's shell, as Terminal closes a tab before its window; the last
    // one hides the drawer. Untaken, it closes the window.
    if ev.is_command('w') && !ev.mods.alt && s.terminals.open {
        s.end_terminal();
        return true;
    }
    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.mods.alt && ev.is_command('0') {
        s.inspector.toggle();
        return true;
    }
    // Back and forward, as in a browser.
    if ev.is_command('[') || ev.is_command(']') {
        s.go(ev.is_command('['));
        return true;
    }
    if ev.mods.alt && ev.mods.cmd {
        let flip: Option<fn(&mut Collate)> = match ev.key {
            Key::Command('c') => Some(|s| s.find_case ^= true),
            Key::Command('w') => Some(|s| s.find_word ^= true),
            Key::Command('r') => Some(|s| s.find_regex ^= true),
            _ => None,
        };
        if let Some(flip) = flip {
            s.search_option(flip);
            return true;
        }
    }
    if ev.is_command('r') {
        return refresh();
    }
    if ev.key == Key::Enter && ev.mods.cmd {
        s.commit();
        return true;
    }
    // Fields take their own ⌘Z first, for their text.
    if ev.is_command('z') && !ev.mods.shift && s.has_project && tx.focused().is_none_or(|f| !(f.contains("filter") || f.contains("message") || f.contains("search"))) {
        s.undo_move();
        return true;
    }
    if ev.is_command('o') {
        s.add_project();
        return true;
    }
    // ⌘K too, as in Slack and Linear; in a focused terminal it clears, as Terminal does, and
    // the terminal takes it before this.
    if (ev.is_command('f') || ev.is_command('k')) && s.has_project {
        s.focus_search();
        return true;
    }
    if ev.is_command('p') && s.has_project {
        s.start_jump("");
        return true;
    }
    if ev.is_command('l') && s.has_project {
        s.start_jump(":");
        return true;
    }
    if s.map.open && ev.key == Key::Escape && tx.focused().is_none_or(|f| !f.contains("filter")) && s.map_up() {
        return true;
    }
    if s.scope == Scope::History && !ev.mods.cmd && tx.focused().is_none_or(|f| !f.contains("filter")) {
        match ev.key {
            Key::Enter if !s.history_diff && s.commit.is_some() => {
                s.history_diff = true;
                return true;
            }
            Key::Escape if s.history_diff => {
                s.close_commit_diff(tx);
                return true;
            }
            _ => {}
        }
    }
    if ev.is_command('g') {
        s.next_hit(!ev.mods.shift, tx);
        return true;
    }
    // Typing in a field isn't a shortcut.
    if tx.focused().is_some_and(|f| f.contains("filter") || f.contains("message") || f.contains("search")) {
        return false;
    }
    let Key::Text(t) = &ev.key else { return s.diff.key(ev, tx) };
    if ev.mods.cmd || ev.mods.ctrl || ev.mods.alt {
        return s.diff.key(ev, tx);
    }
    letter(s, t, tx)
}

/// Says why staging key `t` does nothing for file `f` in this view, so the key isn't just
/// ignored.
fn not_here(s: &mut Collate, t: &str, f: usize) {
    let staged = s.info.get(f).map_or(Staged::No, |i| i.staged);
    let why = match (t, s.scope) {
        (_, Scope::Branch) => "Branch shows what the branch committed: nothing in it can be staged or discarded. All Changes (1) can.",
        (_, Scope::History) => "History shows commits: nothing in it can be staged or discarded. All Changes (1) can.",
        ("a" | "A", Scope::Staged) => "That's staged already; u unstages it",
        ("u" | "U", Scope::Unstaged) => "Nothing here is staged; Staged (2) shows what is",
        ("u", Scope::Uncommitted) if staged == Staged::Partly => "Part of this file is staged: unstage those lines in Staged (2), or the whole file with U",
        ("u", Scope::Uncommitted) => "Nothing of this file is staged",
        _ => return,
    };
    s.say(why, false);
}

/// What a key without ⌘, ⌃ or ⌥ does in the review, whether typed or picked from the menu bar,
/// which names the same keys. False when it does nothing here.
pub fn letter(s: &mut Collate, t: &str, tx: &mut Tx) -> bool {
    let hunk = s.diff.current_hunk();
    let file = s.diff.visible_file();
    // The diff's own key handler steps aside while the tree has focus; these work from either.
    match t {
        "j" => s.diff.next_hunk(true, tx),
        "k" => s.diff.next_hunk(false, tx),
        "]" => s.diff.next_file(true, tx),
        "[" => s.diff.next_file(false, tx),
        "a" | "u" | "x" => {
            // Selected lines first, else the hunk at the top.
            let (f, h, picked) = match s.diff.selected_changes() {
                Some((f, h, p)) => (f, h, Some(p)),
                None => match hunk {
                    Some((f, h)) => (f, h, None),
                    None => {
                        let verb = match t { "a" => "stage", "u" => "unstage", _ => "discard" };
                        s.say(format!("No change at or below the top of the view to {verb}"), false);
                        return true;
                    }
                },
            };
            if s.settings.ignore_whitespace && t != "x" {
                s.say("Whitespace is hidden, so hunks don't match the file; stage whole files (A) or show whitespace (W).", true);
                return true;
            }
            match (t, s.scope) {
                ("a", Scope::Uncommitted | Scope::Unstaged) => s.stage_hunk(f, h, picked),
                ("u", Scope::Staged) => s.unstage_hunk(f, h, picked),
                ("u", Scope::Uncommitted) if s.info.get(f).is_some_and(|i| i.staged == Staged::Fully) => s.unstage_hunk(f, h, picked),
                ("x", _) => s.ask_discard(f, Some(h)),
                _ => not_here(s, t, f),
            }
            true
        }
        "n" | "N" => {
            s.next_hit(t == "n", tx);
            true
        }
        "p" => {
            s.pick_words();
            true
        }
        "m" => {
            if !s.diff.jump_to_twin(tx) {
                s.say("No moved lines here", false);
            }
            true
        }
        "b" => {
            s.toggle_inline_blame();
            true
        }
        "z" => {
            s.fold_all(true, tx);
            true
        }
        "Z" => {
            s.fold_all(false, tx);
            true
        }
        "W" => {
            s.configure(|c| c.ignore_whitespace = !c.ignore_whitespace);
            s.say(if s.settings.ignore_whitespace { "Hiding whitespace changes" } else { "Showing whitespace changes" }, false);
            true
        }
        "?" => {
            s.open_help();
            true
        }
        "A" | "U" if s.scope.read_only() || (t == "A" && s.scope == Scope::Staged) => {
            not_here(s, t, file);
            true
        }
        "A" => {
            s.stage_file(file);
            true
        }
        "U" => {
            s.unstage_file(file);
            true
        }
        "X" => {
            s.ask_discard(file, None);
            true
        }
        "o" => {
            s.open_file(file);
            true
        }
        "c" => {
            s.ask_comment();
            true
        }
        "i" => {
            s.ask_question();
            true
        }
        "y" | "Y" => {
            s.copy_selection(t == "Y", tx);
            true
        }
        "v" => {
            s.toggle_viewed(file, tx);
            true
        }
        "s" => {
            tx.motion(rune::tok::motion::SNAPPY);
            s.configure(|c| c.layout = if c.layout == DiffMode::Split { DiffMode::Unified } else { DiffMode::Split });
            true
        }
        "w" => {
            s.configure(|c| c.wrap = !c.wrap);
            true
        }
        "<" if s.scope == Scope::History => {
            s.step_commit(1, tx);
            true
        }
        ">" if s.scope == Scope::History => {
            s.step_commit(-1, tx);
            true
        }
        "<" | ">" => {
            s.say("< and > step through commits in History (5)", false);
            true
        }
        "1" | "2" | "3" | "4" | "5" => {
            s.show_scope(Scope::ALL[t.parse::<usize>().unwrap() - 1]);
            true
        }
        "r" => refresh(),
        _ => false,
    }
}

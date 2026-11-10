//! The keyboard: what each key does in each place, for the keys the focused control leaves
//! alone.
use rune::{Key, KeyEvent, Tx};
use rune_kit::shell;

use crate::diff::DiffMode;
use crate::git::{Scope, Staged};
use crate::{Collate, Screen, refresh};

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
    // A dialog takes its own keys (Escape closes it); nothing else acts behind it.
    if s.confirm.is_some() || s.words.is_some() || s.prompt.is_some() || s.risky.is_some() || s.compare.is_some() || s.blame.is_some() || s.file_log.is_some() {
        return false;
    }
    if s.show_help {
        if matches!(ev.key, Key::Escape) || matches!(&ev.key, Key::Text(t) if t == "?") {
            s.show_help = false;
            return true;
        }
        return false;
    }
    if ev.is_command('j') && s.has_project {
        s.toggle_terminal();
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
    let hunk = s.diff.current_hunk();
    let file = s.diff.visible_file();
    // The diff's own key handler steps aside while the tree has focus; these work from either.
    match t.as_str() {
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
                    None => return false,
                },
            };
            if s.settings.ignore_whitespace && t != "x" {
                s.say("Whitespace is hidden, so hunks don't match the file; stage whole files (A) or show whitespace (W).", true);
                return true;
            }
            match (t.as_str(), s.scope) {
                ("a", Scope::Uncommitted | Scope::Unstaged) => s.stage_hunk(f, h, picked),
                ("u", Scope::Staged) => s.unstage_hunk(f, h, picked),
                ("u", Scope::Uncommitted) if s.info.get(f).is_some_and(|i| i.staged == Staged::Fully) => s.unstage_hunk(f, h, picked),
                ("x", _) => s.ask_discard(f, Some(h)),
                _ => return false,
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
            s.show_help = true;
            true
        }
        "A" if !s.scope.read_only() && s.scope != Scope::Staged => {
            s.stage_file(file);
            true
        }
        "U" if !s.scope.read_only() => {
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
        "1" | "2" | "3" | "4" | "5" => {
            s.map.open = false;
            s.set_scope(Scope::ALL[t.parse::<usize>().unwrap() - 1]);
            true
        }
        "r" => refresh(),
        _ => false,
    }
}

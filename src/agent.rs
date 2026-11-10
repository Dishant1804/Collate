//! Handing work to the reader's coding agent: the selected lines as a reference or as a
//! prompt-ready excerpt. Collate makes the text; the agent the reader already has does the rest.
use rune::{PopupItem, PopupMenu, Tx};

use crate::diff::Side;
use crate::{Collate, Scope, abbrev};

impl Collate {
    /// The selection as `path#L10-L25` (`path#L10` for one line), as editors and agents read
    /// it. Lines on the old side name the old path and say so, since the file now differs.
    pub fn selection_reference(&self) -> Option<String> {
        let sel = self.diff.selection()?;
        let file = self.diff.files().get(sel.file)?;
        let (side, a, b) = self.diff.selected_span()?;
        let lines = if a == b { format!("L{a}") } else { format!("L{a}-L{b}") };
        Some(match side {
            Side::New => format!("{}#{lines}", file.path()),
            Side::Old => format!("{}#{lines} (before the change)", file.old_path.as_deref().unwrap_or(file.path())),
        })
    }

    /// The selection for an agent: its reference, what the view compares, and the lines as a
    /// diff, so a pasted prompt needs no more context to find and read them.
    pub fn selection_for_agent(&self) -> Option<String> {
        let reference = self.selection_reference()?;
        let lines = self.diff.selected_diff()?;
        Some(format!("`{reference}`, in {}:\n\n```diff\n{lines}\n```", self.compared_text()))
    }

    /// What the current view compares, in git's terms, for text an agent reads.
    fn compared_text(&self) -> String {
        match self.scope {
            Scope::Uncommitted => "the uncommitted changes (HEAD to the working tree)".into(),
            Scope::Staged => "the staged changes (HEAD to the index)".into(),
            Scope::Unstaged => "the unstaged changes (the index to the working tree)".into(),
            Scope::Branch => format!("this branch's changes against {}", self.base.as_deref().unwrap_or("its base")),
            Scope::History => match self.commit.as_deref().map(|c| (c, c.split_once(".."))) {
                Some((_, Some((a, b)))) if a.strip_suffix('^') == Some(b) => format!("commit {}", abbrev(b)),
                Some((_, Some((a, b)))) => format!("the changes {}..{}", abbrev(a), abbrev(b)),
                Some((c, None)) => format!("commit {}", abbrev(c)),
                None => "the history".into(),
            },
        }
    }

    /// Copies the selection as a reference (`agent` false) or as an excerpt for an agent.
    pub fn copy_selection(&mut self, agent: bool, tx: &mut Tx) {
        let got = if agent { self.selection_for_agent() } else { self.selection_reference() };
        let Some(text) = got else {
            self.say("Click a line number first: copying takes the selected lines", true);
            return;
        };
        let said = if agent { "Copied the lines for an agent".to_owned() } else { format!("Copied {text}") };
        tx.copy(text);
        self.say(said, false);
    }

    /// The hunk header's Copy menu, anchored at its `copy-sel` button.
    pub fn copy_menu(&mut self, tx: &mut Tx) {
        let menu = PopupMenu::new()
            .item(PopupItem::new("ref", "Copy Reference"))
            .item(PopupItem::new("agent", "Copy for Agent"));
        tx.popup_menu(menu, "copy-sel", |s: &mut Collate, picked, tx| match picked {
            Some("ref") => s.copy_selection(false, tx),
            Some("agent") => s.copy_selection(true, tx),
            _ => {}
        });
    }
}

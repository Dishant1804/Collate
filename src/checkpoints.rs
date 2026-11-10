//! Checkpoints: the working tree saved as a commit no branch points at, before an agent starts
//! or when the reader asks, to see what changed since and to go back to. HEAD and the index
//! never move.
use rune::{PopupItem, PopupMenu, Tx};

use crate::{Collate, HANDLE, Risky, abbrev, current_repo, day};

/// How long ago `time` was, as a menu says it.
fn ago(time: i64, now: i64) -> String {
    match now - time {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        _ => day(time),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Collate {
    pub fn save_checkpoint(&mut self) {
        self.act_bg("Saving a checkpoint…", |r| r.checkpoint("Checkpoint").map(|sha| format!("Saved a checkpoint ({})", abbrev(&sha))));
    }

    /// Reads the checkpoints off the UI thread, then shows them in a menu under `anchor`.
    pub fn checkpoints_menu(&mut self, anchor: &'static str) {
        let (Some(repo), Some(app)) = (current_repo(), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let got = repo.checkpoints();
            app.update(move |s, tx| match got {
                Ok(list) => s.show_checkpoints(&list, anchor, tx),
                Err(e) => s.say(format!("Couldn't read the checkpoints: {e}"), true),
            });
        });
    }

    fn show_checkpoints(&mut self, list: &[(String, i64, String)], anchor: &'static str, tx: &mut Tx) {
        let now = now_secs();
        let mut menu = PopupMenu::new().item(PopupItem::new("save", "Save Checkpoint Now"));
        if list.is_empty() {
            menu = menu.separator().item(PopupItem::new("none", "No checkpoints yet: one is saved before an agent starts").enabled(false));
        } else {
            menu = menu.separator();
        }
        for (sha, time, label) in list.iter().take(20) {
            let each = PopupMenu::new()
                .item(PopupItem::new(format!("since:{sha}:{time}"), "Show Changes Since"))
                .item(PopupItem::new(format!("restore:{sha}:{time}"), "Restore…"));
            menu = menu.submenu(format!("{} - {label}", ago(*time, now)), each);
        }
        tx.popup_menu(menu, anchor, |s: &mut Collate, picked, _| {
            let Some(picked) = picked else { return };
            if picked == "save" {
                return s.save_checkpoint();
            }
            let mut f = picked.splitn(3, ':');
            let (Some(what), Some(sha), Some(time)) = (f.next(), f.next(), f.next().and_then(|t| t.parse().ok())) else { return };
            match what {
                "since" => s.changes_since(sha.to_owned(), time),
                "restore" => s.risky = Some(Risky::RestoreCheckpoint(sha.to_owned(), time)),
                _ => {}
            }
        });
    }

    /// Compares checkpoint `sha` with the working tree as it is now (saved for the compare, not
    /// kept as a checkpoint), in the compare view.
    pub fn changes_since(&mut self, sha: String, time: i64) {
        let (Some(repo), Some(app)) = (current_repo(), HANDLE.get().cloned()) else { return };
        std::thread::spawn(move || {
            let got = repo.snapshot_tree("Now");
            app.update(move |s, _| match got {
                Ok(now) => {
                    s.open_commit(&format!("{sha}..{now}"));
                    s.compared = Some((format!("Checkpoint {}", ago(time, now_secs())), "Now".to_owned()));
                }
                Err(e) => s.say(format!("Couldn't read the working tree: {e}"), true),
            });
        });
    }

    pub fn restore_checkpoint(&mut self, sha: String) {
        self.act_bg("Restoring the checkpoint…", move |r| {
            r.restore_checkpoint(&sha).map(|_| "Restored the checkpoint; the files it replaced are saved as a checkpoint too".to_owned())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_say_how_long_ago() {
        assert_eq!(ago(1000, 1030), "just now");
        assert_eq!(ago(1000, 1000 + 125), "2 min ago");
        assert_eq!(ago(0, 3 * 3600 + 5), "3 h ago");
        assert_eq!(ago(0, 2 * 86_400), "1970-01-01");
    }
}

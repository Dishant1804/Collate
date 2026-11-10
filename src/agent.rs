//! Handing work to the reader's coding agent: the selected lines as a reference or as a
//! prompt-ready excerpt, their comments, and their review checklist. Collate makes the text;
//! the agent the reader already has does the rest, in a terminal under the diff.
use std::path::Path;

use rune::{PopupItem, PopupMenu, Tx};

use crate::diff::Side;
use crate::{Collate, Scope, abbrev, current_repo, paths_file};

/// The checklist until the reader writes their own: what a reviewer asks of every change.
const CHECKLIST: [&str; 5] = [
    "What does this change do?",
    "Is it correct? What cases does it miss?",
    "Could it be slower or use more memory?",
    "Does its design fit the code around it?",
    "Is it tested? What has no test?",
];

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

    /// The git command whose output is this view's diff, and whether files git doesn't track
    /// yet are in the view too (`git diff` leaves them out).
    fn diff_command(&self) -> (String, bool) {
        match self.scope {
            Scope::Uncommitted => ("git diff HEAD".into(), true),
            Scope::Staged => ("git diff --cached".into(), false),
            Scope::Unstaged => ("git diff".into(), true),
            Scope::Branch => (format!("git diff {}", self.base_commit.as_deref().map_or("HEAD".to_owned(), abbrev)), true),
            Scope::History => match self.commit.as_deref().map(|c| (c, c.split_once(".."))) {
                Some((_, Some((a, b)))) if a.strip_suffix('^') == Some(b) => (format!("git show {}", abbrev(b)), false),
                Some((_, Some((a, b)))) => (format!("git diff {} {}", abbrev(a), abbrev(b)), false),
                Some((c, None)) => (format!("git show {}", abbrev(c)), false),
                None => ("git show HEAD".into(), false),
            },
        }
    }

    /// The prompt that asks the agent `questions` about the change in view, read-only.
    pub fn checklist_prompt(&self, questions: &[String]) -> String {
        let (command, untracked) = self.diff_command();
        let also = if untracked { ", plus the untracked files `git status` lists" } else { "" };
        let list: String = questions.iter().enumerate().map(|(i, q)| format!("{}. {q}\n", i + 1)).collect();
        format!(
            "Review {}. Read the change with `{command}`{also}, and the code around it as you need. Answer each question below in turn, citing files and lines. Don't change any files.\n\n{}",
            self.compared_text(),
            list.trim_end()
        )
    }

    /// Asks the agent the checklist's question `which`, or all of them.
    pub fn ask_checklist(&mut self, which: Option<usize>) {
        let all = load_checklist();
        let questions = match which {
            Some(i) => all.get(i).cloned().into_iter().collect(),
            None => all,
        };
        if questions.is_empty() {
            self.say("The checklist has no questions: add some with Edit Questions", true);
            return;
        }
        let prompt = self.checklist_prompt(&questions);
        self.run_agent(&prompt);
    }

    /// Opens the checklist in the reader's editor, writing the default one first.
    fn edit_checklist(&mut self) {
        let Some(path) = paths_file("checklist.txt") else { return };
        if !path.exists() {
            let text = format!("# One question a line; the agent answers them about the change in view.\n{}\n", CHECKLIST.join("\n"));
            if let Err(e) = std::fs::write(&path, text) {
                self.say(format!("Couldn't write {}: {e}", path.display()), true);
                return;
            }
        }
        let mut cmd = std::process::Command::new("open");
        match self.settings.editor.app() {
            Some(app) => cmd.args(["-a", app]),
            None => cmd.arg("-t"),
        };
        match cmd.arg(&path).status() {
            Ok(st) if st.success() => {}
            Ok(_) => self.say(format!("Couldn't open {}", path.display()), true),
            Err(e) => self.say(format!("Couldn't run open: {e}"), true),
        }
    }

    /// The "⋯" menu's Ask Agent submenu: each question, all of them, the comments, and the
    /// list itself.
    pub fn agent_menu(&self) -> PopupMenu {
        let agent = self.settings.agent.title();
        let mut menu = PopupMenu::new().item(PopupItem::new("ask:all", "Ask Every Question"));
        for (i, q) in load_checklist().iter().enumerate() {
            menu = menu.item(PopupItem::new(format!("ask:{i}"), q.clone()));
        }
        let n = self.comments.len();
        menu.separator()
            .item(PopupItem::new("ask:comments", format!("Send {n} Comment{} to {agent}", if n == 1 { "" } else { "s" })).enabled(n > 0))
            .separator()
            .item(PopupItem::new("ask:edit", "Edit Questions…"))
    }

    /// Acts on an item of [`Collate::agent_menu`]; other ids do nothing.
    pub fn agent_menu_pick(&mut self, id: &str) {
        match id.strip_prefix("ask:") {
            Some("all") => self.ask_checklist(None),
            Some("comments") => self.send_comments(),
            Some("edit") => self.edit_checklist(),
            Some(i) => {
                if let Ok(i) = i.parse() {
                    self.ask_checklist(Some(i));
                }
            }
            None => {}
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

    /// The prompt that asks the agent to address the reader's comments.
    pub fn comments_prompt(&self) -> String {
        format!(
            "Address my review comments on {}. Each names a file and line, quotes the line, then says what I want; a line may have moved since. Change the code where a comment asks for it, and answer the ones that are questions.\n\n{}",
            self.compared_text(),
            self.comments_markdown()
        )
    }

    /// Starts the reader's agent on their comments, in a new terminal under the diff.
    pub fn send_comments(&mut self) {
        if self.comments.is_empty() {
            self.say("No comments to send: press c on a selected line to write one", true);
            return;
        }
        let prompt = self.comments_prompt();
        self.run_agent(&prompt);
    }

    /// Starts the agent in a new terminal in the project's folder, with `prompt` as its first
    /// message. The prompt goes through a file, so the shell neither echoes nor parses it.
    pub fn run_agent(&mut self, prompt: &str) {
        let Some(repo) = current_repo() else {
            self.say("Open a project first", true);
            return;
        };
        let file = std::env::temp_dir().join(format!("collate-prompt-{}-{}.md", std::process::id(), self.terminals.next_id()));
        if let Err(e) = std::fs::write(&file, prompt) {
            self.say(format!("Couldn't write the prompt to {}: {e}", file.display()), true);
            return;
        }
        let agent = self.settings.agent;
        let command = agent_command(agent.command(), &file);
        match self.terminals.run(&repo.root, &command) {
            Ok(()) => self.say(format!("Started {} in the terminal", agent.title()), false),
            Err(e) => self.say(e, true),
        }
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

/// The reader's questions, one a line (`#` starts a note), or the default ones.
fn load_checklist() -> Vec<String> {
    // Tests ask the default questions, whatever the reader has written.
    let text = if cfg!(test) { None } else { paths_file("checklist.txt").and_then(|p| std::fs::read_to_string(p).ok()) };
    match text {
        Some(t) => parse_checklist(&t),
        None => CHECKLIST.map(str::to_owned).to_vec(),
    }
}

fn parse_checklist(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect()
}

/// The shell command that starts `agent` with the prompt in `file` as its argument.
fn agent_command(agent: &str, file: &Path) -> String {
    let quoted = format!("'{}'", file.display().to_string().replace('\'', "'\\''"));
    format!("{agent} \"$(cat {quoted})\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_reads_its_prompt_from_a_file_the_shell_quotes() {
        assert_eq!(agent_command("claude", Path::new("/tmp/it's.md")), "claude \"$(cat '/tmp/it'\\''s.md')\"");
    }

    #[test]
    fn the_checklist_is_a_question_a_line_without_notes_or_blanks() {
        assert_eq!(parse_checklist("# mine\n\n  Is it safe?  \nWho calls it?\n"), ["Is it safe?", "Who calls it?"]);
    }
}

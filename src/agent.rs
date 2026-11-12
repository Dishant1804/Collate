//! Handing work to the reader's coding agent: the selected lines as a reference or as a
//! prompt-ready excerpt, their comments, and their review checklist. Collate makes the text;
//! the agent the reader already has does the rest, in a terminal under the diff.
use std::path::Path;

use rune::{PopupItem, PopupMenu, TextEdit, Tx};

use crate::diff::Side;
use crate::git::Operation;
use crate::settings::Agent;
use crate::{Ask, Collate, HANDLE, Scope, abbrev, current_repo, paths_file};

/// How a commit message should read until the reader writes their own instructions.
const COMMIT_RULES: &str = "A subject line of at most 72 characters in the imperative mood, then, only if the subject doesn't make the reason clear, a blank line and a short body saying why.";

/// The most of a staged diff the agent is given: its prompt is one shell argument, and macOS
/// caps those at about 1 MB.
const DIFF_CAP: usize = 200_000;

/// What the agent is asked to bring together.
#[derive(Clone, Debug, PartialEq)]
pub enum Settle {
    /// The branch and its upstream both moved: commits ahead and behind.
    Diverged(usize, usize),
    /// A merge, rebase, cherry-pick or revert stopped on conflicts in these files.
    Stopped(Operation, Vec<String>),
    /// A pull request (number, branch, base) that conflicts with its base.
    Pull(u32, String, String),
}

/// The prompt for [`Settle`]. Every one ends with the agent leaving the push to the reader:
/// what it settled is reviewed here first.
pub fn settle_prompt(settle: &Settle) -> String {
    let ask = match settle {
        Settle::Diverged(ahead, behind) => format!(
            "This branch and its upstream have both moved: it has {ahead} commit{} the upstream doesn't, and the upstream has {behind} it doesn't. Rebase this branch's own commits onto its upstream: run `git fetch`, then `git rebase @{{u}}`, settle any conflicts, and go on with `GIT_EDITOR=true git rebase --continue` until it's done.",
            if *ahead == 1 { "" } else { "s" }
        ),
        Settle::Stopped(op, files) => {
            let files = if files.is_empty() { "the files `git status` lists as unmerged".to_owned() } else { files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ") };
            format!("A {} is stopped on conflicts in {files}. Settle each conflict, stage the files, and finish with `GIT_EDITOR=true git {} --continue`.", op.noun(), op.command())
        }
        Settle::Pull(number, head, base) => format!(
            "Pull request #{number} ({head} into {base}) conflicts with {base}. On branch `{head}` (switch to it first; stop and tell me if the working tree has uncommitted changes), run `git fetch origin`, then `git merge origin/{base}`, settle the conflicts, and finish with `GIT_EDITOR=true git merge --continue`. Merge rather than rebase, so the commits already pushed stay as they are."
        ),
    };
    format!("{ask}\n\nKeep what both sides meant. Where they truly clash, stop and ask me. Run the project's tests if it has them. Don't push: I'll review the result and push it myself.")
}

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

    /// Opens one of the reader's agent files in their editor, writing `default` to it first
    /// when it doesn't exist yet.
    fn edit_agent_file(&mut self, name: &str, default: &str) {
        let Some(path) = paths_file(name) else { return };
        if !path.exists() {
            let made = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, default));
            if let Err(e) = made {
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
            .item(PopupItem::new("ask:commit-rules", "Edit Commit Message Instructions…"))
    }

    /// The agent to run, with its instructions to edit, under the sidebar's agent row.
    pub fn agent_picker(&mut self, tx: &mut Tx) {
        let mut menu = PopupMenu::new();
        for (i, a) in Agent::ALL.iter().enumerate() {
            menu = menu.item(PopupItem::new(format!("agent:{i}"), a.title()).checked(*a == self.settings.agent));
        }
        menu = menu.separator()
            .item(PopupItem::new("ask:edit", "Edit Review Questions…"))
            .item(PopupItem::new("ask:commit-rules", "Edit Commit Message Instructions…"))
            .separator()
            .item(PopupItem::new("settings", "Agent Settings…"));
        tx.popup_menu(menu, "agent-pick", |s: &mut Collate, picked, _| match picked.and_then(|p| p.strip_prefix("agent:")).and_then(|i| i.parse::<usize>().ok()) {
            Some(i) => s.configure(|c| c.agent = Agent::ALL[i]),
            None if picked == Some("settings") => {
                s.screen = crate::Screen::Settings;
                s.settings_tab = crate::settings_view::AGENT_TAB;
            }
            None => {
                if let Some(id) = picked {
                    s.agent_menu_pick(id);
                }
            }
        });
    }

    /// Acts on an item of [`Collate::agent_menu`]; other ids do nothing.
    pub fn agent_menu_pick(&mut self, id: &str) {
        match id.strip_prefix("ask:") {
            Some("all") => self.ask_checklist(None),
            Some("comments") => self.send_comments(),
            Some("edit") => {
                let text = format!("# One question a line; the agent answers them about the change in view.\n{}\n", CHECKLIST.join("\n"));
                self.edit_agent_file("checklist.txt", &text);
            }
            Some("commit-rules") => self.edit_agent_file("commit-instructions.txt", &format!("# How the agent writes commit messages, in your words.\n{COMMIT_RULES}\n")),
            Some(i) => {
                if let Ok(i) = i.parse() {
                    self.ask_checklist(Some(i));
                }
            }
            None => {}
        }
    }

    /// Has the agent write a commit message for what's staged, off the UI thread. It lands in
    /// the box for the reader to edit and commit; nothing commits by itself.
    pub fn write_message(&mut self) {
        if self.writing_message {
            return;
        }
        if self.staged == 0 && !self.amend {
            self.say("Stage files first: the message describes what's staged", true);
            return;
        }
        let (Some(app), Some(repo)) = (HANDLE.get().cloned(), current_repo()) else {
            self.say("Open a project first", true);
            return;
        };
        self.writing_message = true;
        let (agent, amend, draft) = (self.settings.agent, self.amend, self.message.text.trim().to_owned());
        let rules = read_agent_file("commit-instructions.txt").unwrap_or_else(|| COMMIT_RULES.to_owned());
        std::thread::spawn(move || {
            let got = repo.staged_diff(amend).and_then(|diff| {
                let recent = repo.recent_subjects(10).unwrap_or_default();
                ask_agent(agent, &repo.root, &message_prompt(&rules, &recent, &draft, &diff))
            });
            app.update(move |s, _| {
                s.writing_message = false;
                match got.map(|m| clean_message(&m)) {
                    Ok(m) if !m.is_empty() => {
                        s.message = TextEdit::new(m);
                        s.say(format!("{} wrote the message: read it, then commit", agent.title()), false);
                    }
                    Ok(_) => s.say(format!("{} answered with nothing", agent.title()), true),
                    Err(e) => s.say(format!("{} couldn't write the message: {e}", agent.title()), true),
                }
            });
        });
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

    /// Asks for a question to put to the agent: about the selected lines, or else about the
    /// whole change in view, so asking never waits on knowing how to select.
    pub fn ask_question(&mut self) {
        let about = match self.selection_for_agent() {
            Some(lines) => Ask::Agent(lines, true),
            None => Ask::Agent(self.compared_text(), false),
        };
        self.ask(about);
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

    /// Asks the agent to settle `settle` in a new terminal.
    pub fn settle_with_agent(&mut self, settle: Settle) {
        self.run_agent(&settle_prompt(&settle));
    }

    /// The operation git stopped in, to settle, with the files still in conflict.
    pub fn stopped_settle(&self) -> Option<Settle> {
        let op = self.operation?;
        let files = self.diff.files().iter().zip(&self.info).filter(|(_, i)| i.conflicted).map(|(f, _)| f.path().to_owned()).collect();
        Some(Settle::Stopped(op, files))
    }

    /// Starts the agent in a new terminal in the project's folder, with `prompt` as its first
    /// message, after saving the working tree as a checkpoint to come back to. The prompt goes
    /// through a file, so the shell neither echoes nor parses it.
    pub fn run_agent(&mut self, prompt: &str) {
        let Some(repo) = current_repo() else {
            self.say("Open a project first", true);
            return;
        };
        if !self.terminals.ready() {
            self.say("The terminal isn't ready yet", true);
            return;
        }
        let file = std::env::temp_dir().join(format!("collate-prompt-{}-{}.md", std::process::id(), self.terminals.next_id()));
        if let Err(e) = std::fs::write(&file, prompt) {
            self.say(format!("Couldn't write the prompt to {}: {e}", file.display()), true);
            return;
        }
        let agent = self.settings.agent;
        let first = prompt.split(['.', '\n']).next().unwrap_or("").chars().take(60).collect::<String>();
        let label = format!("Before {}: {first}", agent.title());
        let start = move |s: &mut Collate, saved: Result<String, String>| {
            if let Err(e) = saved {
                return s.say(format!("Didn't start {}: couldn't save a checkpoint first: {e}", agent.title()), true);
            }
            match s.terminals.run(&repo.root, &agent_command(agent.command(), &file)) {
                Ok(()) => s.say(format!("Started {} in the terminal; Checkpoints in the ⋯ menu go back to before it", agent.title()), false),
                Err(e) => s.say(e, true),
            }
        };
        let Some(app) = HANDLE.get().cloned() else {
            let saved = current_repo().map_or(Err("no project".into()), |r| r.checkpoint(&label));
            return start(self, saved);
        };
        let r = current_repo();
        std::thread::spawn(move || {
            let saved = r.map_or(Err("no project".into()), |r| r.checkpoint(&label));
            app.update(move |s, _| start(s, saved));
        });
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

/// One of the reader's agent files, its lines without notes (`#`) or blanks; `None` when
/// there isn't one.
fn read_agent_file(name: &str) -> Option<String> {
    // Tests use the defaults, whatever the reader has written.
    if cfg!(test) {
        return None;
    }
    let text = std::fs::read_to_string(paths_file(name)?).ok()?;
    Some(note_free_lines(&text).join("\n"))
}

/// The reader's questions, one a line, or the default ones.
fn load_checklist() -> Vec<String> {
    match read_agent_file("checklist.txt") {
        Some(t) => t.lines().map(str::to_owned).collect(),
        None => CHECKLIST.map(str::to_owned).to_vec(),
    }
}

fn note_free_lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect()
}

/// The prompt for a commit message: the reader's rules, the repository's recent subjects to
/// match, the reader's draft when there is one, and the diff (cut short past [`DIFF_CAP`]).
fn message_prompt(rules: &str, recent: &[String], draft: &str, diff: &str) -> String {
    let mut p = format!("Write the git commit message for the staged change below. {rules}\n\nReply with the message alone: no preamble, no code fences.\n");
    if !recent.is_empty() {
        p += &format!("\nRecent commit subjects in this repository, whose style to match:\n{}\n", recent.iter().map(|s| format!("- {s}\n")).collect::<String>());
    }
    if !draft.is_empty() {
        p += &format!("\nMy draft, whose intent to keep:\n{draft}\n");
    }
    let mut end = diff.len().min(DIFF_CAP);
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let cut = if end < diff.len() { "\n[the diff goes on; cut here]" } else { "" };
    p + &format!("\nThe staged diff:\n\n```diff\n{}{cut}\n```\n", &diff[..end])
}

/// Runs `agent` once on `prompt` in `dir` and returns what it printed. Through the reader's
/// login shell, which has the PATH their terminal has (an app opened from the Finder doesn't).
fn ask_agent(agent: Agent, dir: &Path, prompt: &str) -> Result<String, String> {
    let file = std::env::temp_dir().join(format!("collate-ask-{}-{:?}.md", std::process::id(), std::thread::current().id()));
    std::fs::write(&file, prompt).map_err(|e| format!("couldn't write the prompt: {e}"))?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-ilc", &agent_command(agent.print_command(), &file)])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output();
    let _ = std::fs::remove_file(&file);
    let out = out.map_err(|e| format!("couldn't start the shell: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it stopped with an error");
        return Err(last.trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The message without what agents wrap one in: spaces round it and a code fence.
fn clean_message(text: &str) -> String {
    let t = text.trim();
    let t = match t.strip_prefix("```") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, body)| body).trim_end().strip_suffix("```").unwrap_or(rest),
        None => t,
    };
    t.trim().to_owned()
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
        assert_eq!(note_free_lines("# mine\n\n  Is it safe?  \nWho calls it?\n"), ["Is it safe?", "Who calls it?"]);
    }

    #[test]
    fn a_commit_message_prompt_carries_the_style_the_draft_and_the_diff() {
        let p = message_prompt("Be brief.", &["Fix the parser".into()], "fix crash", "+x");
        assert!(p.starts_with("Write the git commit message for the staged change below. Be brief."));
        assert!(p.contains("\n- Fix the parser\n") && p.contains("intent to keep:\nfix crash\n") && p.ends_with("```diff\n+x\n```\n"), "{p}");
        let long = "é".repeat(DIFF_CAP);
        let p = message_prompt("", &[], "", &long);
        assert!(p.contains("[the diff goes on; cut here]") && p.len() < DIFF_CAP + 1000, "cut on a character, not mid-way");
        assert_eq!(clean_message("\n```text\nAdd it\n\nBecause.\n```\n"), "Add it\n\nBecause.");
        assert_eq!(clean_message("  Add it  "), "Add it");
    }

    #[test]
    fn the_agent_settles_conflicts_and_leaves_the_push_to_the_reader() {
        let diverged = settle_prompt(&Settle::Diverged(1, 3));
        assert!(diverged.starts_with("This branch and its upstream have both moved: it has 1 commit the upstream doesn't, and the upstream has 3 it doesn't.") && diverged.contains("`git rebase @{u}`"), "{diverged}");
        let stopped = settle_prompt(&Settle::Stopped(Operation::CherryPick, vec!["a.rs".into(), "b.rs".into()]));
        assert!(stopped.starts_with("A cherry-pick is stopped on conflicts in `a.rs`, `b.rs`.") && stopped.contains("`GIT_EDITOR=true git cherry-pick --continue`"), "{stopped}");
        assert!(settle_prompt(&Settle::Stopped(Operation::Merge, vec![])).contains("in the files `git status` lists as unmerged."));
        let pull = settle_prompt(&Settle::Pull(7, "fix".into(), "main".into()));
        assert!(pull.starts_with("Pull request #7 (fix into main) conflicts with main. On branch `fix`") && pull.contains("`git merge origin/main`"), "{pull}");
        for p in [diverged, stopped, pull] {
            assert!(p.ends_with("Don't push: I'll review the result and push it myself."), "never pushes: {p}");
        }
    }
}

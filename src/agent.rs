//! Handing work to the reader's coding agent: the selected lines as a reference or as a
//! prompt-ready excerpt, their comments, and their review checklist. Collate makes the text;
//! the agent the reader already has does the rest, in the staging panel's Agent tab.
use std::path::Path;

use rune::{PopupItem, PopupMenu, TextEdit, Tx};

use crate::diff::Side;
use crate::git::Operation;
use crate::settings::Agent;
use crate::{Collate, HANDLE, PanelTab, Scope, abbrev, current_repo, paths_file};

/// How a commit message should read until the reader writes their own instructions.
const COMMIT_RULES: &str = "A subject line of at most 72 characters in the imperative mood, then, only if the subject doesn't make the reason clear, a blank line and a short body saying why.";

/// The most of a staged diff the agent is given: its prompt is one shell argument, and macOS
/// caps those at about 1 MB.
const DIFF_CAP: usize = 200_000;
/// How many 400 ms ticks Add to Chat waits for a new agent to start before giving up.
const CHAT_WAIT_TICKS: u32 = 50;

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
        if let Err(e) = crate::desktop::edit(self.settings.editor, &path, true) {
            self.say(e, true);
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

    /// The agent to run, with its instructions to edit, under the Agent tab's agent name.
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
        let (Some(app), Some(repo)) = (HANDLE.get().cloned(), current_repo()) else {
            self.say("Open a project first", true);
            return;
        };
        if self.staged == 0 && !self.amend {
            if self.total == 0 {
                self.say("Nothing to describe: there are no changes", true);
                return;
            }
            // The button said so ("Stage All and Write"): the message describes what's staged.
            if let Err(e) = repo.stage_all() {
                self.say(e, true);
                return;
            }
            crate::refresh();
        }
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

    /// Adds the selected lines to the agent's message in the Agent tab, as Zed's Add to Chat:
    /// pasted, not sent, for the reader to ask about them there. With no agent running here,
    /// it starts one and pastes once its screen settles, since nothing says when it reads keys.
    pub fn add_to_chat(&mut self) {
        let Some(lines) = self.selection_for_agent() else {
            self.say("Click a line number first: Add to Chat takes the selected lines", true);
            return;
        };
        if let Some(t) = self.running_chat() {
            t.paste(&format!("{lines}\n\n"));
            return self.reveal_chat();
        }
        // A second selection while the agent starts goes with the first.
        if let Some((waiting, ..)) = &mut self.chat_draft {
            waiting.push_str(&format!("\n\n{lines}"));
            return self.reveal_chat();
        }
        if !self.agent_starting {
            self.start_agent_chat(None);
        }
        // It said why it couldn't start.
        if !self.agent_starting {
            return;
        }
        self.chat_draft = Some((lines, String::new(), 0));
        self.reveal_chat();
        let Some(app) = HANDLE.get().cloned() else { return };
        std::thread::spawn(move || {
            for _ in 0..CHAT_WAIT_TICKS {
                std::thread::sleep(std::time::Duration::from_millis(400));
                app.update(|s, _| s.deliver_chat_draft());
            }
        });
    }

    /// The agent's terminal when it was started in this project and has finished starting.
    fn running_chat(&self) -> Option<&crate::terminal::Session> {
        let repo = current_repo()?;
        let here = self.agent_session.as_ref().is_some_and(|(_, dir)| *dir == repo.root) && !self.agent_starting;
        self.agent_chat.active().filter(|_| here)
    }

    /// A tick of the wait for a new agent: pastes the waiting lines once its screen has held
    /// still for a tick, or gives up when the wait runs out.
    fn deliver_chat_draft(&mut self) {
        let Some((lines, last, ticks)) = self.chat_draft.take() else { return };
        let screen = self.running_chat().map(|t| t.shown()).unwrap_or_default();
        if screen.trim().is_empty() || screen != last {
            if ticks + 1 >= CHAT_WAIT_TICKS {
                return self.say(format!("{} didn't settle in time, so the lines weren't added", self.settings.agent.title()), true);
            }
            self.chat_draft = Some((lines, screen, ticks + 1));
            return;
        }
        let Some(t) = self.running_chat() else { return };
        t.paste(&format!("{lines}\n\n"));
    }

    /// The prompt that asks the agent to address the reader's comments.
    pub fn comments_prompt(&self) -> String {
        format!(
            "Address my review comments on {}. Each names a file and line, quotes the line, then says what I want; a line may have moved since. Change the code where a comment asks for it, and answer the ones that are questions.\n\n{}",
            self.compared_text(),
            self.comments_markdown()
        )
    }

    /// Gives the reader's comments to the agent, in the Agent tab.
    pub fn send_comments(&mut self) {
        if self.comments.is_empty() {
            self.say("No comments to send: press c on a selected line to write one", true);
            return;
        }
        let prompt = self.comments_prompt();
        self.run_agent(&prompt);
    }

    /// Asks the agent to settle `settle`, in the Agent tab.
    pub fn settle_with_agent(&mut self, settle: Settle) {
        self.run_agent(&settle_prompt(&settle));
    }

    /// The operation git stopped in, to settle, with the files still in conflict.
    pub fn stopped_settle(&self) -> Option<Settle> {
        let op = self.operation?;
        let files = self.diff.files().iter().zip(&self.info).filter(|(_, i)| i.conflicted).map(|(f, _)| f.path().to_owned()).collect();
        Some(Settle::Stopped(op, files))
    }

    /// Gives the agent `prompt` in the Agent tab, after saving the working tree as a checkpoint
    /// to come back to: as the next message of the chat running in this project, or else as
    /// the first of a new one. The tab comes up with the keys, for the reader to go on.
    pub fn run_agent(&mut self, prompt: &str) {
        let Some(repo) = current_repo() else {
            self.say("Open a project first", true);
            return;
        };
        if !self.agent_chat.ready() {
            self.say("The terminal isn't ready yet", true);
            return;
        }
        let running = self.agent_chat.active().map(|t| t.id).filter(|_| self.agent_session.as_ref().is_some_and(|(_, dir)| *dir == repo.root));
        let Some(id) = running else {
            return self.start_agent_chat(Some(prompt));
        };
        self.reveal_chat();
        let agent = self.agent_session.as_ref().map_or(self.settings.agent, |(a, _)| *a);
        let first = prompt.split(['.', '\n']).next().unwrap_or("").chars().take(60).collect::<String>();
        let prompt = prompt.to_owned();
        self.checkpointed(format!("Before {}: {first}", agent.title()), move |s, saved| {
            if let Err(e) = saved {
                return s.say(format!("Didn't send it to {}: couldn't save a checkpoint first: {e}", agent.title()), true);
            }
            let Some(t) = s.agent_chat.list.iter().find(|t| t.id == id) else {
                return s.say(format!("{} ended before the message reached it", agent.title()), true);
            };
            t.paste(&prompt);
            // Return a moment after the paste: an agent's input reads Return in the same
            // read as the paste as a line break of the pasted text, not as Send.
            let Some(app) = HANDLE.get().cloned() else { return t.press_return() };
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                app.update(move |s, _| {
                    if let Some(t) = s.agent_chat.list.iter().find(|t| t.id == id) {
                        t.press_return();
                    }
                });
            });
        });
    }

    /// Saves a checkpoint labelled `label` off the UI thread, then calls `then` with how it went.
    fn checkpointed(&mut self, label: String, then: impl FnOnce(&mut Collate, Result<String, String>) + Send + 'static) {
        let Some(app) = HANDLE.get().cloned() else {
            let saved = current_repo().map_or(Err("no project".into()), |r| r.checkpoint(&label));
            return then(self, saved);
        };
        let r = current_repo();
        std::thread::spawn(move || {
            let saved = r.map_or(Err("no project".into()), |r| r.checkpoint(&label));
            app.update(move |s, _| then(s, saved));
        });
    }

    /// Shows the Agent tab with the keys, without starting anything.
    fn reveal_chat(&mut self) {
        self.panel_tab = PanelTab::Agent;
        self.inspector.set_open(true);
        self.terminals.focus(false);
        self.agent_chat.focus(true);
    }

    /// Shows a tab of the staging panel. The Agent tab opens the panel if it's hidden, takes
    /// the keys, and starts the agent the first time.
    pub fn show_panel_tab(&mut self, tab: PanelTab) {
        if tab == PanelTab::Files {
            self.panel_tab = tab;
            return self.agent_chat.focus(false);
        }
        self.reveal_chat();
        if self.agent_chat.list.is_empty() && self.has_project && !self.agent_starting && self.agent_session.is_none() {
            self.start_agent_chat(None);
        }
    }

    /// Starts the reader's agent in the Agent tab, in the project's folder, ending the one
    /// running there, with `prompt` as its first message if given. A checkpoint is saved
    /// first, as for every agent Collate starts. The prompt goes through a file, so the shell
    /// neither echoes nor parses it.
    pub fn start_agent_chat(&mut self, prompt: Option<&str>) {
        let Some(repo) = current_repo() else {
            self.say("Open a project first", true);
            return;
        };
        if !self.agent_chat.ready() {
            self.say("The terminal isn't ready yet", true);
            return;
        }
        if self.agent_starting {
            return;
        }
        let file = match prompt {
            Some(prompt) => {
                let file = std::env::temp_dir().join(format!("collate-prompt-{}-{}.md", std::process::id(), self.agent_chat.next_id()));
                if let Err(e) = std::fs::write(&file, prompt) {
                    self.say(format!("Couldn't write the prompt to {}: {e}", file.display()), true);
                    return;
                }
                Some(file)
            }
            None => None,
        };
        let agent = self.settings.agent;
        // Dropping the session hangs up its terminal, which ends the agent.
        self.agent_chat.list.clear();
        self.agent_starting = true;
        if prompt.is_some() {
            self.reveal_chat();
        }
        let label = match prompt.map(|p| p.split(['.', '\n']).next().unwrap_or("").chars().take(60).collect::<String>()) {
            Some(first) => format!("Before {}: {first}", agent.title()),
            None => format!("Before chatting with {}", agent.title()),
        };
        self.checkpointed(label, move |s, saved| {
            s.agent_starting = false;
            if let Err(e) = saved {
                return s.say(format!("Didn't start {}: couldn't save a checkpoint first: {e}", agent.title()), true);
            }
            match s.agent_chat.exec(&repo.root, &chat_command(agent, file.as_deref())) {
                Ok(()) => {
                    s.agent_session = Some((agent, repo.root.clone()));
                    // The reader may have left the tab while the checkpoint saved.
                    let shown = s.panel_tab == PanelTab::Agent;
                    s.agent_chat.focus(shown);
                    if shown {
                        s.terminals.focus(false);
                    }
                }
                Err(e) => s.say(e, true),
            }
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

/// The shell command for the Agent tab: the agent's program alone, which starts it
/// interactive, or with the prompt in `file` as its first message. Stopped by an error (not
/// installed, a crash), it leaves its message on screen until Return, instead of the tab
/// closing before it can be read.
fn chat_command(agent: Agent, file: Option<&Path>) -> String {
    let start = match file {
        Some(file) => agent_command(agent.command(), file),
        None => agent.command().split(' ').next().unwrap_or("").to_owned(),
    };
    format!("{start}; s=$?; [ $s -eq 0 ] || {{ printf '\\n%s stopped (status %s). Press Return to close.' '{}' $s; read -r _; }}", agent.title())
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
    fn the_chat_starts_the_agent_alone_and_keeps_its_error_on_screen() {
        assert_eq!(chat_command(Agent::Gemini, None), "gemini; s=$?; [ $s -eq 0 ] || { printf '\\n%s stopped (status %s). Press Return to close.' 'Gemini' $s; read -r _; }");
        assert!(chat_command(Agent::Gemini, Some(Path::new("/tmp/p.md"))).starts_with("gemini -i \"$(cat '/tmp/p.md')\"; s=$?;"), "a first message keeps -i, so it stays interactive");
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

//! Collate: a git diff viewer for reviewing and landing changes. `collate [path]` opens the repository
//! containing `path` (default: the current directory) and shows its changes beside a file tree,
//! refreshing as files change.
//!
//! Keys: `j`/`k` next/previous hunk, `]`/`[` next/previous file; on the hunk at the top, `a`
//! stages, `u` unstages, `x` discards; `A`/`U`/`X` do the same for the file in view; `v` marks
//! it viewed, `o` opens it; `s` unified/split, `w` wrap, `1`–`4` all/staged/unstaged/pull request preview,
//! `r` refresh, ⌘↩ commit, ⌘, settings, ⇧⌘B sidebar.

mod agent;
mod avatars;
mod checkpoints;
mod comments;
mod diff;
mod git;
mod github;
mod grammars;
mod graph;
mod history_view;
mod keys;
mod map;
mod map_view;
mod nav;
mod patch;
mod pulls;
mod search;
mod settings;
mod settings_view;
mod terminal;
mod update;
mod watcher;
#[cfg(test)]
mod git_tests;
#[cfg(test)]
mod view_tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use comments::{Comment, comment_row, load_comments};
use diff::{DiffMode, DiffState, DiffView, EDGE, FileDiff, Finder, GHOST_PAD, IMAGE_H, LineKind, LineRef, Query, WordEdit, apply_edits, word_edits};
use git::{Change, Repo, Scope, Staged};
use rune::{App, AppSpec, Cursor, Cx, El, Icon, Key, KeyEvent, PopupItem, PopupMenu, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::shell::{self, PaneState, Sidebar, SidebarRow, SidebarSection, SplitView, Toolbar};
use rune_kit::syntax::Highlighter;
use rune_kit::{
    Checkbox, Button, ButtonVariant, ControlSize, Dialog, FileNode, FileTree, IconButton, IconButtonVariant, Kit, Placement, Popover, SegmentedControl, Select, TextArea, TextInput, Tree,
    TreeItem, TreeModel,
};
use search::fuzzy;
use history_view::{AUTHOR_W, DATE_W, GRAPH_ROW, HASH_W, commit_card, commit_header, history_graph};
use keys::on_key;
use settings::Settings;
use settings_view::{SETTINGS_TABS, settings_panel};
use watcher::{Cmd, DiffOpts, OPTS, Prepared, REFRESH_EVERY, WATCHER, refresh, watch, watch_project_files};

/// The open project; `None` before one is chosen. Swapped when the reader switches project.
static REPO: std::sync::RwLock<Option<std::sync::Arc<Repo>>> = std::sync::RwLock::new(None);

fn current_repo() -> Option<std::sync::Arc<Repo>> {
    REPO.read().unwrap().clone()
}

fn set_repo(r: Option<Repo>) {
    *REPO.write().unwrap() = r.map(std::sync::Arc::new);
}

static HANDLE: OnceLock<rune::AppHandle<Collate>> = OnceLock::new();

/// How long a status message stays up, in seconds.
const NOTICE_FOR: f64 = 5.0;

static MORE_ICON: Icon = Icon {
    name: "collate-more",
    view: 16.0,
    body: r#"<circle cx="3.5" cy="8" r="1.25" fill="currentColor"/><circle cx="8" cy="8" r="1.25" fill="currentColor"/><circle cx="12.5" cy="8" r="1.25" fill="currentColor"/>"#,
};

/// A branch, and the branch it merges back into.
static BRANCH_ICON: Icon = Icon {
    name: "collate-branch",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="3" x2="6" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/></g>"#,
};
static MERGE_ICON: Icon = Icon {
    name: "collate-merge",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><path d="M6 21V9a9 9 0 0 0 9 9"/></g>"#,
};
/// A commit on its line, and a tag, for the toolbar search's results.
static COMMIT_ICON: Icon = Icon {
    name: "collate-commit",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="4"/><line x1="3" y1="12" x2="8" y2="12"/><line x1="16" y1="12" x2="21" y2="12"/></g>"#,
};
static TAG_ICON: Icon = Icon {
    name: "collate-tag",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12V4a1 1 0 0 1 1-1h8l9 9-9 9z"/><circle cx="8" cy="8" r="1.5"/></g>"#,
};

/// Back and forward through the places visited.
static BACK_ICON: Icon = Icon {
    name: "collate-back",
    view: 16.0,
    body: r#"<path d="M13 8H3.5M7.5 4L3.5 8L7.5 12" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static FORWARD_ICON: Icon = Icon {
    name: "collate-forward",
    view: 16.0,
    body: r#"<path d="M3 8H12.5M8.5 4L12.5 8L8.5 12" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// Two arrows chasing round a circle: read again.
static REFRESH_ICON: Icon = Icon {
    name: "collate-refresh",
    view: 16.0,
    body: r#"<path d="M3.1 7.2A5 5 0 0 1 12.3 5.6" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><path d="M12.9 3.3L12.5 5.9L9.9 5.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/><path d="M12.9 8.8A5 5 0 0 1 3.7 10.4" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><path d="M3.1 12.7L3.5 10.1L6.1 10.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// The sidebar's places, Lucide's icons (ISC licence, licenses/lucide.txt): a changed file,
/// changes not yet ready and ready to commit, the graph, pull requests and the map's tiles.
macro_rules! lucide {
    ($name:ident, $id:literal, $body:literal) => {
        static $name: Icon = Icon { name: $id, view: 24.0, body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $body, "</g>") };
    };
}
pub(crate) use lucide;
lucide!(ALL_ICON, "collate-file-diff", r#"<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M9 10h6"/><path d="M12 13V7"/><path d="M9 17h6"/>"#);
lucide!(UNSTAGED_ICON, "collate-circle-dashed", r#"<path d="M10.1 2.182a10 10 0 0 1 3.8 0"/><path d="M13.9 21.818a10 10 0 0 1-3.8 0"/><path d="M17.609 3.721a10 10 0 0 1 2.69 2.7"/><path d="M2.182 13.9a10 10 0 0 1 0-3.8"/><path d="M20.279 17.609a10 10 0 0 1-2.7 2.69"/><path d="M21.818 10.1a10 10 0 0 1 0 3.8"/><path d="M3.721 6.391a10 10 0 0 1 2.7-2.69"/><path d="M6.391 20.279a10 10 0 0 1-2.69-2.7"/>"#);
lucide!(CHECKLIST_ICON, "collate-list-checks", r#"<path d="M13 5h8"/><path d="M13 12h8"/><path d="M13 19h8"/><path d="m3 17 2 2 4-4"/><path d="m3 7 2 2 4-4"/>"#);
lucide!(COMMENTS_ICON, "collate-message-square-text", r#"<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/><path d="M13 8H7"/><path d="M17 12H7"/>"#);
lucide!(CHECKPOINTS_ICON, "collate-history", r#"<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>"#);
lucide!(AGENT_ICON, "collate-bot", r#"<path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2"/><path d="M20 14h2"/><path d="M15 13v2"/><path d="M9 13v2"/>"#);
lucide!(STAGED_ICON, "collate-circle-check", r#"<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>"#);
lucide!(GRAPH_ICON, "collate-git-graph", r#"<circle cx="5" cy="6" r="3"/><path d="M5 9v6"/><circle cx="5" cy="18" r="3"/><path d="M12 3v18"/><circle cx="19" cy="6" r="3"/><path d="M16 15.7A9 9 0 0 0 19 9"/>"#);
lucide!(PULLS_ICON, "collate-git-pull-request", r#"<circle cx="18" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><path d="M13 6h3a2 2 0 0 1 2 2v7"/><path d="M6 9v12"/>"#);
// A pull request's state in the list, as GitHub marks it (open is PULLS_ICON, merged MERGE_ICON).
lucide!(PULL_DRAFT_ICON, "collate-git-pull-request-draft", r#"<circle cx="18" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><path d="M18 6V5"/><path d="M18 11v-1"/><path d="M6 9v12"/>"#);
lucide!(PULL_CLOSED_ICON, "collate-git-pull-request-closed", r#"<circle cx="6" cy="6" r="3"/><path d="M6 9v12"/><path d="m21 3-6 6"/><path d="m21 9-6-6"/><path d="M18 11.5V15"/><circle cx="18" cy="18" r="3"/>"#);
lucide!(MAP_ICON, "collate-layout-dashboard", r#"<rect width="7" height="9" x="3" y="3" rx="1"/><rect width="7" height="5" x="14" y="3" rx="1"/><rect width="7" height="9" x="14" y="12" rx="1"/><rect width="7" height="5" x="3" y="16" rx="1"/>"#);
// Beside a link that leaves the app, as a browser would open it.
lucide!(EXTERNAL_ICON, "collate-external-link", r#"<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>"#);

/// Commits read per page of History; the next page loads as the list nears its end.
const HISTORY_PAGE: usize = 500;

#[derive(Clone, Copy)]
struct Stat {
    added: u32,
    removed: u32,
    letter: char,
    viewed: bool,
}

/// What the actions on a shown file need to know about it.
#[derive(Clone, Copy)]
struct FileInfo {
    staged: Staged,
    /// The old side has the file (HEAD, or the index for unstaged changes): discarding
    /// restores it rather than deleting the file.
    in_base: bool,
    /// What "viewed" remembers: the path and its new side.
    new_hash: u64,
    /// A merge left conflict markers in it.
    conflicted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Screen {
    Review,
    Settings,
}

/// The word changes of one line, being picked: a removed line and the added line that
/// replaces it, in one hunk.
struct WordPick {
    file: usize,
    hunk: usize,
    removed: usize,
    added: usize,
    path: String,
    old: String,
    new: String,
    edits: Vec<WordEdit>,
    picked: Vec<bool>,
}

/// A discard waiting for the reader to confirm it.
#[derive(Clone)]
struct Discard {
    file: usize,
    hunk: Option<usize>,
    path: String,
}

/// What the name typed into the prompt dialog is for.
#[derive(Clone, Debug, PartialEq)]
enum Ask {
    /// A new branch at a commit (HEAD when `None`), switched to.
    Branch(Option<String>),
    /// A new stash's message.
    Stash,
    /// A comment on a line, or a new text for comment `n`.
    Comment(diff::Anchor, Option<usize>),
    /// A new name for a branch.
    Rename(String),
    /// A tag at a commit (HEAD when `None`).
    Tag(Option<String>),
}

/// A repository change that throws work away, waiting for the reader to confirm it.
#[derive(Clone, Debug, PartialEq)]
enum Risky {
    /// `git reset --hard` to a commit.
    ResetHard(String),
    DropStash(String),
    /// A local branch, and whether its commits are all in HEAD.
    DeleteBranch(String, bool),
    /// A tag, and whether to delete it from `origin` too.
    DeleteTag(String, bool),
    /// `git push --force-with-lease` of the current branch.
    ForcePush,
    /// Put the working tree back as a checkpoint (its commit and time) had it.
    RestoreCheckpoint(String, i64),
}

struct Notice {
    text: String,
    error: bool,
    /// When it goes; set on the first frame after it's posted.
    until: Option<f64>,
    /// A discard's backup, offered back as "Undo".
    undo: Option<PathBuf>,
}

struct Collate {
    name: String,
    branch: String,
    head: Option<String>,
    scope: Scope,
    /// The branch a branch review compares against, the merge base it resolved to, and the
    /// branches to pick from.
    base: Option<String>,
    base_commit: Option<String>,
    branches: Vec<String>,
    base_open: bool,
    loaded: bool,
    error: Option<String>,
    settings: Settings,
    save_error: Option<String>,
    screen: Screen,
    diff: DiffState,
    tree: TreeModel<FileNode<Stat>>,
    filter: TextEdit,
    /// Each shown file's content hash, in the diff's order.
    hashes: Vec<u64>,
    info: Vec<FileInfo>,
    /// Files marked viewed, with the content hash they were viewed at: an edit since unmarks them.
    viewed: HashMap<String, u64>,
    /// Low-signal files left out of the review, and whether the reader asked to see them.
    hidden: usize,
    show_low_signal: bool,
    /// Working-copy counts, whatever the view: files with something staged, with something
    /// unstaged, and changed at all.
    staged: usize,
    /// `staged` without hidden low-signal files: what the sidebar shows.
    staged_shown: usize,
    unstaged: usize,
    total: usize,
    /// The staging list's second half (Staged Files); `tree` is the first (Unstaged, or the
    /// view's changed files), and the panel that holds them.
    staged_tree: TreeModel<FileNode<Stat>>,
    inspector: PaneState,
    message: TextEdit,
    committing: bool,
    /// The coding agent is writing the commit message.
    writing_message: bool,
    confirm: Option<Discard>,
    notice: Option<Notice>,
    sidebar: PaneState,
    /// Text searched for in the diff, the lines holding it, and the one last jumped to.
    search: TextEdit,
    /// The search's options: match case, whole words, a regular expression; and why the
    /// pattern doesn't parse, when it doesn't.
    find_case: bool,
    find_word: bool,
    find_regex: bool,
    search_error: Option<String>,
    hits: Vec<(usize, LineRef)>,
    hit: Option<usize>,
    /// Files marked viewed that changed since: shown, not silently unmarked.
    stale: HashSet<String>,
    /// Paths already shown once, so a large file folds the first time only.
    seen: HashSet<String>,
    show_help: bool,
    /// History: recent commits (ids and labels, newest first) and the one shown.
    commit_ids: Vec<String>,
    graph_commits: Vec<graph::Commit>,
    graph_rows: Vec<graph::Row>,
    graph_lanes: usize,
    graph_limit: usize,
    /// A longer history is being read in the background.
    graph_loading: bool,
    graph_version: u64,
    commit: Option<String>,
    /// History: the chosen commit's diff fills the window (from its file list or Enter);
    /// otherwise the commit list does.
    history_diff: bool,
    /// The repository's web page, for "View on GitHub".
    web: Option<String>,
    /// Commit authors' GitHub pictures, by email.
    avatars: avatars::Avatars,
    /// Changed images in the diff, decoded, by the hash of their bytes.
    images: HashMap<u64, rune::Image>,
    /// The repository map, shown in place of the diff while open.
    map: map_view::MapState,
    /// Where back and forward go.
    nav: nav::Nav,
    /// The shown commit's message after its subject.
    commit_body: String,
    words: Option<WordPick>,
    /// Projects the reader added, most recently opened first, and the switcher's state.
    projects: Vec<PathBuf>,
    /// The starred projects, which the switcher lists first.
    favorites: Vec<PathBuf>,
    /// The terminal drawer under the diff and its shells.
    terminals: terminal::Terminals,
    project_open: bool,
    project_search: TextEdit,
    /// The switcher's keyboard pick: an index into `project_order`, or one past it for "Add".
    project_pick: usize,
    /// The folder picker is up: the native dialog runs on its own thread.
    picking: bool,
    /// A project is open (the welcome screen shows until one is).
    has_project: bool,
    /// The settings panel's section.
    settings_tab: usize,
    /// Downloaded grammars: loaded, being prepared, or failed.
    languages: grammars::Languages,
    /// The Pull Requests place and the pull request under review.
    pulls: pulls::PullsState,
    /// Whether a newer Collate was looked for, found or installed.
    update: update::Status,
    /// The next commit rewrites the last one.
    amend: bool,
    /// Commits ahead of and behind the upstream.
    ahead_behind: Option<(usize, usize)>,
    /// A merge, rebase, cherry-pick or revert waiting for its conflicts.
    operation: Option<git::Operation>,
    /// A slow repository change (fetch, push, switch) running in the background, as it's named.
    busy: Option<String>,
    /// The name asked for, and what for.
    prompt: Option<Ask>,
    prompt_text: TextEdit,
    risky: Option<Risky>,
    /// The compare dialog's two ends (branches, remote branches, tags) while it's open, and
    /// the refs to pick from.
    compare: Option<(String, String)>,
    refs: Vec<String>,
    /// The names of the two ends of the range History shows, when it shows one.
    compared: Option<(String, String)>,
    /// The reader's comments on lines, which of them the diff shows (their line is in it), and
    /// a count that moves when they change.
    comments: Vec<Comment>,
    comments_shown: Vec<bool>,
    comments_version: u64,
    /// A file's blame, and a file's history, while shown.
    blame: Option<(String, Vec<git::Blamed>)>,
    file_log: Option<(String, Vec<graph::Commit>)>,
    /// ⌘P's palette is up: what's typed into it (a file, `file:line` or `:line`), and the
    /// match picked. Like ⌘F, the app takes the keys itself.
    jumping: bool,
    jump: String,
    jump_pick: usize,
    /// The toolbar search's text in the Map and in History (the diff's is `search`), what it
    /// found there, best first, and the row picked.
    map_query: TextEdit,
    history_query: TextEdit,
    found: Vec<search::Found>,
    found_pick: usize,
    /// The staging panel's lists folded to their titles, by key ("unstaged", "staged").
    folded_lists: HashSet<&'static str>,
}

/// What the toolbar's search looks through: the diff's lines, the Map's files, or History's
/// commits and refs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Searched {
    Diff,
    Map,
    History,
}

fn init() -> Collate {
    let settings = Settings::load();
    let repo = current_repo();
    let branches = repo.as_deref().map(Repo::branches).unwrap_or_default();
    let base = git::default_base(&branches);
    let mut s = Collate {
        name: repo.as_deref().map(Repo::name).unwrap_or_default(),
        branch: String::new(),
        head: None,
        scope: settings.default_scope,
        base,
        base_commit: None,
        branches,
        base_open: false,
        loaded: false,
        error: None,
        settings,
        save_error: None,
        screen: Screen::Review,
        diff: DiffState::new(Vec::new()),
        tree: TreeModel::new(),
        filter: TextEdit::new(""),
        hashes: Vec::new(),
        info: Vec::new(),
        viewed: HashMap::new(),
        hidden: 0,
        show_low_signal: false,
        staged: 0,
        staged_shown: 0,
        unstaged: 0,
        total: 0,
        staged_tree: TreeModel::new(),
        inspector: PaneState::new(300.0).range(220.0, 520.0),
        message: TextEdit::new(""),
        committing: false,
        writing_message: false,
        confirm: None,
        notice: None,
        sidebar: PaneState::new(290.0).range(180.0, 560.0),
        search: TextEdit::new(""),
        find_case: false,
        find_word: false,
        find_regex: false,
        search_error: None,
        hits: Vec::new(),
        hit: None,
        stale: HashSet::new(),
        seen: HashSet::new(),
        show_help: false,
        map_query: TextEdit::new(""),
        history_query: TextEdit::new(""),
        found: Vec::new(),
        found_pick: 0,
        folded_lists: HashSet::new(),
        jumping: false,
        jump: String::new(),
        jump_pick: 0,
        words: None,
        projects: load_projects(),
        favorites: load_paths("favorites"),
        terminals: terminal::Terminals::default(),
        project_open: false,
        project_search: TextEdit::new(""),
        project_pick: 0,
        picking: false,
        has_project: current_repo().is_some(),
        settings_tab: 0,
        languages: grammars::Languages::new(grammars::root().as_deref()),
        pulls: pulls::PullsState::default(),
        update: update::Status::default(),
        commit_ids: Vec::new(),
        graph_commits: Vec::new(),
        graph_rows: Vec::new(),
        graph_lanes: 1,
        graph_limit: HISTORY_PAGE,
        graph_loading: false,
        graph_version: 0,
        commit: None,
        history_diff: false,
        web: None,
        avatars: avatars::Avatars::default(),
        images: HashMap::new(),
        map: map_view::MapState::default(),
        nav: nav::Nav::default(),
        commit_body: String::new(),
        amend: false,
        ahead_behind: None,
        operation: None,
        busy: None,
        prompt: None,
        prompt_text: TextEdit::new(""),
        risky: None,
        compare: None,
        refs: Vec::new(),
        compared: None,
        blame: None,
        file_log: None,
        comments: Vec::new(),
        comments_shown: Vec::new(),
        comments_version: 0,
    };
    s.viewed = load_viewed();
    s.comments = load_comments();
    if s.scope == Scope::History {
        s.load_history();
    }
    s.apply_settings(None);
    s
}

impl Collate {
    fn set_scope(&mut self, scope: Scope) {
        // Any view of the changes leaves the pull request list, and any but History its pull request.
        self.pulls.open = false;
        if scope != Scope::History {
            self.close_pull();
        }
        if self.scope != scope {
            self.scope = scope;
            self.loaded = false;
            self.error = None;
            if scope == Scope::Branch {
                self.load_branches();
            }
            if scope == Scope::History {
                self.load_history();
            }
            if let Some(w) = WATCHER.get() {
                let _ = w.send(Cmd::Scope(scope));
            }
        }
    }

    /// Opens the git repository containing `dir` in this window: the review starts over on
    /// it, and it goes to the top of the project list.
    fn open_project(&mut self, dir: &std::path::Path, tx: &mut Tx) {
        self.close_projects();
        let repo = match Repo::open(dir) {
            Ok(r) => r,
            Err(e) => {
                self.say(e, true);
                return;
            }
        };
        if current_repo().is_some_and(|r| r.root == repo.root) {
            return;
        }
        let root = repo.root.clone();
        self.has_project = true;
        self.name = repo.name();
        self.branches = repo.branches();
        self.base = git::default_base(&self.branches);
        tx.set_title(format!("{} — Collate", self.name));
        set_repo(Some(repo));
        watch_project_files(Some(root.clone()));
        remember_project(&mut self.projects, &self.favorites, root);
        // Everything read from the last project goes.
        self.branch.clear();
        self.head = None;
        self.base_commit = None;
        self.loaded = false;
        self.error = None;
        self.diff.set_files(Vec::new());
        self.tree = TreeModel::new();
        self.staged_tree = TreeModel::new();
        self.hashes.clear();
        self.info.clear();
        self.viewed = load_viewed();
        self.comments = load_comments();
        // The pull requests were the last project's, but the tab stays: a pull request being
        // reviewed goes back to the new project's list. That shows as last read while it's read again.
        let on_pulls = self.pulls.open || self.pulls.pr.is_some();
        let folded = std::mem::take(&mut self.pulls.folded);
        self.pulls = pulls::PullsState { open: on_pulls, folded, ..Default::default() };
        self.pulls.list = current_repo().and_then(|r| pulls::cached(&r.root)).map(Ok);
        self.load_pulls();
        self.place_comments();
        self.stale.clear();
        self.seen.clear();
        self.hidden = 0;
        self.show_low_signal = false;
        self.staged = 0;
        self.message = TextEdit::new("");
        self.confirm = None;
        self.words = None;
        self.search = TextEdit::new("");
        self.hits.clear();
        self.hit = None;
        self.commit = None;
        self.history_diff = false;
        self.commit_ids.clear();
        self.graph_commits.clear();
        self.reset_map();
        self.forget_places();
        self.graph_rows.clear();
        self.graph_limit = HISTORY_PAGE;
        self.graph_loading = false;
        if self.scope == Scope::History {
            self.load_history();
        }
        if let Some(w) = WATCHER.get() {
            let _ = w.send(Cmd::Switch { base: self.base.clone(), commit: self.commit.clone() });
        }
    }

    /// Ask macOS for a folder (the standard Open dialog, off the UI thread) and open it.
    fn add_project(&mut self) {
        self.close_projects();
        let Some(app) = HANDLE.get().cloned() else { return };
        if std::mem::replace(&mut self.picking, true) {
            return;
        }
        std::thread::spawn(move || {
            let out = std::process::Command::new("osascript")
                .args(["-e", "POSIX path of (choose folder with prompt \"Choose a project (a folder in a git repository)\")"])
                .output();
            app.update(move |s, tx| {
                s.picking = false;
                match out {
                    Ok(o) if o.status.success() => {
                        let path = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                        s.open_project(std::path::Path::new(&path), tx);
                    }
                    // Cancelled (error -128): nothing to say.
                    Ok(o) if String::from_utf8_lossy(&o.stderr).contains("-128") => {}
                    Ok(o) => s.say(format!("Couldn't open the folder picker: {}", String::from_utf8_lossy(&o.stderr).trim()), true),
                    Err(e) => s.say(format!("Couldn't open the folder picker: {e}"), true),
                }
            });
        });
    }

    fn forget_project(&mut self, i: usize) {
        if i < self.projects.len() {
            let p = self.projects.remove(i);
            save_projects(&self.projects);
            if self.favorites.contains(&p) {
                self.toggle_favorite(&p);
            }
        }
    }

    fn toggle_favorite(&mut self, p: &std::path::Path) {
        match self.favorites.iter().position(|f| f == p) {
            Some(i) => {
                self.favorites.remove(i);
            }
            None => self.favorites.push(p.to_owned()),
        }
        save_paths("favorites", &self.favorites);
    }

    /// ⌘J: shows or hides the terminal drawer, starting a shell the first time.
    fn toggle_terminal(&mut self) {
        if self.terminals.open {
            return self.terminals.set_open(false);
        }
        if self.terminals.list.is_empty() {
            self.new_terminal();
        } else {
            self.terminals.set_open(true);
        }
    }

    /// Another shell, in the project's folder (home without one).
    fn new_terminal(&mut self) {
        let dir = current_repo().map(|r| r.root.clone()).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
        if let Err(e) = self.terminals.spawn(&dir) {
            self.say(e, true);
        }
    }

    /// Opens or closes the project switcher; it opens on an empty search, the open project picked.
    fn toggle_projects(&mut self) {
        if self.project_open {
            return self.close_projects();
        }
        self.project_open = true;
        self.project_search.clear();
        self.project_search.focused = true;
        let order = project_order(&self.projects, &self.favorites, "");
        self.project_pick = current_repo().and_then(|r| order.iter().position(|&i| self.projects[i] == r.root)).unwrap_or(0);
    }

    fn close_projects(&mut self) {
        self.project_open = false;
        self.project_search.focused = false;
    }

    /// Return in the switcher: opens the picked project, or the folder picker for "Add".
    fn choose_project(&mut self, tx: &mut Tx) {
        let order = project_order(&self.projects, &self.favorites, &self.project_search.text);
        match order.get(self.project_pick) {
            Some(&i) => {
                let p = self.projects[i].clone();
                self.open_project(&p, tx);
            }
            None => self.add_project(),
        }
    }

    /// Reads the recent commits off the UI thread, keeping the one shown if it's still there,
    /// else HEAD's.
    fn load_history(&mut self) {
        let limit = self.graph_limit;
        self.read_bg(move |r| (r.graph(limit, None), r.web_url()), |s, ((commits, head), web)| {
            s.web = web;
            s.show_history(commits, head);
        });
    }

    /// Reads the branches again, off the UI thread.
    fn load_branches(&mut self) {
        self.read_bg(Repo::branches, |s, branches| s.branches = branches);
    }

    /// Near the end of the list: reads the next page off the main thread, so the list keeps
    /// scrolling, and shows it when it's in. Without a window (tests) it reads it right away.
    fn load_more_history(&mut self) {
        if self.graph_loading || self.commit_ids.len() < self.graph_limit {
            return;
        }
        self.graph_limit += HISTORY_PAGE;
        let (Some(r), Some(app)) = (current_repo(), HANDLE.get().cloned()) else {
            self.load_history();
            return;
        };
        self.graph_loading = true;
        let limit = self.graph_limit;
        std::thread::spawn(move || {
            let (commits, head) = r.graph(limit, None);
            app.update(move |s, _| {
                // A project switch or refresh since started over: this page is stale.
                if s.graph_loading && s.graph_limit == limit {
                    s.graph_loading = false;
                    s.show_history(commits, head);
                }
            });
        });
    }

    fn show_history(&mut self, commits: Vec<graph::Commit>, head: Option<String>) {
        // Only real commits can be stepped through and diffed against their parent.
        self.commit_ids = commits.iter().map(|c| c.id.clone()).collect();
        let commits = graph::with_uncommitted(commits, head.as_deref(), self.total > 0);
        (self.graph_rows, self.graph_lanes) = graph::layout(&commits);
        self.avatars.request(self.web.as_deref(), &commits);
        self.graph_commits = commits;
        self.graph_version = self.graph_version.wrapping_add(1);
        // A commit opened from elsewhere (a stash, a file's history, a compare) stays shown
        // though the graph doesn't list it.
        let keep = self.commit.clone();
        if let Some(c) = keep.or_else(|| head.filter(|h| self.commit_ids.contains(h))).or_else(|| self.commit_ids.first().cloned()) {
            self.set_commit(&c);
        }
    }

    fn set_commit(&mut self, id: &str) {
        if self.commit.as_deref() == Some(id) {
            return;
        }
        self.commit = Some(id.to_owned());
        self.commit_body.clear();
        // A range has no one message. Read off the UI thread, so stepping through commits stays quick.
        if !id.contains("..") {
            let id = id.to_owned();
            self.read_bg(move |r| (r.message(&id), id), |s, (message, id)| {
                if s.commit.as_deref() == Some(id.as_str()) {
                    s.commit_body = message.split_once('\n').map(|(_, rest)| rest.trim().to_owned()).unwrap_or_default();
                }
            });
        }
        if self.scope == Scope::History {
            self.loaded = false;
        }
        if let Some(w) = WATCHER.get() {
            let _ = w.send(Cmd::Commit(id.to_owned()));
        }
    }

    /// Step to an older (`by` > 0) or newer commit in the History view.
    /// Back from a commit's diff to the commit list, showing the commit it was on.
    fn close_commit_diff(&mut self, tx: &mut Tx) {
        self.history_diff = false;
        self.close_pull();
        if let Some(c) = &self.commit {
            tx.scroll_into_view(format!("graph-{c}"));
        }
    }

    fn step_commit(&mut self, by: isize, tx: &mut Tx) {
        let Some(at) = self.commit.as_ref().and_then(|c| self.commit_ids.iter().position(|x| x == c)) else { return };
        let to = at as isize + by;
        if to >= 0 && (to as usize) < self.commit_ids.len() {
            let id = self.commit_ids[to as usize].clone();
            tx.scroll_into_view(format!("graph-{id}"));
            self.set_commit(&id);
        }
    }

    fn set_base(&mut self, base: &str) {
        self.base = Some(base.to_owned());
        self.base_open = false;
        if self.scope == Scope::Branch {
            self.loaded = false;
        }
        if let Some(w) = WATCHER.get() {
            let _ = w.send(Cmd::Base(base.to_owned()));
        }
    }

    /// Reread now, rebuilding every file even if nothing changed on disk.
    fn reload(&mut self) {
        *OPTS.lock().unwrap() = self.opts();
        self.hashes.clear();
        refresh();
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, until: None, undo: None });
    }

    /// Run a change to the repository, report how it went and reread.
    fn act(&mut self, f: impl FnOnce(&Repo) -> Result<String, String>) {
        let Some(repo) = current_repo() else { return };
        match f(&repo) {
            Ok(done) => self.say(done, false),
            Err(e) => self.say(e, true),
        }
        refresh();
    }

    /// Run a change that can be slow (the network, hooks) or that moves branches, off the UI
    /// thread; then report it and reread the files, the branches and the history. One at a
    /// time. Without a window (tests) it runs here.
    fn act_bg(&mut self, doing: &str, f: impl FnOnce(&Repo) -> Result<String, String> + Send + 'static) {
        let Some(repo) = current_repo() else { return };
        if self.busy.is_some() {
            self.say(format!("Wait for {} to finish", self.busy.as_deref().unwrap_or_default().trim_end_matches('…').to_lowercase()), true);
            return;
        }
        let Some(app) = HANDLE.get().cloned() else {
            let result = f(&repo);
            self.landed(result);
            return;
        };
        self.busy = Some(doing.to_owned());
        std::thread::spawn(move || {
            let result = f(&repo);
            app.update(move |s, _| {
                s.busy = None;
                s.landed(result);
            });
        });
    }

    /// How a change from [`Collate::act_bg`] went, and what it may have moved.
    fn landed(&mut self, result: Result<String, String>) {
        match result {
            Ok(done) => self.say(done, false),
            Err(e) => self.say(e, true),
        }
        self.load_branches();
        if self.scope == Scope::History {
            self.load_history();
        }
        refresh();
    }

    /// Show commit `id`'s diff against its parent (or a range, `a..b`), from any view.
    fn open_commit(&mut self, id: &str) {
        self.compared = None;
        self.pulls.pr = None;
        self.pulls.open = false;
        self.map.open = false;
        self.set_scope(Scope::History);
        self.set_commit(id);
        self.history_diff = true;
    }

    /// The name typed into the prompt: make the branch or the stash.
    fn answer_prompt(&mut self) {
        let Some(ask) = self.prompt.take() else { return };
        let name = self.prompt_text.text.trim().to_owned();
        self.prompt_text = TextEdit::new("");
        match ask {
            Ask::Branch(_) if name.is_empty() => self.say("A branch needs a name", true),
            Ask::Branch(at) => self.act_bg("Creating the branch…", move |r| r.new_branch(&name, at.as_deref(), true).map(|_| format!("Switched to the new branch {name}"))),
            Ask::Stash => self.act_bg("Stashing…", move |r| r.stash(&name).map(|_| "Stashed every change".to_owned())),
            Ask::Tag(_) if name.is_empty() => {}
            Ask::Tag(at) => {
                // `v1.2 First stable release`: the name, then an optional message.
                let (tag, message) = name.split_once(' ').map_or((name.clone(), String::new()), |(t, m)| (t.to_owned(), m.trim().to_owned()));
                let at = at.unwrap_or_else(|| "HEAD".to_owned());
                self.act_bg("Tagging…", move |r| r.tag(&tag, &at, &message).map(|_| format!("Tagged {} as {tag}", abbrev(&at))));
            }
            Ask::Rename(from) if name.is_empty() || name == from => {}
            Ask::Rename(from) => self.act_bg("Renaming…", move |r| r.rename_branch(&from, &name).map(|_| format!("Renamed {from} to {name}"))),
            Ask::Comment(_, Some(n)) if name.is_empty() => self.delete_comment(n),
            Ask::Comment(..) if name.is_empty() => {}
            Ask::Comment(at, edit) => {
                let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                // A suggestion is the line as it should read, in the block GitHub offers to apply.
                let body = if std::mem::take(&mut self.pulls.suggest) { format!("```suggestion\n{name}\n```") } else { name };
                let c = Comment { at, body, time };
                match edit.filter(|&n| n < self.comments.len()) {
                    Some(n) => self.comments[n] = c,
                    None => self.comments.push(c),
                }
                self.store_comments();
            }
        }
    }

    /// Reads something from the repository off the UI thread, and hands it to `done` here,
    /// unless the reader has switched projects meanwhile. Without a window (tests) it reads
    /// right away.
    fn read_bg<T: Send + 'static>(&mut self, read: impl FnOnce(&Repo) -> T + Send + 'static, done: impl FnOnce(&mut Collate, T) + Send + 'static) {
        let Some(repo) = current_repo() else { return };
        match HANDLE.get().cloned() {
            Some(app) => {
                std::thread::spawn(move || {
                    let v = read(&repo);
                    app.update(move |s, _| {
                        if current_repo().is_some_and(|r| r.root == repo.root) {
                            done(s, v);
                        }
                    });
                });
            }
            None => {
                let v = read(&repo);
                done(self, v);
            }
        }
    }

    /// Who last changed each line of file `f`: as of the commit shown in History, else in the
    /// working tree.
    fn show_blame(&mut self, f: usize) {
        let Some((file, _)) = self.file(f) else { return };
        let path = file.path().to_owned();
        let at = match (self.scope, self.commit.as_deref()) {
            (Scope::History, Some(c)) => Some(c.split_once("..").map_or(c, |(_, b)| b).to_owned()),
            _ => None,
        };
        self.read_bg(move |r| (path.clone(), r.blame(&path, at.as_deref())), |s, (path, result)| match result {
            Ok(lines) => s.blame = Some((path, lines)),
            Err(e) => s.say(format!("Couldn't blame {path}: {e}"), true),
        });
    }

    /// The commits that changed file `f`.
    fn show_file_log(&mut self, f: usize) {
        let Some((file, _)) = self.file(f) else { return };
        let path = file.path().to_owned();
        self.read_bg(move |r| (path.clone(), r.file_log(&path)), |s, (path, result)| match result {
            Ok(commits) => s.file_log = Some((path, commits)),
            Err(e) => s.say(format!("Couldn't read the history of {path}: {e}"), true),
        });
    }

    /// Opens the compare dialog, from the base to this branch to begin with.
    fn start_compare(&mut self) {
        self.read_bg(Repo::refs, |s, refs| {
            s.refs = refs;
            let to = if s.branch == "HEAD" { "HEAD".to_owned() } else { s.branch.clone() };
            let from = s.base.clone().filter(|b| *b != to).or_else(|| s.refs.iter().find(|b| **b != to).cloned()).unwrap_or_default();
            s.compare = Some((from, to));
        });
    }

    /// The commits a push would send, as one diff from where the branch left its upstream.
    fn show_unpushed(&mut self) {
        let Some(head) = self.head.clone() else { return };
        let Some(r) = current_repo() else { return };
        match r.unpushed() {
            Ok((upstream, base)) => {
                let on = if self.branch == "HEAD" { "HEAD".to_owned() } else { self.branch.clone() };
                self.open_commit(&format!("{base}..{head}"));
                self.compared = Some((upstream, on));
            }
            Err(e) => self.say(e, true),
        }
    }

    /// Picks one end of the compare (`to`, else `from`) from the refs.
    fn pick_ref(&mut self, to: bool, tx: &mut Tx) {
        let Some((from, until)) = &self.compare else { return };
        let current = if to { until.clone() } else { from.clone() };
        let mut refs: Vec<String> = self.refs.clone();
        if !refs.contains(&"HEAD".to_owned()) {
            refs.insert(0, "HEAD".to_owned());
        }
        let menu = PopupMenu::picker(refs.iter().map(|r| (r.clone(), r.clone())), &current);
        tx.popup_menu(menu, if to { "compare-to" } else { "compare-from" }, move |s: &mut Collate, picked, _| {
            if let (Some(p), Some(c)) = (picked, &mut s.compare) {
                if to { c.1 = p.to_owned() } else { c.0 = p.to_owned() }
            }
        });
    }

    /// Shows what changed from one ref to the other, as History shows a commit.
    fn run_compare(&mut self) {
        let Some((from, to)) = self.compare.take() else { return };
        let Some(r) = current_repo() else { return };
        match (r.resolve(&from), r.resolve(&to)) {
            (Ok(a), Ok(b)) => {
                self.open_commit(&format!("{a}..{b}"));
                self.compared = Some((from, to));
            }
            (Err(e), _) | (_, Err(e)) => self.say(e, true),
        }
    }

    fn ask(&mut self, what: Ask) {
        self.prompt = Some(what);
        self.pulls.suggest = false;
        self.prompt_text = TextEdit::new("");
    }

    fn confirm_risky(&mut self) {
        match self.risky.take() {
            Some(Risky::ResetHard(id)) => self.act_bg("Resetting…", move |r| r.reset("hard", &id).map(|_| format!("Reset to {} and threw the changes away", abbrev(&id)))),
            Some(Risky::DropStash(name)) => self.act_bg("Dropping the stash…", move |r| r.stash_do("drop", &name).map(|_| format!("Dropped {name}"))),
            Some(Risky::ForcePush) => self.act_bg("Force pushing…", |r| r.force_push().map(|_| "Force pushed".to_owned())),
            Some(Risky::DeleteTag(name, remote)) => self.act_bg("Deleting the tag…", move |r| {
                r.delete_tag(&name, remote).map(|_| if remote { format!("Deleted {name} here and from origin") } else { format!("Deleted {name}") })
            }),
            Some(Risky::DeleteBranch(name, _)) => self.act_bg("Deleting the branch…", move |r| r.delete_branch(&name).map(|_| format!("Deleted {name}"))),
            Some(Risky::RestoreCheckpoint(sha, _)) => self.restore_checkpoint(sha),
            None => {}
        }
    }

    /// The chosen commit's "⋯" menu in History: build on it, undo it, or move the branch to it.
    fn commit_menu(&mut self, anchor: &str, tx: &mut Tx) {
        let Some(id) = self.commit.clone().filter(|c| c != graph::UNCOMMITTED && !c.contains("..")) else { return };
        let on = if self.branch == "HEAD" { "HEAD".to_owned() } else { self.branch.clone() };
        let is_head = self.head.as_deref() == Some(id.as_str());
        let reset = PopupMenu::new()
            .item(PopupItem::new("reset-soft", "Soft: Keep Changes Staged"))
            .item(PopupItem::new("reset-mixed", "Mixed: Keep Changes Unstaged"))
            .item(PopupItem::new("reset-hard", "Hard: Discard Changes…"));
        let menu = PopupMenu::new()
            .item(PopupItem::new("copy", "Copy Hash"))
            .item(PopupItem::new("branch", "New Branch Here…"))
            .item(PopupItem::new("tag", "Tag This Commit…"))
            .separator()
            .item(PopupItem::new("pick", format!("Cherry-Pick onto {on}")).enabled(!is_head))
            .item(PopupItem::new("revert", "Revert Commit"))
            .item(PopupItem::new("compare", format!("Compare with {on}")).enabled(!is_head && self.head.is_some()))
            .submenu_enabled(format!("Reset {on} to Here"), reset, !is_head);
        tx.popup_menu(menu, anchor.to_owned(), move |s: &mut Collate, picked, tx| {
            let id = id.clone();
            let short_id = abbrev(&id);
            match picked {
                Some("copy") => {
                    tx.copy(id);
                    s.say("Copied the commit hash", false);
                }
                Some("branch") => s.ask(Ask::Branch(Some(id))),
                Some("tag") => s.ask(Ask::Tag(Some(id))),
                Some("pick") => s.act_bg("Cherry-picking…", move |r| r.cherry_pick(&id).map(|_| format!("Cherry-picked {short_id}"))),
                Some("revert") => s.act_bg("Reverting…", move |r| r.revert(&id).map(|_| format!("Reverted {short_id}"))),
                Some("reset-soft") => s.act_bg("Resetting…", move |r| r.reset("soft", &id).map(|_| format!("Reset to {short_id}; its later changes are staged"))),
                Some("reset-mixed") => s.act_bg("Resetting…", move |r| r.reset("mixed", &id).map(|_| format!("Reset to {short_id}; its later changes are unstaged"))),
                Some("reset-hard") => s.risky = Some(Risky::ResetHard(id)),
                Some("compare") => {
                    let (Some(head), on) = (s.head.clone(), if s.branch == "HEAD" { "HEAD".to_owned() } else { s.branch.clone() }) else { return };
                    s.open_commit(&format!("{id}..{head}"));
                    s.compared = Some((short_id, on));
                }
                _ => {}
            }
        });
    }

    fn file(&self, i: usize) -> Option<(&FileDiff, FileInfo)> {
        Some((self.diff.files().get(i)?, *self.info.get(i)?))
    }

    fn stage_hunk(&mut self, f: usize, h: usize, picked: Option<Vec<bool>>) {
        let Some((file, _)) = self.file(f) else { return };
        let Some(p) = patch::lines(file, h, picked.as_deref(), false) else { return };
        let scope = self.scope;
        let what = if picked.is_some() { "the selected lines" } else { "the hunk" };
        self.selection_clear();
        self.act(|r| {
            r.apply(&p, true, false).map(|_| format!("Staged {what}")).map_err(|e| match scope {
                // HEAD → working tree hunks apply to the index only where it still matches HEAD.
                Scope::Uncommitted => format!("Couldn't stage this hunk here; part of the file is already staged. Stage it from Unstaged. ({e})"),
                _ => e,
            })
        });
    }

    fn unstage_hunk(&mut self, f: usize, h: usize, picked: Option<Vec<bool>>) {
        let Some((file, _)) = self.file(f) else { return };
        let Some(p) = patch::lines(file, h, picked.as_deref(), true) else { return };
        let what = if picked.is_some() { "the selected lines" } else { "the hunk" };
        self.selection_clear();
        self.act(|r| r.apply(&p, true, true).map(|_| format!("Unstaged {what}")));
    }

    /// When the selection is one removed line and one added line of a hunk (a row of the
    /// split view), the word changes between them, for picking.
    fn word_candidate(&self) -> Option<WordPick> {
        let (file, hunk, picked) = self.diff.selected_changes()?;
        let f = self.diff.files().get(file)?;
        let lines = &f.hunks.get(hunk)?.lines;
        let chosen: Vec<usize> = (0..lines.len()).filter(|&i| picked[i]).collect();
        let [a, b] = chosen[..] else { return None };
        let (removed, added) = match (lines[a].kind, lines[b].kind) {
            (LineKind::Removed, LineKind::Added) => (a, b),
            (LineKind::Added, LineKind::Removed) => (b, a),
            _ => return None,
        };
        let (old, new) = (lines[removed].text.clone(), lines[added].text.clone());
        let edits = word_edits(&old, &new);
        if edits.is_empty() {
            return None;
        }
        let picked = vec![true; edits.len()];
        Some(WordPick { file, hunk, removed, added, path: f.path().to_owned(), old, new, edits, picked })
    }

    fn pick_words(&mut self) {
        if self.scope.read_only() || self.settings.ignore_whitespace {
            return;
        }
        match self.word_candidate() {
            Some(w) => self.words = Some(w),
            None => self.say("Pick one changed line and its replacement first: click a row's line number in split view.", true),
        }
    }

    /// Stage, unstage or discard the picked word changes of the line being picked.
    fn act_on_words(&mut self, how: Scope) {
        let Some(w) = self.words.take() else { return };
        // The list may have refreshed while the dialog was up: act only on the same file.
        let Some(file) = self.diff.files().get(w.file).filter(|f| f.path() == w.path).cloned() else {
            self.say("The changes moved while the dialog was open; nothing was done.", true);
            return;
        };
        let n = w.picked.iter().filter(|&&p| p).count();
        let what = format!("{n} word change{} in {}", if n == 1 { "" } else { "s" }, w.path);
        // Forwards (stage) the old line takes the picked edits; backwards (unstage, discard)
        // the new line keeps only the others.
        let staging = how == Scope::Unstaged;
        let target = if staging { apply_edits(&w.old, &w.new, &w.edits, &w.picked) } else { apply_edits(&w.old, &w.new, &w.edits, &w.picked.iter().map(|p| !p).collect::<Vec<_>>()) };
        let Some(p) = patch::line_to(&file, w.hunk, w.removed, w.added, &target, !staging) else { return };
        self.selection_clear();
        match how {
            Scope::Unstaged => self.act(|r| r.apply(&p, true, false).map(|_| format!("Staged {what}"))),
            Scope::Staged => self.act(|r| r.apply(&p, true, true).map(|_| format!("Unstaged {what}"))),
            _ => {
                let Some(repo) = current_repo() else { return };
                let result = repo.backup(&w.path).and_then(|b| repo.apply(&p, false, true).map(|_| b));
                self.after_discard(result, format!("Discarded {what}"));
            }
        }
    }

    /// The current view's name, for messages that say where something isn't.
    fn scope_title(&self) -> String {
        match self.scope {
            Scope::Uncommitted => "All Changes".into(),
            Scope::Staged => "Staged Changes".into(),
            Scope::Unstaged => "Unstaged Changes".into(),
            // As GitHub Desktop names it: the branch's whole change against its base.
            Scope::Branch => "Pull Request Preview".into(),
            Scope::History if self.history_diff && self.commit.as_deref().is_some_and(|c| c.contains("..")) => "Compare".into(),
            Scope::History => "Git Graph".into(),
        }
    }

    fn stage_all(&mut self) {
        self.act(|r| r.stage_all().map(|_| "Staged all changes".to_owned()));
    }

    fn unstage_all(&mut self) {
        self.act(|r| r.unstage_all().map(|_| "Unstaged everything".to_owned()));
    }

    fn stage_path(&mut self, path: String) {
        self.act(|r| r.stage(&path).map(|_| format!("Staged {path}")));
    }

    fn unstage_path(&mut self, path: String) {
        self.act(|r| r.unstage(&path).map(|_| format!("Unstaged {path}")));
    }

    /// The file header's "⋯" menu: everything else you can do to the file, the likely action
    /// first and Discard (which asks) last.
    fn file_menu(&mut self, f: usize, anchor: &str, tx: &mut Tx) {
        let Some((file, info)) = self.file(f) else { return };
        let path = file.path().to_owned();
        let editable = !self.scope.read_only();
        let mut menu = PopupMenu::new();
        if editable {
            menu = match (self.scope, info.staged) {
                (Scope::Staged, _) | (Scope::Uncommitted, Staged::Fully) => menu.item(PopupItem::new("unstage", "Unstage File")),
                (Scope::Uncommitted, Staged::Partly) => menu.item(PopupItem::new("stage", "Stage File")).item(PopupItem::new("unstage", "Unstage File")),
                _ => menu.item(PopupItem::new("stage", "Stage File")),
            };
            menu = menu.separator();
        }
        menu = menu
            .item(PopupItem::new("open", "Open in Editor").enabled(file.new_path.is_some()))
            .item(PopupItem::new("copy", "Copy Path"))
            .item(PopupItem::new("viewed", "Viewed").checked(self.is_viewed(f)))
            .separator()
            .item(PopupItem::new("blame", "Blame").enabled(file.new_path.is_some()))
            .item(PopupItem::new("log", "File History"));
        if matches!(self.scope, Scope::Uncommitted | Scope::Unstaged) {
            menu = menu.separator().item(PopupItem::new("discard", "Discard Changes…"));
        }
        tx.popup_menu(menu, anchor.to_owned(), move |s: &mut Collate, picked, tx| match picked {
            Some("stage") => s.stage_file(f),
            Some("unstage") => s.unstage_file(f),
            Some("open") => s.open_file(f),
            Some("copy") => {
                tx.copy(path.clone());
                s.say(format!("Copied {path}"), false);
            }
            Some("viewed") => s.toggle_viewed(f, tx),
            Some("blame") => s.show_blame(f),
            Some("log") => s.show_file_log(f),
            Some("discard") => s.ask_discard(f, None),
            _ => {}
        });
    }

    /// The toolbar's "⋯" menu: how the diff is drawn, and the app's less frequent commands.
    /// The title bar's branch menu, as GitHub Desktop's: sync with the remote, switch or make a
    /// branch, stash, and the branch as a pull request against a base.
    fn branch_menu(&mut self, tx: &mut Tx) {
        let repo = current_repo();
        if let Some(r) = &repo {
            self.branches = r.branches();
        }
        let stashes = repo.as_deref().map(Repo::stashes).unwrap_or_default();
        let base = self.base.clone().unwrap_or_default();
        let (ahead, behind) = self.ahead_behind.unwrap_or((0, 0));
        let diverged = ahead > 0 && behind > 0;
        let count = |verb: &str, n: usize| if n == 0 { verb.to_owned() } else { format!("{verb} {n} Commit{}", if n == 1 { "" } else { "s" }) };
        let idle = self.busy.is_none();
        let mut switch = PopupMenu::new();
        let locals: HashSet<&str> = self.branches.iter().filter(|b| !b.starts_with("origin/")).map(String::as_str).collect();
        for b in self.branches.iter().filter(|b| **b != self.branch) {
            // A remote branch already checked out here is its local branch.
            if b.strip_prefix("origin/").is_some_and(|l| locals.contains(l) || l == self.branch) {
                continue;
            }
            switch = switch.item(PopupItem::new(format!("switch:{b}"), b.clone()));
        }
        let tags = repo.as_deref().map(Repo::tags).unwrap_or_default();
        let mut tag_menu = PopupMenu::new().item(PopupItem::new("new-tag", "New Tag at HEAD…").enabled(idle)).separator();
        for t in tags.iter().take(30) {
            let one = PopupMenu::new()
                .item(PopupItem::new(format!("show-tag:{t}"), "Show Commit"))
                .item(PopupItem::new(format!("push-tag:{t}"), "Push to origin").enabled(idle))
                .separator()
                .item(PopupItem::new(format!("delete-tag:{t}"), "Delete…").enabled(idle))
                .item(PopupItem::new(format!("delete-remote-tag:{t}"), "Delete Here and from origin…").enabled(idle));
            tag_menu = tag_menu.submenu(t.clone(), one);
        }
        let mut stash_menu = PopupMenu::new();
        for st in &stashes {
            let one = PopupMenu::new()
                .item(PopupItem::new(format!("show:{}", st.id), "Show Changes"))
                .item(PopupItem::new(format!("apply:{}", st.name), "Apply"))
                .item(PopupItem::new(format!("pop:{}", st.name), "Apply and Drop"))
                .separator()
                .item(PopupItem::new(format!("drop:{}", st.name), "Drop…"));
            stash_menu = stash_menu.submenu(format!("{} - {}", st.name, st.message), one);
        }
        // Any other branch can be merged in.
        let (mut merges, mut rebases) = (PopupMenu::new(), PopupMenu::new());
        for b in self.branches.iter().filter(|b| **b != self.branch).take(40) {
            // Its own upstream can be rebased onto (to take what others pushed), not merged in.
            if b.strip_prefix("origin/") != Some(self.branch.as_str()) {
                merges = merges.item(PopupItem::new(format!("merge:{b}"), b.clone()));
            }
            rebases = rebases.item(PopupItem::new(format!("rebase:{b}"), b.clone()));
        }
        let detached = self.branch.is_empty() || self.branch == "HEAD";
        // Local branches other than this one can be deleted.
        let mut deletes = PopupMenu::new();
        for b in self.branches.iter().filter(|b| **b != self.branch && !b.starts_with("origin/")).take(40) {
            deletes = deletes.item(PopupItem::new(format!("delete:{b}"), b.clone()));
        }
        let mut bases = PopupMenu::new();
        for b in self.branches.iter().filter(|b| **b != self.branch && (!b.contains('/') || b.starts_with("origin/"))).take(30) {
            bases = bases.item(PopupItem::new(format!("base:{b}"), b.clone()).checked(*b == base));
        }
        let menu = PopupMenu::new()
            .item(PopupItem::new("fetch", "Fetch").enabled(idle))
            .item(PopupItem::new("pull", count("Pull", behind)).enabled(idle))
            .item(PopupItem::new("agent-rebase", format!("Rebase Onto Upstream with {}", self.settings.agent.title())).enabled(idle && diverged && self.operation.is_none()))
            .item(PopupItem::new("push", count("Push", ahead)).enabled(idle))
            .item(PopupItem::new("force-push", "Force Push…").enabled(idle && self.ahead_behind.is_some()))
            .item(PopupItem::new("unpushed", if ahead == 0 { "Show Unpushed Commits".to_owned() } else { format!("Show {ahead} Unpushed Commit{}", if ahead == 1 { "" } else { "s" }) }).enabled(ahead > 0))
            .separator()
            .submenu_enabled("Switch To", switch, idle)
            .item(PopupItem::new("new-branch", "New Branch…").enabled(idle))
            .item(PopupItem::new("rename", format!("Rename {}…", self.branch)).enabled(idle && !detached))
            .submenu_enabled("Delete Branch", deletes, idle)
            .submenu_enabled(format!("Merge Into {}", self.branch), merges, idle && !detached && self.operation.is_none())
            .submenu_enabled(format!("Rebase {} Onto", self.branch), rebases, idle && !detached && self.operation.is_none() && self.total == 0)
            .separator()
            .item(PopupItem::new("stash", "Stash All Changes…").enabled(idle && self.total > 0))
            .submenu_enabled(format!("Stashes ({})", stashes.len()), stash_menu, !stashes.is_empty())
            .submenu(format!("Tags ({})", tags.len()), tag_menu)
            .separator()
            .item(PopupItem::new("preview", if base.is_empty() { "Preview Pull Request".to_owned() } else { format!("Preview Pull Request into {base}") }).checked(self.scope == Scope::Branch && !self.map.open))
            .submenu("Compare Against", bases)
            .item(PopupItem::new("compare", "Compare Branches or Tags…"));
        tx.popup_menu(menu, "branch", move |s: &mut Collate, picked, _| {
            let Some(p) = picked else { return };
            let (what, arg) = p.split_once(':').unwrap_or((p, ""));
            let arg = arg.to_owned();
            match what {
                "fetch" => s.act_bg("Fetching…", |r| r.fetch().map(|_| "Fetched every remote".to_owned())),
                // A pull only fast-forwards; one that can't says what can.
                "pull" if diverged => s.say(format!("This branch and its upstream have both moved, so a pull can't fast-forward: rebase or merge, or Rebase Onto Upstream with {}", s.settings.agent.title()), true),
                "pull" => s.act_bg("Pulling…", |r| r.pull().map(|_| "Pulled".to_owned())),
                "agent-rebase" => s.settle_with_agent(agent::Settle::Diverged(ahead, behind)),
                "push" => s.act_bg("Pushing…", |r| r.push().map(|_| "Pushed".to_owned())),
                "force-push" => s.risky = Some(Risky::ForcePush),
                "switch" => s.act_bg("Switching…", move |r| r.switch(&arg).map(|_| format!("Switched to {arg}"))),
                "merge" => s.act_bg("Merging…", move |r| r.merge(&arg).map(|_| format!("Merged {arg}"))),
                "rebase" => s.act_bg("Rebasing…", move |r| r.rebase(&arg).map(|_| format!("Rebased onto {arg}"))),
                "new-branch" => s.ask(Ask::Branch(None)),
                "rename" => {
                    let from = s.branch.clone();
                    s.ask(Ask::Rename(from.clone()));
                    s.prompt_text = TextEdit::new(&from);
                }
                "delete" => {
                    let merged = current_repo().is_some_and(|r| r.merged(&arg));
                    s.risky = Some(Risky::DeleteBranch(arg, merged));
                }
                "compare" => s.start_compare(),
                "unpushed" => s.show_unpushed(),
                "stash" => s.ask(Ask::Stash),
                "new-tag" => s.ask(Ask::Tag(None)),
                "show-tag" => {
                    if let Some(id) = current_repo().and_then(|r| r.resolve(&arg).ok()) {
                        s.open_commit(&id);
                    }
                }
                "push-tag" => s.act_bg("Pushing the tag…", move |r| r.push_tag(&arg).map(|_| format!("Pushed {arg}"))),
                "delete-tag" => s.risky = Some(Risky::DeleteTag(arg, false)),
                "delete-remote-tag" => s.risky = Some(Risky::DeleteTag(arg, true)),
                "show" => s.open_commit(&arg),
                "apply" => s.act_bg("Applying the stash…", move |r| r.stash_do("apply", &arg).map(|_| format!("Applied {arg}"))),
                "pop" => s.act_bg("Applying the stash…", move |r| r.stash_do("pop", &arg).map(|_| format!("Applied and dropped {arg}"))),
                "drop" => s.risky = Some(Risky::DropStash(arg)),
                "preview" => {
                    s.map.open = false;
                    s.set_scope(Scope::Branch);
                }
                "base" => {
                    s.set_base(&arg);
                    s.map.open = false;
                    s.set_scope(Scope::Branch);
                }
                _ => {}
            }
        });
    }

    fn more_menu(&mut self, tx: &mut Tx) {
        let st = &self.settings;
        let menu = PopupMenu::new()
            .item(PopupItem::new("ws", "Ignore Whitespace").checked(st.ignore_whitespace))
            .item(PopupItem::new("moved", "Highlight Moved Code").checked(st.show_moved))
            .item(PopupItem::new("wrap", "Wrap Long Lines").checked(st.wrap))
            .separator()
            .item(PopupItem::new("collapse", "Collapse All Files"))
            .item(PopupItem::new("expand", "Expand All Files"))
            .separator()
            .item(PopupItem::new("comments", format!("Copy {} Comment{} as Markdown", self.comments.len(), if self.comments.len() == 1 { "" } else { "s" })).enabled(!self.comments.is_empty()))
            .submenu("Ask Agent", self.agent_menu())
            .separator()
            .item(PopupItem::new("reread", "Reread Repository"))
            .item(PopupItem::new("add", "Add Local Project…"))
            .separator()
            .item(PopupItem::new("keys", "Keyboard Shortcuts"))
            .item(PopupItem::new("settings", "Settings…"));
        tx.popup_menu(menu, "more", |s: &mut Collate, picked, tx| match picked {
            Some("ws") => s.configure(|c| c.ignore_whitespace = !c.ignore_whitespace),
            Some("moved") => s.configure(|c| c.show_moved = !c.show_moved),
            Some("wrap") => s.configure(|c| c.wrap = !c.wrap),
            Some("collapse") => s.fold_all(true, tx),
            Some("expand") => s.fold_all(false, tx),
            Some("reread") => {
                refresh();
            }
            Some("comments") => s.copy_comments(tx),
            Some("add") => s.add_project(),
            Some("keys") => s.show_help = true,
            Some("settings") => s.screen = Screen::Settings,
            Some(id) => s.agent_menu_pick(id),
            None => {}
        });
    }

    /// The selected lines when they're in hunk `h` of file `f`.
    fn picked(&self, f: usize, h: usize) -> Option<Vec<bool>> {
        self.diff.selected_changes().filter(|(sf, sh, _)| *sf == f && *sh == h).map(|(_, _, p)| p)
    }

    fn discard_hunk(&mut self, f: usize, h: usize, picked: Option<Vec<bool>>) {
        let Some((file, _)) = self.file(f) else { return };
        let Some(p) = patch::lines(file, h, picked.as_deref(), true) else { return };
        let path = file.path().to_owned();
        let what = if picked.is_some() { "the selected lines" } else { "the hunk" };
        let Some(repo) = current_repo() else { return };
        let result = repo.backup(&path).and_then(|b| repo.apply(&p, false, true).map(|_| b));
        self.after_discard(result, format!("Discarded {what} in {path}"));
    }

    /// Report a discard, offering its backup back as Undo.
    fn after_discard(&mut self, result: Result<PathBuf, String>, done: String) {
        match result {
            Ok(backup) => {
                self.notice = Some(Notice { text: done, error: false, until: None, undo: Some(backup) });
                self.selection_clear();
            }
            Err(e) => self.say(e, true),
        }
        refresh();
    }

    fn undo_discard(&mut self) {
        let Some(backup) = self.notice.as_mut().and_then(|n| n.undo.take()) else { return };
        self.act(|r| r.restore(&backup).map(|n| format!("Restored {n} file{} from the backup", if n == 1 { "" } else { "s" })));
    }

    fn selection_clear(&mut self) {
        self.diff.set_selection(None);
    }

    fn stage_file(&mut self, f: usize) {
        let Some((file, _)) = self.file(f) else { return };
        let path = file.path().to_owned();
        self.act(|r| r.stage(&path).map(|_| format!("Staged {path}")));
    }

    fn unstage_file(&mut self, f: usize) {
        let Some((file, _)) = self.file(f) else { return };
        let path = file.path().to_owned();
        self.act(|r| r.unstage(&path).map(|_| format!("Unstaged {path}")));
    }

    fn discard_file(&mut self, f: usize) {
        let Some((file, info)) = self.file(f) else { return };
        let path = file.path().to_owned();
        let scope = self.scope;
        let Some(repo) = current_repo() else { return };
        let result = repo.discard(&path, scope, info.in_base);
        self.after_discard(result, format!("Discarded {path}"));
    }

    /// Settles the conflicts of file `f` (those in hunk `h` alone, when given) by keeping one
    /// side or both. The working file is backed up first, and a file settled whole is staged,
    /// which marks it resolved.
    fn resolve(&mut self, f: usize, h: Option<usize>, keep: patch::Keep) {
        let Some((file, _)) = self.file(f) else { return };
        let path = file.path().to_owned();
        let within = h.and_then(|h| file.hunks.get(h)).map(|h| (h.new_start, h.new_start + h.new_count.max(1) - 1));
        let Some(repo) = current_repo() else { return };
        let side = match keep {
            patch::Keep::Ours => "ours",
            patch::Keep::Theirs => "theirs",
            patch::Keep::Both => "both sides",
        };
        let result = repo.backup(&path).and_then(|backup| {
            if h.is_none() && keep != patch::Keep::Both {
                return repo.take_side(&path, side).map(|_| backup);
            }
            let full = repo.root.join(&path);
            let text = std::fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))?;
            let (text, n) = patch::resolve(&text, keep, within);
            if n == 0 {
                return Err(format!("No conflict markers {} {path}", if h.is_some() { "in this hunk of" } else { "in" }));
            }
            std::fs::write(&full, text).map_err(|e| format!("{path}: {e}"))?;
            if h.is_none() {
                repo.stage(&path)?;
            }
            Ok(backup)
        });
        let what = if h.is_some() { "this conflict" } else { "every conflict" };
        self.after_discard(result, format!("Kept {side} for {what} in {path}"));
    }

    /// Ask before a discard: it changes the working tree.
    fn ask_discard(&mut self, f: usize, hunk: Option<usize>) {
        if hunk.is_some() && self.settings.ignore_whitespace {
            self.say("Whitespace is hidden, so hunks don't match the file; discard the whole file or show whitespace.", true);
            return;
        }
        if !matches!(self.scope, Scope::Uncommitted | Scope::Unstaged) {
            return;
        }
        if let Some((file, _)) = self.file(f) {
            self.confirm = Some(Discard { file: f, hunk, path: file.path().to_owned() });
        }
    }

    fn confirm_discard(&mut self) {
        let Some(d) = self.confirm.take() else { return };
        // The list may have refreshed while the dialog was up: act only on the same file.
        if self.diff.files().get(d.file).is_none_or(|f| f.path() != d.path) {
            self.say("The changes moved while the dialog was open; nothing was discarded.", true);
            return;
        }
        match d.hunk {
            Some(h) => {
                let picked = self.picked(d.file, h);
                self.discard_hunk(d.file, h, picked)
            }
            None => self.discard_file(d.file),
        }
    }

    fn open_file(&mut self, f: usize) {
        let Some(repo) = current_repo() else { return };
        let Some((file, _)) = self.file(f) else { return };
        let Some(path) = file.new_path.clone() else {
            self.say("The file was deleted; there's nothing to open.", true);
            return;
        };
        let full = repo.root.join(&path);
        let mut cmd = std::process::Command::new("open");
        if let Some(app) = self.settings.editor.app() {
            cmd.args(["-a", app]);
        }
        match cmd.arg(&full).status() {
            Ok(st) if st.success() => {}
            Ok(_) => self.say(format!("Couldn't open {path} in {}", self.settings.editor.app().unwrap_or("the default app")), true),
            Err(e) => self.say(format!("Couldn't run open: {e}"), true),
        }
    }

    fn commit(&mut self) {
        let msg = self.message.text.trim().to_owned();
        let amend = self.amend;
        if msg.is_empty() || (self.staged == 0 && !amend) || self.committing {
            return;
        }
        let (Some(repo_handle), Some(repo)) = (HANDLE.get().cloned(), current_repo()) else { return };
        self.committing = true;
        // Hooks can take a while; the window stays live.
        std::thread::spawn(move || {
            let result = repo.commit(&msg, amend);
            repo_handle.update(move |s, _| {
                s.committing = false;
                match result {
                    Ok(summary) => {
                        s.message = TextEdit::new("");
                        s.amend = false;
                        s.say(format!("{} {summary}", if amend { "Amended" } else { "Committed" }), false);
                        if s.scope == Scope::History {
                            s.load_history();
                        }
                    }
                    Err(e) => s.say(format!("Commit failed: {e}"), true),
                }
                refresh();
            });
        });
    }

    /// Amend on: start from the last commit's message, unless one is written already.
    fn toggle_amend(&mut self) {
        self.amend = !self.amend;
        if self.amend
            && self.message.text.trim().is_empty()
            && let Some(m) = current_repo().and_then(|r| r.last_message())
        {
            self.message = TextEdit::new(m.trim_end());
        }
    }

    /// Push settings into the diff and the watcher; `was` is the previous settings, to skip
    /// work that didn't change.
    fn apply_settings(&mut self, was: Option<&Settings>) {
        let s = &self.settings;
        self.diff.set_style(s.style());
        self.diff.set_mode(s.layout);
        self.diff.set_wrap(s.wrap);
        let recolour = was.is_none_or(|w| w.syntax != s.syntax);
        let fetch = was.is_some_and(|w| (!w.syntax && s.syntax) || (!w.download_languages && s.download_languages));
        let look = was.is_some_and(|w| s.check_updates && (!w.check_updates || w.update_from != s.update_from));
        REFRESH_EVERY.store(if s.auto_refresh { s.refresh_ms } else { 0 }, Ordering::Relaxed);
        *OPTS.lock().unwrap() = self.opts();
        if let Some(w) = was
            && (w.context != s.context || w.hide_low_signal != s.hide_low_signal || w.source_first != s.source_first || w.ignore_whitespace != s.ignore_whitespace || w.show_moved != s.show_moved)
        {
            self.reload();
        }
        if recolour {
            self.set_colours();
        }
        if fetch {
            self.want_languages();
        }
        // Look again at once when checking is turned on or the source changes.
        if look {
            if !self.update.busy() {
                self.update = update::Status::Idle;
            }
            self.check_update();
        }
    }

    /// Colour the diff with the built-in scanners and the grammars loaded so far, or not at
    /// all with syntax highlighting off.
    fn set_colours(&mut self) {
        if !self.settings.syntax {
            self.diff.set_highlighter(Highlighter::new(|_: Option<&str>, _: &str| Vec::new()).key("off"), |_| false);
            return;
        }
        if self.languages.loaded.is_empty() {
            self.diff.set_highlighter(Highlighter::builtin(), rune_kit::syntax::known);
            return;
        }
        let loaded = Rc::new(self.languages.loaded.clone());
        let mut ids: Vec<&str> = loaded.keys().copied().collect();
        ids.sort_unstable();
        let mine = loaded.clone();
        // Keyed by the grammars it holds, so a newly loaded one doesn't reuse plain results.
        let h = Highlighter::new(move |lang: Option<&str>, code: &str| match lang.and_then(grammars::for_language).and_then(|g| mine.get(g.id)) {
            Some(c) => grammars::colour(c, code),
            None => Vec::new(),
        })
        .key(ids);
        self.diff.set_highlighter(h, move |lang| rune_kit::syntax::known(lang) || grammars::for_language(lang).is_some_and(|g| loaded.contains_key(g.id)));
    }

    /// Load the grammars the diff's files need, installing missing ones if the reader lets
    /// Collate download them. One that failed waits until it's asked for in the settings.
    fn want_languages(&mut self) {
        if !self.settings.syntax {
            return;
        }
        let mut wanted: Vec<&'static grammars::Grammar> = Vec::new();
        for g in self.diff.files().iter().filter_map(|f| f.language().and_then(grammars::for_language)) {
            if !wanted.iter().any(|w| w.id == g.id) {
                wanted.push(g);
            }
        }
        for g in wanted {
            let l = &self.languages;
            if l.loaded.contains_key(g.id) || l.working.contains(&g.id) || l.failed.contains_key(g.id) {
                continue;
            }
            if self.settings.download_languages || l.status(g) == grammars::Status::Installed {
                self.prepare_language(g);
            }
        }
    }

    /// Install `g` if it's missing or outdated, then load it, off the UI thread.
    fn prepare_language(&mut self, g: &'static grammars::Grammar) {
        let (Some(root), Some(app)) = (grammars::root(), HANDLE.get().cloned()) else { return };
        self.languages.failed.remove(g.id);
        self.languages.working.push(g.id);
        std::thread::spawn(move || {
            let result = grammars::prepare(&root, g).map(Arc::new);
            app.update(move |s, _| s.language_ready(g, result));
        });
    }

    fn language_ready(&mut self, g: &'static grammars::Grammar, result: Result<Arc<grammars::Config>, String>) {
        self.languages.working.retain(|id| *id != g.id);
        self.languages.rescan(grammars::root().as_deref());
        match result {
            Ok(c) => {
                self.languages.loaded.insert(g.id, c);
                self.set_colours();
            }
            Err(e) => {
                self.say(format!("No colours for {}: {e}", g.title), true);
                self.languages.failed.insert(g.id, e);
            }
        }
    }

    fn remove_language(&mut self, g: &'static grammars::Grammar) {
        let Some(root) = grammars::root() else { return };
        if let Err(e) = grammars::remove(&root, g) {
            self.say(format!("Couldn't remove {}: {e}", g.title), true);
        }
        self.languages.failed.remove(g.id);
        self.languages.rescan(Some(&root));
        if self.languages.loaded.remove(g.id).is_some() {
            self.set_colours();
        }
    }

    /// Change a setting, apply it and save the file.
    fn configure(&mut self, f: impl FnOnce(&mut Settings)) {
        let was = self.settings.clone();
        f(&mut self.settings);
        if self.settings == was {
            return;
        }
        self.apply_settings(Some(&was));
        self.save_error = self.settings.save().err();
    }

    fn is_viewed(&self, i: usize) -> bool {
        let (Some(f), Some(info)) = (self.diff.files().get(i), self.info.get(i)) else { return false };
        self.viewed.get(f.path()) == Some(&info.new_hash)
    }

    fn toggle_viewed(&mut self, i: usize, tx: &mut Tx) {
        let (Some(path), Some(hash)) = (self.diff.files().get(i).map(|f| f.path().to_owned()), self.info.get(i).map(|i| i.new_hash)) else { return };
        let now = !self.is_viewed(i);
        if now {
            self.viewed.insert(path.clone(), hash);
        } else {
            self.viewed.remove(&path);
        }
        self.stale.remove(&path);
        save_viewed(&self.viewed);
        for t in [&mut self.tree, &mut self.staged_tree] {
            if let Some(Some(st)) = t.get_mut(&path).map(|n| n.data.as_mut()) {
                st.viewed = now;
            }
        }
        if self.settings.collapse_viewed {
            self.diff.set_collapsed(i, now);
            // Keep the header where the reader is looking rather than letting the fold pull
            // the next file under the pointer.
            self.diff.scroll_to_file(i, tx);
        }
    }

    /// The settings that shape the diffs the watcher builds.
    fn opts(&self) -> DiffOpts {
        let s = &self.settings;
        DiffOpts { context: s.context, ignore_whitespace: s.ignore_whitespace, moved: s.show_moved, hide_low_signal: s.hide_low_signal && !self.show_low_signal, source_first: s.source_first }
    }

    #[cfg(test)]
    fn apply_snapshot(&mut self, snap: git::Snapshot, tx: &mut Tx) {
        let p = watcher::prepare(snap, self.opts(), &mut watcher::DiffCache::new()).expect("a full snapshot");
        self.apply(p, tx);
    }

    /// Show diffs the watcher prepared. Only the cheap part runs here, on the UI thread: the
    /// file tree, the rows and the folds.
    fn apply(&mut self, p: Prepared, tx: &mut Tx) {
        let Prepared { snap, files, hidden, opts } = p;
        // Made for a scope, base or setting the reader has since left: a newer one is coming.
        let wanted = match snap.scope {
            Scope::Branch => self.base.as_ref(),
            Scope::History => self.commit.as_ref(),
            _ => None,
        };
        let stale = snap.scope.read_only() && snap.base.as_ref().map(|b| &b.0) != wanted;
        if snap.scope != self.scope || stale || opts != self.opts() {
            return;
        }
        self.has_project = true;
        self.branch = snap.branch;
        self.head = snap.head;
        self.base_commit = snap.base.map(|b| b.1);
        self.staged = snap.staged;
        self.staged_shown = if opts.hide_low_signal { snap.working.iter().filter(|(p, s, _)| *s && !settings::low_signal(p)).count() } else { snap.staged };
        self.unstaged = snap.unstaged;
        self.total = snap.total;
        self.ahead_behind = snap.ahead_behind;
        self.operation = snap.operation;
        let shows_uncommitted = self.graph_commits.first().is_some_and(|c| c.id == graph::UNCOMMITTED);
        if self.scope == Scope::History && (self.total > 0) != shows_uncommitted {
            self.load_history();
            // Rows went in above the list's anchor: a reader at the newest commit sees them.
            if !shows_uncommitted && self.commit.is_some() && self.commit == self.head {
                tx.scroll_into_view(format!("graph-{}", graph::UNCOMMITTED));
            }
        }
        self.error = None;
        self.hidden = hidden;
        let changes = snap.changes;
        let hashes: Vec<u64> = changes.iter().map(|c| c.hash).collect();
        let info: Vec<FileInfo> = changes.iter().map(|c| FileInfo { staged: c.staged, in_base: c.old.is_some(), new_hash: c.new_hash, conflicted: c.conflicted }).collect();
        // Staging can change what a file's actions offer without changing its two sides.
        let staging_moved = info.iter().map(|i| i.staged).ne(self.info.iter().map(|i| i.staged));
        self.info = info;
        if self.loaded && hashes == self.hashes {
            if staging_moved {
                // A file moves between Unstaged and Staged without its diff changing.
                self.build_lists(&changes);
            }
            return;
        }
        if !self.loaded {
            // The first diff lands as it is; nothing should fade or slide in under the reader.
            tx.instant();
        }
        let was = self.diff.files().get(self.diff.visible_file()).map(|f| f.path().to_owned());
        // Folds the reader set survive a refresh.
        let folded: HashSet<String> = (0..self.diff.files().len()).filter(|&i| self.diff.collapsed(i)).map(|i| self.diff.files()[i].path().to_owned()).collect();
        // A viewed file that changed since stays listed as such rather than silently unmarked.
        for c in &changes {
            if self.viewed.get(&c.path).is_some_and(|h| *h != c.new_hash) {
                self.viewed.remove(&c.path);
                self.stale.insert(c.path.clone());
            }
        }
        self.hashes = hashes;
        let fold_large = self.settings.fold_large;
        let folded: Vec<bool> = files
            .iter()
            .zip(&changes)
            .map(|(f, c)| {
                let first_sight = self.seen.insert(c.path.clone());
                let large = fold_large > 0 && (f.additions() + f.deletions()) as u32 > fold_large;
                let viewed = self.viewed.get(&c.path) == Some(&c.new_hash);
                folded.contains(&c.path) || (self.settings.collapse_viewed && viewed) || (first_sight && large)
            })
            .collect();
        // Decode the changed images in the diff; ones no longer in it go.
        let mut images = HashMap::new();
        for (hash, bytes) in files.iter().filter_map(|f| f.images.as_ref()).flatten().flatten() {
            if let Some(img) = self.images.remove(hash).or_else(|| self.avatars.load(bytes.clone())) {
                images.insert(*hash, img);
            }
        }
        self.images = images;
        self.diff.set_files_folded(files, &folded);
        self.want_languages();
        // Opened before the window could read it (a scene, a switch of project).
        if self.pulls.open && self.pulls.list.is_none() {
            self.load_pulls();
        }
        self.place_comments();
        self.build_lists(&changes);
        // The map's sizes and imports follow the working tree.
        if self.map.open {
            self.build_map();
        }
        self.find();
        self.loaded = true;
        // Keep the reader's place across a refresh.
        if let Some(i) = was.and_then(|p| self.diff.file_index(&p)) {
            self.diff.scroll_to_file(i, tx);
            let path = self.diff.files()[i].path().to_owned();
            self.select_in_lists(&path);
        }
    }

    /// The staging panel's lists for this view, from the shown files: Unstaged and Staged for
    /// All Changes (a partly staged file is in both, as in GitKraken), one list otherwise.
    fn build_lists(&mut self, changes: &[Change]) {
        type Pick = fn(&Change) -> bool;
        let files = self.diff.files();
        let (main, staged): (Pick, Pick) = match self.scope {
            Scope::Uncommitted => (|c| c.staged != Staged::Fully, |c| c.staged != Staged::No),
            Scope::Staged => (|_| false, |_| true),
            Scope::Unstaged | Scope::Branch | Scope::History => (|_| true, |_| false),
        };
        self.tree = tree(changes, files, &self.viewed, main);
        self.staged_tree = tree(changes, files, &self.viewed, staged);
        self.apply_filter();
    }

    /// Highlights `path` in whichever list holds it, following the diff as it scrolls.
    fn select_in_lists(&mut self, path: &str) {
        for t in [&mut self.tree, &mut self.staged_tree] {
            if t.contains(path) {
                t.select(path);
            }
        }
    }

    /// Rerun the search over the shown lines, keeping the current hit when it still holds.
    fn find(&mut self) {
        // Spaces around a plain search are a slip; in a pattern they may be meant.
        let text = if self.find_regex { self.search.text.clone() } else { self.search.text.trim().to_owned() };
        let finder = Finder::new(Query { text, case: self.find_case, word: self.find_word, regex: self.find_regex });
        self.search_error = finder.as_ref().err().cloned();
        self.diff.set_search(finder.ok().flatten());
        self.hits = self.diff.search_hits();
        self.hit = self.hit.filter(|&h| h < self.hits.len());
    }

    /// ⌘P's matches for what's typed: the diff's files, best first, and the line asked for.
    /// `:line` alone is the file in view.
    fn jump_matches(&self) -> (Vec<usize>, Option<u32>) {
        let (q, line) = match self.jump.rsplit_once(':') {
            Some((q, n)) if n.chars().all(|c| c.is_ascii_digit()) => (q.trim(), n.parse().ok()),
            _ => (self.jump.trim(), None),
        };
        let files = self.diff.files();
        if q.is_empty() && line.is_some() {
            return ((self.diff.visible_file() < files.len()).then_some(vec![self.diff.visible_file()]).unwrap_or_default(), line);
        }
        let mut scored: Vec<(i32, usize)> = files.iter().enumerate().filter_map(|(i, f)| fuzzy(q, f.path()).map(|sc| (sc, i))).collect();
        scored.sort_by_key(|&(sc, i)| (std::cmp::Reverse(sc), files[i].path().len(), i));
        (scored.into_iter().take(50).map(|(_, i)| i).collect(), line)
    }

    /// Goes to ⌘P's pick (or match `pick`): the file, at the line when one was typed.
    fn jump_to(&mut self, pick: Option<usize>, tx: &mut Tx) {
        let (matches, line) = self.jump_matches();
        let Some(&f) = matches.get(pick.unwrap_or(self.jump_pick)) else { return };
        self.jumping = false;
        self.map.open = false;
        if self.scope == Scope::History {
            self.history_diff = true;
        }
        match line {
            Some(n) => {
                let side = if self.diff.files()[f].new_path.is_some() { diff::Side::New } else { diff::Side::Old };
                self.diff.scroll_to_line(f, LineRef::new(side, n), tx);
            }
            None => self.diff.scroll_to_file(f, tx),
        }
        let path = self.diff.files()[f].path().to_owned();
        self.select_in_lists(&path);
    }

    fn start_jump(&mut self, text: &str) {
        self.jumping = true;
        self.jump = text.to_owned();
        self.jump_pick = 0;
    }

    /// Flips one of the search's options and searches again.
    fn search_option(&mut self, flip: fn(&mut Collate)) {
        flip(self);
        self.hit = None;
        self.find();
    }

    /// Where the toolbar's search looks now.
    fn searched(&self) -> Searched {
        if self.map.open {
            Searched::Map
        } else if self.scope == Scope::History && !self.history_diff {
            Searched::History
        } else {
            Searched::Diff
        }
    }

    /// The toolbar search's text in `place`.
    fn query_mut(&mut self, place: Searched) -> &mut TextEdit {
        match place {
            Searched::Diff => &mut self.search,
            Searched::Map => &mut self.map_query,
            Searched::History => &mut self.history_query,
        }
    }

    /// ⌘F or ⌘K: the toolbar's search takes the keys, its text selected so typing replaces it.
    fn focus_search(&mut self) {
        let place = self.searched();
        let q = self.query_mut(place);
        q.focused = true;
        (q.anchor, q.focus) = (0, q.text.len());
        self.lookup();
    }

    /// Runs the Map's or History's search again for what's typed.
    fn lookup(&mut self) {
        self.found_pick = 0;
        self.found = match self.searched() {
            Searched::Diff => Vec::new(),
            Searched::Map => self.map.data.as_deref().map(|m| search::files(m, &self.map_query.text)).unwrap_or_default(),
            Searched::History => search::commits(&self.graph_commits, &self.history_query.text),
        };
    }

    /// Goes to result `i`: a folder opens in the Map and a file is picked in its folder; a
    /// commit, branch or tag is picked in the graph.
    fn take_found(&mut self, i: usize, tx: &mut Tx) {
        let Some(f) = self.found.get(i).cloned() else { return };
        let place = self.searched();
        self.query_mut(place).focused = false;
        tx.blur();
        match place {
            Searched::Map => {
                let Some(m) = self.map.data.clone() else { return };
                let Some(&node) = m.by_path.get(&f.target) else { return };
                let dir = m.nodes[node].dir;
                self.map.zoom = if dir { f.target } else { f.target.rsplit_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default() };
                self.map.hover = (!dir).then_some(node);
            }
            Searched::History => {
                tx.scroll_into_view(format!("graph-{}", f.target));
                self.set_commit(&f.target);
            }
            Searched::Diff => {}
        }
    }

    /// Keys in the toolbar's search: Return steps through the diff's matches or takes the
    /// picked result, the arrows move the pick, Escape gives the keys back.
    fn search_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let place = self.searched();
        match ev.key {
            Key::Escape => {
                self.query_mut(place).focused = false;
                tx.blur();
            }
            Key::Enter if place == Searched::Diff => self.next_hit(!ev.mods.shift, tx),
            Key::Enter => self.take_found(self.found_pick, tx),
            Key::Up | Key::Down if place != Searched::Diff && !self.found.is_empty() => {
                let last = self.found.len() - 1;
                self.found_pick = if ev.key == Key::Up { self.found_pick.saturating_sub(1) } else { (self.found_pick + 1).min(last) };
                tx.scroll_into_view(format!("found-{}", self.found_pick));
            }
            _ => return false,
        }
        true
    }

    /// Jump to the next (or previous) line holding the search text, opening its file.
    fn next_hit(&mut self, forward: bool, tx: &mut Tx) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len();
        let i = match self.hit {
            None => if forward { 0 } else { n - 1 },
            Some(h) if forward => (h + 1) % n,
            Some(h) => (h + n - 1) % n,
        };
        self.hit = Some(i);
        let (f, at) = self.hits[i];
        self.diff.scroll_to_line(f, at, tx);
    }

    fn fold_all(&mut self, fold: bool, tx: &mut Tx) {
        let at = self.diff.visible_file();
        for i in 0..self.diff.files().len() {
            self.diff.set_collapsed(i, fold);
        }
        self.diff.scroll_to_file(at, tx);
    }

    fn apply_filter(&mut self) {
        let q = self.filter.text.trim().to_lowercase();
        for t in [&mut self.tree, &mut self.staged_tree] {
            if q.is_empty() {
                t.clear_filter();
            } else {
                t.filter(|n| !n.dir && n.path.to_lowercase().contains(&q));
            }
        }
    }
}

/// Where this repository's viewed marks are kept, so they survive a restart:
/// `~/Library/Application Support/Collate/viewed/<hash of the repository's path>`.
fn viewed_file() -> Option<PathBuf> {
    project_data("viewed")
}

/// The open project's file of `kind` (viewed marks, comments) in Collate's support folder.
fn project_data(kind: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let repo = current_repo()?;
    let root = &repo.root;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(format!("Library/Application Support/Collate/{kind}/{:016x}", h.finish())))
}

/// `~/Library/Application Support/Collate/<name>`: one path per line. `projects` holds the
/// repositories most recently opened first, `favorites` the starred ones.
fn paths_file(name: &str) -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/Collate").join(name))
}

fn load_paths(name: &str) -> Vec<PathBuf> {
    // Tests start from empty lists, whatever the reader has starred or opened.
    if cfg!(test) {
        return Vec::new();
    }
    let text = paths_file(name).and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect()
}

fn save_paths(name: &str, paths: &[PathBuf]) {
    // Tests open throwaway repositories; they leave the reader's lists alone.
    if cfg!(test) {
        return;
    }
    let Some(path) = paths_file(name) else { return };
    let text: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    // Losing the list isn't worth interrupting a review over; the next change tries again.
    let _ = path.parent().map(std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text).ok());
}

fn load_projects() -> Vec<PathBuf> {
    load_paths("projects")
}

fn save_projects(projects: &[PathBuf]) {
    save_paths("projects", projects);
}

/// Puts `root` first in the list (once), keeping the 30 most recent and every starred one.
fn remember_project(projects: &mut Vec<PathBuf>, favorites: &[PathBuf], root: PathBuf) {
    projects.retain(|p| *p != root);
    projects.insert(0, root);
    let mut n = 0;
    projects.retain(|p| {
        n += 1;
        n <= 30 || favorites.contains(p)
    });
    save_projects(projects);
}

/// The switcher's rows, as indices into `projects`: starred ones first, then the rest. An empty
/// `query` keeps each group most recent first; otherwise only matches show, best first.
fn project_order(projects: &[PathBuf], favorites: &[PathBuf], query: &str) -> Vec<usize> {
    let query = query.trim();
    let mut rows: Vec<(bool, i32, usize)> = projects
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            // The folder and its parent: the whole path would let nearly any letters match.
            let tail: Vec<_> = p.iter().rev().take(2).map(|c| c.to_string_lossy()).collect();
            let name = tail.iter().rev().cloned().collect::<Vec<_>>().join("/");
            let score = if query.is_empty() { 0 } else { fuzzy(query, &name)? };
            Some((!favorites.contains(p), -score, i))
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, _, i)| i).collect()
}

/// Menu labels for `projects`: each folder's name, with its parent folder's when two share a
/// name (`api - work`, `api - personal`), so the menu stays short and every entry is distinct.
fn project_labels(projects: &[PathBuf]) -> Vec<String> {
    let name = |p: &std::path::Path| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
    projects
        .iter()
        .map(|p| {
            let n = name(p);
            let twins = projects.iter().filter(|q| name(q) == n).count();
            match (twins, p.parent().and_then(|d| d.file_name())) {
                (2.., Some(parent)) => format!("{n} - {}", parent.to_string_lossy()),
                _ => n,
            }
        })
        .collect()
}

/// `p` as a reader would write it, home shortened to `~`.
fn tilde(p: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let full = p.display().to_string();
    match full.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => full,
    }
}

const ADD_PROJECT: &str = "Add local project…";

/// Lines of `<hash> <path>`.
fn load_viewed() -> HashMap<String, u64> {
    let text = viewed_file().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.lines().filter_map(|l| l.split_once(' ')).filter_map(|(h, p)| Some((p.to_owned(), u64::from_str_radix(h, 16).ok()?))).collect()
}

fn save_viewed(viewed: &HashMap<String, u64>) {
    // Tests run against throwaway repositories; they leave the reader's marks alone.
    if cfg!(test) {
        return;
    }
    let Some(path) = viewed_file() else { return };
    let text: String = viewed.iter().map(|(p, h)| format!("{h:016x} {p}\n")).collect();
    // Losing marks isn't worth interrupting a review over; the next toggle tries again.
    let _ = path.parent().map(std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text).ok());
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(b, &mut h);
    std::hash::Hasher::finish(&h)
}

/// A PDF's first page as a JPEG, by macOS's `sips`, at 72 points per inch: about 80 ms a page, on
/// the watcher thread. JPEG rather than PNG because it has no transparency: the page comes out
/// white, not the diff's background. On failure the PDF's own bytes stay, and the view says it
/// can't decode them.
fn pdf_page(pdf: &[u8]) -> Result<Vec<u8>, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!("collate-pdf-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let (src, out) = (base.with_extension("pdf"), base.with_extension("jpg"));
    let result = std::fs::write(&src, pdf).map_err(|e| e.to_string()).and_then(|()| {
        let o = std::process::Command::new("sips").args(["-s", "format", "jpeg", "-s", "formatOptions", "90"]).arg(&src).arg("--out").arg(&out).output().map_err(|e| format!("can't run sips: {e}"))?;
        if !o.status.success() {
            return Err(String::from_utf8_lossy(&o.stderr).trim().to_owned());
        }
        std::fs::read(&out).map_err(|e| e.to_string())
    });
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
    result
}

fn tree(changes: &[Change], files: &[FileDiff], viewed: &HashMap<String, u64>, keep: fn(&Change) -> bool) -> TreeModel<FileNode<Stat>> {
    let rows = changes.iter().zip(files).filter(|(c, _)| keep(c)).map(|(c, f)| {
        let letter = match (&c.old, &c.new) {
            _ if c.untracked => 'U',
            (None, _) => 'A',
            (_, None) => 'D',
            _ if c.old_path.is_some() => 'R',
            _ => 'M',
        };
        let viewed = viewed.get(&c.path) == Some(&c.new_hash);
        (c.path.clone(), Stat { added: f.additions() as u32, removed: f.deletions() as u32, letter, viewed })
    });
    let mut t = FileTree::new().build(rows);
    t.expand_all();
    t
}

fn label(n: &FileNode<Stat>) -> &str {
    &n.name
}

fn reveal(s: &mut Collate, id: &str, tx: &mut Tx) {
    // A file picked from the lists shows its diff, not the map.
    s.map.open = false;
    // History's file list is the way into a commit's diff.
    if s.scope == Scope::History {
        s.history_diff = true;
    }
    if let Some(i) = s.diff.file_index(id) {
        if s.diff.collapsed(i) {
            s.diff.set_collapsed(i, false);
        }
        s.diff.scroll_to_file(i, tx);
    }
}

fn kit(s: &Collate, cx: &Cx<Collate>) -> Kit {
    let dark = s.settings.dark(cx.system_dark());
    let mut k = Kit::neutral(dark);
    // Focus rings in slate, the hue of the app's greys, rather than the kit's blue accent;
    // light enough on dark to be seen, darker on light.
    let slate = rune::oklch(if dark { 0.68 } else { 0.55 }, 0.035, 262.0);
    k.c.border_focus = slate;
    k.c.border_active = slate;
    k
}

fn view(s: &Collate, cx: &mut Cx<Collate>) -> El {
    // Set on every commit: the clock keeps the last rate it was given.
    cx.time_scale(s.settings.motion.speed());
    let k = kit(s, cx);
    let mut layer = Vec::new();
    // Settings float over the review, which stays where it was underneath.
    let base = review(s, cx, &k, &mut layer);
    if s.screen == Screen::Settings {
        settings_panel(s, &k, &mut layer);
    }
    rune_kit::root(base, layer)
}

/// The project menu: a search, the added projects (starred ones first, each with its star),
/// then "Add local project…".
fn project_switcher(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> El {
    let labels = project_labels(&s.projects);
    let current = current_repo().and_then(|r| s.projects.iter().position(|p| *p == r.root));
    let trigger = Select::new::<&str>("project", &[])
        .current(current.map(|i| labels[i].as_str()))
        .placeholder("Open a project")
        .label("Project")
        .open(s.project_open)
        .on_toggle(|s: &mut Collate| s.toggle_projects())
        .trigger(k);
    let popover = Popover::new("projects").trigger(trigger).label("Project").placement(Placement::BottomStart).padding(0.0).open(s.project_open)
        .on_close(|s: &mut Collate| s.close_projects());
    if !s.project_open {
        return popover.build(k, layer);
    }
    let order = project_order(&s.projects, &s.favorites, &s.project_search.text);
    let pick = s.project_pick.min(order.len());
    let field = TextInput::new(&s.project_search)
        .plain()
        .leading_icon(k.glyph.search)
        .height(36.0)
        .width(None)
        .placeholder("Search projects")
        .label("Search projects")
        .on_edit(|s: &mut Collate| &mut s.project_search)
        .on_change(|s: &mut Collate, _| s.project_pick = 0)
        .on_key(|s: &mut Collate, ev: &KeyEvent, tx: &mut Tx| {
            let last = project_order(&s.projects, &s.favorites, &s.project_search.text).len();
            match ev.key {
                Key::Escape => s.close_projects(),
                Key::Enter => s.choose_project(tx),
                Key::Up => s.project_pick = s.project_pick.min(last).saturating_sub(1),
                Key::Down => s.project_pick = (s.project_pick + 1).min(last),
                _ => return false,
            }
            if matches!(ev.key, Key::Up | Key::Down) {
                tx.scroll_into_view(format!("project-option-{}", s.project_pick));
            }
            true
        })
        .build(k)
        .key("project-search");
    // One row's look: the picked one tinted, the rest on hover; hovering picks.
    let option = |n: usize, label: String| {
        row().key(format!("project-option-{n}")).w_full().h(28.0).pl(8.0).pr(4.0).gap(8.0).items_center().radius(k.radius.sm)
            .fill(if n == pick { k.c.hover } else { k.alpha(k.c.hover, 0.0) })
            .cursor(Cursor::Pointer).role("option").label(label)
            .on_hover(move |s: &mut Collate| s.project_pick = n)
    };
    let mut rows = col().key("project-options").w_full().padding(4.0);
    for (n, &i) in order.iter().enumerate() {
        let path = s.projects[i].clone();
        let starred = s.favorites.contains(&path);
        let star = IconButton::new(if starred { &rune_kit::glyphs::STAR_FILLED } else { &rune_kit::glyphs::STAR })
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(if starred { "Unstar" } else { "Star" })
            .help(if starred { "Unstar: list it by when it was opened" } else { "Star: keep it at the top" })
            .on_press({
                let path = path.clone();
                move |s: &mut Collate| s.toggle_favorite(&path)
            })
            .build(k)
            .key(format!("project-star-{i}"));
        let check = icon(k.glyph.check_small, 14.0).color(k.c.icon_base).opacity(if current == Some(i) { 1.0 } else { 0.0 }).a11y_hidden();
        rows = rows.child(
            option(n, labels[i].clone())
                .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.open_project(&path, tx))
                .child(text(labels[i].clone()).font(k.ty.ui).color(k.c.text_base).grow(1.0).shrink().truncate().a11y_hidden())
                .child(check)
                .child(star),
        );
    }
    if order.is_empty() && !s.projects.is_empty() {
        rows = rows.child(row().key("project-none").h(28.0).px(8.0).items_center().child(text("No project matches").font(k.ty.ui).color(k.c.text_faint)));
    }
    let add = option(order.len(), ADD_PROJECT.to_owned())
        .on_press(|s: &mut Collate| s.add_project())
        .child(text(ADD_PROJECT.to_owned()).font(k.ty.ui).color(k.c.text_base).a11y_hidden());
    let hairline = |key: &str| el().key(key.to_owned()).w_full().h(1.0).fill(k.c.border_weak).a11y_hidden();
    let body = col().key("projects-body").w(280.0).shrink().min_h(0.0)
        .child(row().key("project-search-row").w_full().px(4.0).child(field))
        .child(hairline("project-search-line"))
        .child(col().key("project-list").w_full().max_h(320.0).shrink().min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(rows))
        .child(hairline("project-add-line"))
        .child(col().key("project-add").w_full().padding(4.0).child(add));
    popover.child(body).build(k, layer)
}

/// What the window shows with no project open: the projects to pick from and a way to add one.
fn welcome(s: &Collate, k: &Kit) -> El {
    let mut list = col().key("recent").w_full().max_w(560.0).gap(4.0);
    let labels = project_labels(&s.projects);
    for (i, p) in s.projects.iter().enumerate() {
        let path = p.clone();
        // A project whose folder has gone (moved, deleted) shows faint.
        let exists = p.exists();
        list = list.child(
            row()
                .key(format!("p{i}"))
                .w_full()
                .h(40.0)
                .px(12.0)
                .gap(10.0)
                .items_center()
                .radius(8.0)
                .hover(k.alpha(k.c.text_base, 0.06))
                .cursor(Cursor::Pointer)
                .role("button")
                .label(labels[i].clone())
                .on_press_tx(move |s: &mut Collate, tx| s.open_project(&path, tx))
                .child(text(labels[i].clone()).font(k.ty.ui_medium).color(if exists { k.c.text_base } else { k.c.text_faint }))
                .child(text(tilde(p)).font(k.ty.text_small).color(k.c.text_faint).truncate().min_w(0.0).grow(1.0))
                .child(Button::new("Remove").variant(ButtonVariant::GhostMuted).control_size(ControlSize::Small).on_press(move |s: &mut Collate| s.forget_project(i)).build(k)),
        );
    }
    let add = Button::new(if s.picking { "Choose a folder…" } else { ADD_PROJECT }).variant(ButtonVariant::Contrast).shortcut("⌘O").disabled(s.picking).on_press(|s: &mut Collate| s.add_project()).build(k).key("add-project");
    let mut c = col()
        .key("welcome")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .gap(14.0)
        .px(24.0)
        .fill(k.c.bg_base)
        .child(text("Open a project").font(rune::TextStyle { size: 22.0, line_height: 28.0, ..k.ty.ui_medium }).color(k.c.text_base))
        .child(text("Pick a folder inside a git repository to review its changes.").font(k.ty.ui).color(k.c.text_muted))
        .child(add);
    if !s.projects.is_empty() {
        c = c.child(text("Recent").font(k.ty.text_xsmall_medium).color(k.c.text_muted).pt(12.0)).child(list);
    }
    if let Some(n) = &s.notice {
        c = c.child(text(n.text.clone()).font(k.ty.text_small).color(if n.error { k.c.danger.icon } else { k.c.text_muted }));
    }
    c
}

/// A small text button for file and hunk actions.
fn action(k: &Kit, key: &str, label: &str, help: &str, danger: bool, f: impl Fn(&mut Collate) + 'static) -> El {
    let b = Button::new(label.to_owned()).variant(if danger { ButtonVariant::GhostMuted } else { ButtonVariant::Ghost }).control_size(ControlSize::Small).help(help.to_owned());
    b.on_press(f).build(k).key(key.to_owned())
}

/// A file header's controls: its one likely action (Stage File or Unstage File), a "⋯" menu
/// with the rest (the destructive one last), and the viewed check.
fn file_actions(s: &Collate, k: &Kit, f: usize) -> El {
    let mut r = row().gap(4.0).items_center();
    if s.diff.files().get(f).is_some_and(|file| s.stale.contains(file.path())) {
        r = r.child(
            row()
                .key("stale")
                .h(20.0)
                .px(6.0)
                .mr(4.0)
                .items_center()
                .radius(4.0)
                .fill(k.alpha(k.c.warning.icon, 0.14))
                .help("You marked this file viewed; it has changed since.")
                .child(text("Changed since viewed").font(k.ty.text_xsmall_medium).color(k.c.warning.icon)),
        );
    }
    let staged = s.info.get(f).map_or(Staged::No, |i| i.staged);
    if s.info.get(f).is_some_and(|i| i.conflicted) && !s.scope.read_only() {
        // A merge stopped on this file: settle it with a side, both, or by hand and mark it.
        let keep = |key: &str, label: &str, help: &str, how: patch::Keep| action(k, key, label, help, false, move |s| s.resolve(f, None, how));
        r = r.child(row().key("conflict").h(20.0).px(6.0).mr(4.0).items_center().radius(4.0).fill(k.alpha(k.c.danger.fg, 0.14))
                .help("A merge couldn't combine both sides' changes here. Keep a side, both, or edit it and mark it resolved.")
                .child(text("Conflict").font(k.ty.text_xsmall_medium).color(k.c.danger.fg)))
            .child(keep("ours", "Use Ours", "Keep this branch's version of the whole file, and mark it resolved", patch::Keep::Ours))
            .child(keep("theirs", "Use Theirs", "Keep the incoming version of the whole file, and mark it resolved", patch::Keep::Theirs))
            .child(keep("both", "Use Both", "Keep both sides of every conflict, ours first, and mark it resolved", patch::Keep::Both))
            .child(action(k, "resolved", "Mark Resolved", "Stage the file as it is now", false, move |s| s.stage_file(f)));
        let more_key = format!("file-more-{f}");
        let anchor = more_key.clone();
        let more = IconButton::new(&MORE_ICON).variant(IconButtonVariant::GhostMuted).label("More actions for this file").help("More actions")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.file_menu(f, &anchor, tx)).build(k).key(more_key);
        return r.child(more).child(viewed_toggle(k, f, s.is_viewed(f)));
    }
    let primary = match s.scope {
        Scope::Uncommitted if staged == Staged::Fully => Some(action(k, "unstage", "Unstage File", "Unstage the whole file (U)", false, move |s| s.unstage_file(f))),
        Scope::Uncommitted | Scope::Unstaged => Some(action(k, "stage", "Stage File", "Stage the whole file (A)", false, move |s| s.stage_file(f))),
        Scope::Staged => Some(action(k, "unstage", "Unstage File", "Unstage the whole file (U)", false, move |s| s.unstage_file(f))),
        Scope::Branch | Scope::History => None,
    };
    if let Some(p) = primary {
        r = r.child(p);
    }
    let more_key = format!("file-more-{f}");
    let anchor = more_key.clone();
    let more = IconButton::new(&MORE_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .label("More actions for this file")
        .help("More actions")
        .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.file_menu(f, &anchor, tx))
        .build(k)
        .key(more_key);
    r.child(more).child(viewed_toggle(k, f, s.is_viewed(f)))
}

/// A hunk header's controls: its likely action, Discard after it with less weight (it asks
/// first), and Pick Words when one line pair is selected.
fn hunk_actions(s: &Collate, k: &Kit, f: usize, h: usize) -> El {
    let mut r = row().gap(2.0).items_center();
    // A comment, in any view, on the lines picked in this hunk.
    if s.picked(f, h).is_some() || s.diff.selection().is_some_and(|sel| sel.file == f && s.diff.current_hunk() == Some((f, h))) {
        r = r.child(action(k, "comment", "Comment…", "Comment on the selected line (c)", false, |s| s.ask_comment()));
        let copy = Button::new("Copy…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help("Copy the selected lines' reference (y), or the lines for an agent (Y)");
        r = r.child(copy.on_press_tx(|s: &mut Collate, tx: &mut Tx| s.copy_menu(tx)).build(k).key("copy-sel"));
    }
    if s.scope.read_only() {
        return r;
    }
    if s.settings.ignore_whitespace {
        // Hunks drawn without whitespace don't match the file byte for byte.
        return r.child(text("Whitespace hidden: stage whole files").font(k.ty.text_small).color(k.c.text_faint));
    }
    let staged = s.info.get(f).map_or(Staged::No, |i| i.staged);
    let conflict = s.info.get(f).is_some_and(|i| i.conflicted)
        && s.diff.files().get(f).and_then(|file| file.hunks.get(h)).is_some_and(|hunk| hunk.lines.iter().any(|l| l.text.starts_with("<<<<<<<")));
    if conflict && s.scope != Scope::Staged {
        let keep = |key: &str, label: &str, help: &str, how: patch::Keep| action(k, key, label, help, false, move |s| s.resolve(f, Some(h), how));
        return r
            .child(keep("ours", "Ours", "Keep this branch's side of the conflicts in this hunk", patch::Keep::Ours))
            .child(keep("theirs", "Theirs", "Keep the incoming side of the conflicts in this hunk", patch::Keep::Theirs))
            .child(keep("both", "Both", "Keep both sides of the conflicts in this hunk, ours first", patch::Keep::Both));
    }
    // With lines selected in this hunk, its actions act on them alone.
    let lines = s.picked(f, h).is_some();
    let (st, un, di) = if lines { ("Stage Lines", "Unstage Lines", "Discard Lines…") } else { ("Stage Hunk", "Unstage Hunk", "Discard Hunk…") };
    let stage = action(k, "stage", st, "Stage (a)", false, move |s| {
        let p = s.picked(f, h);
        s.stage_hunk(f, h, p)
    });
    let unstage = action(k, "unstage", un, "Unstage (u)", false, move |s| {
        let p = s.picked(f, h);
        s.unstage_hunk(f, h, p)
    });
    let discard = action(k, "discard", di, "Discard (x); asks first", true, move |s| s.ask_discard(f, Some(h)));
    let words = s.word_candidate().filter(|w| w.file == f && w.hunk == h).is_some();
    let r = if words { r.child(action(k, "words", "Pick Words…", "Stage, unstage or discard single word changes in this line (p)", false, |s| s.pick_words())) } else { r };
    match s.scope {
        // All of a file staged: its HEAD → working tree hunks are exactly what's in the index.
        Scope::Uncommitted if staged == Staged::Fully => r.child(unstage).child(discard),
        Scope::Uncommitted | Scope::Unstaged => r.child(stage).child(discard),
        Scope::Staged => r.child(unstage),
        Scope::Branch | Scope::History => r,
    }
}

fn review(s: &Collate, cx: &mut Cx<Collate>, k: &Kit, layer: &mut Vec<El>) -> El {
    let no_project = !s.has_project;
    let sidebar = if no_project { col().key("sidebar").w_full().h_full() } else { views_sidebar(s, k) };
    let inspector = if no_project { col().key("staging").w_full().h_full() } else { staging_panel(s, k) };

    // Toolbar, Mac regions: where you are (project, branch) leading, what you're looking at as
    // the title, and the tools for it trailing: layout, the staging panel, and a menu of the rest.
    // Where the work is, as Zed's title bar says it: the branch it comes from, then the branch
    // it's on (`main / fix/x`). On the base branch itself, just its name.
    let branch = if s.branch.is_empty() {
        row()
    } else {
        // Only the current branch's name gives way to a narrow bar.
        let part = |key: &str, glyph: &'static Icon, name: String, gives: bool| {
            let label = text(name).font(k.ty.ui).color(k.c.text_muted);
            let r = row().key(key.to_owned()).gap(5.0).items_center().child(icon(glyph, 14.0).color(k.c.icon_faint));
            if gives { r.min_w(0.0).shrink().child(label.truncate().min_w(0.0).shrink()) } else { r.child(label) }
        };
        // One button, as GitHub Desktop's branch menu: preview the branch as a pull request,
        // and choose the base it's compared against.
        let mut r = row().key("branch").gap(8.0).px(6.0).h(26.0).radius(6.0).items_center().min_w(0.0).shrink()
            .hover(k.alpha(k.c.text_base, 0.06)).cursor(Cursor::Pointer).role("button")
            .label(format!("Branch {}: sync, switch, stash, preview pull request", s.branch))
            .help("Fetch, pull and push, switch branches, stash, preview a pull request")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.branch_menu(tx));
        let base = s.base.as_deref().map(|b| b.strip_prefix("origin/").unwrap_or(b).to_owned()).filter(|b| *b != s.branch);
        if let Some(b) = base {
            r = r.child(part("base-branch", &MERGE_ICON, b, false)).child(text("/".to_owned()).font(k.ty.ui).color(k.c.text_faint));
        }
        r = r.child(part("current-branch", &BRANCH_ICON, s.branch.clone(), true));
        // What a push or pull would move, or the one running, as GitHub Desktop shows it.
        let sync = match (&s.busy, s.ahead_behind) {
            (Some(doing), _) => doing.clone(),
            (None, Some((a, b))) if a + b > 0 => [(a, "↑"), (b, "↓")].iter().filter(|(n, _)| *n > 0).map(|(n, arrow)| format!("{arrow}{n}")).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        if !sync.is_empty() {
            r = r.child(text(sync).key("sync").font(k.ty.text_small).color(k.c.text_muted));
        }
        r.child(icon(k.glyph.chevron_down, 12.0).color(k.c.icon_faint))
    };
    // The switcher's label, not its hover box, starts on the content's 16-point edge below
    // (the toolbar pads 12; the trigger pads its label 8).
    // With the sidebar closed the toolbar starts just past its toggle, where back and forward
    // sit: it starts past them instead.
    let past_arrows = if s.sidebar.is_open() { 0.0 } else { NAV_W + 8.0 };
    let leading = row().key("toolbar-leading").gap(10.0).items_center().min_w(0.0).shrink().ml(EDGE - 12.0 - 8.0 + past_arrows).child(project_switcher(s, k, layer)).child(branch);
    let split = s.diff.mode() == DiffMode::Split;
    let modes = SegmentedControl::new(&["Unified", "Split"], Some(split as usize)).label("Diff layout").fit().on_pick(|s: &mut Collate, i, tx: &mut Tx| {
        tx.motion(rune::tok::motion::SNAPPY);
        s.configure(|c| c.layout = if i == 1 { DiffMode::Split } else { DiffMode::Unified });
    });
    let panel = IconButton::new(&shell::INSPECTOR)
        .variant(IconButtonVariant::GhostMuted)
        .expanded(s.inspector.is_open())
        .label(if s.inspector.is_open() { "Hide staging panel" } else { "Show staging panel" })
        .help("Staging panel (⌥⌘0)")
        .on_press(|s: &mut Collate| s.inspector.toggle())
        .build(k)
        .key("panel-toggle");
    let term = IconButton::new(&terminal::TERMINAL_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .expanded(s.terminals.open)
        .label(if s.terminals.open { "Hide terminal" } else { "Show terminal" })
        .help("Terminal")
        .shortcut("⌘J")
        .disabled(no_project)
        .on_press(|s: &mut Collate| s.toggle_terminal())
        .build(k)
        .key("terminal-toggle");
    let more = IconButton::new(&MORE_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .label("View options and more")
        .help("View options, shortcuts, settings")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.more_menu(tx))
        .build(k)
        .key("more");
    // No title: the sidebar already marks the view shown.
    let toolbar = Toolbar::new()
        .leading(leading)
        .trailing(row().gap(6.0).items_center().child(toolbar_search(s, k, layer)).child(modes.build(k)).child(term).child(panel).child(more));

    let n = s.diff.files().len();
    let body = if !s.has_project {
        welcome(s, k)
    } else if let Some(e) = &s.error {
        col().key("failed").w_full().grow(1.0).min_h(0.0).child(message(k, "Can't show these changes", e))
    } else if !s.loaded {
        message(k, "Reading changes…", "")
    } else if n == 0 {
        let why = match s.scope {
            Scope::Uncommitted => "The working tree matches HEAD.",
            Scope::Staged => "Nothing is staged.",
            Scope::Unstaged => "Everything is staged.",
            Scope::Branch => "This branch has no changes against its base.",
            Scope::History => "This commit changes no files (an empty commit, or a merge).",
        };
        col().key("empty").w_full().grow(1.0).min_h(0.0).child(summary(s, k, layer)).child(message(k, if s.hidden > 0 { "Only low-signal files changed" } else { "No changes" }, why))
    } else {
        let (fk, hk, ik, ck) = (k.clone(), k.clone(), k.clone(), k.clone());
        // Images draw again as each finishes decoding.
        let decoded = s.images.values().filter(|i| i.is_ready()).count() as u64;
        let diff = DiffView::new(|s: &Collate| &s.diff, |s: &mut Collate| &mut s.diff)
            .images(decoded * 4096 + s.images.len() as u64, move |s: &Collate, f| image_row(s, &ik, f))
            .comments(s.comments_version, move |s: &Collate, n| comment_row(s, &ck, n))
            .header_actions(move |s: &Collate, f| file_actions(s, &fk, f))
            .hunk_actions(move |s: &Collate, f, h| hunk_actions(s, &hk, f, h))
            .dim(|s: &Collate, f| s.is_viewed(f))
            .on_visible_file(|s: &mut Collate, f| {
                if let Some(p) = s.diff.files().get(f).map(|f| f.path().to_owned()) {
                    s.select_in_lists(&p);
                }
            })
            .build(s, k);
        col().key("review").w_full().grow(1.0).min_h(0.0).child(summary(s, k, layer)).child(diff)
    };
    let body = if s.has_project && s.scope == Scope::History && s.error.is_none() {
        if s.history_diff {
            col().key("history-body").w_full().grow(1.0).min_h(0.0).child(commit_header(s, k)).child(body)
        } else {
            history_graph(s, k)
        }
    } else { body };
    let body = if s.has_project && s.map.open { map_view::map_view(s, k) } else { body };
    let body = if s.has_project && s.pulls.open { pulls::pulls_view(s, k) } else { body };
    let body = match operation_bar(s, k) {
        Some(bar) => col().key("with-operation").w_full().grow(1.0).min_h(0.0).child(bar).child(body),
        None => body,
    };
    let body = match status_bar(s, k) {
        Some(bar) => col().key("body").w_full().grow(1.0).min_h(0.0).child(body).child(bar),
        None => body,
    };
    let body = if s.terminals.open && s.has_project {
        let window_h = rune::memo::layout_with("window", |r| r[3]).unwrap_or(840.0);
        let drawer = terminal::drawer(&s.terminals, k, window_h, |s: &mut Collate| &mut s.terminals, |s: &mut Collate| s.new_terminal());
        col().key("with-terminal").w_full().grow(1.0).min_h(0.0).child(body).child(drawer)
    } else {
        body
    };

    if s.show_help {
        let keys: &[(&str, &str)] = &[
            ("j  k", "Next / previous change"),
            ("]  [", "Next / previous file"),
            ("a  u  x", "Stage, unstage, discard the change at the top"),
            ("Click line numbers", "Pick lines; a, u and x then act on just those (shift-click or drag for more)"),
            ("p", "With one line and its replacement picked: pick single word changes"),
            ("A  U  X", "Stage, unstage, discard the whole file in view"),
            ("v", "Mark the file viewed"),
            ("c", "Comment on the selected line"),
            ("y  Y", "Copy the selected lines' reference (path#L10-L25), or the lines as a diff for an agent"),
            ("o", "Open the file in your editor"),
            ("⌘F  ⌘K", "Search this view: the diff (⏎ next match, ⇧⏎ previous), the Map's files, History's commits and branches"),
            ("⌘P", "Go to a file: type part of its path; add :line for a line"),
            ("⌘L", "Go to a line of the file in view"),
            ("n  N  ⌘G", "Next / previous search match"),
            ("⌥⌘C  ⌥⌘W  ⌥⌘R", "Search: match case, whole words, regular expression"),
            ("m", "Jump to the other end of a move (blue lines)"),
            ("z  Z", "Collapse / expand every file"),
            ("s", "Unified / split"),
            ("w  W", "Wrap long lines / hide whitespace changes"),
            ("1 2 3 4 5", "All changes, staged, unstaged, pull request preview, history"),
            ("<  >", "Git Graph: older / newer commit"),
            ("r", "Reread the repository"),
            ("⌘↩", "Commit what's staged"),
            ("⌘O", "Add a local project (the project menu, top left, switches)"),
            ("⌘,", "Settings"),
            ("⇧⌘B", "Show / hide the views sidebar"),
            ("⌥⌘0", "Show / hide the staging panel"),
            ("⌘J", "Show / hide the terminal; ⌘K clears it"),
        ];
        let mono = rune::TextStyle { size: 12.0, line_height: 18.0, ..k.ty.mono };
        let rows = keys.iter().enumerate().map(|(i, (key, what))| {
            row()
                .key(format!("k{i}"))
                .w_full()
                .gap(16.0)
                .items_start()
                .child(text(key.to_string()).font(mono).color(k.c.text_base).w(150.0))
                .child(text(what.to_string()).font(k.ty.text_small).color(k.c.text_muted).wrap().grow(1.0).min_w(0.0))
        });
        Dialog::new("help", "Keyboard shortcuts")
            .open(true)
            .fit()
            .width(560.0)
            .on_close(|s: &mut Collate| s.show_help = false)
            .child(col().key("keys").w_full().px(24.0).pb(20.0).gap(8.0).children(rows))
            .build(k, layer);
    }

    if let Some(w) = &s.words {
        let mono = rune::TextStyle { size: 12.5, line_height: 19.0, ..k.ty.mono };
        let rows = w.edits.iter().enumerate().map(|(i, e)| {
            let (before, after) = (&w.old[e.old.clone()], &w.new[e.new.clone()]);
            let show = |t: &str| if t.is_empty() { "∅".to_owned() } else { t.to_owned() };
            row()
                .key(format!("w{i}"))
                .w_full()
                .gap(10.0)
                .items_center()
                .child(Checkbox::new(w.picked[i]).label(format!("{before} → {after}")).hide_label().on_toggle(move |s: &mut Collate| {
                    if let Some(w) = &mut s.words {
                        w.picked[i] ^= true;
                    }
                }).build(k))
                .child(text(show(before)).font(mono).color(k.c.danger.fg))
                .child(text("→").font(mono).color(k.c.text_faint))
                .child(text(show(after)).font(mono).color(k.c.success.fg))
        });
        let preview = apply_edits(&w.old, &w.new, &w.edits, &w.picked);
        let any = w.picked.iter().any(|&p| p);
        let mut buttons = row().key("word-buttons").w_full().gap(8.0).justify_end().child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.words = None).build(k));
        buttons = match s.scope {
            Scope::Staged => buttons.child(Button::new("Unstage picked").variant(ButtonVariant::Contrast).disabled(!any).on_press(|s: &mut Collate| s.act_on_words(Scope::Staged)).build(k).key("words-unstage")),
            _ => buttons
                .child(Button::new("Discard picked").variant(ButtonVariant::Danger).disabled(!any).on_press(|s: &mut Collate| s.act_on_words(Scope::Uncommitted)).build(k).key("words-discard"))
                .child(Button::new("Stage picked").variant(ButtonVariant::Contrast).disabled(!any).on_press(|s: &mut Collate| s.act_on_words(Scope::Unstaged)).build(k).key("words-stage")),
        };
        let body = col()
            .key("words")
            .w_full()
            .px(24.0)
            .pb(20.0)
            .gap(10.0)
            .children(rows)
            .child(text("The old line with just the ticked changes:").font(k.ty.text_small).color(k.c.text_muted).pt(6.0))
            .child(text(preview).font(mono).color(k.c.text_base).wrap())
            .child(buttons.pt(8.0));
        Dialog::new("words", format!("Word changes in {}", w.path.rsplit('/').next().unwrap_or(&w.path)))
            .open(true)
            .fit()
            .width(600.0)
            .on_close(|s: &mut Collate| s.words = None)
            .child(body)
            .build(k, layer);
    }

    if let Some(d) = &s.confirm {
        let (title, what) = match d.hunk {
            Some(_) => ("Discard this hunk?".to_owned(), format!("The hunk's changes to {} are undone in the working tree.", d.path)),
            None => (format!("Discard changes to {}?", d.path.rsplit('/').next().unwrap_or(&d.path)), format!("{} goes back to how it was{}.", d.path, if s.scope == Scope::Unstaged { " when staged" } else { " at the last commit" })),
        };
        let buttons = row()
            .key("confirm-buttons")
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.confirm = None).build(k))
            .child(Button::new("Discard").variant(ButtonVariant::Danger).on_press(|s: &mut Collate| s.confirm_discard()).build(k).key("confirm-discard"));
        Dialog::new("discard", title)
            .open(true)
            .fit()
            .description(format!("{what} A copy of the file is kept in .git/collate-backup."))
            .on_close(|s: &mut Collate| s.confirm = None)
            .child(buttons)
            .build(k, layer);
    }

    if let Some(ask) = &s.prompt {
        let (title, what, button, placeholder) = match ask {
            Ask::Branch(None) => ("New branch", "Made at HEAD and switched to. What isn't committed comes along.", "Create Branch", "Branch name"),
            Ask::Branch(Some(_)) => ("New branch here", "Made at this commit and switched to.", "Create Branch", "Branch name"),
            Ask::Stash => ("Stash all changes", "Every change, untracked files too, is put away and the working tree goes back to HEAD.", "Stash", "Message (optional)"),
            Ask::Tag(None) => ("New tag", "Made at HEAD, here; push it from the branch menu's Tags.", "Create Tag", "Tag name, then a message after a space for an annotated tag"),
            Ask::Tag(Some(_)) => ("Tag this commit", "Made here; push it from the branch menu's Tags.", "Create Tag", "Tag name, then a message after a space for an annotated tag"),
            Ask::Rename(_) => ("Rename branch", "Only here: a pushed branch keeps its old name on the remote.", "Rename", "Branch name"),
            // In a pull request, a comment goes with the next review, if GitHub takes it on that line.
            Ask::Comment(_, None) if s.pulls.pr.is_some() && !s.pr_whole() => ("Comment",
                "Lines of part of a pull request aren't the ones GitHub numbers, so this stays on this Mac.", "Comment", "Write a comment"),
            Ask::Comment(at, None) if s.pulls.pr.as_ref().is_some_and(|p| !p.commentable(&at.path, at.side == diff::Side::Old, at.line)) => ("Comment",
                "GitHub only takes comments within three lines of a change, so this one stays on this Mac.", "Comment", "Write a comment"),
            Ask::Comment(_, None) if s.pulls.pr.is_some() => ("Comment", "Goes to GitHub with your next review. Until then it stays on this Mac.", "Comment", "Write a comment"),
            Ask::Comment(_, None) => ("Comment", "Comments stay on this Mac. Copy them as Markdown from the summary to share them.", "Comment", "Write a comment"),
            Ask::Comment(_, Some(_)) => ("Edit comment", "An empty comment is deleted.", "Save", "Write a comment"),
        };
        let field = TextInput::new(&s.prompt_text)
            .placeholder(placeholder)
            .on_edit(|s: &mut Collate| &mut s.prompt_text)
            .on_submit(|s: &mut Collate, _: &mut Tx| s.answer_prompt())
            .width(None)
            .build(k)
            .key("prompt-field")
            .w_full();
        let empty = s.prompt_text.text.trim().is_empty() && matches!(ask, Ask::Branch(_) | Ask::Comment(_, None) | Ask::Rename(_) | Ask::Tag(_));
        let mut buttons = row().key("prompt-buttons").w_full().gap(8.0).justify_end();
        // On a pull request's line: suggest what it should say instead, starting from what it says.
        if let Ask::Comment(at, None) = ask
            && s.pulls.pr.is_some()
            && !s.pulls.suggest
        {
            let line = at.text.clone();
            buttons = buttons.child(Button::new("Suggest a Change").variant(ButtonVariant::Ghost).help("Write the line as it should read; the author can apply it")
                .on_press(move |s: &mut Collate| {
                    s.pulls.suggest = true;
                    s.prompt_text = TextEdit::new(&line);
                }).build(k).key("prompt-suggest")).child(row().grow(1.0));
        }
        let buttons = buttons
            .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.prompt = None).build(k))
            .child(Button::new(button).variant(ButtonVariant::Contrast).disabled(empty).on_press(|s: &mut Collate| s.answer_prompt()).build(k).key("prompt-ok"));
        Dialog::new("prompt", title)
            .open(true)
            .fit()
            .description(what)
            .on_close(|s: &mut Collate| s.prompt = None)
            .child(col().key("prompt-body").w_full().gap(12.0).child(field).child(buttons))
            .build(k, layer);
    }

    pulls::compose_dialog(s, k, layer);
    pulls::rewrite_dialog(s, k, layer);

    if let Some((from, to)) = &s.compare {
        let end = |key: &'static str, label: &str, value: &str, to: bool| {
            let pick = Button::new(if value.is_empty() { "Choose…".to_owned() } else { value.to_owned() })
                .variant(ButtonVariant::Outline)
                .help(format!("Choose the {}", label.to_lowercase()))
                .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.pick_ref(to, tx))
                .build(k)
                .key(key);
            row().w_full().gap(12.0).items_center().child(text(label.to_owned()).w(48.0).font(k.ty.text_small).color(k.c.text_muted)).child(pick)
        };
        let ready = !from.is_empty() && !to.is_empty() && from != to;
        let buttons = row()
            .key("compare-buttons")
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.compare = None).build(k))
            .child(Button::new("Compare").variant(ButtonVariant::Contrast).disabled(!ready).on_press(|s: &mut Collate| s.run_compare()).build(k).key("compare-ok"));
        Dialog::new("compare", "Compare")
            .open(true)
            .fit()
            .description("What changed from one branch, tag or commit to another.")
            .on_close(|s: &mut Collate| s.compare = None)
            .child(col().key("compare-body").w_full().gap(10.0).child(end("compare-from", "From", from, false)).child(end("compare-to", "To", to, true)).child(buttons.pt(4.0)))
            .build(k, layer);
    }

    if s.jumping {
        jump_palette(s, k, layer);
    }

    if let Some((path, lines)) = &s.blame {
        blame_dialog(k, path, lines.len(), layer);
    }
    if let Some((path, commits)) = &s.file_log {
        file_log_dialog(k, path, commits.len(), layer);
    }

    if let Some(r) = &s.risky {
        let (title, what, button) = match r {
            Risky::ResetHard(id) => (format!("Reset {} to {}?", s.branch, abbrev(id)), "Commits after it leave the branch, and every change not committed is thrown away. There's no backup.".to_owned(), "Reset and Discard"),
            Risky::DropStash(name) => (format!("Drop {name}?"), "Its changes are deleted. There's no backup.".to_owned(), "Drop Stash"),
            Risky::ForcePush => (
                format!("Force push {}?", s.branch),
                "The remote branch is replaced with this one, its commits that aren't here dropped. It fails if someone pushed since your last fetch.".to_owned(),
                "Force Push",
            ),
            Risky::DeleteTag(name, false) => (format!("Delete {name}?"), "Only here: a pushed tag stays on origin.".to_owned(), "Delete Tag"),
            Risky::DeleteTag(name, true) => (format!("Delete {name} here and from origin?"), "Anyone who fetched it keeps it until they delete it too.".to_owned(), "Delete Tag"),
            Risky::DeleteBranch(name, true) => (format!("Delete {name}?"), format!("Its commits are all in {} already, so nothing is lost.", s.branch), "Delete Branch"),
            Risky::RestoreCheckpoint(_, time) => (
                format!("Restore the checkpoint from {}?", day(*time)),
                "Every file goes back to how it was then, and files made since are removed. Commits and what's staged stay. The files as they are now are saved as a checkpoint first, so you can come back.".to_owned(),
                "Restore",
            ),
            Risky::DeleteBranch(name, false) => (
                format!("Delete {name}?"),
                format!("It has commits that aren't in {}. Once it's deleted they're only in git's reflog, for a while.", s.branch),
                "Delete Branch",
            ),
        };
        let buttons = row()
            .key("risky-buttons")
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.risky = None).build(k))
            .child(Button::new(button).variant(ButtonVariant::Danger).on_press(|s: &mut Collate| s.confirm_risky()).build(k).key("risky-ok"));
        Dialog::new("risky", title).open(true).fit().description(what).on_close(|s: &mut Collate| s.risky = None).child(buttons).build(k, layer);
    }

    let lights = cx.titlebar_inset();
    let split = SplitView::new(body)
        .sidebar(&s.sidebar, |s: &mut Collate| &mut s.sidebar, sidebar)
        .inspector(&s.inspector, |s: &mut Collate| &mut s.inspector, inspector)
        .toolbar(toolbar)
        .titlebar_inset(lights)
        // Room after the traffic lights, as T3 Code leaves, so the toggle doesn't crowd them.
        .sidebar_toggle(shell::SidebarToggle::new().past_lights(16.0))
        .reduced_motion(cx.is_reduced_motion())
        .build(k);
    if no_project {
        return split;
    }
    // Back and forward just past the sidebar's toggle (24 points, 8 after it), as T3 Code has
    // them, in the title band whether the sidebar is open or not.
    let toggle_end = if lights > 0.0 { lights + 16.0 } else { 12.0 } + 24.0;
    let arrow = |icon: &'static Icon, back: bool, label: &str, keys: &str| {
        IconButton::new(icon)
            .variant(IconButtonVariant::GhostMuted)
            .disabled(!s.can_go(back))
            .label(label.to_owned())
            .help(label.to_owned())
            .shortcut(keys.to_owned())
            .on_press(move |s: &mut Collate| s.go(back))
            .build(k)
            .key(if back { "nav-back" } else { "nav-forward" })
    };
    let arrows = row().key("nav").gap(2.0).items_center().absolute(toggle_end + 8.0, (48.0 - 24.0) / 2.0)
        .child(arrow(&BACK_ICON, true, "Back", "⌘["))
        .child(arrow(&FORWARD_ICON, false, "Forward", "⌘]"));
    col().key("window").w_full().h_full().child(split).child(arrows)
}

/// How well `q` matches `path` as letters in order, ignoring case: higher is better, `None`
/// when it doesn't. Runs of letters, letters starting a word and letters in the file name
/// count most, as editors' quick open scores them.
/// ⌘P: the files of the diff matching what's typed, best first; Return goes to the one picked.
fn jump_palette(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let (matches, line) = s.jump_matches();
    let typed = if s.jump.is_empty() {
        text("File name, file:line, or :line".to_owned()).font(k.ty.ui).color(k.c.text_faint)
    } else {
        text(format!("{}\u{2502}", s.jump)).font(k.ty.ui).color(k.c.text_base)
    };
    let field = row().key("jump-field").w_full().h(32.0).px(10.0).gap(8.0).items_center().radius(k.radius.md).border(2.0, k.c.border_focus)
        .child(icon(k.glyph.search, 14.0).color(k.c.icon_faint)).child(typed);
    let files = s.diff.files();
    let rows = matches.iter().enumerate().map(|(n, &f)| {
        let path = files[f].path();
        let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
        let picked = n == s.jump_pick;
        row().key(format!("jump-{n}")).w_full().h(28.0).px(10.0).gap(8.0).items_center().radius(k.radius.sm)
            .fill(if picked { k.alpha(k.c.text_base, 0.10) } else { k.alpha(k.c.text_base, 0.0) })
            .hover(k.alpha(k.c.text_base, 0.06)).cursor(Cursor::Pointer).role("option").label(path.to_owned())
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.jump_to(Some(n), tx))
            .child(text(name.to_owned()).font(k.ty.ui).color(k.c.text_base))
            .child(text(dir.to_owned()).font(k.ty.text_small).color(k.c.text_faint).truncate().min_w(0.0).grow(1.0))
    });
    let note = match (matches.is_empty(), line) {
        (true, _) => "No file in this diff matches".to_owned(),
        (false, Some(n)) => format!("Return goes to line {n}"),
        (false, None) => "↑ ↓ to pick, Return to go, esc to close".to_owned(),
    };
    Dialog::new("jump", "Go to File")
        .open(true)
        .fit()
        .width(560.0)
        .on_close(|s: &mut Collate| s.jumping = false)
        .child(col().key("jump-body").w_full().px(16.0).pb(16.0).gap(8.0)
            .child(field)
            .child(col().key("jump-list").w_full().max_h(360.0).scroll(rune::ScrollSpec { stick_bottom: false }).children(rows))
            .child(text(note).font(k.ty.text_small).color(k.c.text_muted)))
        .build(k, layer);
}

/// Seconds since 1970 as a day, `2026-11-10`, in UTC.
fn day(time: i64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = time.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y}-{m:02}-{d:02}")
}

/// A blame row's height.
const BLAME_ROW: f32 = 20.0;

/// Who last changed each line of a file, the commit named once at the top of each run of
/// lines it owns; a commit opens its diff.
fn blame_dialog(k: &Kit, path: &str, n: usize, layer: &mut Vec<El>) {
    let kit = k.clone();
    let rows = col().key("blame-rows").growth(rune::GrowthSpec::default()).w_full().virtual_rows(n, BLAME_ROW, |_: &Collate, i| format!("b{i}").into(), move |s: &Collate, i| {
        let k = &kit;
        let Some((_, lines)) = &s.blame else { return row() };
        let b = &lines[i];
        let mono = rune::TextStyle { size: 11.5, line_height: BLAME_ROW, ..k.ty.mono };
        let first = i == 0 || lines[i - 1].id != b.id;
        let committed = !b.id.bytes().all(|c| c == b'0');
        let mut who = row().w(300.0).h(BLAME_ROW).gap(8.0).items_center();
        if first {
            let id = b.id.clone();
            let hash = text(if committed { abbrev(&b.id) } else { "-------".to_owned() }).font(mono).color(if committed { k.c.text_accent } else { k.c.text_faint }).w(56.0);
            let hash = if committed {
                hash.cursor(Cursor::Pointer).role("link").label(format!("Open commit {}", abbrev(&id))).help(b.summary.clone()).on_press(move |s: &mut Collate| {
                    s.blame = None;
                    s.open_commit(&id);
                })
            } else { hash };
            let author = if committed { b.author.clone() } else { "Not committed yet".to_owned() };
            who = who.child(hash).child(text(author).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0));
            if committed {
                who = who.child(text(day(b.time)).font(k.ty.text_small).color(k.c.text_faint));
            }
        }
        row().w_full().h(BLAME_ROW).gap(12.0).items_center()
            .child(who)
            .child(text(format!("{}", i + 1)).font(mono).color(k.c.text_faint).w(40.0))
            .child(text(b.text.replace('\t', "    ")).font(mono).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
    });
    Dialog::new("blame", format!("Blame - {path}"))
        .open(true)
        .width(1000.0)
        .on_close(|s: &mut Collate| s.blame = None)
        .child(col().key("blame-scroll").w_full().h(560.0).px(16.0).pb(16.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(rows))
        .build(k, layer);
}

/// The commits that changed a file, newest first; one opens its diff.
fn file_log_dialog(k: &Kit, path: &str, n: usize, layer: &mut Vec<El>) {
    let kit = k.clone();
    let rows = col().key("log-rows").growth(rune::GrowthSpec::default()).w_full().virtual_rows(n, GRAPH_ROW, |s: &Collate, i| format!("log-{}", s.file_log.as_ref().map_or("", |l| l.1[i].id.as_str())).into(), move |s: &Collate, i| {
        let k = &kit;
        let Some((_, commits)) = &s.file_log else { return row() };
        let c = &commits[i];
        let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
        let id = c.id.clone();
        row().w_full().h(GRAPH_ROW).px(8.0).gap(12.0).items_center().radius(k.radius.sm)
            .hover(k.alpha(k.c.text_base, 0.06)).cursor(Cursor::Pointer).role("button").label(format!("Open commit {}: {}", abbrev(&c.id), c.subject))
            .on_press(move |s: &mut Collate| {
                s.file_log = None;
                s.open_commit(&id);
            })
            .child(text(abbrev(&c.id)).font(mono).color(k.c.text_muted).w(HASH_W))
            .child(text(c.subject.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
            .child(text(c.author.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().w(AUTHOR_W))
            .child(text(c.date.clone()).font(k.ty.text_small).color(k.c.text_faint).w(DATE_W))
    });
    let body = if n == 0 {
        col().key("log-empty").w_full().px(24.0).pb(20.0).child(text("No commits have changed this file yet.").font(k.ty.text_small).color(k.c.text_muted))
    } else {
        col().key("log-scroll").w_full().h(480.0).px(16.0).pb(16.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(rows)
    };
    Dialog::new("file-log", format!("History - {path}")).open(true).width(820.0).fit().on_close(|s: &mut Collate| s.file_log = None).child(body).build(k, layer);
}

/// The back and forward buttons' width together.
const NAV_W: f32 = 24.0 * 2.0 + 2.0;

/// The leading sidebar: the places to look, with how much is in each. Working Copy is what
/// isn't committed; Review is what is.
fn views_sidebar(s: &Collate, k: &Kit) -> El {
    let place = |key: &str, label: &str, glyph: &'static Icon, scope: Scope, badge: Option<usize>, detail: Option<String>| {
        let elsewhere = s.map.open || s.pulls.open || s.pulls.pr.is_some();
        let mut r = SidebarRow::new(label.to_owned()).icon(glyph).selected(s.scope == scope && !elsewhere).on_select(move |s: &mut Collate, _: &mut Tx| {
            s.map.open = false;
            s.pulls.open = false;
            s.close_pull();
            s.set_scope(scope);
        });
        if let Some(n) = badge.filter(|&n| n > 0) {
            r = r.badge(n.to_string());
        }
        if let Some(d) = detail {
            r = r.detail(d);
        }
        r.build(k).key(key.to_owned())
    };
    let working = SidebarSection::new("Working Copy").rows([
        place("view-all", "All Changes", &ALL_ICON, Scope::Uncommitted, Some(s.total), None),
        place("view-unstaged", "Unstaged", &UNSTAGED_ICON, Scope::Unstaged, Some(s.unstaged), None),
        place("view-staged", "Staged", &STAGED_ICON, Scope::Staged, Some(s.staged_shown), None),
    ]);
    let review = SidebarSection::new("Review").rows([
        place("view-history", "Git Graph", &GRAPH_ICON, Scope::History, None, None),
        // The repository's open pull requests, and the one being reviewed.
        {
            let mut r = SidebarRow::new("Pull Requests").icon(&PULLS_ICON).selected(s.pulls.open || s.pulls.pr.is_some()).on_select(|s: &mut Collate, _: &mut Tx| s.open_pulls());
            if s.pulls.asked() > 0 {
                r = r.badge(s.pulls.asked().to_string());
            }
            r.build(k).key("view-pulls")
        },
        // The whole repository, with this view's changes on it.
        SidebarRow::new("Map").icon(&MAP_ICON).selected(s.map.open).on_select(|s: &mut Collate, _: &mut Tx| {
            s.pulls.open = false;
            s.open_map();
        }).build(k).key("view-map"),
    ]);
    // The coding agent's part of the loop, always in sight: ask it about the change, hand it
    // the comments, go back to before it ran, and have it settle what git stopped on.
    let agent = s.settings.agent.title();
    let mut agent_rows = vec![
        SidebarRow::new("Review Change").icon(&CHECKLIST_ICON).on_select(|s: &mut Collate, _: &mut Tx| s.ask_checklist(None)).build(k).key("agent-review"),
        {
            let mut r = SidebarRow::new("Send Comments").icon(&COMMENTS_ICON).on_select(|s: &mut Collate, _: &mut Tx| s.send_comments());
            if !s.comments.is_empty() {
                r = r.badge(s.comments.len().to_string());
            }
            r.build(k).key("agent-comments")
        },
        SidebarRow::new("Checkpoints").icon(&CHECKPOINTS_ICON).on_select(|s: &mut Collate, _: &mut Tx| s.checkpoints_menu("agent-checkpoints")).build(k).key("agent-checkpoints"),
    ];
    let (ahead, behind) = s.ahead_behind.unwrap_or((0, 0));
    let settle = s.stopped_settle().or((ahead > 0 && behind > 0).then_some(agent::Settle::Diverged(ahead, behind)));
    if let Some(settle) = settle {
        agent_rows.push(SidebarRow::new("Settle Conflicts").icon(&MERGE_ICON).on_select(move |s: &mut Collate, _: &mut Tx| s.settle_with_agent(settle.clone())).build(k).key("agent-settle"));
    }
    let agents = SidebarSection::new("Agent").rows(agent_rows);
    let picker = SidebarRow::new(agent).icon(&AGENT_ICON).detail("Change").on_select(|s: &mut Collate, tx: &mut Tx| s.agent_picker(tx)).build(k).key("agent-pick");
    let apart = |key: &str| col().key(key.to_owned()).w_full().h(12.0).a11y_hidden();
    // The same top inset as the other columns. Rune's section heading adds 4 pt above its own
    // 24 pt row, so 2 less here centres "Working Copy" in the same 28 pt band.
    let bar = Sidebar::new().section(working).child(apart("sections-apart")).section(review).child(apart("agent-apart")).section(agents).footer(picker);
    col().key("sidebar").w_full().h_full().pt(COLUMN_TOP - 2.0).child(bar.build(k))
}

/// The staging panel's inset, shared by its filter, list titles, rows and commit box.
const PANEL_INSET: f32 = 12.0;
/// The one top inset every column uses: each first row (the sidebar's "Working Copy", the
/// diff's summary, the panel's filter) is a 28 pt band this far below the card's top edge.
const COLUMN_TOP: f32 = 6.0;

/// A list's bulk action: its label and what it does.
type Bulk<'a> = (&'a str, fn(&mut Collate));

/// One list of the staging panel: a header with its count and its bulk action, then the
/// files. A row's trailing button stages or unstages that file at once. `(key, title)` names
/// it; its title folds it.
fn file_section(s: &Collate, k: &Kit, (key, title): (&'static str, &str), count: usize, bulk: Option<Bulk>, list: El, empty: &str) -> El {
    let folded = s.folded_lists.contains(key);
    // The title, its count and a disclosure chevron; a click anywhere up to the bulk action folds the list to its title or opens it.
    let toggle = row()
        .key(format!("{key}-toggle"))
        .grow(1.0)
        .h_full()
        .gap(4.0)
        .items_center()
        .cursor(Cursor::Pointer)
        .role("button")
        .label(title.to_owned())
        .description(format!("{count} files"))
        .expanded(!folded)
        .help(if folded { "Show the list" } else { "Fold the list to its title" })
        .on_press(move |s: &mut Collate| {
            if !s.folded_lists.remove(key) {
                s.folded_lists.insert(key);
            }
        })
        // Set as the sidebar sets "Working Copy", so the two columns' titles read as one kind.
        .child(text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
        .child(text(count.to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint).a11y_hidden())
        // The file tree's chevron, turned as it turns its folders'. Its glyph sits mid-box:
        // pulled in so it's as far from the count as the count is from the title.
        .child(icon(k.glyph.chevron_down, 16.0).key("disclosure").ml(-4.0).color(k.c.icon_faint).icon_rotation(if folded { -std::f32::consts::FRAC_PI_2 } else { 0.0 }).motion(k.motion.control).a11y_hidden());
    let mut head = row()
        .key(format!("{key}-head"))
        .w_full()
        // As tall as a sidebar row, so the first title is level with the sidebar's first row.
        .h(28.0)
        .px(PANEL_INSET)
        .items_center()
        .child(toggle);
    if let Some((label, f)) = bulk.filter(|_| count > 0) {
        // A ghost button pads its label; pull it out by that much so the label, not the
        // invisible box, ends on the panel's edge with the filter above it.
        head = head.child(Button::new(label.to_owned()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).on_press(f).build(k).key(format!("{key}-bulk")).mr(-GHOST_PAD));
    }
    // Folded, a list is its title alone and gives its space to the other.
    if folded {
        return col().key(key.to_owned()).w_full().motion(k.motion.disclosure).child(head);
    }
    // An empty list says so in one line and gives its space to the list that has files.
    if count == 0 {
        return col().key(key.to_owned()).w_full().child(head).child(row().key(format!("{key}-empty")).w_full().h(28.0).px(PANEL_INSET).items_center().child(text(empty.to_owned()).font(k.ty.text_small).color(k.c.text_faint)));
    }
    col().key(key.to_owned()).w_full().grow(1.0).min_h(0.0).motion(k.motion.disclosure).child(head).child(list)
}

/// A file list for the staging panel. `quick` is the row button: stage (`true`) or unstage.
fn file_list(s: &Collate, k: &Kit, staged_list: bool, quick: Option<bool>) -> El {
    let c = k.c;
    let small = k.ty.text_xsmall_medium;
    let hover = k.alpha(k.c.text_base, 0.08);
    let get: fn(&Collate) -> &TreeModel<FileNode<Stat>> = if staged_list { |s| &s.staged_tree } else { |s| &s.tree };
    let get_mut: fn(&mut Collate) -> &mut TreeModel<FileNode<Stat>> = if staged_list { |s| &mut s.staged_tree } else { |s| &mut s.tree };
    let mut style = rune_kit::TreeStyle::from_kit(k);
    // The chevron's box is wider than its glyph: start the box so the glyph is on the inset.
    style.padding = PANEL_INSET - 4.0;
    Tree::new(get, get_mut, label)
        .name(if staged_list { "Staged files" } else { "Changed files" })
        .style(style)
        .row(move |n, r| {
            let item = TreeItem::file(n, r);
            let Some(st) = n.data else { return item };
            let color = match st.letter {
                'A' | 'U' => c.success.icon,
                'D' => c.danger.icon,
                _ => c.warning.icon,
            };
            let item = if st.letter == 'D' || st.viewed { item.label_color(c.text_faint) } else { item };
            // The tree pads rows 4 pt less than the panel's inset (for its chevron); give it back
            // on the right so the row buttons end on the inset like Stage All.
            let mut trail = row().key("trail").gap(4.0).mr(4.0).items_center().child(text(st.letter.to_string()).key("status").font(small).color(color).w(10.0));
            if let Some(stage) = quick {
                let path = n.path.clone();
                let (glyph, what) = if stage { ("+", "Stage file") } else { ("−", "Unstage file") };
                trail = trail.child(
                    row()
                        .key("quick")
                        .size(16.0, 20.0)
                        .center()
                        .radius(5.0)
                        .hover(hover)
                        .cursor(Cursor::Pointer)
                        .role("button")
                        .label(format!("{what} {path}"))
                        .help(what)
                        .on_press(move |s: &mut Collate| if stage { s.stage_path(path.clone()) } else { s.unstage_path(path.clone()) })
                        .child(text(glyph.to_owned()).font(small).color(c.text_muted)),
                );
            }
            item.changes(st.added, st.removed).trailing(trail)
        })
        .on_select(reveal)
        .on_open(reveal)
        .build(s, k)
        // A focused tree takes typed letters (type-to-select) before the app's shortcuts see
        // them, so after a click j, k, a, x and the rest would jump around the file list. The
        // list stays click-only; the filter field and ] [ do its keyboard work.
        .focusable(false)
        .grow(1.0)
        .min_h(0.0)
}

/// The trailing staging panel, as GitKraken lays it out: what's unstaged, what's staged, and
/// the commit composer under them. Branch and History, being read-only, list their files.
fn staging_panel(s: &Collate, k: &Kit) -> El {
    let mut filter = TextInput::new(&s.filter).placeholder("Filter files").leading_icon(k.glyph.search);
    // A clear button only once there's something to clear.
    if !s.filter.text.is_empty() {
        filter = filter.clear_button();
    }
    let filter = filter.on_edit(|s: &mut Collate| &mut s.filter).on_change(|s: &mut Collate, _| s.apply_filter()).build(k).key("filter");
    let unstaged_n = s.info.iter().filter(|i| i.staged != Staged::Fully).count();
    let staged_n = s.info.iter().filter(|i| i.staged != Staged::No).count();
    let all = s.info.len();
    // Everything in the panel hangs off one 12 pt inset: the filter's edge, the list titles,
    // the rows' chevrons and the trailing buttons.
    // No gap between the panel's parts: each title's own 28 pt band spaces it, so every title
    // sits as far below what's above it (the filter, a hairline) as the next.
    let mut p = col().key("staging").w_full().h_full().pt(COLUMN_TOP);
    // History leads with the chosen commit; its files follow, as in Zed's commit view.
    if let Some(card) = pulls::pr_card(s, k) {
        p = p.child(card.mb(4.0));
    } else if s.scope == Scope::History
        && let Some(card) = commit_card(s, k)
    {
        p = p.child(card.mb(4.0));
    }
    // The first list title follows the filter with no gap, as the sidebar's first row follows
    // its section title: both second rows then sit on one line too.
    p = p.child(row().key("filter-row").w_full().px(PANEL_INSET).child(filter.w_full()));
    // A list that shows files takes the room above the commit box; with none showing (folded
    // or empty), a spacer does, so the box stays at the foot either way.
    let shows = |key: &str, n: usize| n > 0 && !s.folded_lists.contains(key);
    let foot = |p: El, lists_grow: bool| (if lists_grow { p } else { p.child(row().key("foot-spacer").w_full().grow(1.0)) }).child(commit_box(s, k));
    p = match s.scope {
        Scope::Uncommitted => p
            .child(file_section(s, k, ("unstaged", "Unstaged Files"), unstaged_n, Some(("Stage All", Collate::stage_all)), file_list(s, k, false, Some(true)), "Nothing unstaged"))
            .child(col().key("between").w_full().h(1.0).fill(k.c.border_weak))
            .child(file_section(s, k, ("staged", "Staged Files"), staged_n, Some(("Unstage All", Collate::unstage_all)), file_list(s, k, true, Some(false)), "Nothing staged yet")),
        Scope::Unstaged => p.child(file_section(s, k, ("unstaged", "Unstaged Files"), all, Some(("Stage All", Collate::stage_all)), file_list(s, k, false, Some(true)), "Nothing unstaged")),
        Scope::Staged => p.child(file_section(s, k, ("staged", "Staged Files"), all, Some(("Unstage All", Collate::unstage_all)), file_list(s, k, true, Some(false)), "Nothing staged")),
        Scope::Branch | Scope::History => return p.child(file_section(s, k, ("changed", "Changed Files"), all, None, file_list(s, k, false, None), "No files changed")),
    };
    let lists_grow = match s.scope {
        Scope::Uncommitted => shows("unstaged", unstaged_n) || shows("staged", staged_n),
        Scope::Unstaged => shows("unstaged", all),
        _ => shows("staged", all),
    };
    foot(p, lists_grow)
}

/// The commit message box's height: near square at the panel's default width.
const COMMIT_BOX_H: f32 = 150.0;

/// The commit composer at the foot of the staging panel: a message and the screen's one
/// prominent action, which commits what's staged.
fn commit_box(s: &Collate, k: &Kit) -> El {
    let ready = (s.staged > 0 || s.amend) && !s.message.text.trim().is_empty() && !s.committing;
    // A box, not a line: room for a summary and a body under it. Return starts a new line;
    // ⌘↩ commits.
    let field = TextArea::new(&s.message)
        .placeholder(if s.staged == 0 && !s.amend { "Stage files to write a commit".to_owned() } else { "Summary, then a blank line and the details".to_owned() })
        .label("Commit message")
        .rows(8)
        .max_height(COMMIT_BOX_H)
        .on_edit(|s: &mut Collate| &mut s.message)
        .on_key(|s: &mut Collate, ev: &KeyEvent, _: &mut Tx| {
            if ev.key == Key::Enter && ev.mods.cmd {
                s.commit();
                return true;
            }
            false
        })
        .width(None)
        .build(k)
        .key("message")
        // Fill the column exactly, not a field's default width.
        .w_full()
        .min_w(0.0);
    let label = match (s.committing, s.amend, s.staged) {
        (true, ..) => "Committing…".to_owned(),
        (_, true, _) => "Amend Last Commit".to_owned(),
        (_, _, 0) => "Commit".to_owned(),
        (_, _, 1) => "Commit 1 File".to_owned(),
        (_, _, n) => format!("Commit {n} Files"),
    };
    let help = match (s.amend, s.staged) {
        (true, _) => "Rewrite the last commit with this message and what's staged (⌘↩)",
        (_, 0) => "Stage files first",
        _ => "Commit what's staged (⌘↩)",
    };
    let button = Button::new(label).variant(ButtonVariant::Contrast).shortcut("⌘↩").disabled(!ready).help(help).on_press(|s: &mut Collate| s.commit()).build(k).key("commit").w_full();
    let amend = Checkbox::new(s.amend).label("Amend last commit").disabled(s.head.is_none() || s.committing).on_toggle(|s: &mut Collate| s.toggle_amend()).build(k).key("amend");
    // Set as the lists above are: an edge-to-edge hairline, as between them, then a 28 pt title
    // right under it.
    let agent = s.settings.agent.title();
    let write = Button::new(if s.writing_message { "Writing…".to_owned() } else { format!("Write with {agent}") })
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .disabled(s.writing_message || (s.staged == 0 && !s.amend))
        .help(format!("{agent} drafts the message from what's staged, keeping the intent of what you've typed; you commit it"))
        .on_press(|s: &mut Collate| s.write_message())
        .build(k)
        .key("write-message")
        .mr(-GHOST_PAD);
    let head = row().key("commit-head").w_full().h(28.0).px(PANEL_INSET).items_center()
        .child(text("Commit").font(k.ty.text_xsmall_medium).color(k.c.text_muted))
        .child(col().grow(1.0))
        .child(write);
    col()
        .key("commit-box")
        .w_full()
        .child(col().key("rule").w_full().h(1.0).fill(k.c.border_weak))
        .child(head)
        .child(col().key("commit-body").w_full().px(PANEL_INSET).pb(PANEL_INSET).gap(8.0).child(field).child(amend).child(button))
}

/// What the diff is, above it: the count, what it's measured against, the totals and how much
/// of it has been read.
/// What a Branch or History view compares, as a control on the summary's basis line.
fn compare_picker(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    match s.scope {
        Scope::Branch => {
            let base = Select::new("base", &s.branches)
                .current(s.base.as_deref())
                .placeholder("Base branch")
                .label("Compare against")
                .width(200.0)
                .open(s.base_open)
                .on_toggle(|s: &mut Collate| s.base_open = !s.base_open)
                .on_close(|s: &mut Collate| s.base_open = false)
                .on_choose(|s: &mut Collate, v: &str, _: &mut Tx| s.set_base(v))
                .build(k, layer);
            Some(row().key("compare").gap(8.0).items_center().child(text("Compare against").font(k.ty.text_small).color(k.c.text_muted)).child(base))
        }
        // A pull request: all of it, what changed since the reader's review, or one commit.
        Scope::History if s.pulls.pr.is_some() => {
            let d = s.pulls.pr.as_ref()?;
            let commit = s.commit.as_deref().unwrap_or("");
            let label = if s.pr_whole() {
                "All Changes".to_owned()
            } else if let Some(i) = d.commits.iter().position(|(id, _)| commit == format!("{id}^..{id}")) {
                format!("Commit {} of {}", i + 1, d.commits.len())
            } else {
                "Since Your Last Review".to_owned()
            };
            let chevron = k.glyph.chevron_down;
            let pick = Button::new(label.clone()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
                .label_with(move |font, color| row().gap(4.0).items_center().child(text(label).font(font).color(color)).child(icon(chevron, 12.0).color(color)))
                .help("All the changes, the changes since your last review, or one commit")
                .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.pr_part_menu(tx)).build(k).key("pr-part");
            Some(row().key("compare").gap(6.0).items_center().child(text("Showing").font(k.ty.text_small).color(k.c.text_muted)).child(pick))
        }
        Scope::History => {
            let at = s.commit.as_ref().and_then(|c| s.commit_ids.iter().position(|x| x == c));
            let older = Button::new("‹ Older").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(at.is_none_or(|i| i + 1 >= s.commit_ids.len())).help("Older commit (<)").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.step_commit(1, tx)).build(k).key("older");
            let newer = Button::new("Newer ›").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(at.is_none_or(|i| i == 0)).help("Newer commit (>)").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.step_commit(-1, tx)).build(k).key("newer");
            Some(row().key("compare").gap(6.0).items_center().child(older).child(newer))
        }
        _ => None,
    }
}

/// A changed image: before and after side by side, each at its own size up to what fits.
fn image_row(s: &Collate, k: &Kit, f: usize) -> El {
    let Some(sides) = s.diff.files().get(f).and_then(|f| f.images.clone()) else { return row() };
    let (max_w, max_h) = (440.0, IMAGE_H - 24.0 - 26.0);
    let pane = |key: &str, label: &str, side: &Option<diff::ImageSide>, tint: rune::Color, missing: &str| {
        let img = side.as_ref().and_then(|(h, _)| s.images.get(h));
        let size = img.and_then(|i| i.size());
        let detail = match (size, side) {
            (Some([w, h]), Some((_, b))) => format!("{w} × {h} - {}", file_size(b.len())),
            (None, Some((_, b))) => file_size(b.len()),
            _ => String::new(),
        };
        let head = row().gap(8.0).items_center()
            .child(text(label.to_owned()).font(k.ty.text_xsmall_medium).color(tint))
            .child(text(detail).font(k.ty.text_small).color(k.c.text_faint));
        let body = match (img, size) {
            (Some(i), Some([w, h])) if i.error().is_none() => {
                let scale = (max_w / w as f32).min(max_h / h as f32).min(1.0);
                row().size((w as f32 * scale).max(1.0), (h as f32 * scale).max(1.0)).border(1.0, k.alpha(tint, 0.6)).pixels_fit(i.pixels(), rune::SurfaceFit::Fit)
                    .role("img").label(format!("{label} image"))
            }
            (Some(i), _) if i.error().is_some() => text("Can't decode this image".to_owned()).font(k.ty.text_small).color(k.c.text_muted),
            (Some(_), _) => text("Loading…".to_owned()).font(k.ty.text_small).color(k.c.text_muted),
            (None, _) if side.is_none() => text(missing.to_owned()).font(k.ty.text_small).color(k.c.text_muted),
            (None, _) => text("Binary file not shown".to_owned()).font(k.ty.text_small).color(k.c.text_muted),
        };
        col().key(key.to_owned()).grow(1.0).min_w(0.0).h_full().gap(8.0).items_center().child(head).child(col().grow(1.0).min_h(0.0).center().child(body))
    };
    row().w_full().h(IMAGE_H).px(EDGE).py(12.0).gap(16.0)
        .child(pane("before", "Before", &sides[0], k.c.danger.fg, "Not there before"))
        .child(col().w(1.0).h_full().fill(k.c.border_weak))
        .child(pane("after", "After", &sides[1], k.c.success.fg, "Deleted"))
}

/// A file's size as Finder gives it: `840 bytes`, `12.4 KB`, `3.1 MB`.
fn file_size(n: usize) -> String {
    match n {
        0..1_000 => format!("{n} bytes"),
        1_000..1_000_000 => format!("{:.1} KB", n as f64 / 1_000.0),
        _ => format!("{:.1} MB", n as f64 / 1_000_000.0),
    }
}

/// A commit id as git abbreviates it.
pub fn abbrev(id: &str) -> String {
    id.chars().take(7).collect()
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

fn summary(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> El {
    let n = s.diff.files().len();
    let (adds, dels) = s.diff.totals();
    let viewed = (0..n).filter(|&i| s.is_viewed(i)).count();
    let short = |h: &str| h[..h.len().min(8)].to_owned();
    let head = s.head.as_deref().map(|h| format!(" ({})", short(h))).unwrap_or_default();
    let basis = match (s.scope, s.head.is_some()) {
        (Scope::Branch, _) => match (&s.base, &s.base_commit) {
            (Some(b), Some(mb)) => format!("since {b} - merge base {}", short(mb)),
            (Some(b), None) => format!("vs {b}"),
            _ => "choose a base branch".to_owned(),
        },
        (Scope::History, _) => match &s.commit {
            Some(c) => format!("{} against its parent", short(c)),
            None => "no commits yet".to_owned(),
        },
        (_, false) => "before the first commit".to_owned(),
        (Scope::Uncommitted, _) => format!("working tree vs HEAD{head}"),
        (Scope::Staged, _) => format!("index vs HEAD{head}"),
        (Scope::Unstaged, _) => "working tree vs index".to_owned(),
    };
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let files = match n {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    // Row 1: what this is (counts, what it's measured against) and the tools that work on it.
    let mut what = row()
        .key("what")
        .gap(10.0)
        .items_center()
        .min_w(0.0)
        .shrink()
        .child(text(files).font(k.ty.ui_medium).color(k.c.text_base))
        .child(text(format!("+{adds}")).font(mono).color(k.c.success.fg))
        .child(text(format!("\u{2212}{dels}")).font(mono).color(k.c.danger.fg))
        .child(stat_bar(k, adds, dels, 72.0));
    what = match compare_picker(s, k, layer) {
        Some(picker) => what.child(picker),
        None => what.child(text(basis).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).shrink()),
    };
    // As tall as the staging panel's filter field, so the columns' first rows centre alike.
    let top = row()
        .key("totals")
        .w_full()
        .h(28.0)
        .gap(12.0)
        .items_center()
        .child(what)
        .child(row().grow(1.0))
        .child(text(progress(s.diff.visible_file(), n, viewed)).key("viewed-count").font(k.ty.text_small).color(k.c.text_muted));
    // Row 2, only when there's something to say about how the diff is drawn.
    // A 28 pt band right under the first, as the sidebar's and the panel's second rows are, so
    // all three second rows centre on one line.
    let mut notes = row().key("notes").w_full().h(28.0).gap(14.0).items_center();
    let mut any = false;
    if s.hidden > 0 || s.show_low_signal {
        any = true;
        let (note, word) = if s.show_low_signal {
            (String::new(), "Hide Low-Signal Files")
        } else {
            (format!("{} low-signal file{} hidden", s.hidden, if s.hidden == 1 { "" } else { "s" }), "Show")
        };
        notes = notes.child(chip(k, "low-signal", &note, word, "Lockfiles, bundles, snapshots and generated files", |s: &mut Collate, _: &mut Tx| {
            s.show_low_signal = !s.show_low_signal;
            s.reload();
        }));
    }
    let moved: usize = s.diff.files().iter().map(|f| f.moves.len()).sum();
    if moved > 0 {
        any = true;
        notes = notes.child(
            row()
                .key("moved")
                .gap(5.0)
                .items_center()
                .help("Blocks removed in one place and added unchanged in another show in blue: moves, not new code. m jumps between the two ends.")
                .child(col().size(8.0, 8.0).radius(2.0).fill(k.c.info.icon))
                .child(text(format!("{moved} moved lines")).font(k.ty.text_small).color(k.c.text_muted)),
        );
    }
    if !s.comments.is_empty() {
        any = true;
        let (n, away) = (s.comments.len(), s.comments_shown.iter().filter(|&&on| !on).count());
        let mut note = format!("{n} comment{}", if n == 1 { "" } else { "s" });
        if away > 0 {
            note.push_str(&format!(" - {away} on lines not in this view"));
        }
        notes = notes.child(chip(k, "comments", &note, "Copy as Markdown", "Copy every comment, with its file, line and the line's text", |s: &mut Collate, tx: &mut Tx| s.copy_comments(tx)).child(
            Button::new(format!("Send to {}", s.settings.agent.title()))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .help("Start the coding agent in a terminal under the diff, asked to address every comment (Settings - Coding agent)")
                .on_press(|s: &mut Collate| s.send_comments())
                .build(k)
                .key("send-comments"),
        ));
    }
    if s.settings.ignore_whitespace {
        any = true;
        notes = notes.child(chip(k, "ws", "Whitespace changes hidden", "Show", "Lines that only changed whitespace show as unchanged (W)", |s: &mut Collate, _: &mut Tx| s.configure(|c| c.ignore_whitespace = false)));
    }
    let mut c = col().key("summary").w_full().px(16.0).pt(COLUMN_TOP).pb(PANEL_INSET).fill(k.c.bg_base).child(top);
    if any {
        c = c.child(notes);
    }
    c
}

/// Where the reader is in the review: the file at the top of the diff and how many are
/// viewed, so a long review shows its progress without counting ticks.
fn progress(at: usize, files: usize, viewed: usize) -> String {
    match files {
        0 | 1 => format!("{viewed}/{files} viewed"),
        n => format!("File {} of {n} - {viewed} viewed", at.min(n - 1) + 1),
    }
}

/// Feedback after an action, at the bottom of the diff: what happened, and Undo for a
/// discard. It goes by itself after a few seconds.
/// A merge, rebase, cherry-pick or revert waiting on the reader: what to do, and the ways on.
fn operation_bar(s: &Collate, k: &Kit) -> Option<El> {
    let op = s.operation?;
    let conflicts = s.info.iter().filter(|i| i.conflicted).count();
    let say = if conflicts > 0 {
        format!("{} - {conflicts} file{} with conflicts. Settle and stage them, then continue.", op.doing(), if conflicts == 1 { "" } else { "s" })
    } else {
        format!("{} - the conflicts are settled. Stage what's left, then continue.", op.doing())
    };
    let idle = s.busy.is_none();
    let step = move |how: &'static str, done: &'static str| {
        move |s: &mut Collate| s.act_bg(&format!("{}…", how[..1].to_uppercase() + &how[1..]), move |r| r.resume(op, how).map(|_| format!("{done} the {}", op.noun())))
    };
    let mut r = row().key("operation").w_full().h(40.0).px(EDGE).gap(8.0).items_center().fill(k.alpha(k.c.warning.icon, 0.10))
        .role("status").label(say.clone())
        .child(text(say).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
        .child(Button::new(format!("Settle with {}", s.settings.agent.title())).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(!idle)
            .help("Start the coding agent in the terminal to settle the conflicts and continue; it doesn't push")
            .on_press(|s: &mut Collate| if let Some(settle) = s.stopped_settle() { s.settle_with_agent(settle) }).build(k).key("op-agent"))
        .child(Button::new("Abort").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(!idle)
            .help(format!("Stop the {} and put everything back as it was", op.noun())).on_press(step("abort", "Aborted")).build(k).key("op-abort"));
    if op == git::Operation::Rebase {
        r = r.child(Button::new("Skip Commit").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(!idle)
            .help("Leave out the commit the rebase stopped at").on_press(step("skip", "Skipped a commit of")).build(k).key("op-skip"));
    }
    Some(r.child(Button::new("Continue").variant(ButtonVariant::Contrast).control_size(ControlSize::Small).disabled(!idle || conflicts > 0)
        .help("Commit the settled files and go on").on_press(step("continue", "Continued")).build(k).key("op-continue")))
}

fn status_bar(s: &Collate, k: &Kit) -> Option<El> {
    let note = s.notice.as_ref()?;
    let color = if note.error { k.c.danger.icon } else { k.c.text_muted };
    let mut r = row()
        .key("status-bar")
        .w_full()
        .h(34.0)
        .px(16.0)
        .gap(12.0)
        .items_center()
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .role("status")
        .label(note.text.clone())
        .child(text(note.text.clone()).key("notice").font(k.ty.text_small).color(color).truncate().min_w(0.0).grow(1.0));
    if note.undo.is_some() {
        r = r.child(Button::new("Undo").variant(ButtonVariant::Outline).control_size(ControlSize::Small).help("Put the discarded file back from its backup").on_press(|s: &mut Collate| s.undo_discard()).build(k).key("undo"));
    }
    Some(r)
}

/// A note in the summary row with the action that undoes it: the note as text, the action
/// the same small button as Stage All.
fn chip(k: &Kit, key: &str, note: &str, label: &str, help: &str, f: impl Fn(&mut Collate, &mut Tx) + 'static) -> El {
    let button = Button::new(label.to_owned()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help(help.to_owned()).on_press_tx(f).build(k).key("act");
    let r = row().key(key.to_owned()).items_center();
    if note.is_empty() {
        // Alone, its label (not its padding) starts where a note would.
        return r.child(button.ml(-GHOST_PAD));
    }
    r.child(text(note.to_owned()).font(k.ty.text_small).color(k.c.text_muted)).child(button)
}

fn stat_bar(k: &Kit, adds: usize, dels: usize, w: f32) -> El {
    let bar = row().key("stat").w(w).h(4.0).radius(2.0).fill(k.c.border_weak).a11y_hidden();
    let total = (adds + dels) as f32;
    if total == 0.0 {
        return bar;
    }
    let add_w = (w * adds as f32 / total).round();
    bar.child(col().key("a").w(add_w).h_full().radius(2.0).fill(k.c.success.icon)).child(col().key("d").w(w - add_w).h_full().radius(2.0).fill(k.c.danger.icon))
}

/// The "viewed" check at the end of a file's header.
fn viewed_toggle(k: &Kit, f: usize, viewed: bool) -> El {
    let (fill, border, glyph) = if viewed {
        (k.alpha(k.c.success.icon, 0.14), k.alpha(k.c.success.icon, 0.5), k.c.success.icon)
    } else {
        (k.c.bg_base, k.c.border_weak, k.c.text_faint)
    };
    row()
        .key("viewed")
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .fill(fill)
        .border(1.0, border)
        .hover(k.alpha(k.c.text_base, 0.06))
        .cursor(Cursor::Pointer)
        .role("checkbox")
        .label("Viewed")
        .checked(viewed)
        .help(if viewed { "Mark as not viewed (v)" } else { "Mark as viewed (v)" })
        .on_press_tx(move |s: &mut Collate, tx| s.toggle_viewed(f, tx))
        .child(icon(k.glyph.check_small, 14.0).color(glyph))
}

fn message(k: &Kit, title: &str, detail: &str) -> El {
    let mut c = col().key("message").grow(1.0).w_full().center().gap(6.0).fill(k.c.bg_base).child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base));
    if !detail.is_empty() {
        c = c.child(text(detail.to_owned()).font(k.ty.ui).color(k.c.text_muted).wrap().max_w(520.0));
    }
    c
}

/// The toolbar search's width: room in the diff for its match count and options.
const SEARCH_W: f32 = 300.0;

/// The toolbar's search, left of the layout switch, one field for every place: the diff's
/// lines in a review or a commit's diff, files in the Map, commits and refs in History, with
/// what it found there listed under it.
fn toolbar_search(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> El {
    let place = s.searched();
    let (edit, hint) = match place {
        Searched::Diff => (&s.search, "Search the diff"),
        Searched::Map => (&s.map_query, "Search files"),
        Searched::History => (&s.history_query, "Search commits, branches, tags"),
    };
    let mut field = TextInput::new(edit).placeholder(hint).label(hint).leading_icon(k.glyph.search).width(Some(SEARCH_W));
    if !edit.text.is_empty() {
        field = field.clear_button();
    }
    if place == Searched::Diff {
        field = field.trailing(find_extras(s, k));
    }
    let field = field
        .on_edit(|s: &mut Collate| {
            let place = s.searched();
            s.query_mut(place)
        })
        .on_change(|s: &mut Collate, tx: &mut Tx| {
            if s.searched() == Searched::Diff {
                s.hit = None;
                s.find();
                // Land on the first match as you type, as a browser's find does.
                s.next_hit(true, tx);
            } else {
                s.lookup();
            }
        })
        .on_key(|s: &mut Collate, ev: &KeyEvent, tx: &mut Tx| s.search_key(ev, tx))
        .build(k)
        .key("search");
    if place != Searched::Diff && edit.focused && !edit.text.trim().is_empty() {
        layer.push(found_list(s, k, place));
    }
    field
}

/// Inside the diff's search: where the reader is among the matches, and the options, as an
/// editor's find bar has them.
fn find_extras(s: &Collate, k: &Kit) -> El {
    let hits = match (s.hits.len(), s.hit) {
        _ if s.search_error.is_some() => "Bad pattern".to_owned(),
        (0, _) if !s.search.text.trim().is_empty() => "No matches".to_owned(),
        (0, _) => String::new(),
        (n, Some(i)) => format!("{} of {n}", i + 1),
        (n, None) => format!("{n} matches"),
    };
    let hits = match &s.search_error {
        Some(e) => text(hits).key("hits").font(k.ty.text_small).color(k.c.danger.fg).help(e.clone()),
        None => text(hits).key("hits").font(k.ty.text_small).color(k.c.text_muted),
    };
    let option = |key: &str, glyph: &str, name: &str, keys: &str, on: bool, flip: fn(&mut Collate)| {
        // The glyph centred in a box of its own, so all three line up and match in size.
        let glyph = text(glyph.to_owned()).font(rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono }).color(if on { k.c.text_base } else { k.c.text_faint });
        row()
            .key(key.to_owned())
            .w(24.0)
            .h(20.0)
            .center()
            .child(glyph)
            .radius(k.radius.sm)
            .fill(if on { k.alpha(k.c.text_base, 0.12) } else { k.alpha(k.c.text_base, 0.0) })
            .hover(k.alpha(k.c.text_base, 0.08))
            .cursor(Cursor::Pointer)
            .role("checkbox")
            .label(format!("{name}{}", if on { ", on" } else { ", off" }))
            .help(format!("{name} ({keys})"))
            .on_press(move |s: &mut Collate| s.search_option(flip))
    };
    row()
        .key("search-options")
        .gap(2.0)
        .items_center()
        .child(hits.mr(4.0))
        .child(option("find-case", "Aa", "Match case", "⌥⌘C", s.find_case, |s| s.find_case ^= true))
        .child(option("find-word", "ab", "Whole words", "⌥⌘W", s.find_word, |s| s.find_word ^= true))
        .child(option("find-regex", ".*", "Regular expression", "⌥⌘R", s.find_regex, |s| s.find_regex ^= true))
}

/// Under the search in the Map and History: what it found, best first. Return or a click
/// goes to the pick; the arrows move it.
fn found_list(s: &Collate, k: &Kit, place: Searched) -> El {
    let mut rows = col().key("found-rows").w_full().padding(4.0);
    if s.found.is_empty() {
        let none = if place == Searched::Map { "No file matches".to_owned() } else { format!("Nothing matches in the {} commits loaded", s.commit_ids.len()) };
        rows = rows.child(row().key("found-none").h(28.0).px(8.0).items_center().child(text(none).font(k.ty.ui).color(k.c.text_faint)));
    }
    for (n, f) in s.found.iter().enumerate() {
        let glyph: &'static Icon = match f.kind {
            search::Kind::File => &rune_kit::glyphs::FILE,
            search::Kind::Folder => &rune_kit::glyphs::FOLDER,
            search::Kind::Commit => &COMMIT_ICON,
            search::Kind::Branch => &BRANCH_ICON,
            search::Kind::Tag => &TAG_ICON,
        };
        rows = rows.child(
            row().key(format!("found-{n}")).w_full().h(28.0).px(8.0).gap(8.0).items_center().radius(k.radius.sm)
                .fill(if n == s.found_pick { k.c.hover } else { k.alpha(k.c.hover, 0.0) })
                .cursor(Cursor::Pointer).role("option").label(format!("{} - {}", f.title, f.detail))
                .on_hover(move |s: &mut Collate| s.found_pick = n)
                .on_press_tx(move |s: &mut Collate, tx: &mut Tx| s.take_found(n, tx))
                .child(icon(glyph, 14.0).color(k.c.icon_faint).a11y_hidden())
                .child(text(f.title.clone()).font(k.ty.ui).color(k.c.text_base).truncate().min_w(0.0).shrink().a11y_hidden())
                .child(text(f.detail.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).shrink().grow(1.0).a11y_hidden()),
        );
    }
    let body = col().key("found").w(420.0).max_h(360.0).min_h(0.0).child(rune_kit::popover::fit_scroll(rows));
    rune_kit::popover::anchored(k, rune_kit::popover::raised(k, body, k.radius.md), "search", Placement::BottomEnd, 4.0)
}

/// Before every frame: retire a status message once it's been up long enough.
fn poll(s: &mut Collate, now: f64, tx: &mut Tx) -> bool {
    // Whatever moved the reader since the last frame, back now returns there.
    let moved = s.note_place();
    let Some(n) = &mut s.notice else { return moved };
    match n.until {
        None => {
            n.until = Some(now + NOTICE_FOR);
            tx.wake_after(Duration::from_secs_f64(NOTICE_FOR));
            moved
        }
        Some(t) if now >= t => {
            s.notice = None;
            true
        }
        Some(_) => moved,
    }
}

fn main() {
    // A folder named on the command line, else the project opened last; none is fine (the
    // window offers to add one, as it does when Collate is launched from the Dock).
    // Only the first argument can be the folder: later ones are flags and their values
    // (`--snap out.png`), and Finder may pass a `-psn_…` one.
    let arg = std::env::args().nth(1).filter(|a| !a.starts_with('-')).map(PathBuf::from);
    let mut projects = load_projects();
    let repo = match &arg {
        Some(dir) => match Repo::open(dir) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("collate: {e}");
                std::process::exit(1);
            }
        },
        None => projects.iter().find_map(|p| Repo::open(p).ok()),
    };
    if let Some(r) = &repo {
        remember_project(&mut projects, &load_paths("favorites"), r.root.clone());
    }
    let title: &'static str = Box::leak(repo.as_ref().map_or_else(|| "Collate".to_owned(), |r| format!("{} — Collate", r.name())).into_boxed_str());
    let base = repo.as_ref().and_then(|r| git::default_base(&r.branches()));
    // History opens on the latest commit; the window's own state picks the same one.
    let first_commit = repo.as_ref().and_then(Repo::head_id);
    let root = repo.as_ref().map(|r| r.root.clone());
    set_repo(repo);
    let start = Settings::load().default_scope;

    let spec = AppSpec {
        on_key: Some(on_key),
        poll: Some(poll),
        setup: Some(|s: &mut Collate, waker| {
            // Shells' events, from their threads, run here on the UI thread.
            s.terminals.connect(std::sync::Arc::new(|id, event| {
                if let Some(app) = HANDLE.get() {
                    app.update_if(move |s, tx| s.terminals.event(id, event, tx));
                }
            }));
            pulls::watch_reviews();
            update::watch();
            s.avatars.start(rune::ImageCache::new(waker), |email, bytes| {
                if let Some(app) = HANDLE.get() {
                    app.update(move |s, _| s.avatars.landed(email, bytes));
                }
            });
        }),
        families: ("Inter", "ui-monospace, Inter"),
        min_size: Some((760.0, 440.0)),
        scenes: &[
            ("settings", |s: &mut Collate| s.screen = Screen::Settings),
            ("settings-diff", |s: &mut Collate| {
                s.screen = Screen::Settings;
                s.settings_tab = 1;
            }),
            ("settings-agent", |s: &mut Collate| {
                s.screen = Screen::Settings;
                s.settings_tab = settings_view::AGENT_TAB;
            }),
            ("settings-updates", |s: &mut Collate| {
                s.screen = Screen::Settings;
                s.settings_tab = SETTINGS_TABS.len() - 1;
                s.update = update::Status::Ready(update::Update::Release { version: "1.2.0".into(), zip: String::new(), sums: String::new(), name: String::new() });
            }),
            ("help", |s: &mut Collate| s.show_help = true),
            ("staged", |s: &mut Collate| s.set_scope(Scope::Staged)),
            ("unstaged", |s: &mut Collate| s.set_scope(Scope::Unstaged)),
            ("branch", |s: &mut Collate| s.set_scope(Scope::Branch)),
            ("history", |s: &mut Collate| s.set_scope(Scope::History)),
            ("map", |s: &mut Collate| s.open_map()),
            ("pulls", |s: &mut Collate| s.pulls.open = true),
            // One pull request in each state, the finished groups opened.
            ("pulls-list", |s: &mut Collate| {
                let pull = |number: u32, author: &str, state: github::PrState, draft: bool, ci: Option<github::CheckState>| github::Pull {
                    number, title: format!("Change {number}"), author: author.into(), head: format!("branch-{number}"), base: "main".into(), state, draft,
                    decision: "REVIEW_REQUIRED".into(), requested: vec![], additions: 120, deletions: 8, updated: "2026-11-10T09:00:00Z".into(),
                    labels: vec![], checks: vec![], ci, url: String::new(),
                };
                use github::{CheckState::*, PrState::*};
                let list = vec![pull(8, "me", Open, false, Some(Passed)), pull(9, "ana", Open, true, Some(Pending)), pull(10, "bo", Open, false, Some(Failed)),
                    pull(6, "ana", Merged, false, None), pull(5, "bo", Closed, false, None)];
                s.pulls.open = true;
                s.pulls.list = Some(Ok(("me".into(), list)));
                s.pulls.folded.clear();
            }),
            ("pulls-connect", |s: &mut Collate| {
                s.pulls.open = true;
                s.pulls.list = Some(Err(github::SIGNED_OUT.into()));
            }),
            ("history-diff", |s: &mut Collate| {
                s.set_scope(Scope::History);
                s.history_diff = true;
            }),
            ("discard", |s: &mut Collate| s.confirm = Some(Discard { file: 0, hunk: Some(0), path: "src/state.rs".into() })),
        ],
        ..AppSpec::new(title, (1280.0, 840.0), init, view)
    };
    // The traffic lights centred on the 48-point title band the toolbar and the sidebar's
    // toggle sit in (its middle 24 points down), 16 points in, as SplitView lays out for.
    let chrome = rune::Chrome { full_size_content: true, traffic_lights: Some((16.0, 17.0)), drag_regions: true };
    let app = App::new(spec).chrome(chrome).modifier_keys().standard_menus();
    let (tx, rx) = mpsc::channel();
    let _ = WATCHER.set(tx);
    watch_project_files(root);
    let handle = app.handle();
    let _ = HANDLE.set(handle.clone());
    std::thread::spawn(move || watch(handle, rx, start, base, first_commit));
    // Read the first snapshot right away rather than after one poll interval.
    let _ = WATCHER.get().unwrap().send(Cmd::Refresh);
    app.run();
}

#[cfg(test)]
mod sizes {
    use super::*;
    use crate::watcher::{DiffCache, prepare};

    /// Rough heap sizes of what a heavy review keeps, by part. `COLLATE_BENCH_REPO=<repo> cargo
    /// test --release heap_sizes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn heap_sizes() {
        let dir = std::env::var("COLLATE_BENCH_REPO").expect("COLLATE_BENCH_REPO");
        let repo = Repo::open(std::path::Path::new(&dir)).unwrap();
        let raw = repo.status_raw().unwrap();
        let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
        let p = prepare(snap, DiffOpts { context: 3, ignore_whitespace: false, moved: true, hide_low_signal: true, source_first: true }, &mut DiffCache::new()).unwrap();
        let mut texts = 0usize;
        let (mut lines, mut line_text, mut changes) = (0usize, 0usize, 0usize);
        for f in &p.files {
            texts += [diff::Side::Old, diff::Side::New].iter().filter_map(|&s| f.text(s)).map(|t| t.len() + t.lines().count() * 8).sum::<usize>();
            for l in f.hunks.iter().flat_map(|h| &h.lines) {
                lines += 1;
                line_text += l.text.capacity();
                changes += l.changes.capacity() * 16;
            }
        }
        let line_struct = std::mem::size_of::<diff::Line>();
        let mut st = DiffState::new(Vec::new());
        st.set_files(p.files.clone());
        eprintln!("files {}", p.files.len());
        eprintln!("whole texts (both sides, with line index)  {:6.1} MB", texts as f64 / 1e6);
        eprintln!("hunk lines: {lines} × {line_struct} B + text {:.1} MB + word ranges {:.1} MB = {:6.1} MB per copy", line_text as f64 / 1e6, changes as f64 / 1e6, (lines * line_struct + line_text + changes) as f64 / 1e6);
        eprintln!("rows {} (keys made on demand; no per-row index)", st.rows.len());
    }
}

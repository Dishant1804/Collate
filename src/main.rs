//! Collate: a git diff viewer for reviewing and landing changes. `collate [path]` opens the repository
//! containing `path` (default: the current directory) and shows its changes beside a file tree,
//! refreshing as files change.
//!
//! Keys: `j`/`k` next/previous hunk, `]`/`[` next/previous file; on the hunk at the top, `a`
//! stages, `u` unstages, `x` discards; `A`/`U`/`X` do the same for the file in view; `v` marks
//! it viewed, `o` opens it; `s` unified/split, `w` wrap, `1`–`4` all/staged/unstaged/pull request preview,
//! `r` refresh, ⌘↩ commit, ⌘, settings, ⇧⌘B sidebar.

mod avatars;
mod diff;
mod git;
mod graph;
mod map;
mod map_view;
mod nav;
mod patch;
mod settings;
#[cfg(test)]
mod git_tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use diff::{DiffMode, DiffState, DiffView, EDGE, FileDiff, Finder, GHOST_PAD, LineKind, LineRef, Query, WordEdit, apply_edits, mark_moved, parse, word_edits};
use git::{Blob, Change, Repo, Scope, Snapshot, Staged};
use rune::{App, AppSpec, Cursor, Cx, El, Icon, Key, KeyEvent, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::shell::{self, PaneState, Sidebar, SidebarRow, SidebarSection, SplitView, Toolbar};
use rune_kit::syntax::Highlighter;
use rune_kit::{
    Checkbox, Button, ButtonVariant, ControlSize, Dialog, FileNode, FileTree, IconButton, IconButtonVariant, Kit, SegmentedControl, Select, Switch, TextInput, Tree,
    TreeItem, TreeModel,
};
use settings::{CONTEXT_LINES, Density, Editor, FOLD_LARGE, FONT_SIZES, REFRESH_MS, Settings, Theme};

/// The open project; `None` before one is chosen. Swapped when the reader switches project.
static REPO: std::sync::RwLock<Option<std::sync::Arc<Repo>>> = std::sync::RwLock::new(None);

fn current_repo() -> Option<std::sync::Arc<Repo>> {
    REPO.read().unwrap().clone()
}

fn set_repo(r: Option<Repo>) {
    *REPO.write().unwrap() = r.map(std::sync::Arc::new);
}

/// The FSEvents watcher for the open project, replaced on a switch.
static FILES: std::sync::Mutex<Option<notify::RecommendedWatcher>> = std::sync::Mutex::new(None);

/// Points the file watcher at `root` (or nowhere).
fn watch_project_files(root: Option<PathBuf>) {
    let w = root.zip(WATCHER.get().cloned()).and_then(|(root, tx)| watch_files(root, tx));
    EVENTS.store(w.is_some(), Ordering::Relaxed);
    *FILES.lock().unwrap() = w;
}
static HANDLE: OnceLock<rune::AppHandle<Collate>> = OnceLock::new();

/// Tells the watcher to switch scope or base, or to reread now.
enum Cmd {
    Scope(Scope),
    Base(String),
    /// The commit the History view shows.
    Commit(String),
    Refresh,
    /// Files changed on disk (from FSEvents): reread soon, once the burst settles.
    Changed,
    /// Another project was opened: forget the last one's reads, with its base and commit.
    Switch { base: Option<String>, commit: Option<String> },
}

/// The file watcher is running: polling is only a slow safety net.
static EVENTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a change at `path` can change what Collate shows. Inside `.git`, only the index,
/// HEAD and refs matter; objects, logs, locks and Collate's own backups don't.
fn relevant(path: &std::path::Path, root: &std::path::Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return true };
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    match parts.next().as_deref() {
        Some(".git") => matches!(parts.next().as_deref(), Some("index" | "HEAD" | "refs" | "packed-refs" | "MERGE_HEAD")),
        _ => true,
    }
}

/// Watches the working tree with FSEvents, nudging the watcher thread on each relevant
/// change. `None` if the platform watcher can't start; polling covers it then.
fn watch_files(root: std::path::PathBuf, tx: Sender<Cmd>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let base = root.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev.paths.iter().any(|p| relevant(p, &base))
        {
            let _ = tx.send(Cmd::Changed);
        }
    })
    .ok()?;
    w.watch(&root, notify::RecursiveMode::Recursive).ok()?;
    Some(w)
}

static WATCHER: OnceLock<Sender<Cmd>> = OnceLock::new();

/// The settings that shape the diffs, as the watcher thread reads them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffOpts {
    context: u32,
    ignore_whitespace: bool,
    moved: bool,
    hide_low_signal: bool,
}

static OPTS: std::sync::Mutex<DiffOpts> = std::sync::Mutex::new(DiffOpts { context: 3, ignore_whitespace: false, moved: true, hide_low_signal: true });

/// Built diffs by content hash and the settings that shape them, so a refresh rediffs only
/// the files that changed.
type DiffCache = HashMap<(u64, u32, bool), FileDiff>;

/// A snapshot with its diffs built, ready for the UI.
struct Prepared {
    snap: Snapshot,
    files: Vec<FileDiff>,
    hidden: usize,
    opts: DiffOpts,
}

/// Diff a snapshot's files, off the UI thread: low-signal files left out when hidden, each
/// file diffed unless `cache` has it (new ones spread across cores), and moves marked across
/// them all. `None` if a file the snapshot skipped as unchanged isn't in the cache: the
/// caller rereads everything.
fn prepare(mut snap: Snapshot, opts: DiffOpts, cache: &mut DiffCache) -> Option<Prepared> {
    let total = snap.changes.len();
    if opts.hide_low_signal {
        snap.changes.retain(|c| !(c.generated || settings::low_signal(&c.path)));
    }
    let hidden = total - snap.changes.len();
    // The sidebar's counts leave out what's hidden, so they match the lists.
    if opts.hide_low_signal {
        let shown: Vec<&(String, bool, bool)> = snap.working.iter().filter(|(p, ..)| !settings::low_signal(p)).collect();
        snap.total = shown.len();
        snap.unstaged = shown.iter().filter(|w| w.2).count();
    }
    let key = |c: &Change| (c.hash, opts.context, opts.ignore_whitespace);
    if snap.changes.iter().any(|c| c.reused && !cache.contains_key(&key(c))) {
        return None;
    }
    // Diff what the cache lacks, in parallel: each file is independent.
    let missing: Vec<usize> = (0..snap.changes.len()).filter(|&i| !cache.contains_key(&key(&snap.changes[i]))).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let built: Vec<(usize, FileDiff)> = if missing.len() < 8 {
        missing.iter().map(|&i| (i, file_diff(&snap.changes[i], opts.context, opts.ignore_whitespace))).collect()
    } else {
        let changes = &snap.changes;
        std::thread::scope(|scope| {
            let jobs: Vec<_> = missing
                .chunks(missing.len().div_ceil(threads))
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|&i| {
                                let f = file_diff(&changes[i], opts.context, opts.ignore_whitespace);
                                // Measured once here, so the cached copy carries it to every later refresh.
                                f.width();
                                (i, f)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
        })
    };
    let mut fresh: HashMap<usize, FileDiff> = built.into_iter().collect();
    let mut next = DiffCache::with_capacity(snap.changes.len());
    let mut files: Vec<FileDiff> = Vec::with_capacity(snap.changes.len());
    for (i, c) in snap.changes.iter().enumerate() {
        let k = key(c);
        let f = match fresh.remove(&i) {
            Some(f) => {
                f.width();
                f
            }
            None => cache.remove(&k)?,
        };
        next.insert(k, f.clone());
        files.push(f);
    }
    *cache = next;
    if opts.moved {
        mark_moved(&mut files);
    }
    // The diffs hold the texts now; the copies read from git needn't cross to the UI thread.
    for c in &mut snap.changes {
        c.forget_text();
    }
    Some(Prepared { snap, files, hidden, opts })
}
/// How often the watcher asks git whether anything changed, in milliseconds; 0 when off.
/// One `git status` is a few milliseconds, and nothing is reread unless its fingerprint moves.
static REFRESH_EVERY: AtomicU64 = AtomicU64::new(1000);

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
    /// The repository map, shown in place of the diff while open.
    map: map_view::MapState,
    /// Where back and forward go.
    nav: nav::Nav,
    /// The shown commit's message after its subject.
    commit_body: String,
    words: Option<WordPick>,
    /// Projects the reader added, most recently opened first, and the switcher's state.
    projects: Vec<PathBuf>,
    project_open: bool,
    /// The folder picker is up: the native dialog runs on its own thread.
    picking: bool,
    /// A project is open (the welcome screen shows until one is).
    has_project: bool,
    /// The settings panel's section.
    settings_tab: usize,
    /// ⌘F was pressed: typing goes to the search (Rune can't move keyboard focus from code,
    /// so the app takes the keys itself) until Escape.
    searching: bool,
}

fn init() -> Collate {
    let settings = Settings::load();
    let repo = current_repo();
    let branches = repo.as_deref().map(Repo::branches).unwrap_or_default();
    let base = repo.as_deref().and_then(Repo::default_base);
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
        searching: false,
        words: None,
        projects: load_projects(),
        project_open: false,
        picking: false,
        has_project: current_repo().is_some(),
        settings_tab: 0,
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
        map: map_view::MapState::default(),
        nav: nav::Nav::default(),
        commit_body: String::new(),
    };
    s.viewed = load_viewed();
    if s.scope == Scope::History {
        s.load_history();
    }
    s.apply_settings(None);
    s
}

impl Collate {
    fn set_scope(&mut self, scope: Scope) {
        if self.scope != scope {
            self.scope = scope;
            self.loaded = false;
            self.error = None;
            if scope == Scope::Branch
                && let Some(r) = current_repo()
            {
                self.branches = r.branches();
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
        self.project_open = false;
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
        self.base = repo.default_base();
        tx.set_title(format!("{} — Collate", self.name));
        set_repo(Some(repo));
        watch_project_files(Some(root.clone()));
        remember_project(&mut self.projects, root);
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
        self.project_open = false;
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
            self.projects.remove(i);
            save_projects(&self.projects);
        }
    }

    /// Reads the recent commits, keeping the one shown if it's still there, else HEAD's.
    fn load_history(&mut self) {
        let Some(r) = current_repo() else { return };
        let (commits, head) = r.graph(self.graph_limit, None);
        self.web = r.web_url();
        self.show_history(commits, head);
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
        let keep = self.commit.clone().filter(|c| self.commit_ids.contains(c));
        if let Some(c) = keep.or_else(|| head.filter(|h| self.commit_ids.contains(h))).or_else(|| self.commit_ids.first().cloned()) {
            self.set_commit(&c);
        }
    }

    fn set_commit(&mut self, id: &str) {
        if self.commit.as_deref() == Some(id) {
            return;
        }
        self.commit = Some(id.to_owned());
        self.commit_body = current_repo().map(|r| r.message(id)).unwrap_or_default()
            .split_once('\n').map(|(_, rest)| rest.trim().to_owned()).unwrap_or_default();
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

    /// The current view's name, for the window's title.
    fn scope_title(&self) -> String {
        match self.scope {
            Scope::Uncommitted => "All Changes".into(),
            Scope::Staged => "Staged Changes".into(),
            Scope::Unstaged => "Unstaged Changes".into(),
            // As GitHub Desktop names it: the branch's whole change against its base.
            Scope::Branch => "Pull Request Preview".into(),
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
            .item(PopupItem::new("viewed", "Viewed").checked(self.is_viewed(f)));
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
            Some("discard") => s.ask_discard(f, None),
            _ => {}
        });
    }

    /// The toolbar's "⋯" menu: how the diff is drawn, and the app's less frequent commands.
    /// The title bar's branch menu: the branch as a pull request against its base, and the
    /// bases to choose from (local branches and `origin`'s).
    fn branch_menu(&mut self, tx: &mut Tx) {
        if let Some(r) = current_repo() {
            self.branches = r.branches();
        }
        let base = self.base.clone().unwrap_or_default();
        let mut menu = PopupMenu::new()
            .item(PopupItem::new("preview", if base.is_empty() { "Preview Pull Request".to_owned() } else { format!("Preview Pull Request into {base}") }).checked(self.scope == Scope::Branch && !self.map.open))
            .separator();
        let candidates: Vec<&String> = self.branches.iter().filter(|b| **b != self.branch && (!b.contains('/') || b.starts_with("origin/"))).take(20).collect();
        for b in candidates {
            menu = menu.item(PopupItem::new(format!("base:{b}"), format!("Compare against {b}")).checked(*b == base));
        }
        tx.popup_menu(menu, "branch", |s: &mut Collate, picked, _| match picked {
            Some("preview") => {
                s.map.open = false;
                s.set_scope(Scope::Branch);
            }
            Some(p) if p.starts_with("base:") => {
                s.set_base(&p["base:".len()..]);
                s.map.open = false;
                s.set_scope(Scope::Branch);
            }
            _ => {}
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
            Some("add") => s.add_project(),
            Some("keys") => s.show_help = true,
            Some("settings") => s.screen = Screen::Settings,
            _ => {}
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
        if msg.is_empty() || self.staged == 0 || self.committing {
            return;
        }
        let (Some(repo_handle), Some(repo)) = (HANDLE.get().cloned(), current_repo()) else { return };
        self.committing = true;
        // Hooks can take a while; the window stays live.
        std::thread::spawn(move || {
            let result = repo.commit(&msg);
            repo_handle.update(move |s, _| {
                s.committing = false;
                match result {
                    Ok(summary) => {
                        s.message = TextEdit::new("");
                        s.say(format!("Committed {summary}"), false);
                    }
                    Err(e) => s.say(format!("Commit failed: {e}"), true),
                }
                refresh();
            });
        });
    }

    /// Push settings into the diff and the watcher; `was` is the previous settings, to skip
    /// work that didn't change.
    fn apply_settings(&mut self, was: Option<&Settings>) {
        let s = &self.settings;
        self.diff.set_style(s.style());
        self.diff.set_mode(s.layout);
        self.diff.set_wrap(s.wrap);
        if was.is_none_or(|w| w.syntax != s.syntax) {
            self.diff.set_highlighter(if s.syntax { Highlighter::builtin() } else { Highlighter::new(|_: Option<&str>, _: &str| Vec::new()).key("off") });
        }
        REFRESH_EVERY.store(if s.auto_refresh { s.refresh_ms } else { 0 }, Ordering::Relaxed);
        *OPTS.lock().unwrap() = self.opts();
        if let Some(w) = was
            && (w.context != s.context || w.hide_low_signal != s.hide_low_signal || w.ignore_whitespace != s.ignore_whitespace || w.show_moved != s.show_moved)
        {
            self.reload();
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
        DiffOpts { context: s.context, ignore_whitespace: s.ignore_whitespace, moved: s.show_moved, hide_low_signal: s.hide_low_signal && !self.show_low_signal }
    }

    #[cfg(test)]
    fn apply_snapshot(&mut self, snap: Snapshot, tx: &mut Tx) {
        let p = prepare(snap, self.opts(), &mut DiffCache::new()).expect("a full snapshot");
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
        let info: Vec<FileInfo> = changes.iter().map(|c| FileInfo { staged: c.staged, in_base: c.old.is_some(), new_hash: c.new_hash }).collect();
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
        self.diff.set_files_folded(files, &folded);
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

    /// Flips one of the search's options and searches again.
    fn search_option(&mut self, flip: fn(&mut Collate)) {
        flip(self);
        self.hit = None;
        self.find();
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
    use std::hash::{Hash, Hasher};
    let repo = current_repo()?;
    let root = &repo.root;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(format!("Library/Application Support/Collate/viewed/{:016x}", h.finish())))
}

/// `~/Library/Application Support/Collate/projects`: one repository path per line, most
/// recently opened first.
fn projects_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/Collate/projects"))
}

fn load_projects() -> Vec<PathBuf> {
    let text = projects_file().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect()
}

fn save_projects(projects: &[PathBuf]) {
    // Tests open throwaway repositories; they leave the reader's list alone.
    if cfg!(test) {
        return;
    }
    let Some(path) = projects_file() else { return };
    let text: String = projects.iter().map(|p| format!("{}\n", p.display())).collect();
    // Losing the list isn't worth interrupting a review over; the next open tries again.
    let _ = path.parent().map(std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text).ok());
}

/// Puts `root` first in the list (once), keeping it to the 30 most recent.
fn remember_project(projects: &mut Vec<PathBuf>, root: PathBuf) {
    projects.retain(|p| *p != root);
    projects.insert(0, root);
    projects.truncate(30);
    save_projects(projects);
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

fn file_diff(c: &Change, context: u32, ignore_whitespace: bool) -> FileDiff {
    let old_path = c.old.as_ref().map(|_| c.old_path.clone().unwrap_or_else(|| c.path.clone()));
    let new_path = c.new.as_ref().map(|_| c.path.clone());
    match (&c.old, &c.new) {
        (None | Some(Blob::Text(_)), None | Some(Blob::Text(_))) => {
            fn t(b: &Option<Blob>) -> &str {
                match b {
                    Some(Blob::Text(t)) => t,
                    _ => "",
                }
            }
            FileDiff::between_opts(old_path, new_path, t(&c.old), t(&c.new), context, ignore_whitespace)
        }
        _ => {
            // Binary or too large on either side: let the patch parser make a binary entry.
            let a = old_path.as_ref().map_or("/dev/null".to_owned(), |p| format!("a/{p}"));
            let b = new_path.as_ref().map_or("/dev/null".to_owned(), |p| format!("b/{p}"));
            let mut patch = format!("diff --git a/{} b/{}\n", old_path.as_ref().unwrap_or(&c.path), c.path);
            if old_path.is_none() {
                patch.push_str("new file mode 100644\n");
            }
            if new_path.is_none() {
                patch.push_str("deleted file mode 100644\n");
            }
            patch.push_str(&format!("Binary files {a} and {b} differ\n"));
            parse(&patch).into_iter().next().unwrap_or_else(|| FileDiff::between(old_path, new_path, "", ""))
        }
    }
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
    Kit::neutral(s.settings.dark(cx.system_dark()))
}

fn view(s: &Collate, cx: &mut Cx<Collate>) -> El {
    let k = kit(s, cx);
    let mut layer = Vec::new();
    // Settings float over the review, which stays where it was underneath.
    let base = review(s, cx, &k, &mut layer);
    if s.screen == Screen::Settings {
        settings_panel(s, &k, &mut layer);
    }
    rune_kit::root(base, layer)
}

/// The project menu: every added project, then "Add local project…".
fn project_switcher(s: &Collate, k: &Kit, layer: &mut Vec<El>) -> El {
    let mut options = project_labels(&s.projects);
    let current = current_repo().and_then(|r| s.projects.iter().position(|p| *p == r.root)).map(|i| options[i].clone());
    options.push(ADD_PROJECT.to_owned());
    Select::new("project", &options)
        .current(current.as_deref())
        .placeholder("Open a project")
        .label("Project")
        .width(240.0)
        .open(s.project_open)
        .on_toggle(|s: &mut Collate| s.project_open = !s.project_open)
        .on_close(|s: &mut Collate| s.project_open = false)
        .on_choose(|s: &mut Collate, v: &str, tx: &mut Tx| {
            if v == ADD_PROJECT {
                s.add_project();
            } else if let Some(i) = project_labels(&s.projects).iter().position(|l| l == v) {
                let p = s.projects[i].clone();
                s.open_project(&p, tx);
            }
        })
        .build(k, layer)
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
    let r = row().gap(2.0).items_center();
    if s.scope.read_only() {
        return r;
    }
    if s.settings.ignore_whitespace {
        // Hunks drawn without whitespace don't match the file byte for byte.
        return r.child(text("Whitespace hidden: stage whole files").font(k.ty.text_small).color(k.c.text_faint));
    }
    let staged = s.info.get(f).map_or(Staged::No, |i| i.staged);
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
            .label(format!("Branch {}: preview pull request, choose base", s.branch))
            .help("Preview pull request, choose the base branch")
            .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.branch_menu(tx));
        let base = s.base.as_deref().map(|b| b.strip_prefix("origin/").unwrap_or(b).to_owned()).filter(|b| *b != s.branch);
        if let Some(b) = base {
            r = r.child(part("base-branch", &MERGE_ICON, b, false)).child(text("/".to_owned()).font(k.ty.ui).color(k.c.text_faint));
        }
        r.child(part("current-branch", &BRANCH_ICON, s.branch.clone(), true))
            .child(icon(k.glyph.chevron_down, 12.0).color(k.c.icon_faint))
    };
    // The switcher's label, not its hover box, starts on the content's 16-point edge below
    // (the toolbar pads 12; the trigger pads its label 8).
    // With the sidebar closed the toolbar starts just past its toggle, where back and forward
    // sit: it starts past them instead.
    let past_arrows = if s.sidebar.is_open() { 0.0 } else { NAV_W + 8.0 };
    let mut leading = row().key("toolbar-leading").gap(10.0).items_center().min_w(0.0).ml(EDGE - 12.0 - 8.0 + past_arrows).child(project_switcher(s, k, layer)).child(branch);
    // Long branch names truncate 16 points before the centred title rather than run under it.
    let title_x = rune::memo::layout_with("title", |r| (r[0] * 2.0).round() / 2.0);
    let lead_x = rune::memo::layout_with("toolbar-leading", |r| (r[0] * 2.0).round() / 2.0);
    if let (Some(t), Some(l)) = (title_x, lead_x) {
        leading = leading.max_w((t - l - 16.0).max(120.0));
    }
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
    let more = IconButton::new(&MORE_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .label("View options and more")
        .help("View options, shortcuts, settings")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.more_menu(tx))
        .build(k)
        .key("more");
    let panel_w = if s.inspector.is_open() && !no_project { s.inspector.width() } else { 0.0 };
    // The title's slot is the bar less 96 points each side.
    let bar = rune::memo::layout_with("toolbar", |r| r[2]).unwrap_or(f32::MAX);
    let title_offset = if bar - 192.0 - panel_w >= 140.0 { panel_w } else { 0.0 };
    let toolbar = Toolbar::new()
        .leading(leading)
        // Centred over the diff it names, not over the diff and the staging panel together;
        // over the whole bar when that leaves the title too little room.
        .center(
            row().w_full().pr(title_offset).center().child(
                text(if no_project { "Collate".to_owned() } else if s.map.open { "Repository Map".to_owned() } else { s.scope_title() }).key("title").font(k.ty.ui_medium).color(k.c.text_base).truncate(),
            ),
        )
        .trailing(row().gap(6.0).items_center().child(modes.build(k)).child(panel).child(more));

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
        let (fk, hk) = (k.clone(), k.clone());
        let diff = DiffView::new(|s: &Collate| &s.diff, |s: &mut Collate| &mut s.diff)
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
    let body = match status_bar(s, k) {
        Some(bar) => col().key("body").w_full().grow(1.0).min_h(0.0).child(body).child(bar),
        None => body,
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
            ("o", "Open the file in your editor"),
            ("⌘F", "Search the diff: type, ⏎ next match, ⇧⏎ previous, esc done"),
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

/// The back and forward buttons' width together.
const NAV_W: f32 = 24.0 * 2.0 + 2.0;

/// The leading sidebar: the places to look, with how much is in each. Working Copy is what
/// isn't committed; Review is what is.
fn views_sidebar(s: &Collate, k: &Kit) -> El {
    let place = |key: &str, label: &str, scope: Scope, badge: Option<usize>, detail: Option<String>| {
        let mut r = SidebarRow::new(label.to_owned()).selected(s.scope == scope && !s.map.open).on_select(move |s: &mut Collate, _: &mut Tx| {
            s.map.open = false;
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
        place("view-all", "All Changes", Scope::Uncommitted, Some(s.total), None),
        place("view-unstaged", "Unstaged", Scope::Unstaged, Some(s.unstaged), None),
        place("view-staged", "Staged", Scope::Staged, Some(s.staged_shown), None),
    ]);
    let review = SidebarSection::new("Review").rows([
        place("view-history", "Git Graph", Scope::History, None, None),
        // The whole repository, with this view's changes on it.
        SidebarRow::new("Map").selected(s.map.open).on_select(|s: &mut Collate, _: &mut Tx| s.open_map()).build(k).key("view-map"),
    ]);
    let apart = col().key("sections-apart").w_full().h(12.0).a11y_hidden();
    // The same top inset as the other columns. Rune's section heading adds 4 pt above its own
    // 24 pt row, so 2 less here centres "Working Copy" in the same 28 pt band.
    col().key("sidebar").w_full().h_full().pt(COLUMN_TOP - 2.0).child(Sidebar::new().section(working).child(apart).section(review).build(k))
}

/// The staging panel's inset, shared by its filter, list titles, rows and commit box.
const PANEL_INSET: f32 = 12.0;
/// The one top inset every column uses: each first row (the sidebar's "Working Copy", the
/// diff's summary, the panel's filter) is a 28 pt band this far below the card's top edge.
const COLUMN_TOP: f32 = 6.0;

/// A list's bulk action: its label and what it does.
type Bulk<'a> = (&'a str, fn(&mut Collate));

/// One list of the staging panel: a header with its count and its bulk action, then the
/// files. A row's trailing button stages or unstages that file at once.
fn file_section(k: &Kit, key: &str, title: &str, count: usize, bulk: Option<Bulk>, list: El, empty: &str) -> El {
    let mut head = row()
        .key(format!("{key}-head"))
        .w_full()
        // As tall as a sidebar row, so the first title is level with the sidebar's first row.
        .h(28.0)
        .px(PANEL_INSET)
        .gap(4.0)
        .items_center()
        // Set as the sidebar sets "Working Copy", so the two columns' titles read as one kind.
        .child(text(title.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted))
        .child(text(count.to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint))
        .child(row().grow(1.0));
    if let Some((label, f)) = bulk.filter(|_| count > 0) {
        // A ghost button pads its label; pull it out by that much so the label, not the
        // invisible box, ends on the panel's edge with the filter above it.
        head = head.child(Button::new(label.to_owned()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).on_press(f).build(k).key(format!("{key}-bulk")).mr(-GHOST_PAD));
    }
    // An empty list says so in one line and gives its space to the list that has files.
    if count == 0 {
        return col().key(key.to_owned()).w_full().child(head).child(row().key(format!("{key}-empty")).w_full().h(28.0).px(PANEL_INSET).items_center().child(text(empty.to_owned()).font(k.ty.text_small).color(k.c.text_faint)));
    }
    col().key(key.to_owned()).w_full().grow(1.0).min_h(0.0).child(head).child(list)
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
    let mut p = col().key("staging").w_full().h_full().pt(COLUMN_TOP).gap(4.0);
    // History leads with the chosen commit; its files follow, as in Zed's commit view.
    if s.scope == Scope::History
        && let Some(card) = commit_card(s, k)
    {
        p = p.child(card);
    }
    // The first list title follows the filter with no gap, as the sidebar's first row follows
    // its section title: both second rows then sit on one line too.
    p = p.child(row().key("filter-row").w_full().px(PANEL_INSET).mb(-4.0).child(filter.w_full()));
    p = match s.scope {
        Scope::Uncommitted => p
            .child(file_section(k, "unstaged", "Unstaged Files", unstaged_n, Some(("Stage All", Collate::stage_all)), file_list(s, k, false, Some(true)), "Nothing unstaged"))
            .child(col().key("between").w_full().h(1.0).fill(k.c.border_weak))
            .child(file_section(k, "staged", "Staged Files", staged_n, Some(("Unstage All", Collate::unstage_all)), file_list(s, k, true, Some(false)), "Nothing staged yet"))
            .child(commit_box(s, k)),
        Scope::Unstaged => p.child(file_section(k, "unstaged", "Unstaged Files", all, Some(("Stage All", Collate::stage_all)), file_list(s, k, false, Some(true)), "Nothing unstaged")).child(commit_box(s, k)),
        Scope::Staged => p.child(file_section(k, "staged", "Staged Files", all, Some(("Unstage All", Collate::unstage_all)), file_list(s, k, true, Some(false)), "Nothing staged")).child(commit_box(s, k)),
        Scope::Branch | Scope::History => p.child(file_section(k, "changed", "Changed Files", all, None, file_list(s, k, false, None), "No files changed")),
    };
    p
}

/// The commit composer at the foot of the staging panel: a message and the screen's one
/// prominent action, which commits what's staged.
fn commit_box(s: &Collate, k: &Kit) -> El {
    let ready = s.staged > 0 && !s.message.text.trim().is_empty() && !s.committing;
    let field = TextInput::new(&s.message)
        .placeholder(if s.staged == 0 { "Stage files to write a commit".to_owned() } else { "Summary".to_owned() })
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
    let label = match (s.committing, s.staged) {
        (true, _) => "Committing…".to_owned(),
        (_, 0) => "Commit".to_owned(),
        (_, 1) => "Commit 1 File".to_owned(),
        (_, n) => format!("Commit {n} Files"),
    };
    let button = Button::new(label).variant(ButtonVariant::Contrast).shortcut("⌘↩").disabled(!ready).help(if s.staged == 0 { "Stage files first" } else { "Commit what's staged (⌘↩)" }).on_press(|s: &mut Collate| s.commit()).build(k).key("commit").w_full();
    col()
        .key("commit-box")
        .w_full()
        .px(PANEL_INSET)
        .pt(10.0)
        .pb(12.0)
        .gap(8.0)
        .child(col().key("rule").w_full().h(1.0).fill(k.c.border_weak))
        .child(text("Commit").font(k.ty.text_small).color(k.c.text_base))
        .child(field)
        .child(button)
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
        Scope::History => {
            let at = s.commit.as_ref().and_then(|c| s.commit_ids.iter().position(|x| x == c));
            let older = Button::new("‹ Older").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(at.is_none_or(|i| i + 1 >= s.commit_ids.len())).help("Older commit (<)").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.step_commit(1, tx)).build(k).key("older");
            let newer = Button::new("Newer ›").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(at.is_none_or(|i| i == 0)).help("Newer commit (>)").on_press_tx(|s: &mut Collate, tx: &mut Tx| s.step_commit(-1, tx)).build(k).key("newer");
            Some(row().key("compare").gap(6.0).items_center().child(older).child(newer))
        }
        _ => None,
    }
}

/// The graph's geometry: a row's height and the spacing of its lanes.
const GRAPH_ROW: f32 = 28.0;
const LANE: f32 = 16.0;
/// The fixed columns after the description: author, date and short hash.
const AUTHOR_W: f32 = 112.0;
const DATE_W: f32 = 126.0;
const HASH_W: f32 = 64.0;

/// A lane's colour, as Git Graph cycles them: each branch line keeps its lane's colour.
fn lane_color(lane: usize) -> rune::Color {
    const LANES: [&str; 8] = ["#4c9df0", "#e0609e", "#3fbf6a", "#e09a3c", "#a678e8", "#e5584f", "#36bfb5", "#b8c440"];
    rune::color::hex(LANES[lane % LANES.len()]).unwrap()
}

fn graph_width(lanes: usize) -> f32 {
    lanes.max(1) as f32 * LANE + 8.0
}

// Keep topology order: sorting by author or date independently would break graph edges.
fn history_graph(s: &Collate, k: &Kit) -> El {
    let width = graph_width(s.graph_lanes);
    let label = |t: &str| text(t.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_faint);
    // Outside the scroll, so the column names stay put while the rows move under them.
    let header = row().key("graph-header").w_full().h(24.0).px(EDGE).gap(12.0).items_center()
        .child(label("Graph").w(width))
        .child(label("Description").grow(1.0).min_w(0.0))
        .child(label("Author").w(AUTHOR_W))
        .child(label("Date").w(DATE_W))
        .child(label("Commit").w(HASH_W));
    let kit = k.clone();
    let rows = col().key("graph-rows").growth(rune::GrowthSpec::default()).w_full()
        .virtual_rows(s.graph_commits.len(), GRAPH_ROW,
            |s: &Collate, i| format!("graph-{}", s.graph_commits[i].id).into(),
            move |s: &Collate, i| graph_row(s, &kit, i, width))
        .virtual_rows_version(s.graph_version)
        // Within a hundred rows of the end, the next page starts loading.
        .on_visible_rows(|s: &mut Collate, rows| {
            if rows.end + 100 >= s.graph_commits.len() && !s.graph_loading && s.commit_ids.len() >= s.graph_limit {
                s.load_more_history();
                return true;
            }
            false
        });
    let refresh = IconButton::new(&REFRESH_ICON)
        .variant(IconButtonVariant::GhostMuted)
        .label("Refresh history")
        .help("Read the history again (⌘R)")
        .on_press(|s: &mut Collate| { s.load_history(); refresh(); })
        .build(k)
        .key("graph-refresh");
    let shown = s.commit_ids.len();
    let title = row().key("graph-title").w_full().h(36.0).px(EDGE).gap(8.0).items_center()
        .child(text("Commits").font(k.ty.text_small_medium).color(k.c.text_base))
        .child(text(format!("{shown}{} on all branches{}", if shown >= s.graph_limit { "+" } else { "" }, if s.graph_loading { " - loading more…" } else { "" })).font(k.ty.text_small).color(k.c.text_muted).grow(1.0).truncate().min_w(0.0))
        .child(refresh.mr(-4.0));
    col().key("history-graph").w_full().grow(1.0).min_h(0.0)
        .child(title)
        .child(header)
        .child(col().w_full().h(1.0).fill(k.c.border_weak))
        .child(col().key("graph-scroll").w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(rows))
}

/// A round badge with someone's initials, tinted by their name so each author keeps a colour.
/// An author's GitHub picture, else their initials on a colour of their own.
fn avatar(k: &Kit, s: &Collate, c: &graph::Commit, size: f32) -> El {
    if let Some(img) = s.avatars.get(&c.email) {
        return row().size(size, size).radius(size / 2.0).border(1.0, k.c.border_weak).pixels_fit(img.pixels(), rune::SurfaceFit::Fill).a11y_hidden();
    }
    let name = c.author.as_str();
    let initials: String = name.split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-')
        .filter_map(|w| w.chars().next()).take(if size < 24.0 { 1 } else { 2 }).collect::<String>().to_uppercase();
    let hue = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    let tint = lane_color(hue);
    row().size(size, size).center().radius(size / 2.0).fill(k.alpha(tint, 0.22)).border(1.0, k.alpha(tint, 0.6))
        .child(text(if initials.is_empty() { "?".to_owned() } else { initials }).font(if size >= 40.0 { k.ty.text_small_medium } else if size >= 24.0 { k.ty.text_xsmall_medium } else { rune::TextStyle { size: 9.0, line_height: 12.0, ..k.ty.text_xsmall_medium } }).color(k.c.text_base))
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// The host's name for a web link: "GitHub" for github.com, else the host.
fn web_name(url: &str) -> String {
    let host = url.trim_start_matches("https://").split('/').next().unwrap_or_default();
    match host {
        "github.com" => "GitHub".to_owned(),
        "gitlab.com" => "GitLab".to_owned(),
        "bitbucket.org" => "Bitbucket".to_owned(),
        h => h.to_owned(),
    }
}

/// The chosen commit at the head of History's side panel, as Zed shows it: who and when, its
/// branches, email, full hash, a link to the host, and the whole message.
fn commit_card(s: &Collate, k: &Kit) -> Option<El> {
    let id = s.commit.as_deref()?;
    let c = s.graph_commits.iter().find(|c| c.id == id)?;
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    // Named fields on one column edge, so the values line up.
    let line = |key: &str, name: &str, body: El| row().key(key.to_owned()).w_full().gap(8.0).items_center()
        .child(text(name.to_owned()).w(52.0).font(k.ty.text_small).color(k.c.text_faint)).child(body);
    let mut card = col().key("commit-card").w_full().px(PANEL_INSET).pt(4.0).pb(10.0).gap(8.0)
        .child(col().w_full().items_center().gap(4.0)
            .child(avatar(k, s, c, 44.0))
            .child(text(c.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().min_w(0.0))
            .child(text(c.date.clone()).font(k.ty.text_small).color(k.c.text_muted)));
    let labels = ref_labels(&c.refs);
    if !labels.is_empty() {
        // Wrapped onto as many lines as the names need, each name whole unless it alone is too wide.
        let mut refs = row().key("commit-refs").w_full().gap(4.0).justify_center().flex_wrap();
        for (label, kind, _) in labels {
            let tag = kind == graph::RefKind::Tag;
            refs = refs.child(text(label).font(mono).color(k.c.text_base).truncate().min_w(0.0).px(6.0).h(20.0).radius(4.0)
                .border(1.0, if tag { k.c.border_weak } else { k.alpha(lane_color(s.graph_rows.get(s.graph_commits.iter().position(|x| x.id == c.id).unwrap_or(0)).map_or(0, |r| r.col)), 0.8) }));
        }
        card = card.child(refs);
    }
    let sha = c.id.clone();
    card = card
        .child(line("commit-email", "Email", text(c.email.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0)))
        .child(line("commit-sha", "Commit", text(c.id.clone()).font(mono).color(k.c.text_muted).truncate().min_w(0.0).grow(1.0)
            .cursor(Cursor::Pointer).role("button").label("Copy the commit's full hash").help("Copy hash")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| { tx.copy(sha.clone()); s.say("Copied the commit hash", false); })));
    if let Some(web) = &s.web {
        let url = format!("{web}/commit/{}", c.id);
        card = card.child(line("commit-web", "", text(format!("View on {}", web_name(web))).font(k.ty.text_small).color(k.c.text_accent)
            .cursor(Cursor::Pointer).role("link").label(format!("Open {url}")).on_press(move |_: &mut Collate| open_url(&url))));
    }
    if c.parents.len() > 1 {
        let parents = c.parents.iter().map(|p| p.chars().take(8).collect::<String>()).collect::<Vec<_>>().join("  ");
        card = card.child(line("commit-parents", "Merge", text(parents).font(mono).color(k.c.text_muted)));
    }
    let message = if s.commit_body.is_empty() { c.subject.clone() } else { format!("{}\n\n{}", c.subject, s.commit_body) };
    let (adds, dels) = s.diff.totals();
    card = card
        .child(col().w_full().h(1.0).fill(k.c.border_weak))
        .child(text("Message").font(k.ty.text_xsmall_medium).color(k.c.text_faint))
        .child(text(message).font(k.ty.text_small).color(k.c.text_base).wrap().w_full().max_lines(14))
        .child(row().key("commit-stat").w_full().gap(6.0).items_center().pt(4.0)
            .child(text(if s.loaded { "Lines changed" } else { "Reading files…" }.to_owned()).font(k.ty.text_small).color(k.c.text_muted).grow(1.0))
            .child(text(format!("+{adds}")).font(mono).color(k.c.success.fg))
            .child(text(format!("−{dels}")).font(mono).color(k.c.danger.fg)));
    Some(card)
}

/// Above a commit's diff: the way back to the list, who made it and when, its subject, and
/// the hash to copy.
fn commit_header(s: &Collate, k: &Kit) -> El {
    let back = Button::new("‹ All Commits").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .help("Back to the commit list (Esc)")
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.close_commit_diff(tx)).build(k).key("history-back");
    let mut r = row().key("commit-header").w_full().px(EDGE).py(10.0).gap(12.0).items_center().child(back.ml(-GHOST_PAD));
    if let Some(c) = s.commit.as_deref().and_then(|id| s.graph_commits.iter().find(|c| c.id == id)) {
        let sha = c.id.clone();
        let copy = Button::new("Copy SHA").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .help("Copy the commit's full hash")
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| { tx.copy(sha.clone()); s.say("Copied the commit hash", false); }).build(k).key("copy-sha");
        r = r.child(avatar(k, s, c, 32.0))
            .child(col().grow(1.0).min_w(0.0).gap(2.0)
                .child(row().gap(8.0).items_center()
                    .child(text(c.author.clone()).font(k.ty.text_small_medium).color(k.c.text_base))
                    .child(text(format!("{} - {}", c.date, c.email)).font(k.ty.text_small).color(k.c.text_muted).truncate().min_w(0.0)))
                .child(text(c.subject.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0)))
            .child(copy.mr(-GHOST_PAD));
    }
    col().w_full().child(r).child(col().w_full().h(1.0).fill(k.c.border_weak))
}

/// Points on the S-curve from lane `a` in one row's middle to lane `b` in the next row's, cut
/// at the row boundary: the upper half (`upper`) in the first row's coordinates, else the lower
/// half in the second's. Vertical where it meets each dot, as Git Graph draws its branches.
fn edge_half(xa: f32, xb: f32, upper: bool) -> rune::Path {
    let (h, c) = (GRAPH_ROW, GRAPH_ROW / 2.0);
    let mid = [(xa + xb) / 2.0, if upper { h } else { 0.0 }];
    if upper {
        rune::Path::new().move_to([xa, c]).cubic_to([xa, c + h / 4.0], [(3.0 * xa + xb) / 4.0, c + 3.0 * h / 8.0], mid)
    } else {
        rune::Path::new().move_to(mid).cubic_to([(xa + 3.0 * xb) / 4.0, h / 8.0], [xb, h / 4.0], [xb, c])
    }
}

fn graph_row(s: &Collate, k: &Kit, i: usize, width: f32) -> El {
    use rune::{Shape, LineCap, canvas};
    let x = |lane: usize| 4.0 + LANE / 2.0 + lane as f32 * LANE;
    let r = &s.graph_rows[i];
    let c = &s.graph_commits[i];
    let uncommitted = c.id == graph::UNCOMMITTED;
    let head = s.head.as_deref() == Some(c.id.as_str());
    let faint = k.c.text_faint;
    // A line belongs to the branch that opens or closes on it: the lane further right.
    let edge_color = |from_uncommitted: bool, a: usize, b: usize| if from_uncommitted { faint } else { lane_color(a.max(b)) };
    let mut shape = Shape::new();
    if i > 0 {
        let above = s.graph_commits[i - 1].id == graph::UNCOMMITTED;
        for &(a, b) in &s.graph_rows[i - 1].down {
            let color = edge_color(above, a, b);
            shape = if a == b {
                shape.line([x(a), 0.0], [x(a), GRAPH_ROW / 2.0], 2.0, LineCap::Butt, color)
            } else {
                shape.stroke_path(&edge_half(x(a), x(b), false), 2.0, LineCap::Butt, color)
            };
        }
    }
    for &(a, b) in &r.down {
        let color = edge_color(uncommitted, a, b);
        shape = if a == b {
            shape.line([x(a), GRAPH_ROW / 2.0], [x(a), GRAPH_ROW], 2.0, LineCap::Butt, color)
        } else {
            shape.stroke_path(&edge_half(x(a), x(b), true), 2.0, LineCap::Butt, color)
        };
    }
    let ring = |radius: f32| -> Vec<[f32; 2]> {
        (0..=32).map(|n| { let t = n as f32 * std::f32::consts::TAU / 32.0; [x(r.col) + t.cos() * radius, GRAPH_ROW / 2.0 + t.sin() * radius] }).collect()
    };
    let dot = if uncommitted { faint } else { lane_color(r.col) };
    // HEAD and the uncommitted changes are hollow, so where you are reads at a glance.
    shape = if head || uncommitted {
        shape.fill(ring(5.0), k.c.bg_base).stroke(ring(4.0), 2.0, dot)
    } else {
        shape.fill(ring(4.0), dot)
    };
    let selected = s.commit.as_deref() == Some(c.id.as_str());
    let mut description = row().gap(6.0).items_center().grow(1.0).min_w(0.0);
    // Two labels at most, each cut short if long, so the subject keeps its room; the side
    // panel lists them all.
    let labels = ref_labels(&c.refs);
    let more = labels.len().saturating_sub(2);
    for (label, kind, at_head) in labels.into_iter().take(2) {
        let tint = lane_color(r.col);
        let pill = text(label).font(if at_head { k.ty.text_xsmall_medium } else { k.ty.text_xsmall }).truncate().max_w(220.0).px(6.0).h(18.0).radius(4.0);
        description = description.child(match kind {
            graph::RefKind::Tag => pill.color(k.c.text_base).border(1.0, k.c.border_weak).fill(k.alpha(k.c.text_base, 0.06)),
            graph::RefKind::Remote => pill.color(k.c.text_muted).border(1.0, k.alpha(tint, 0.5)),
            graph::RefKind::Branch => pill.color(k.c.text_base).border(1.0, k.alpha(tint, if at_head { 1.0 } else { 0.6 })).fill(k.alpha(tint, 0.18)),
        });
    }
    if more > 0 {
        description = description.child(text(format!("+{more}")).font(k.ty.text_xsmall).color(k.c.text_muted).px(6.0).h(18.0).radius(4.0).border(1.0, k.c.border_weak)
            .help(format!("{more} more on this commit: listed in the side panel")));
    }
    let subject = if uncommitted { format!("Uncommitted Changes ({})", s.total) } else { c.subject.clone() };
    description = description.child(text(subject).font(if head { k.ty.text_small_medium } else { k.ty.text_small })
        .color(if uncommitted { k.c.text_muted } else { k.c.text_base }).truncate().min_w(0.0).grow(1.0));
    let muted = |t: String, w: f32| text(t).w(w).truncate().font(k.ty.text_small).color(k.c.text_muted);
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let fill = if selected { k.alpha(k.c.border_focus, 0.22) } else { k.alpha(k.c.text_base, 0.0) };
    let id = c.id.clone();
    row().key(format!("graph-{}", c.id)).w_full().h(GRAPH_ROW).px(EDGE).gap(12.0).items_center()
        .fill(fill).hover(if selected { fill } else { k.alpha(k.c.text_base, 0.06) }).cursor(Cursor::Pointer).role("button")
        .label(if uncommitted { "Uncommitted changes: open All Changes".to_owned() } else { format!("{}{}, {}, {}", if selected { "Selected: " } else { "" }, c.subject, c.author, c.id) })
        .on_press(move |s: &mut Collate| if id == graph::UNCOMMITTED { s.set_scope(Scope::Uncommitted) } else { s.set_commit(&id) })
        .on_double_press(move |s: &mut Collate| if !uncommitted { s.history_diff = true })
        .child(canvas(shape).w(width).h(GRAPH_ROW)).child(description)
        .child(if uncommitted { muted(String::new(), AUTHOR_W) } else {
            row().w(AUTHOR_W).gap(6.0).items_center().child(avatar(k, s, c, 16.0)).child(muted(c.author.clone(), AUTHOR_W - 22.0).min_w(0.0).shrink())
        })
        .child(muted(c.date.clone(), DATE_W))
        .child(text(if uncommitted { String::new() } else { c.id.chars().take(8).collect() }).w(HASH_W).font(mono).color(k.c.text_muted))
}

/// A commit's labels, a local branch and its remote counterparts shown as one
/// (`main - origin`), as Git Graph does: (text, kind, whether HEAD is on it).
fn ref_labels(refs: &[graph::Ref]) -> Vec<(String, graph::RefKind, bool)> {
    let mut out = Vec::new();
    let mut merged = vec![false; refs.len()];
    for (i, r) in refs.iter().enumerate() {
        match r.kind {
            graph::RefKind::Branch => {
                let mut label = r.name.clone();
                for (j, o) in refs.iter().enumerate() {
                    if o.kind == graph::RefKind::Remote
                        && let Some((remote, name)) = o.name.split_once('/')
                        && name == r.name
                    {
                        label.push_str(&format!(" - {remote}"));
                        merged[j] = true;
                    }
                }
                out.push((label, r.kind, r.head));
            }
            graph::RefKind::Remote if !merged[i] => out.push((r.name.clone(), r.kind, false)),
            graph::RefKind::Remote => {}
            graph::RefKind::Tag => out.push((r.name.clone(), r.kind, false)),
        }
    }
    out
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
    let hits = match (s.hits.len(), s.hit) {
        _ if s.search_error.is_some() => "Bad pattern".to_owned(),
        (0, _) if !s.search.text.trim().is_empty() => "No matches".to_owned(),
        (0, _) => String::new(),
        (n, Some(i)) => format!("{} of {n}", i + 1),
        (n, None) => format!("{n} matches"),
    };
    let mut search = TextInput::new(&s.search).placeholder("Search the diff").leading_icon(k.glyph.search);
    if !s.search.text.is_empty() {
        search = search.clear_button();
    }
    let search = search
        .width(Some(220.0))
        .on_edit(|s: &mut Collate| &mut s.search)
        .on_change(|s: &mut Collate, _| {
            s.hit = None;
            s.find();
        })
        .on_key(|s: &mut Collate, ev: &KeyEvent, tx: &mut Tx| {
            if ev.key == Key::Enter {
                s.next_hit(!ev.mods.shift, tx);
                return true;
            }
            false
        })
        .build(k)
        .key("search");
    // While ⌘F has the keys, the field looks focused.
    let search = if s.searching { row().key("ring").radius(8.0).border(2.0, k.c.border_focus).child(search) } else { search };
    // The search's options, as an editor's find bar has them.
    let option = |key: &str, glyph: &str, name: &str, keys: &str, on: bool, flip: fn(&mut Collate)| {
        text(glyph.to_owned())
            .key(key.to_owned())
            .font(rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono })
            .color(if on { k.c.text_base } else { k.c.text_faint })
            .px(5.0)
            .h(20.0)
            .radius(k.radius.sm)
            .fill(if on { k.alpha(k.c.text_base, 0.12) } else { k.alpha(k.c.text_base, 0.0) })
            .hover(k.alpha(k.c.text_base, 0.08))
            .cursor(Cursor::Pointer)
            .role("checkbox")
            .label(format!("{name}{}", if on { ", on" } else { ", off" }))
            .help(format!("{name} ({keys})"))
            .on_press(move |s: &mut Collate| s.search_option(flip))
    };
    let options = row()
        .key("search-options")
        .gap(2.0)
        .items_center()
        .child(option("find-case", "Aa", "Match case", "⌥⌘C", s.find_case, |s| s.find_case ^= true))
        .child(option("find-word", "ab", "Whole words", "⌥⌘W", s.find_word, |s| s.find_word ^= true))
        .child(option("find-regex", ".*", "Regular expression", "⌥⌘R", s.find_regex, |s| s.find_regex ^= true));
    let hits = match &s.search_error {
        Some(e) => text(hits).key("hits").font(k.ty.text_small).color(k.c.danger.fg).help(e.clone()),
        None => text(hits).key("hits").font(k.ty.text_small).color(k.c.text_muted),
    };
    let top = row()
        .key("totals")
        .w_full()
        .gap(12.0)
        .items_center()
        .child(what)
        .child(row().grow(1.0))
        .child(hits)
        .child(search)
        .child(options)
        .child(text(format!("{viewed}/{n} viewed")).key("viewed-count").font(k.ty.text_small).color(k.c.text_muted));
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

/// Feedback after an action, at the bottom of the diff: what happened, and Undo for a
/// discard. It goes by itself after a few seconds.
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

/// The width every settings control is right-aligned in, so each description wraps at the
/// same measure and rows of the same kind come out the same height.
const CONTROL_W: f32 = 272.0;

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
    row().key(key.to_owned()).w_full().py(14.0).gap(24.0).items_center().child(words).child(row().w(CONTROL_W).justify_end().items_center().child(control))
}

fn pick<T: Copy + PartialEq + 'static>(k: &Kit, labels: &[&str], values: &'static [T], current: T, set: fn(&mut Settings, T)) -> El {
    SegmentedControl::new(labels, values.iter().position(|v| *v == current))
        .fit()
        .on_pick(move |s: &mut Collate, i, _: &mut Tx| s.configure(|c| set(c, values[i])))
        .build(k)
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

const SETTINGS_TABS: [&str; 5] = ["Appearance", "Diff", "Review", "Projects", "Refresh"];

/// Settings as a floating panel over the review: sections down the left, the chosen one's
/// settings on the right, on one surface. Esc, the close button or a click outside put it
/// away; every change applies and saves at once, so there's nothing to confirm.
fn settings_panel(s: &Collate, k: &Kit, layer: &mut Vec<El>) {
    let st = &s.settings;
    static THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
    static DENSITIES: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];
    static LAYOUTS: [DiffMode; 2] = [DiffMode::Unified, DiffMode::Split];
    static NUMBERS: [bool; 2] = [true, false];
    let tab = s.settings_tab.min(SETTINGS_TABS.len() - 1);
    let rows: Vec<El> = match tab {
        0 => vec![
            setting(k, "theme", "Theme", &[], "", pick(k, &["System", "Light", "Dark"], &THEMES, st.theme, |c, v| c.theme = v)),
            setting(k, "font", "Code size", &[], "In points, for code and line numbers", pick(k, &["11", "12", "13", "14", "16"], &FONT_SIZES, st.font_size, |c, v| c.font_size = v)),
            setting(k, "density", "Line spacing", &[], "", pick(k, &["Compact", "Comfortable", "Spacious"], &DENSITIES, st.density, |c, v| c.density = v)),
        ],
        1 => vec![
            setting(k, "layout", "Layout", &["S"], "Split shows old and new side by side", pick(k, &["Unified", "Split"], &LAYOUTS, st.layout, |c, v| c.layout = v)),
            setting(k, "numbers", "Line numbers", &[], "Unified view: new numbers only, or both", pick(k, &["One column", "Old and new"], &NUMBERS, st.one_number, |c, v| c.one_number = v)),
            setting(k, "context", "Context lines", &[], "Unchanged lines kept around each change", pick(k, &["3", "10", "25", "Full file"], &CONTEXT_LINES, st.context, |c, v| c.context = v)),
            setting(k, "ws", "Ignore whitespace", &["⇧", "W"], "Hides indent-only changes; stage whole files while on", switch(k, "Ignore whitespace", st.ignore_whitespace, |c, v| c.ignore_whitespace = v)),
            setting(k, "moved", "Highlight moved code", &["M"], "Moved blocks show in blue, M jumps between their ends", switch(k, "Highlight moved code", st.show_moved, |c, v| c.show_moved = v)),
            setting(k, "wrap", "Wrap long lines", &["W"], "Off scrolls them sideways; split view always wraps", switch(k, "Wrap long lines", st.wrap, |c, v| c.wrap = v)),
            setting(k, "markers", "+ and − markers", &[], "A sign beside each changed line", switch(k, "Markers", st.markers, |c, v| c.markers = v)),
            setting(k, "words", "Word highlights", &[], "Tint the words that changed within a line", switch(k, "Word highlights", st.word_diff, |c, v| c.word_diff = v)),
            setting(k, "syntax", "Syntax highlighting", &[], "", switch(k, "Syntax highlighting", st.syntax, |c, v| c.syntax = v)),
        ],
        2 => vec![
            setting(k, "scope", "Open to", &[], "The view a new window starts on", pick(k, &["All", "Staged", "Unstaged", "Pull Request Preview", "Git Graph"], &Scope::ALL, st.default_scope, |c, v| c.default_scope = v)),
            setting(k, "editor", "Open files in", &["O"], "Where Open in Editor sends a file", pick(k, &["Default", "Zed", "VS Code", "Cursor"], &Editor::ALL, st.editor, |c, v| c.editor = v)),
            setting(k, "collapse", "Fold viewed files", &["V"], "Marking a file viewed folds it away", switch(k, "Fold viewed files", st.collapse_viewed, |c, v| c.collapse_viewed = v)),
            setting(k, "large", "Fold large files", &[], "Files above this many changed lines start folded", pick(k, &["Never", "300", "1000", "3000"], &FOLD_LARGE, st.fold_large, |c, v| c.fold_large = v)),
            setting(k, "low", "Hide low-signal files", &[], "Lockfiles, bundles, snapshots and generated files", switch(k, "Hide low-signal files", st.hide_low_signal, |c, v| c.hide_low_signal = v)),
        ],
        3 => {
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
                    row().key(format!("proj{i}")).w_full().py(14.0).gap(24.0).items_center().child(words).child(row().w(CONTROL_W).justify_end().child(remove))
                })
                .collect();
            if rows.is_empty() {
                rows.push(setting(k, "none", "No projects yet", &[], "Add a folder inside a git repository", row()));
            }
            rows
        }
        _ => vec![
            setting(k, "auto", "Watch for changes", &[], "Reread when files change on disk", switch(k, "Watch for changes", st.auto_refresh, |c, v| c.auto_refresh = v)),
            setting(k, "every", "Check every", &[], "Only when file events aren't available", pick(k, &["0.5 s", "1 s", "2 s", "5 s"], &REFRESH_MS, st.refresh_ms, |c, v| c.refresh_ms = v)),
        ],
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
    if tab == 3 {
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
    Dialog::new("settings", "Settings").open(true).fit().width(880.0).on_close(|s: &mut Collate| s.screen = Screen::Review).child(body).build(k, layer);
}

fn on_key(s: &mut Collate, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command(',') {
        s.screen = if s.screen == Screen::Settings { Screen::Review } else { Screen::Settings };
        return true;
    }
    // The settings panel is modal: it takes its own keys (Escape closes it).
    if s.screen == Screen::Settings {
        return false;
    }
    // A dialog takes its own keys (Escape closes it); nothing else acts behind it.
    if s.confirm.is_some() || s.words.is_some() {
        return false;
    }
    if s.show_help {
        if matches!(ev.key, Key::Escape) || matches!(&ev.key, Key::Text(t) if t == "?") {
            s.show_help = false;
            return true;
        }
        return false;
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
    if ev.is_command('f') {
        s.searching = true;
        return true;
    }
    // Typing into the search after ⌘F, unless a field has the keyboard (it types there).
    if s.searching && tx.focused().is_none_or(|f| !(f.contains("filter") || f.contains("message"))) && !ev.mods.cmd && !ev.mods.ctrl {
        let mut q = s.search.text.clone();
        match &ev.key {
            Key::Text(t) => q.push_str(t),
            Key::Backspace => {
                q.pop();
            }
            Key::Enter => {
                s.next_hit(!ev.mods.shift, tx);
                return true;
            }
            Key::Escape => {
                s.searching = false;
                return true;
            }
            _ => return false,
        }
        s.search = TextEdit::new(q);
        s.hit = None;
        s.find();
        // Land on the first match as you type, as a browser's find does.
        s.next_hit(true, tx);
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

fn refresh() -> bool {
    let Some(w) = WATCHER.get() else { return false };
    let _ = w.send(Cmd::Refresh);
    true
}

/// Polls `git status`, and rereads and posts a snapshot when the fingerprint, scope or base
/// changes.
fn watch(app: rune::AppHandle<Collate>, rx: mpsc::Receiver<Cmd>, mut scope: Scope, mut base: Option<String>, mut commit: Option<String>) {
    let mut last: Option<u64> = None;
    let mut cache = DiffCache::new();
    let mut memo = git::Memo::default();
    let mut fixed_index: Option<std::time::Instant> = None;
    loop {
        let every = match REFRESH_EVERY.load(Ordering::Relaxed) {
            0 => Duration::from_secs(3600),
            // Events say when to look; this is only a safety net for anything they miss.
            _ if EVENTS.load(Ordering::Relaxed) => Duration::from_secs(30),
            ms => Duration::from_millis(ms),
        };
        match rx.recv_timeout(every) {
            Ok(Cmd::Scope(s)) => {
                scope = s;
                last = None;
            }
            Ok(Cmd::Base(b)) => {
                base = Some(b);
                last = None;
            }
            Ok(Cmd::Commit(c)) => {
                commit = Some(c);
                last = None;
            }
            Ok(Cmd::Refresh) => {
                // Asked for (a setting changed, or r): read every file again.
                last = None;
                memo.clear();
            }
            Ok(Cmd::Switch { base: b, commit: c }) => {
                (base, commit) = (b, c);
                last = None;
                memo.clear();
                cache.clear();
            }
            Ok(Cmd::Changed) => {
                if REFRESH_EVERY.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                // An editor's save or a build is a burst of events: wait for a quiet 80 ms
                // (at most half a second) and look once. Other commands still land.
                let until = std::time::Instant::now() + Duration::from_millis(500);
                loop {
                    let now = std::time::Instant::now();
                    if now >= until {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(80).min(until - now)) {
                        Ok(Cmd::Changed) => {}
                        Ok(Cmd::Scope(s)) => {
                            scope = s;
                            last = None;
                        }
                        Ok(Cmd::Base(b)) => {
                            base = Some(b);
                            last = None;
                        }
                        Ok(Cmd::Commit(c)) => {
                            commit = Some(c);
                            last = None;
                        }
                        Ok(Cmd::Refresh) => {
                            last = None;
                            memo.clear();
                        }
                        Ok(Cmd::Switch { base: b, commit: c }) => {
                            (base, commit) = (b, c);
                            last = None;
                            memo.clear();
                            cache.clear();
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            // Watching was switched off while we slept: wait for an explicit refresh.
            Err(RecvTimeoutError::Timeout) if REFRESH_EVERY.load(Ordering::Relaxed) == 0 => continue,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // No project open: nothing to read until one is.
        let Some(repo) = current_repo() else { continue };
        let started = std::time::Instant::now();
        let result = repo.status_raw().and_then(|raw| {
            // A slow status usually means a stale ("racy") index after a checkout or clone:
            // git rehashes every file each time until something rewrites the index, and we
            // never take its lock. Let git refresh it, at most once a minute.
            if started.elapsed() > Duration::from_millis(60) && fixed_index.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                fixed_index = Some(std::time::Instant::now());
                repo.refresh_index();
            }
            let merge_base = match (scope, &base) {
                (Scope::Branch, Some(b)) => Some(repo.merge_base(b)?),
                _ => None,
            };
            let salt = match scope {
                Scope::History => commit.as_deref().unwrap_or(""),
                _ => merge_base.as_deref().unwrap_or(""),
            };
            let fp = repo.fingerprint(&raw, salt);
            if last == Some(fp) {
                return Ok(None);
            }
            last = Some(fp);
            let pair = match scope {
                Scope::History => commit.as_deref().map(|c| (c, c)),
                _ => base.as_deref().zip(merge_base.as_deref()),
            };
            repo.snapshot_with(scope, &raw, pair, &mut memo).map(|snap| Some((snap, raw, pair.map(|(a, b)| (a.to_owned(), b.to_owned())))))
        });
        match result {
            Ok(None) => {}
            Ok(Some((snap, raw, pair))) => {
                let opts = *OPTS.lock().unwrap();
                let pair = pair.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
                let prepared = match prepare(snap, opts, &mut cache) {
                    Some(p) => p,
                    // A skipped file's diff wasn't cached: read everything once more.
                    None => {
                        memo.clear();
                        match repo.snapshot_with(scope, &raw, pair, &mut memo).ok().and_then(|snap| prepare(snap, opts, &mut cache)) {
                            Some(p) => p,
                            None => continue,
                        }
                    }
                };
                // `COLLATE_TRACE=1`: when each refresh is ready, for timing it from outside.
                if std::env::var_os("COLLATE_TRACE").is_some() {
                    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                    eprintln!("collate: refreshed {} files at {t}", prepared.files.len());
                }
                app.update(move |s, tx| s.apply(prepared, tx));
            }
            Err(e) => {
                last = None;
                let at = scope;
                app.update(move |s, _| {
                    if s.scope == at {
                        s.error = Some(e);
                    }
                });
            }
        }
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
        remember_project(&mut projects, r.root.clone());
    }
    let title: &'static str = Box::leak(repo.as_ref().map_or_else(|| "Collate".to_owned(), |r| format!("{} — Collate", r.name())).into_boxed_str());
    let base = repo.as_ref().and_then(Repo::default_base);
    // History opens on the latest commit; the window's own state picks the same one.
    let first_commit = repo.as_ref().and_then(|r| r.graph(1, None).1);
    let root = repo.as_ref().map(|r| r.root.clone());
    set_repo(repo);
    let start = Settings::load().default_scope;

    let spec = AppSpec {
        on_key: Some(on_key),
        poll: Some(poll),
        setup: Some(|s: &mut Collate, waker| {
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
            ("help", |s: &mut Collate| s.show_help = true),
            ("staged", |s: &mut Collate| s.set_scope(Scope::Staged)),
            ("unstaged", |s: &mut Collate| s.set_scope(Scope::Unstaged)),
            ("branch", |s: &mut Collate| s.set_scope(Scope::Branch)),
            ("history", |s: &mut Collate| s.set_scope(Scope::History)),
            ("map", |s: &mut Collate| s.open_map()),
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
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    #[test]
    fn history_graph_renders_and_selects_commits() {
        for width in [760.0, 1280.0] {
            let mut s = init();
            s.has_project = true;
            s.scope = Scope::History;
            s.loaded = true;
            s.graph_commits = (0..600).map(|i| graph::Commit {
                id: format!("{i:040x}"), parents: if i < 599 { vec![format!("{:040x}", i + 1)] } else { vec![] },
                author: "Ada".into(), email: "ada@example.com".into(), date: "09 Nov 2026 10:00".into(),
                refs: vec![], subject: format!("Commit {i}"),
            }).collect();
            (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
            s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
            s.commit = Some(s.commit_ids[0].clone());
            let second = s.commit_ids[1].clone();
            let mut rt = Runtime::new(s, view, (width, 840.0), Env { system_dark: true, scale: 2.0 });
            rt.frame(0.0);
            assert!(rt.presented("history-graph").is_some());
            assert!(rt.presented(&format!("graph-{}", rt.state.commit_ids[599])).is_none(), "far rows are virtualized");
            let (rect, _) = rt.presented(&format!("graph-{second}")).unwrap();
            rt.pointer_down(rect[0] + 40.0, rect[1] + 15.0);
            rt.pointer_up(rect[0] + 40.0, rect[1] + 15.0);
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.commit.as_deref(), Some(second.as_str()));
            assert!(!rt.state.loaded, "selection requests a new diff");
            assert!(rt.presented("history-graph").is_some(), "graph stays visible while diff loads");
        }
    }

    /// Counts heap bytes in use and their peak, for the colouring A/B.
    struct Counting;
    static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
            let p = unsafe { std::alloc::System.alloc(l) };
            if !p.is_null() {
                let now = LIVE.fetch_add(l.size(), std::sync::atomic::Ordering::Relaxed) + l.size();
                PEAK.fetch_max(now, std::sync::atomic::Ordering::Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(p, l) };
            LIVE.fetch_sub(l.size(), std::sync::atomic::Ordering::Relaxed);
        }
    }
    #[global_allocator]
    static ALLOC: Counting = Counting;

    /// A long TypeScript file of varied code, `n` lines; `edit` changes every 40th line.
    fn typescript(n: usize, edit: bool) -> String {
        let shapes = [
            "export async function load{i}(id: string, opts?: Options): Promise<Result<User, Error>> {",
            "  const url = `${BASE}/users/${id}?include=${opts?.include ?? \"profile\"}&v={i}`;",
            "  // Fetch with a timeout so a stuck request never holds the queue ({i}).",
            "  const res = await fetch(url, { headers: { Authorization: `Bearer ${token}` }, signal });",
            "  if (!res.ok) return { ok: false, error: new HttpError(res.status, \"load {i} failed\") };",
            "  const body = (await res.json()) as { items: Array<{ id: number; name: string }> };",
            "  for (const [k, v] of Object.entries(body.items)) { cache.set(`${k}:{i}`, v ?? null); }",
            "  return { ok: true, value: body.items.map((x) => ({ ...x, seen: Date.now() + {i} })) };",
            "}",
            "/* {i}: block comment that the scanner has to track across */ let flag{i} = 0x{i}f && /re{i}+/g.test(s);",
        ];
        (0..n).map(|i| {
            let line = shapes[i % shapes.len()].replace("{i}", &i.to_string());
            if edit && i % 40 == 7 { format!("{line} // changed\n") } else { format!("{line}\n") }
        }).collect()
    }

    /// Scrolling a 60,000-line TypeScript diff, reading pace then flicking, at 60 Hz: frame
    /// times and heap, with code coloured (`on`) or plain (`off`). Run each arm alone:
    /// `COLLATE_COLOUR=on|off cargo test --release colour_ab -- --ignored --nocapture`.
    /// Colouring off a background thread measured no faster (and drew rows twice), so it's
    /// done inline as rows first draw.
    #[test]
    #[ignore]
    fn colour_ab() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let arm = std::env::var("COLLATE_COLOUR").unwrap_or_else(|_| "on".into());
        let split = std::env::var("COLLATE_AB_LAYOUT").is_ok_and(|v| v == "split");
        let (old, new) = (typescript(60_000, false), typescript(60_000, true));
        let base = LIVE.load(Ordering::Relaxed);
        let mut state = init();
        state.settings = Settings::default();
        state.settings.layout = if split { DiffMode::Split } else { DiffMode::Unified };
        state.apply_settings(None);
        if arm == "off" {
            state.diff.set_highlighter(Highlighter::new(|_: Option<&str>, _: &str| Vec::new()).key("off"));
        }
        state.diff.set_files(vec![FileDiff::between_with_context(Some("app.ts".into()), Some("app.ts".into()), &old, &new, 25)]);
        drop((old, new));
        state.has_project = true;
        state.loaded = true;
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let start = Instant::now();
        let mut rt = Runtime::new(state, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        let first = start.elapsed();
        let (x, y) = (700.0, 500.0);
        let mut times = Vec::new();
        // Reading pace (150 pt a frame), then flicking (2,500 pt a frame). `COLLATE_AB_PROFILE=1`:
        // flicking down and back up again without pausing, long enough to profile.
        let profile = std::env::var_os("COLLATE_AB_PROFILE").is_some();
        let phases: Vec<(usize, f32)> = if profile { (0..8).map(|n| (150, if n % 2 == 0 { -2500.0 } else { 2500.0 })).collect() } else { vec![(300, -150.0), (150, -2500.0)] };
        for (frames, dy) in phases {
            let mut phase = Vec::new();
            for _ in 0..frames {
                let t = Instant::now();
                rt.wheel(x, y, dy);
                rt.frame(1.0 / 60.0);
                let spent = t.elapsed();
                phase.push(spent.as_secs_f64() * 1000.0);
                if !profile {
                    std::thread::sleep(Duration::from_micros(16_667).saturating_sub(spent));
                }
            }
            times.push(phase);
        }
        let stats = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            let over = v.iter().filter(|&&t| t > 16.7).count();
            format!("p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms, {over} over 16.7", q(0.5), q(0.95), q(0.99), q(1.0))
        };
        let mb = |b: usize| b as f64 / 1048576.0;
        // What colouring the whole file costs on one thread, for scale.
        let t = Instant::now();
        let text = typescript(60_000, true);
        let blocks: usize = text.split_inclusive('\n').collect::<Vec<_>>().chunks(128).map(|c| rune_kit::syntax::builtin(Some("ts"), &c.concat()).len()).sum();
        println!("scanner: all 60,000 lines in {:.1} ms ({blocks} tokens)", t.elapsed().as_secs_f64() * 1000.0);
        println!("arm {arm} ({}): first frame {:.1} ms", if split { "split" } else { "unified" }, first.as_secs_f64() * 1000.0);
        println!("  reading: {}", stats(&times[0]));
        println!("  flicking: {}", stats(&times[1]));
        println!("  heap: peak {:.1} MB, held at end {:.1} MB (above the {:.1} MB before the diff)", mb(PEAK.load(Ordering::Relaxed).saturating_sub(base)), mb(LIVE.load(Ordering::Relaxed).saturating_sub(base)), mb(base));
    }

    #[test]
    fn scrolling_near_the_end_asks_for_more_history() {
        let mut s = init();
        s.has_project = true;
        s.scope = Scope::History;
        s.loaded = true;
        s.graph_commits = (0..HISTORY_PAGE).map(|i| graph::Commit {
            id: format!("{i:040x}"), parents: if i + 1 < HISTORY_PAGE { vec![format!("{:040x}", i + 1)] } else { vec![] },
            author: "Ada".into(), email: "a@x".into(), date: String::new(), refs: vec![], subject: format!("Commit {i}"),
        }).collect();
        (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
        s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
        let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        assert!(rt.presented("graph-refresh").is_some(), "refresh is an icon button");
        assert_eq!(rt.state.graph_limit, HISTORY_PAGE, "the top of the list asks for nothing");
        let (r, _) = rt.presented("graph-scroll").unwrap();
        for _ in 0..200 {
            rt.wheel(r[0] + 200.0, r[1] + 200.0, -3000.0);
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.graph_limit, 2 * HISTORY_PAGE, "the end of the list asks for the next page, once");
    }

    #[test]
    fn a_commit_opens_from_its_files_and_esc_goes_back() {
        let mut s = init();
        s.has_project = true;
        s.scope = Scope::History;
        s.loaded = true;
        s.graph_commits = (0..3).map(|i| graph::Commit {
            id: format!("{i:040x}"), parents: if i < 2 { vec![format!("{:040x}", i + 1)] } else { vec![] },
            author: "Ada Lovelace".into(), email: "ada@example.com".into(), date: "09 Nov 2026 10:00".into(),
            refs: vec![], subject: format!("Commit {i}"),
        }).collect();
        (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
        s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
        s.commit = Some(s.commit_ids[1].clone());
        s.commit_body = "Why it changed.".into();
        let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
        rt.on_key = Some(on_key);
        rt.frame(0.0);
        assert!(rt.presented("history-graph").is_some(), "the list fills the window first");
        assert!(rt.presented("commit-card").is_some(), "the side panel shows the chosen commit");
        assert!(rt.presented("commit-header").is_none());
        // A file in the side panel's list opens the commit's diff.
        rt.update(|s, tx| reveal(s, "src/a.rs", tx));
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(rt.state.history_diff);
        assert!(rt.presented("commit-header").is_some(), "the commit heads its diff");
        assert!(rt.presented("history-graph").is_none(), "the list makes way");
        rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(!rt.state.history_diff, "Esc goes back to the list");
        assert!(rt.presented("history-graph").is_some());
        // Enter opens the chosen commit too.
        rt.key(KeyEvent { key: Key::Enter, mods: Default::default() });
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(rt.state.history_diff);
        let (x, y) = rt.find("history-back").expect("a way back");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(!rt.state.history_diff, "All Commits goes back");
    }

    #[test]
    fn the_uncommitted_row_opens_all_changes() {
        let mut s = init();
        s.has_project = true;
        s.scope = Scope::History;
        s.loaded = true;
        s.total = 2;
        let head = format!("{:040x}", 1);
        let commits = vec![graph::Commit { id: head.clone(), parents: vec![], author: "Ada".into(), email: "a@x".into(), date: String::new(), refs: vec![], subject: "Only".into() }];
        s.commit_ids = vec![head.clone()];
        s.graph_commits = graph::with_uncommitted(commits, Some(&head), true);
        (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
        let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        let (rect, _) = rt.presented(&format!("graph-{}", graph::UNCOMMITTED)).expect("uncommitted row on top");
        rt.pointer_down(rect[0] + 60.0, rect[1] + 14.0);
        rt.pointer_up(rect[0] + 60.0, rect[1] + 14.0);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.scope, Scope::Uncommitted);
    }

    #[test]
    fn a_branch_and_its_remote_share_a_label() {
        let r = |name: &str, kind, head| graph::Ref { name: name.into(), kind, head };
        let labels = ref_labels(&[
            r("main", graph::RefKind::Branch, true),
            r("origin/main", graph::RefKind::Remote, false),
            r("upstream/main", graph::RefKind::Remote, false),
            r("origin/dev", graph::RefKind::Remote, false),
            r("v1", graph::RefKind::Tag, false),
        ]);
        let text: Vec<&str> = labels.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(text, ["main - origin - upstream", "origin/dev", "v1"]);
        assert!(labels[0].2, "HEAD's branch is marked");
    }

    /// The repository the view tests read: `COLLATE_TEST_REPO` when set (any repository with many
    /// changed files under `src/`), else one made here, once: four folders of twenty files on
    /// `main`, each file changed in two places since its one commit, and a `develop` branch.
    fn test_repo() -> &'static std::path::Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            if let Some(dir) = std::env::var_os("COLLATE_TEST_REPO") {
                return PathBuf::from(dir);
            }
            let dir = std::env::temp_dir().join(format!("collate-fixture-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let folders = ["api", "core", "ui", "util"];
            let write = |edited: bool| {
                for (at, folder) in folders.iter().enumerate() {
                    std::fs::create_dir_all(dir.join("src").join(folder)).unwrap();
                    for i in 1..=20 {
                        let mut text = format!("use crate::{}::file{i};\n\n", folders[(at + 1) % folders.len()]);
                        for n in 0..60 {
                            text += &match n {
                                10 => format!("    let value_{n} = {}({n});\n", if edited { "changed" } else { "original" }),
                                40 => format!("    let value_{n} = {}({n});\n", if edited { "also changed" } else { "untouched" }),
                                n if n % 12 == 0 => format!("fn function_{folder}_{i}_{n}() {{\n"),
                                n if n % 12 == 11 => "}\n".to_owned(),
                                n => format!("    let value_{n} = compute({n}, {i});\n"),
                            };
                        }
                        std::fs::write(dir.join(format!("src/{folder}/file{i}.rs")), text).unwrap();
                    }
                }
            };
            write(false);
            for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"], &["add", "-A"], &["commit", "-qm", "init"], &["branch", "develop"]] {
                let out = std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
                assert!(out.status.success(), "{args:?}");
            }
            write(true);
            dir
        })
    }

    fn snapshot() -> Snapshot {
        let repo = Repo::open(test_repo()).unwrap();
        let raw = repo.status_raw().unwrap();
        repo.snapshot(Scope::Uncommitted, &raw, None).unwrap()
    }

    fn loaded() -> Runtime<Collate> {
        let env = Env { system_dark: true, scale: 2.0 };
        let mut state = init();
        state.settings = Settings::default();
        state.apply_settings(None);
        let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
        rt.frame(0.0);
        let snap = snapshot();
        rt.update(move |s, tx| s.apply_snapshot(snap, tx));
        for _ in 0..10 {
            rt.frame(1.0 / 60.0);
        }
        rt
    }

    /// Whether a row of the staging panel's Unstaged list is in its visible part, not just
    /// mounted in the virtual list's overscan.
    fn on_screen(rt: &Runtime<Collate>, key: &str) -> bool {
        let Some((list, _)) = rt.presented("unstaged") else { return false };
        rt.presented(key).is_some_and(|(r, _)| r[1] >= list[1] + 30.0 && r[1] + r[3] <= list[1] + list[3])
    }

    /// The middle of the Unstaged list, where a reader scrolls it.
    fn list_point(rt: &Runtime<Collate>) -> (f32, f32) {
        let (r, _) = rt.presented("unstaged").expect("the staging panel's list");
        (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
    }

    fn click(rt: &mut Runtime<Collate>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
    }

    /// The file's header sits right under the summary, at the top of the diff.
    fn header_at_top(rt: &Runtime<Collate>, i: usize) {
        let summary = rt.presented("summary").expect("summary").0;
        let top = summary[1] + summary[3];
        let y = rt.presented(&format!("df{i}")).map(|p| p.0[1]).unwrap_or_else(|| panic!("file {i}'s header is on screen"));
        assert!((y - top).abs() < 2.0, "file {i}'s header is at y={y}, not at the diff's top {top}");
    }

    #[test]
    fn clicking_files_in_the_sidebar_brings_each_to_the_top() {
        let mut rt = loaded();
        // Down the list, then back up past where we started.
        for target in ["src/core/file5.rs", "src/ui/file2.rs", "src/api/file3.rs", "src/util/file15.rs"] {
            // Scroll the sidebar to it, as a reader would: down for a row below, up for one above.
            let down = rt.presented(target).is_none_or(|(r, _)| r[1] > 400.0);
            for _ in 0..80 {
                if on_screen(&rt, target) {
                    break;
                }
                let (x, y) = list_point(&rt);
                rt.wheel(x, y, if down { -60.0 } else { 60.0 });
                rt.frame(1.0 / 60.0);
            }
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            assert!(on_screen(&rt, target), "{target} scrolled into the sidebar");
            click(&mut rt, target);
            let i = rt.state.diff.file_index(target).unwrap();
            assert_eq!(rt.state.tree.cursor(), Some(target));
            header_at_top(&rt, i);
        }
        // Scrolling up from the last file hands the selection back to the file above.
        let last = rt.state.diff.files().len() - 1;
        for _ in 0..20 {
            rt.wheel(700.0, 500.0, 120.0);
            rt.frame(1.0 / 60.0);
        }
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert!(rt.state.diff.visible_file() < last, "the selection follows the scroll again");
        let above = rt.state.diff.files()[rt.state.diff.visible_file()].path().to_owned();
        assert_eq!(rt.state.tree.cursor(), Some(above.as_str()));
    }

    #[test]
    fn shortcuts_work_after_clicking_a_file_in_the_sidebar() {
        let mut rt = loaded();
        rt.on_key = Some(on_key);
        click(&mut rt, "src/api/file3.rs");
        assert_eq!(rt.state.tree.cursor(), Some("src/api/file3.rs"));
        let at = rt.state.diff.visible_file();
        // Routed as the window routes it: focused element first, then the app.
        rt.key(KeyEvent { key: Key::Text("]".into()), mods: Default::default() });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.diff.visible_file(), at + 1, "] reached the diff, not the file list");
        rt.key(KeyEvent { key: Key::Text("v".into()), mods: Default::default() });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.is_viewed(at + 1), "v marked the file in view");
    }

    #[test]
    fn command_f_then_typing_searches() {
        let mut rt = loaded();
        rt.on_key = Some(on_key);
        click(&mut rt, "src/api/file2.rs");
        let key = |rt: &mut Runtime<Collate>, key: Key, cmd: bool| {
            rt.key(KeyEvent { key, mods: rune::Mods { cmd, ..Default::default() } });
            rt.frame(1.0 / 60.0);
        };
        key(&mut rt, Key::Command('f'), true);
        assert!(rt.state.searching);
        for c in "also chang".chars() {
            key(&mut rt, Key::Text(c.to_string()), false);
        }
        key(&mut rt, Key::Backspace, false);
        key(&mut rt, Key::Text("g".into()), false);
        assert_eq!(rt.state.search.text, "also chang");
        assert_eq!(rt.state.hits.len(), rt.state.diff.files().len(), "every file matches");
        assert_eq!(rt.state.hit, Some(0), "typing lands on the first match");
        key(&mut rt, Key::Enter, false);
        assert_eq!(rt.state.hit, Some(1), "Return steps on");
        key(&mut rt, Key::Escape, false);
        assert!(!rt.state.searching, "Escape ends typing into the search");
        key(&mut rt, Key::Text("v".into()), false);
        assert_eq!(rt.state.search.text, "also chang", "and shortcuts are shortcuts again");
    }

    #[test]
    fn m_jumps_between_the_ends_of_a_move() {
        let block = "let total = compute_total();\nrecord_metric(total);\nflush_metrics_now();\n";
        let pad: String = (0..60).map(|i| format!("filler line {i}\n")).collect();
        let mut files = vec![
            FileDiff::between_with_context(Some("c.rs".into()), Some("c.rs".into()), &format!("head\n{block}{pad}"), &format!("head\n{pad}"), 3),
            FileDiff::between_with_context(Some("d.rs".into()), Some("d.rs".into()), &format!("{pad}start\n"), &format!("{pad}start\n{block}"), 3),
        ];
        mark_moved(&mut files);
        let env = Env { system_dark: true, scale: 2.0 };
        let mut state = init();
        state.settings = Settings::default();
        state.apply_settings(None);
        state.diff.set_files(files);
        state.loaded = true;
        let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
        rt.on_key = Some(on_key);
        for _ in 0..5 {
            rt.frame(1.0 / 60.0);
        }
        rt.key(KeyEvent { key: Key::Text("m".into()), mods: Default::default() });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.diff.visible_file(), 1, "m went to where the block landed");
        rt.key(KeyEvent { key: Key::Text("m".into()), mods: Default::default() });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.diff.visible_file(), 0, "and back to where it came from");
    }

    #[test]
    fn the_sidebar_switches_views_and_the_file_menu_acts() {
        let mut rt = loaded();
        // Views are sidebar places now, not a toolbar segmented control.
        click(&mut rt, "view-staged");
        assert_eq!(rt.state.scope, Scope::Staged);
        click(&mut rt, "view-all");
        assert_eq!(rt.state.scope, Scope::Uncommitted);
        rt.update(|s, tx| {
            s.hashes.clear();
            s.apply_snapshot(snapshot(), tx);
        });
        for _ in 0..10 {
            rt.frame(1.0 / 60.0);
        }
        // A file header's "⋯" opens a native menu with the rest of its commands.
        click(&mut rt, "file-more-0");
        let menu = rt.popup_menu().expect("the file menu opened");
        for id in ["stage", "open", "copy", "viewed", "discard"] {
            assert!(menu.find(id).is_some(), "the file menu has {id}");
        }
        assert_eq!(menu.find("discard").map(|i| i.title().to_owned()), Some("Discard Changes…".into()), "discard asks first, and says so");
        rt.resolve_popup(Some("viewed")).unwrap();
        rt.frame(1.0 / 60.0);
        assert!(rt.state.is_viewed(0), "picking Viewed marks the file");
    }

    #[test]
    fn settings_float_over_the_review_and_close_with_escape() {
        let mut rt = loaded();
        rt.on_key = Some(on_key);
        rt.key(KeyEvent { key: Key::Command(','), mods: rune::Mods { cmd: true, ..Default::default() } });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.screen, Screen::Settings);
        assert!(rt.find("summary").is_some(), "the review stays underneath");
        click(&mut rt, "tab-diff");
        assert_eq!(rt.state.settings_tab, 1);
        assert!(rt.find("words").is_some(), "the Diff section's settings show");
        rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.screen, Screen::Review, "Escape puts the panel away");
    }

    #[test]
    fn the_staging_panel_lines_up() {
        let rt = loaded();
        let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
        let (panel, filter, search) = (rect("staging"), rect("filter"), rect("search"));
        let bulk = rect("unstaged-bulk");
        let left = panel[0] + PANEL_INSET;
        let right = panel[0] + panel[2] - PANEL_INSET;
        assert!((filter[0] - left).abs() < 1.0, "the filter starts on the inset: {} vs {left}", filter[0]);
        assert!((filter[0] + filter[2] - right).abs() < 1.0, "and ends on it: {} vs {right}", filter[0] + filter[2]);
        let label_end = bulk[0] + bulk[2] - GHOST_PAD;
        assert!((label_end - right).abs() < 1.5, "Stage All's label ends on the same edge: {label_end} vs {right}");
        assert!((filter[3] - search[3]).abs() < 0.5, "the filter is as tall as the diff's search");
        // A row's chevron: its ink (4.5 pt into its 16 pt box) starts on the inset. `find`
        // takes a path (the key alone also matches the diff's chevrons) and gives the centre.
        let (cx, _) = rt.find("src/chevron").expect("the list's first chevron");
        let ink = cx - 8.0 + 4.5;
        assert!((ink - left).abs() < 1.0, "the list's chevron ink at {ink} vs {left}");
        // Row buttons end on the inset too.
        let quick = rect("quick");
        assert!((quick[0] + quick[2] - right).abs() < 1.0, "a row's + ends on the inset: {} vs {right}", quick[0] + quick[2]);
        for key in ["message", "commit"] {
            let r = rect(key);
            assert!((r[0] - left).abs() < 1.0 && (r[0] + r[2] - right).abs() < 1.0, "the commit {key} spans the inset: {r:?} vs {left}..{right}");
        }
    }

    #[test]
    fn the_diff_has_one_edge_each_side() {
        let rt = loaded();
        let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
        let summary = rect("summary");
        let (left, right) = (summary[0] + 16.0, summary[0] + summary[2] - 16.0);
        // The summary's text starts on the edge and its last item ends on it.
        let what = rect("what");
        assert!((what[0] - left).abs() < 1.0, "summary text at {} vs {left}", what[0]);
        let viewed = rect("viewed-count");
        assert!((viewed[0] + viewed[2] - right).abs() < 1.0, "viewed count ends at {} vs {right}", viewed[0] + viewed[2]);
        // A file header: the chevron glyph (4 pt into its 24 pt box) and the viewed box.
        let toggle = rect("toggle");
        assert!((toggle[0] + 8.0 - left).abs() < 1.0, "the header triangle's ink at {} vs {left}", toggle[0] + 8.0);
        let check = rect("viewed");
        assert!((check[0] + check[2] - right).abs() < 1.0, "viewed box ends at {} vs {right}", check[0] + check[2]);
        // A hunk row's last action: its label (inside the ghost padding) ends on the edge.
        let discard = rect("discard");
        assert!((discard[0] + discard[2] - 8.0 - right).abs() < 1.5, "Discard Hunk… ends at {} vs {right}", discard[0] + discard[2] - 8.0);
    }

    #[test]
    fn the_map_zooms_into_folders_and_opens_a_changed_file() {
        let mut rt = loaded();
        click(&mut rt, "view-map");
        assert!(rt.state.map.open, "Map opens");
        // Another test owns the shared repository, so this one maps the test repository itself.
        let built = map::build(test_repo()).expect("the test repository maps");
        assert!(built.files > 0);
        rt.update(move |s, _| s.map.data = Some(std::sync::Arc::new(built)));
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.scope_title(), "All Changes", "the view's changes stay what the map shows");
        let canvas = rt.presented("map-canvas").expect("the map is drawn").0;
        // A folder takes clicks on its name band; inside it, its files do.
        let press = |rt: &mut Runtime<Collate>, path: &str, folder: bool| {
            let r = rt.state.map_rect(path).unwrap_or_else(|| panic!("{path} is on the map"));
            let (x, y) = (canvas[0] + r[0] + r[2] / 2.0, canvas[1] + r[1] + if folder { 6.0 } else { r[3] / 2.0 });
            rt.pointer_move(x, y);
            rt.frame(1.0 / 60.0);
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
        };
        // Everything in the test repository is under src/: a click opens that folder.
        press(&mut rt, "src", true);
        assert_eq!(rt.state.map.zoom, "src");
        let changed = rt.state.diff.files()[0].path().to_owned();
        press(&mut rt, &changed, false);
        assert!(!rt.state.map.open, "a changed file opens its diff");
        assert!(rt.find("summary").is_some(), "the diff shows again");
    }

    #[test]
    fn the_branch_menu_previews_the_pull_request() {
        let mut rt = loaded();
        assert!(rt.find("view-branch").is_none(), "no Branch place in the sidebar, as in GitHub Desktop");
        // On a feature branch, so the repository's other branches are bases to pick. (The menu
        // rereads the branches from whichever repository the tests share, when one is open.)
        rt.update(|s, _| {
            s.branch = "feature".into();
            s.branches = vec!["main".into()];
        });
        click(&mut rt, "branch");
        let base = rt.state.branches.iter().find(|b| *b != "feature").cloned().expect("a branch to compare against");
        let menu = rt.popup_menu().expect("the branch menu opened");
        assert!(menu.find("preview").is_some() && menu.find(&format!("base:{base}")).is_some(), "preview and the bases to compare against");
        rt.resolve_popup(Some(&format!("base:{base}"))).unwrap();
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.scope, Scope::Branch);
        assert_eq!(rt.state.base.as_deref(), Some(base.as_str()));
        assert_eq!(rt.state.scope_title(), "Pull Request Preview");
    }

    #[test]
    fn the_three_columns_start_on_one_line() {
        let rt = loaded();
        let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
        let mid = |r: [f32; 4]| r[1] + r[3] / 2.0;
        // One top inset: each column's first row starts COLUMN_TOP below the card's edge and
        // centres its text in the same band.
        let (card, summary_row, filter, label) = (rect("main-pane"), rect("totals"), rect("filter"), rect("section-title"));
        for (name, r) in [("summary row", summary_row), ("filter", filter)] {
            assert!((r[1] - card[1] - COLUMN_TOP).abs() < 0.5, "the {name} starts {COLUMN_TOP} pt under the card's edge: {} vs {}", r[1], card[1]);
        }
        assert!((mid(filter) - mid(summary_row)).abs() < 0.5, "filter and summary centred alike: {} vs {}", mid(filter), mid(summary_row));
        assert!((mid(label) - mid(summary_row)).abs() < 0.5, "Working Copy centred alike: {} vs {}", mid(label), mid(summary_row));
        let mut rt = rt;
        rt.update(|s, _| s.show_low_signal = true);
        rt.frame(0.0);
        let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
        let (notes, first_row, list_title) = (rect("notes"), rect("view-all"), rect("unstaged-head"));
        assert!((mid(notes) - mid(first_row)).abs() < 0.5 && (mid(notes) - mid(list_title)).abs() < 0.5, "second rows centred alike: {} {} {}", mid(notes), mid(first_row), mid(list_title));
        let (first_row, list_title) = (rect("view-all"), rect("unstaged-head"));
        assert!((mid(list_title) - mid(first_row)).abs() < 1.0, "the panel's first list title level with the sidebar's first row: {} vs {}", mid(list_title), mid(first_row));
    }

    #[test]
    fn a_partly_staged_file_is_in_both_lists() {
        let mut rt = loaded();
        rt.update(|s, tx| {
            let mut snap = snapshot();
            snap.changes[0].staged = Staged::Partly;
            snap.changes[1].staged = Staged::Fully;
            s.hashes.clear();
            s.apply_snapshot(snap, tx);
        });
        let st = &rt.state;
        let (a, b, c) = (st.diff.files()[0].path(), st.diff.files()[1].path(), st.diff.files()[2].path());
        assert!(st.tree.contains(a) && st.staged_tree.contains(a), "partly staged: both lists");
        assert!(!st.tree.contains(b) && st.staged_tree.contains(b), "fully staged: Staged only");
        assert!(st.tree.contains(c) && !st.staged_tree.contains(c), "not staged: Unstaged only");
    }

    #[test]
    fn recent_projects_keep_the_latest_first_once() {
        let mut list = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        remember_project(&mut list, PathBuf::from("/b"));
        assert_eq!(list, [PathBuf::from("/b"), PathBuf::from("/a")]);
        remember_project(&mut list, PathBuf::from("/c"));
        assert_eq!(list[0], PathBuf::from("/c"));
        let labels = project_labels(&[PathBuf::from("/w/api"), PathBuf::from("/p/api"), PathBuf::from("/w/web")]);
        assert_eq!(labels, ["api - w", "api - p", "web"], "short, and distinct when names repeat");
    }

    #[test]
    fn only_changes_that_matter_wake_the_watcher() {
        let root = std::path::Path::new("/r");
        for p in ["/r/src/a.rs", "/r/.git/index", "/r/.git/HEAD", "/r/.git/refs/heads/main"] {
            assert!(relevant(std::path::Path::new(p), root), "{p}");
        }
        for p in ["/r/.git/objects/ab/cdef", "/r/.git/index.lock", "/r/.git/collate-backup/1/a.rs", "/r/.git/logs/HEAD"] {
            assert!(!relevant(std::path::Path::new(p), root), "{p}");
        }
    }

    #[test]
    fn a_viewed_mark_holds_across_views() {
        let mut rt = loaded();
        rt.update(|s, tx| s.toggle_viewed(0, tx));
        let path = rt.state.diff.files()[0].path().to_owned();
        // The same file read for another view: its old side differs, its new side doesn't.
        rt.update(move |s, tx| {
            let mut snap = snapshot();
            for c in &mut snap.changes {
                c.hash ^= 0xdead_beef;
            }
            s.hashes.clear();
            s.apply_snapshot(snap, tx);
        });
        let i = rt.state.diff.file_index(&path).unwrap();
        assert!(rt.state.is_viewed(i), "still viewed");
        assert!(rt.state.stale.is_empty(), "and not flagged as changed");
    }

    #[test]
    fn the_diff_and_the_sidebar_list_files_in_the_same_order() {
        let rt = loaded();
        let diff: Vec<&str> = rt.state.diff.files().iter().map(|f| f.path()).collect();
        let tree: Vec<&str> = (0..rt.state.tree.visible_len())
            .filter_map(|r| rt.state.tree.visible_id(r))
            .filter(|id| rt.state.tree.get(id).is_some_and(|n| !n.dir))
            .collect();
        assert_eq!(diff, tree);
    }

    #[test]
    fn the_diff_scrolls_to_its_last_file() {
        let mut rt = loaded();
        for _ in 0..200 {
            rt.wheel(700.0, 500.0, -400.0);
            rt.frame(1.0 / 60.0);
        }
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let last = rt.state.diff.files().len() - 1;
        assert!(rt.presented(&format!("df{last}")).is_some(), "the last file's header was reached");
    }

    #[test]
    fn marking_a_file_viewed_folds_it_and_counts_it() {
        let mut rt = loaded();
        click(&mut rt, "src/api/file1.rs");
        let i = rt.state.diff.file_index("src/api/file1.rs").unwrap();
        rt.update(move |s, tx| s.toggle_viewed(i, tx));
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert!(rt.state.is_viewed(i));
        assert!(rt.state.diff.collapsed(i));
        rt.update(move |s, tx| s.toggle_viewed(i, tx));
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.is_viewed(i) && !rt.state.diff.collapsed(i));
    }

    #[test]
    fn settings_restyle_the_diff() {
        let mut rt = loaded();
        rt.update(|s, _| s.settings.font_size = 16.0);
        rt.update(|s, _| {
            let was = Settings::default();
            s.apply_settings(Some(&was));
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.diff.style().font_size, 16.0);
        assert_eq!(rt.state.diff.style().line_h, (16.0f32 * 1.6).round());
    }

    /// Stage a hunk, discard another behind the dialog, commit: the review loop end to end,
    /// through the app's own handlers, on a throwaway repository.
    #[test]
    fn the_review_loop_stages_discards_and_commits() {
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let dir = std::env::temp_dir().join(format!("collate-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &text).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"], &["add", "-A"], &["commit", "-qm", "init"]] {
            git(&dir, args);
        }
        std::fs::write(dir.join("a.txt"), text.replace("line 5\n", "line five\n").replace("line 35\n", "line thirty-five\n")).unwrap();
        // The one test that changes a repository owns REPO.
        set_repo(Some(Repo::open(&dir).unwrap()));
        let repo = current_repo().unwrap();
        let reread = |rt: &mut Runtime<Collate>| {
            let raw = repo.status_raw().unwrap();
            let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
            rt.update(move |s, tx| s.apply_snapshot(snap, tx));
            for _ in 0..5 {
                rt.frame(1.0 / 60.0);
            }
        };
        let env = Env { system_dark: true, scale: 2.0 };
        let mut state = init();
        state.settings = Settings::default();
        state.scope = Scope::Uncommitted;
        state.apply_settings(None);
        let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
        rt.frame(0.0);
        reread(&mut rt);
        assert_eq!(rt.state.diff.files()[0].hunks.len(), 2);

        // Stage the first hunk.
        rt.update(|s, _| s.stage_hunk(0, 0, None));
        reread(&mut rt);
        assert!(git(&dir, &["diff", "--cached"]).contains("+line five"));
        assert_eq!(rt.state.info[0].staged, Staged::Partly);
        assert_eq!(rt.state.staged, 1);
        assert!(rt.state.notice.as_ref().is_some_and(|n| !n.error), "a confirmation shows");

        // Discard the second: nothing happens until the dialog is confirmed.
        rt.update(|s, _| s.ask_discard(0, Some(1)));
        // Let the dialog finish opening before reaching for its button.
        for _ in 0..40 {
            rt.frame(1.0 / 60.0);
        }
        assert!(rt.find("confirm-discard").is_some(), "the dialog asks first");
        assert!(std::fs::read_to_string(dir.join("a.txt")).unwrap().contains("thirty-five"));
        click(&mut rt, "confirm-discard");
        let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(!on_disk.contains("thirty-five") && on_disk.contains("line five"), "only the second hunk is undone");
        assert!(rt.state.confirm.is_none());

        // Commit what's staged.
        reread(&mut rt);
        rt.update(|s, _| s.message = TextEdit::new("Spell out five"));
        let summary = repo.commit("Spell out five").unwrap();
        assert!(summary.ends_with("Spell out five"));
        reread(&mut rt);
        assert_eq!(rt.state.diff.files().len(), 0, "nothing left to review");

        // Pick one line of a three-line replacement and stage just it, from the keyboard.
        let press = |rt: &mut Runtime<Collate>, t: &str| {
            let ev = KeyEvent { key: Key::Text(t.into()), mods: Default::default() };
            rt.update(move |s, tx| {
                on_key(s, &ev, tx);
            });
            rt.frame(1.0 / 60.0);
        };
        let text: String = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        std::fs::write(dir.join("a.txt"), text.replace("line 20\nline 21\nline 22\n", "line XX\nline XXI\nline XXII\n")).unwrap();
        reread(&mut rt);
        rt.update(|s, _| {
            // Side by side, old line 21 and its replacement share a row: one click picks both.
            s.diff.set_mode(DiffMode::Split);
            s.diff.set_selection(Some(diff::LineRange { file: 0, start: LineRef::new(diff::Side::Old, 21), end: LineRef::new(diff::Side::Old, 21) }));
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.diff.selected_changes().is_some(), "lines are picked");
        press(&mut rt, "a");
        let index = git(&dir, &["show", ":a.txt"]);
        assert!(index.contains("line 20\nline XXI\nline 22\n"), "only line 21 is staged, in place: {}", &index[index.find("line 19").unwrap()..index.find("line 23").unwrap()]);
        assert!(rt.state.diff.selection().is_none(), "the pick is spent");

        // Discard a file, then take it back.
        reread(&mut rt);
        let before = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        rt.update(|s, _| s.discard_file(0));
        assert_ne!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), before);
        assert!(rt.state.notice.as_ref().is_some_and(|n| n.undo.is_some()), "undo is offered");
        rt.update(|s, _| s.undo_discard());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), before, "undo puts the file back");

        // Viewed, then edited: flagged, not silently forgotten.
        reread(&mut rt);
        rt.update(|s, tx| s.toggle_viewed(0, tx));
        assert!(rt.state.is_viewed(0));
        std::fs::write(dir.join("a.txt"), before.replace("line 30\n", "line thirty\n")).unwrap();
        reread(&mut rt);
        assert!(!rt.state.is_viewed(0) && rt.state.stale.contains("a.txt"));

        // Stage one of two word changes in a line: pick the row, p, untick one, Stage picked.
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "checkpoint"]);
        std::fs::write(dir.join("w.rs"), "fn a() {\n    let total = price * qty;\n}\n").unwrap();
        git(&dir, &["add", "w.rs"]);
        git(&dir, &["commit", "-qm", "w"]);
        std::fs::write(dir.join("w.rs"), "fn a() {\n    let sum = price * quantity;\n}\n").unwrap();
        reread(&mut rt);
        rt.on_key = Some(on_key);
        let f = rt.state.diff.file_index("w.rs").unwrap();
        rt.update(move |s, _| {
            s.diff.set_mode(DiffMode::Split);
            s.diff.set_selection(Some(diff::LineRange { file: f, start: LineRef::new(diff::Side::Old, 2), end: LineRef::new(diff::Side::Old, 2) }));
        });
        rt.key(KeyEvent { key: Key::Text("p".into()), mods: Default::default() });
        for _ in 0..40 {
            rt.frame(1.0 / 60.0);
        }
        let shown: Vec<String> = rt.state.words.as_ref().expect("the word picker opened").edits.iter().map(|e| format!("{:?}", e.old)).collect();
        assert_eq!(shown.len(), 2);
        rt.update(|s, _| s.words.as_mut().unwrap().picked[0] = false);
        rt.frame(1.0 / 60.0);
        click(&mut rt, "words-stage");
        assert!(rt.state.words.is_none());
        assert_eq!(git(&dir, &["show", ":w.rs"]), "fn a() {\n    let total = price * quantity;\n}\n", "only qty → quantity is staged");

        // Switch to another project: the review starts over there, and the list remembers both.
        let other = std::env::temp_dir().join(format!("collate-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        std::fs::create_dir_all(other.join("deep/folder")).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"]] {
            git(&other, args);
        }
        rt.update(|s, _| s.viewed.insert("w.rs".into(), 1).map(drop).unwrap_or(()));
        let deep = other.join("deep/folder");
        rt.update(move |s, tx| s.open_project(&deep, tx));
        let root = current_repo().unwrap().root.clone();
        assert_eq!(root.canonicalize().unwrap(), other.canonicalize().unwrap(), "a folder inside a repository opens the repository");
        assert_eq!(rt.state.name, format!("collate-other-{}", std::process::id()));
        assert!(!rt.state.loaded && rt.state.diff.files().is_empty() && rt.state.viewed.is_empty(), "nothing carries over");
        assert_eq!(rt.state.projects[0], root, "the opened project goes first");
        let back = dir.clone();
        rt.update(move |s, tx| s.open_project(&back, tx));
        assert_eq!(rt.state.projects.len().min(2), 2);
        assert_eq!(rt.state.projects[1], root, "and the other stays in the list");
        rt.update(|s, tx| s.open_project(std::path::Path::new("/"), tx));
        assert!(rt.state.notice.as_ref().is_some_and(|n| n.error), "a folder outside git says so and changes nothing");
        assert_eq!(current_repo().unwrap().root.canonicalize().unwrap(), dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&other);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_options_narrow_the_matches() {
        let mut rt = loaded();
        let hits = |rt: &mut Runtime<Collate>, q: &str, flip: Option<fn(&mut Collate)>| {
            rt.update(|s, _| {
                s.search = TextEdit::new(q);
                if let Some(flip) = flip {
                    s.search_option(flip);
                }
                s.find();
            });
            let n = rt.state.hits.len();
            rt.update(|s, _| (s.find_case, s.find_word, s.find_regex) = (false, false, false));
            n
        };
        let files = rt.state.diff.files().len();
        assert_eq!(hits(&mut rt, "CHANGED", None), files * 2, "both added lines of each file, in any case");
        assert_eq!(hits(&mut rt, "CHANGED", Some(|s| s.find_case = true)), 0, "match case");
        assert_eq!(hits(&mut rt, "chang", Some(|s| s.find_word = true)), 0, "whole words only");
        assert_eq!(hits(&mut rt, r"also\s+changed\(4\d\)", Some(|s| s.find_regex = true)), files, "a regular expression");
        rt.update(|s, _| {
            s.find_regex = true;
            s.search = TextEdit::new("(");
            s.find();
        });
        assert!(rt.state.search_error.is_some() && rt.state.hits.is_empty(), "a bad pattern says so and finds nothing");
    }

    #[test]
    fn search_finds_lines_and_steps_through_them() {
        let mut rt = loaded();
        rt.update(|s, _| {
            s.search = TextEdit::new("also changed");
            s.find();
        });
        let n = rt.state.hits.len();
        assert_eq!(n, rt.state.diff.files().len(), "one hit per file");
        rt.update(|s, tx| s.next_hit(true, tx));
        rt.update(|s, tx| s.next_hit(true, tx));
        assert_eq!(rt.state.hit, Some(1));
        rt.update(|s, tx| s.next_hit(false, tx));
        rt.update(|s, tx| s.next_hit(false, tx));
        assert_eq!(rt.state.hit, Some(n - 1), "stepping back from the first wraps to the last");
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let last = n - 1;
        assert!(rt.presented(&format!("df{last}")).is_some() || rt.state.diff.visible_file() == last, "the last match's file is in view");
    }

    #[test]
    fn large_files_start_folded_and_folds_survive_a_refresh() {
        let mut rt = loaded();
        // Every file in the fixture changes 4 lines; call that large.
        rt.update(|s, tx| {
            s.settings.fold_large = 3;
            s.seen.clear();
            s.hashes.clear();
            let snap = snapshot();
            s.apply_snapshot(snap, tx);
        });
        assert!((0..rt.state.diff.files().len()).all(|i| rt.state.diff.collapsed(i)), "large files start folded");
        rt.update(|s, _| s.diff.set_collapsed(2, false));
        rt.update(|s, tx| {
            s.hashes.clear();
            let snap = snapshot();
            s.apply_snapshot(snap, tx);
        });
        assert!(!rt.state.diff.collapsed(2), "a file the reader opened stays open");
        assert!(rt.state.diff.collapsed(3), "and one left folded stays folded");
    }


    /// Refresh cost on a heavy review, by phase. `COLLATE_BENCH_REPO=<repo> cargo test --release
    /// bench_refresh -- --ignored --nocapture`; medians of 7 runs, in milliseconds.
    #[test]
    #[ignore]
    fn bench_refresh() {
        use std::time::Instant;
        let dir = std::env::var("COLLATE_BENCH_REPO").expect("COLLATE_BENCH_REPO");
        let repo = Repo::open(std::path::Path::new(&dir)).unwrap();
        let median = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (v[v.len() / 2], v[0], v[v.len() - 1])
        };
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let runs = 7;
        let (mut status, mut fp, mut snap_t, mut cold, mut warm, mut frame) = (vec![], vec![], vec![], vec![], vec![], vec![]);
        let (mut prep_cold, mut prep_warm) = (vec![], vec![]);
        let (mut memo_cold, mut memo_edit) = (vec![], vec![]);
        for _ in 0..runs {
            let t = Instant::now();
            let raw = repo.status_raw().unwrap();
            status.push(ms(t));
            let t = Instant::now();
            let _ = repo.fingerprint(&raw, "");
            fp.push(ms(t));
            let t = Instant::now();
            let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
            snap_t.push(ms(t));
            let env = Env { system_dark: true, scale: 2.0 };
            let mut state = init();
            state.settings = Settings::default();
            state.apply_settings(None);
            let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
            rt.frame(0.0);
            // Cold: every file diffed, as on launch (watcher thread), then shown (UI thread).
            let opts = rt.state.opts();
            let mut cache = DiffCache::new();
            let t = Instant::now();
            let p = prepare(snap, opts, &mut cache).unwrap();
            prep_cold.push(ms(t));
            let t = Instant::now();
            rt.update(move |s, tx| s.apply(p, tx));
            cold.push(ms(t));
            let t = Instant::now();
            rt.frame(1.0 / 60.0);
            frame.push(ms(t));
            // Warm: a refresh where one file changed (the cache serves the rest).
            let mut snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
            snap.changes[0].hash ^= 1;
            let t = Instant::now();
            let p = prepare(snap, opts, &mut cache).unwrap();
            prep_warm.push(ms(t));
            let t = Instant::now();
            rt.update(move |s, tx| s.apply(p, tx));
            warm.push(ms(t));
        }
        // A real refresh: the watcher's memory in place, one file edited on disk.
        let readme = std::path::Path::new(&dir).join("README.md");
        let original = std::fs::read(&readme).unwrap();
        for i in 0..runs {
            let mut memo = git::Memo::default();
            let raw = repo.status_raw().unwrap();
            let t = Instant::now();
            let _ = repo.snapshot_with(Scope::Uncommitted, &raw, None, &mut memo).unwrap();
            memo_cold.push(ms(t));
            std::fs::write(&readme, [&original[..], format!("\nbench edit {i}\n").as_bytes()].concat()).unwrap();
            let raw = repo.status_raw().unwrap();
            let t = Instant::now();
            let snap = repo.snapshot_with(Scope::Uncommitted, &raw, None, &mut memo).unwrap();
            memo_edit.push(ms(t));
            assert_eq!(snap.changes.iter().filter(|c| !c.reused).count(), 1, "only the edited file is read");
        }
        std::fs::write(&readme, &original).unwrap();
        let files = repo.snapshot(Scope::Uncommitted, &repo.status_raw().unwrap(), None).unwrap().changes.len();
        eprintln!("files {files}");
        for (name, v) in [
            ("git status", status),
            ("fingerprint", fp),
            ("snapshot, no memory (blobs)", snap_t),
            ("snapshot, first (memo)", memo_cold),
            ("snapshot, 1 file edited (memo)", memo_edit),
            ("diff, cold (watcher thread)", prep_cold),
            ("apply cold (UI thread)", cold),
            ("first frame", frame),
            ("diff, 1 file changed (watcher)", prep_warm),
            ("apply, 1 file changed (UI thread)", warm),
        ] {
            let (m, lo, hi) = median(v);
            eprintln!("{name:36} median {m:8.1} ms   min {lo:8.1}   max {hi:8.1}");
        }
    }
}

#[cfg(test)]
mod sizes {
    use super::*;

    /// Rough heap sizes of what a heavy review keeps, by part. `COLLATE_BENCH_REPO=<repo> cargo
    /// test --release heap_sizes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn heap_sizes() {
        let dir = std::env::var("COLLATE_BENCH_REPO").expect("COLLATE_BENCH_REPO");
        let repo = Repo::open(std::path::Path::new(&dir)).unwrap();
        let raw = repo.status_raw().unwrap();
        let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
        let p = prepare(snap, DiffOpts { context: 3, ignore_whitespace: false, moved: true, hide_low_signal: true }, &mut DiffCache::new()).unwrap();
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

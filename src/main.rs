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

use diff::{DiffMode, DiffState, DiffView, EDGE, FileDiff, GHOST_PAD, LineKind, LineRef, WordEdit, apply_edits, mark_moved, parse, word_edits};
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


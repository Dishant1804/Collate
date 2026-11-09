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
        let q = self.search.text.trim().to_owned();
        self.diff.set_search((!q.is_empty()).then_some(q));
        self.hits = self.diff.search_hits();
        self.hit = self.hit.filter(|&h| h < self.hits.len());
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


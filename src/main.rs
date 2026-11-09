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


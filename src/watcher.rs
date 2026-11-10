//! The watcher thread: it polls `git status` and listens to FSEvents, rereads the repository
//! when its fingerprint moves, diffs the files off the UI thread and hands them over.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::diff::{FileDiff, mark_moved, parse};
use crate::git::{self, Blob, Change, Scope, Snapshot};
use crate::{Collate, current_repo, hash_bytes, pdf_page, settings};

/// The FSEvents watcher for the open project, replaced on a switch.
pub static FILES: std::sync::Mutex<Option<notify::RecommendedWatcher>> = std::sync::Mutex::new(None);

/// Points the file watcher at `root` (or nowhere).
pub fn watch_project_files(root: Option<PathBuf>) {
    let w = root.zip(WATCHER.get().cloned()).and_then(|(root, tx)| watch_files(root, tx));
    EVENTS.store(w.is_some(), Ordering::Relaxed);
    *FILES.lock().unwrap() = w;
}

/// Tells the watcher to switch scope or base, or to reread now.
pub enum Cmd {
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
pub static EVENTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a change at `path` can change what Collate shows. Inside `.git`, only the index,
/// HEAD and refs matter; objects, logs, locks and Collate's own backups don't.
pub fn relevant(path: &std::path::Path, root: &std::path::Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return true };
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    match parts.next().as_deref() {
        Some(".git") => matches!(
            parts.next().as_deref(),
            Some("index" | "HEAD" | "refs" | "packed-refs" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "rebase-merge" | "rebase-apply")
        ),
        _ => true,
    }
}

/// Watches the working tree with FSEvents, nudging the watcher thread on each relevant
/// change. `None` if the platform watcher can't start; polling covers it then.
pub fn watch_files(root: std::path::PathBuf, tx: Sender<Cmd>) -> Option<notify::RecommendedWatcher> {
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

pub static WATCHER: OnceLock<Sender<Cmd>> = OnceLock::new();

/// The settings that shape the diffs, as the watcher thread reads them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffOpts {
    pub context: u32,
    pub ignore_whitespace: bool,
    pub moved: bool,
    pub hide_low_signal: bool,
    pub source_first: bool,
}

pub static OPTS: std::sync::Mutex<DiffOpts> = std::sync::Mutex::new(DiffOpts { context: 3, ignore_whitespace: false, moved: true, hide_low_signal: true, source_first: true });

/// Built diffs by content hash and the settings that shape them, so a refresh rediffs only
/// the files that changed.
pub type DiffCache = HashMap<(u64, u32, bool), FileDiff>;

/// A snapshot with its diffs built, ready for the UI.
pub struct Prepared {
    pub snap: Snapshot,
    pub files: Vec<FileDiff>,
    pub hidden: usize,
    pub opts: DiffOpts,
}

/// Diff a snapshot's files, off the UI thread: low-signal files left out when hidden, source
/// put before tests when asked, each file diffed unless `cache` has it (new ones spread across
/// cores), and moves marked across them all. `None` if a file the snapshot skipped as unchanged isn't in the cache: the
/// caller rereads everything.
pub fn prepare(mut snap: Snapshot, opts: DiffOpts, cache: &mut DiffCache) -> Option<Prepared> {
    let total = snap.changes.len();
    if opts.hide_low_signal {
        snap.changes.retain(|c| !(c.generated || settings::low_signal(&c.path)));
    }
    let hidden = total - snap.changes.len();
    // Stable, so each rank keeps the tree's order.
    if opts.source_first {
        snap.changes.sort_by_key(|c| settings::review_rank(&c.path, c.generated));
    }
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
pub static REFRESH_EVERY: AtomicU64 = AtomicU64::new(1000);

pub fn file_diff(c: &Change, context: u32, ignore_whitespace: bool) -> FileDiff {
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
            let mut f = FileDiff::between_opts(old_path, new_path, t(&c.old), t(&c.new), context, ignore_whitespace);
            // An SVG is text to review and a picture to look at: draw both sides above its lines.
            if c.path.to_ascii_lowercase().ends_with(".svg") {
                let side = |b: &Option<Blob>| match b {
                    Some(Blob::Text(t)) => Some((hash_bytes(t.as_bytes()), t.as_bytes().into())),
                    _ => None,
                };
                f.images = Some([side(&c.old), side(&c.new)]);
            }
            f
        }
        _ => {
            // Binary or too large on either side: let the patch parser make a binary entry,
            // with an image's bytes to draw when that's what it is.
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
            let mut f = parse(&patch).into_iter().next().unwrap_or_else(|| FileDiff::between(old_path, new_path, "", ""));
            let image = |b: &Option<Blob>| match b {
                // Keyed by the PDF's own bytes, so the same PDF is the same picture.
                Some(Blob::Image(bytes)) if bytes.starts_with(b"%PDF-") => Some((hash_bytes(bytes), pdf_page(bytes).map_or_else(|_| bytes.clone(), Into::into))),
                Some(Blob::Image(bytes)) => Some((hash_bytes(bytes), bytes.clone())),
                _ => None,
            };
            let sides = [image(&c.old), image(&c.new)];
            // Drawn only when every side there is an image: an image against a non-image (or a
            // too-large one) stays a note.
            let drawable = [&c.old, &c.new].iter().zip(&sides).all(|(b, i)| b.is_none() || i.is_some());
            if drawable && sides.iter().any(Option::is_some) {
                f.images = Some(sides);
            }
            f
        }
    }
}

pub fn refresh() -> bool {
    let Some(w) = WATCHER.get() else { return false };
    let _ = w.send(Cmd::Refresh);
    true
}

/// Polls `git status`, and rereads and posts a snapshot when the fingerprint, scope or base
/// changes.
pub fn watch(app: rune::AppHandle<Collate>, rx: mpsc::Receiver<Cmd>, mut scope: Scope, mut base: Option<String>, mut commit: Option<String>) {
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

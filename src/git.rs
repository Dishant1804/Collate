//! Reads a repository's changes with the `git` CLI: one `status` for the file list and one
//! `cat-file --batch` for every blob, so a refresh costs two processes however many files changed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Files larger than this show as "too large" instead of being diffed.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Which two versions of each file to compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// HEAD against the working tree: everything not yet committed.
    Uncommitted,
    /// HEAD against the index.
    Staged,
    /// The index against the working tree.
    Unstaged,
    /// Where this branch left a base (`main`) against the working tree: the whole branch, as a
    /// pull request would show it, plus what isn't committed yet.
    Branch,
    /// One past commit against its parent.
    History,
}

impl Scope {
    pub const ALL: [Scope; 5] = [Scope::Uncommitted, Scope::Staged, Scope::Unstaged, Scope::Branch, Scope::History];

    /// Views of committed history: nothing in them can be staged or discarded.
    pub fn read_only(self) -> bool {
        matches!(self, Scope::Branch | Scope::History)
    }
}

/// How much of a file's change is in the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staged {
    No,
    Partly,
    Fully,
}

/// One side of a change.
#[derive(Clone, Debug)]
pub enum Blob {
    Text(String),
    Binary,
    TooLarge(usize),
}

/// One changed file, with both texts read; diffing happens on the UI thread because
/// `FileDiff` isn't `Send`.
#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub old: Option<Blob>,
    pub new: Option<Blob>,
    /// Hash of both sides, so the UI rediffs only files whose contents changed.
    pub hash: u64,
    pub staged: Staged,
    /// Not tracked by git yet.
    pub untracked: bool,
    /// Hash of the path and its new side alone: what "viewed" remembers, the same in every view.
    pub new_hash: u64,
    /// Marked `linguist-generated` in `.gitattributes`: built output a review skips.
    pub generated: bool,
    /// Unchanged since the last snapshot: the sides hold no text, the diff is in the cache.
    pub reused: bool,
}

impl Change {
    /// Drops both sides' text, keeping which sides exist (and that they were text): what's
    /// left to know once the file is diffed.
    pub fn forget_text(&mut self) {
        for b in [&mut self.old, &mut self.new].into_iter().flatten() {
            if let Blob::Text(t) = b {
                *t = String::new();
            }
        }
    }
}

pub struct Snapshot {
    pub scope: Scope,
    pub branch: String,
    /// HEAD's commit id; `None` before the first commit.
    pub head: Option<String>,
    /// For `Scope::Branch`: the base asked for and the commit the branch left it at. For
    /// `Scope::History`: the commit shown, twice.
    pub base: Option<(String, String)>,
    /// Files with something staged, whatever the scope shows: what a commit would take.
    pub staged: usize,
    /// Files with changes not staged (untracked included), and files changed at all: the
    /// working copy's counts, whatever the scope shows.
    pub unstaged: usize,
    pub total: usize,
    /// Every changed path in the working copy, with whether it has staged and unstaged
    /// changes: for counts that leave out what the view hides.
    pub working: Vec<(String, bool, bool)>,
    pub changes: Vec<Change>,
}

pub struct Repo {
    pub root: PathBuf,
}

/// An entry of `git status --porcelain=v2`.
struct Entry {
    index: u8,
    worktree: u8,
    path: String,
    orig: Option<String>,
    untracked: bool,
    /// The file's blob ids in HEAD and the index, from the status; `None` where it has none.
    head_oid: Option<String>,
    index_oid: Option<String>,
}

/// A blob id from a status record; all zeros means the file isn't there.
fn oid(s: &str) -> Option<String> {
    (!s.bytes().all(|b| b == b'0')).then(|| s.to_owned())
}

/// What a refresh remembers about the files it read, so the next one reads only files that
/// changed: per path, what it was read from and what it hashed to, and each path's
/// `linguist-generated` attribute.
#[derive(Default)]
pub struct Memo {
    files: std::collections::HashMap<String, Remembered>,
    generated: std::collections::HashMap<String, bool>,
}

impl Memo {
    /// Forget everything: the next refresh reads every file (after a setting changes how
    /// files are diffed, or `.gitattributes` may have changed).
    pub fn clear(&mut self) {
        self.files.clear();
        self.generated.clear();
    }
}

#[derive(Clone, Copy)]
struct Remembered {
    /// Where both sides came from: blob ids, or a working file's size and modified time.
    identity: u64,
    has_old: bool,
    has_new: bool,
    hash: u64,
    new_hash: u64,
}

/// Where one side of a file comes from.
enum Source {
    Missing,
    /// A `git cat-file` spec: a blob id, or `<commit>:<path>`.
    Git(String),
    Worktree,
}

/// One file to read for a snapshot.
struct Plan {
    path: String,
    orig: Option<String>,
    old: Source,
    new: Source,
    staged: Staged,
    untracked: bool,
    /// Whether `old` and `new` name content that can't change under the same name (blob ids,
    /// commits), so the identity is safe to remember.
    stable: bool,
}

impl Repo {
    pub fn open(dir: &Path) -> Result<Repo, String> {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().map_err(|e| format!("can't run git: {e}"))?;
        if !out.status.success() {
            return Err(format!("{} is not inside a git repository", dir.display()));
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        Ok(Repo { root: PathBuf::from(root) })
    }

    pub fn name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.root.display().to_string())
    }

    fn git(&self) -> Command {
        let mut c = Command::new("git");
        // Never take the index lock: an editor or a commit in another app must not fail because we looked.
        c.arg("--no-optional-locks").arg("-C").arg(&self.root);
        c
    }

    /// Raw `status` output, also the cheap half of the change fingerprint.
    pub fn status_raw(&self) -> Result<Vec<u8>, String> {
        let out = self
            .git()
            .args(["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all", "--no-renames"])
            .output()
            .map_err(|e| format!("git status: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        Ok(out.stdout)
    }

    /// A fingerprint of everything a refresh would read: the status (which carries the HEAD
    /// and index blob ids) plus each changed file's size and mtime in the working tree, and
    /// `salt` (a branch review's merge base).
    pub fn fingerprint(&self, raw: &[u8], salt: &str) -> u64 {
        let mut h = DefaultHasher::new();
        raw.hash(&mut h);
        salt.hash(&mut h);
        for e in parse_status(raw).2 {
            if let Ok(m) = std::fs::metadata(self.root.join(&e.path)) {
                m.len().hash(&mut h);
                m.modified().ok().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Reads both sides of every changed file for `scope`. A branch review compares against
    /// `base`'s merge base with HEAD; `merge_base` is it, already resolved.
    #[cfg(test)]
    pub fn snapshot(&self, scope: Scope, raw: &[u8], base: Option<(&str, &str)>) -> Result<Snapshot, String> {
        self.snapshot_with(scope, raw, base, &mut Memo::default())
    }

    /// [`Repo::snapshot`], reading only the files that changed since the snapshot `memo`
    /// remembers; the rest come back with their hashes and no text (their diffs are cached).
    pub fn snapshot_with(&self, scope: Scope, raw: &[u8], base: Option<(&str, &str)>, memo: &mut Memo) -> Result<Snapshot, String> {
        let (branch, head, mut entries) = parse_status(raw);
        let staged = entries.iter().filter(|e| !e.untracked && e.index != b'.').count();
        let unstaged = entries.iter().filter(|e| e.untracked || e.worktree != b'.').count();
        let total = entries.len();
        let working: Vec<(String, bool, bool)> = entries.iter().map(|e| (e.path.clone(), !e.untracked && e.index != b'.', e.untracked || e.worktree != b'.')).collect();
        let base_commit = match (scope, base) {
            (Scope::Branch | Scope::History, Some((_, c))) => Some(c),
            (Scope::Branch, None) => return Err("Choose a base branch to compare against.".into()),
            (Scope::History, None) => return Err("This repository has no commits yet.".into()),
            _ => None,
        };
        let plans: Vec<Plan> = if let (Scope::History, Some(sha)) = (scope, base_commit) {
            self.commit_plans(sha)?
        } else {
            if let (Scope::Branch, Some(mb)) = (scope, base_commit) {
                // Every file that differs from the merge base, committed on this branch or not.
                let out = self.git().args(["diff", "--name-only", "-z", "--no-renames", mb]).output().map_err(|e| format!("git diff: {e}"))?;
                if !out.status.success() {
                    return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
                }
                let known: std::collections::HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
                for p in String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()) {
                    if !known.contains(p) {
                        entries.push(Entry { index: b'.', worktree: b'.', path: p.to_owned(), orig: None, untracked: false, head_oid: None, index_oid: None });
                    }
                }
                entries.sort_by(|a, b| tree_order(&a.path, &b.path));
            }
            entries
                .into_iter()
                .filter(|e| match scope {
                    Scope::Uncommitted | Scope::Branch | Scope::History => true,
                    Scope::Staged => !e.untracked && e.index != b'.',
                    Scope::Unstaged => e.untracked || e.worktree != b'.',
                })
                .map(|e| {
                    let old_path = e.orig.as_deref().unwrap_or(&e.path);
                    let by_id = |id: &Option<String>| id.clone().map_or(Source::Missing, Source::Git);
                    // A conflicted file has no single index blob: read it by path, and don't
                    // remember it (the index can change under the same spec).
                    let conflicted = !e.untracked && e.head_oid.is_none() && e.index_oid.is_none() && e.index == b'M';
                    // Old side: HEAD for Uncommitted and Staged, the index for Unstaged, the
                    // merge base for Branch. New side: the index for Staged, the working tree
                    // otherwise. By blob id where the status gives one: no tree lookups.
                    let old = match (scope, base_commit) {
                        _ if e.untracked => Source::Missing,
                        (Scope::Branch, Some(mb)) => Source::Git(format!("{mb}:{old_path}")),
                        _ if conflicted => Source::Git(if scope == Scope::Unstaged { format!(":{old_path}") } else { format!("HEAD:{old_path}") }),
                        (Scope::Unstaged, _) => by_id(&e.index_oid),
                        _ => by_id(&e.head_oid),
                    };
                    let new = match scope {
                        Scope::Staged if conflicted => Source::Git(format!(":{}", e.path)),
                        Scope::Staged => by_id(&e.index_oid),
                        _ => Source::Worktree,
                    };
                    let staged = match (e.untracked, e.index, e.worktree) {
                        (true, ..) | (_, b'.', _) => Staged::No,
                        (_, _, b'.') => Staged::Fully,
                        _ => Staged::Partly,
                    };
                    Plan { path: e.path, orig: e.orig, old, new, staged, untracked: e.untracked, stable: !conflicted }
                })
                .collect()
        };
        let mut changes = self.read_plans(plans, memo)?;
        // A branch review lists files git named; an identical pair (a change undone) drops out.
        if scope == Scope::Branch {
            changes.retain(|c| c.reused || !same(&c.old, &c.new));
        }
        let base = base.map(|(name, c)| (name.to_owned(), c.to_owned()));
        Ok(Snapshot { scope, branch, head, base, staged, unstaged, total, working, changes })
    }


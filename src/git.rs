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
    /// A PNG, JPEG or PDF, which the app can draw (a PDF by its first page).
    Image(std::sync::Arc<[u8]>),
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
    /// Both sides of a merge changed it and git couldn't combine them: the working file holds
    /// conflict markers.
    pub conflicted: bool,
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
    /// Commits ahead of and behind the upstream, when the branch has one.
    pub ahead_behind: Option<(usize, usize)>,
    /// A merge, rebase, cherry-pick or revert git stopped in the middle of.
    pub operation: Option<Operation>,
}

/// A multi-step operation git stopped in, waiting for its conflicts to be settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Operation {
    /// The git command that continues or aborts it.
    pub fn command(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
    }

    pub fn doing(self) -> &'static str {
        match self {
            Operation::Merge => "Merging",
            Operation::Rebase => "Rebasing",
            Operation::CherryPick => "Cherry-picking",
            Operation::Revert => "Reverting",
        }
    }
}

pub struct Repo {
    pub root: PathBuf,
}

/// One line of a blame: the commit that last changed it, and the line.
#[derive(Clone, Debug, PartialEq)]
pub struct Blamed {
    pub id: String,
    pub author: String,
    /// Seconds since 1970.
    pub time: i64,
    pub summary: String,
    pub text: String,
}

/// `git blame --porcelain`: each line follows a header naming its commit, and a commit's
/// details come once, after its first header.
fn parse_blame(out: &str) -> Vec<Blamed> {
    let mut known: std::collections::HashMap<String, (String, i64, String)> = Default::default();
    let mut lines = Vec::new();
    let mut id = String::new();
    for l in out.lines() {
        if let Some(text) = l.strip_prefix('\t') {
            let (author, time, summary) = known.get(&id).cloned().unwrap_or_default();
            lines.push(Blamed { id: id.clone(), author, time, summary, text: text.to_owned() });
            continue;
        }
        let (key, value) = l.split_once(' ').unwrap_or((l, ""));
        match key {
            "author" => known.entry(id.clone()).or_default().0 = value.to_owned(),
            "author-time" => known.entry(id.clone()).or_default().1 = value.parse().unwrap_or(0),
            "summary" => known.entry(id.clone()).or_default().2 = value.to_owned(),
            // A header: the commit's id (SHA-1 or SHA-256), then line numbers.
            k if matches!(k.len(), 40 | 64) && k.bytes().all(|b| b.is_ascii_hexdigit()) => id = k.to_owned(),
            _ => {}
        }
    }
    lines
}

/// One entry of `git stash list`.
#[derive(Clone, Debug)]
pub struct Stash {
    pub name: String,
    pub id: String,
    pub message: String,
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
    conflicted: bool,
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

    /// The repository's git folder: `.git`, or where a worktree's `.git` file points.
    fn git_dir(&self) -> PathBuf {
        let dot = self.root.join(".git");
        match std::fs::read_to_string(&dot) {
            Ok(link) => link.strip_prefix("gitdir:").map_or(dot.clone(), |p| self.root.join(p.trim())),
            Err(_) => dot,
        }
    }

    /// The operation git is in the middle of, from the files it keeps while it waits.
    pub fn operation(&self) -> Option<Operation> {
        let d = self.git_dir();
        Some(if d.join("rebase-merge").exists() || d.join("rebase-apply").exists() {
            Operation::Rebase
        } else if d.join("MERGE_HEAD").exists() {
            Operation::Merge
        } else if d.join("CHERRY_PICK_HEAD").exists() {
            Operation::CherryPick
        } else if d.join("REVERT_HEAD").exists() {
            Operation::Revert
        } else {
            return None;
        })
    }

    /// Carry on with `op` (`continue`), give it up (`abort`), or, in a rebase, drop the commit
    /// it stopped at (`skip`).
    pub fn resume(&self, op: Operation, how: &str) -> Result<String, String> {
        self.run(&[op.command(), &format!("--{how}")], None)
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
        // Starting or finishing a merge can leave the status as it was.
        self.operation().hash(&mut h);
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
                    Plan { path: e.path, orig: e.orig, old, new, staged, untracked: e.untracked, stable: !conflicted, conflicted }
                })
                .collect()
        };
        let mut changes = self.read_plans(plans, memo)?;
        // A branch review lists files git named; an identical pair (a change undone) drops out.
        if scope == Scope::Branch {
            changes.retain(|c| c.reused || !same(&c.old, &c.new));
        }
        let base = base.map(|(name, c)| (name.to_owned(), c.to_owned()));
        Ok(Snapshot { scope, branch, head, base, staged, unstaged, total, working, changes, ahead_behind: ahead_behind(raw), operation: self.operation() })
    }

    /// The files commit `sha` changed, each against its first parent (nothing, for a root
    /// commit); or for a range `a..b`, the files that differ between the two. Commits don't
    /// change, so every one of them can be remembered.
    fn commit_plans(&self, sha: &str) -> Result<Vec<Plan>, String> {
        let (old, new) = match sha.split_once("..") {
            Some((a, b)) => (a.to_owned(), b.to_owned()),
            None => (format!("{sha}^"), sha.to_owned()),
        };
        let mut cmd = self.git();
        cmd.args(["diff-tree", "--no-commit-id", "-r", "-z", "--name-only", "--no-renames"]);
        match sha.split_once("..") {
            Some((a, b)) => cmd.args([a, b]),
            None => cmd.args(["--root", sha]),
        };
        let out = cmd
            .output()
            .map_err(|e| format!("git diff-tree: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()).map(str::to_owned).collect();
        paths.sort_by(|a, b| tree_order(a, b));
        Ok(paths
            .into_iter()
            .map(|p| Plan {
                old: Source::Git(format!("{old}:{p}")),
                new: Source::Git(format!("{new}:{p}")),
                path: p,
                orig: None,
                staged: Staged::No,
                untracked: false,
                stable: true,
                conflicted: false,
            })
            .collect())
    }

    /// Reads the files `plans` name, skipping those `memo` saw unchanged: one `cat-file` pass
    /// (split across processes when there are many) for the blobs, and the working files.
    fn read_plans(&self, plans: Vec<Plan>, memo: &mut Memo) -> Result<Vec<Change>, String> {
        let identity = |p: &Plan| -> Option<u64> {
            let mut h = DefaultHasher::new();
            for side in [&p.old, &p.new] {
                match side {
                    Source::Missing => 0u8.hash(&mut h),
                    Source::Git(spec) => (1u8, spec).hash(&mut h),
                    Source::Worktree => {
                        let m = std::fs::symlink_metadata(self.root.join(&p.path)).ok();
                        (2u8, m.as_ref().map(|m| m.len()), m.and_then(|m| m.modified().ok())).hash(&mut h);
                    }
                }
            }
            p.stable.then(|| h.finish())
        };
        let ids: Vec<Option<u64>> = plans.iter().map(identity).collect();
        let fresh: Vec<bool> = plans.iter().zip(&ids).map(|(p, id)| id.is_none() || memo.files.get(&p.path).is_none_or(|r| Some(r.identity) != *id)).collect();
        // The blobs to read: (plan, is old side) for every git-backed side of a fresh plan.
        let mut wanted: Vec<(usize, bool)> = Vec::new();
        let mut specs: Vec<String> = Vec::new();
        for (i, p) in plans.iter().enumerate().filter(|(i, _)| fresh[*i]) {
            for (is_old, side) in [(true, &p.old), (false, &p.new)] {
                if let Source::Git(spec) = side {
                    wanted.push((i, is_old));
                    specs.push(spec.clone());
                }
            }
        }
        let mut blobs = self.cat_files_parallel(&specs)?.into_iter();
        let mut read: Vec<(Option<Blob>, Option<Blob>)> = (0..plans.len()).map(|_| (None, None)).collect();
        for &(i, is_old) in &wanted {
            let b = blobs.next().flatten();
            if is_old { read[i].0 = b } else { read[i].1 = b }
        }
        // `linguist-generated`, asked of git only for paths not seen before (all of them again
        // when a `.gitattributes` changed).
        if plans.iter().zip(&fresh).any(|(p, &f)| f && p.path.rsplit('/').next() == Some(".gitattributes")) {
            memo.generated.clear();
        }
        let unknown: Vec<&str> = plans.iter().map(|p| p.path.as_str()).filter(|p| !memo.generated.contains_key(*p)).collect();
        if !unknown.is_empty() {
            let marked = self.generated(unknown.iter().copied());
            for p in unknown {
                memo.generated.insert(p.to_owned(), marked.contains(p));
            }
        }
        let mut out = Vec::with_capacity(plans.len());
        for (i, p) in plans.into_iter().enumerate() {
            let generated = memo.generated.get(&p.path).copied().unwrap_or(false);
            if !fresh[i] {
                let r = memo.files[&p.path];
                // Known: which sides exist and the hashes; no text (the diff is cached).
                let blank = |has: bool| has.then(|| Blob::Text(String::new()));
                out.push(Change { path: p.path, old_path: p.orig, old: blank(r.has_old), new: blank(r.has_new), hash: r.hash, staged: p.staged, untracked: p.untracked, new_hash: r.new_hash, generated, reused: true, conflicted: p.conflicted });
                continue;
            }
            let (old, git_new) = std::mem::take(&mut read[i]);
            let new = match p.new {
                Source::Worktree => self.read_worktree(&p.path),
                _ => git_new,
            };
            if old.is_none() && new.is_none() {
                memo.files.remove(&p.path);
                continue;
            }
            let mut h = DefaultHasher::new();
            p.path.hash(&mut h);
            hash_blob(&new, &mut h);
            let new_hash = h.finish();
            hash_blob(&old, &mut h);
            let hash = h.finish();
            match ids[i] {
                Some(identity) => {
                    memo.files.insert(p.path.clone(), Remembered { identity, has_old: old.is_some(), has_new: new.is_some(), hash, new_hash });
                }
                None => {
                    memo.files.remove(&p.path);
                }
            }
            out.push(Change { path: p.path, old_path: p.orig, old, new, hash, staged: p.staged, untracked: p.untracked, new_hash, generated, reused: false, conflicted: p.conflicted });
        }
        Ok(out)
    }

    /// [`Repo::cat_files`] split across up to four `git cat-file` processes when there's a
    /// lot to read: inflating blobs is git's work, and it's one core per process.
    fn cat_files_parallel(&self, specs: &[String]) -> Result<Vec<Option<Blob>>, String> {
        const PER_PROCESS: usize = 64;
        let parts = specs.len().div_ceil(PER_PROCESS).clamp(1, 4);
        if parts == 1 {
            return self.cat_files(specs);
        }
        let size = specs.len().div_ceil(parts);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = specs.chunks(size).map(|chunk| scope.spawn(move || self.cat_files(chunk))).collect();
            let mut out = Vec::with_capacity(specs.len());
            for j in jobs {
                out.extend(j.join().map_err(|_| "git cat-file reader panicked".to_owned())??);
            }
            Ok(out)
        })
    }

    fn read_worktree(&self, path: &str) -> Option<Blob> {
        let p = self.root.join(path);
        let meta = std::fs::symlink_metadata(&p).ok()?;
        if meta.file_type().is_symlink() {
            return std::fs::read_link(&p).ok().map(|t| Blob::Text(t.to_string_lossy().into_owned()));
        }
        if !meta.is_file() {
            return None;
        }
        if meta.len() as usize > MAX_BYTES {
            return Some(Blob::TooLarge(meta.len() as usize));
        }
        std::fs::read(&p).ok().map(classify)
    }

    /// Reads every spec (`HEAD:path`, `:path`) through one `git cat-file --batch`; `None` for a
    /// spec git reports missing (an added file has no HEAD blob).
    fn cat_files(&self, specs: &[String]) -> Result<Vec<Option<Blob>>, String> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let mut child = self
            .git()
            .args(["cat-file", "--batch", "--buffer"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("git cat-file: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let input: String = specs.iter().map(|s| format!("{s}\n")).collect();
        // Write from another thread so a large output can't fill the pipe while we're still writing.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut blobs = Vec::with_capacity(specs.len());
        let mut header = String::new();
        for _ in specs {
            header.clear();
            if out.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
                return Err("git cat-file ended early".into());
            }
            let fields: Vec<&str> = header.trim_end().split(' ').collect();
            match fields.as_slice() {
                [_, kind, size] => {
                    let size: usize = size.parse().map_err(|_| format!("bad cat-file header: {header}"))?;
                    let mut buf = vec![0; size + 1];
                    out.read_exact(&mut buf).map_err(|e| e.to_string())?;
                    buf.pop();
                    blobs.push(match *kind {
                        "blob" if size > MAX_BYTES => Some(Blob::TooLarge(size)),
                        "blob" => Some(classify(buf)),
                        // A submodule shows as a commit; there's no text to diff.
                        _ => None,
                    });
                }
                _ => blobs.push(None),
            }
        }
        let _ = writer.join();
        let _ = child.wait();
        Ok(blobs)
    }
}

/// Changing the repository: staging, unstaging, discarding and committing. Each runs one git
/// command and reports git's own message when it refuses.
impl Repo {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        self.run_env(args, stdin, &[])
    }

    /// [`Repo::run`] with environment variables set, as `GIT_INDEX_FILE`.
    fn run_env(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &Path)]) -> Result<String, String> {
        let mut c = Command::new("git");
        c.envs(env.iter().map(|(k, v)| (k, v)));
        // No terminal to ask on: a fetch or push that needs a password fails instead of waiting.
        // Nor an editor: continuing a merge or rebase keeps the message git prepared.
        c.arg("-C").arg(&self.root).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true").stdout(Stdio::piped()).stderr(Stdio::piped());
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = c.spawn().map_err(|e| format!("can't run git: {e}"))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).map_err(|e| e.to_string())?;
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            Err(if err.is_empty() { format!("git {} failed", args[0]) } else { err.lines().take(3).collect::<Vec<_>>().join(" ") })
        }
    }

    /// Applies `patch` to the index (`cached`) or the working tree, backwards when `reverse`.
    pub fn apply(&self, patch: &str, cached: bool, reverse: bool) -> Result<(), String> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if cached {
            args.push("--cached");
        }
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.run(&args, Some(patch)).map(drop)
    }

    pub fn stage(&self, path: &str) -> Result<(), String> {
        self.run(&["add", "-A", "--", path], None).map(drop)
    }

    /// Stages every change, untracked files included (GitKraken's Stage All).
    pub fn stage_all(&self) -> Result<(), String> {
        self.run(&["add", "-A"], None).map(drop)
    }

    /// Unstages everything, leaving the working tree as it is.
    pub fn unstage_all(&self) -> Result<(), String> {
        let has_head = self.run(&["rev-parse", "--verify", "-q", "HEAD"], None).is_ok();
        if has_head {
            self.run(&["reset", "-q"], None).map(drop)
        } else {
            self.run(&["rm", "-r", "-q", "--cached", "."], None).map(drop)
        }
    }

    pub fn unstage(&self, path: &str) -> Result<(), String> {
        let has_head = self.run(&["rev-parse", "--verify", "-q", "HEAD"], None).is_ok();
        if has_head {
            self.run(&["restore", "--staged", "--", path], None).map(drop)
        } else {
            self.run(&["rm", "-q", "--cached", "--", path], None).map(drop)
        }
    }

    /// Undoes `path`'s change in the working tree: back to HEAD (and out of the index) for
    /// `Scope::Uncommitted`, back to the index for `Scope::Unstaged`. A file git doesn't
    /// have there is removed. The working copy is backed up first; returns where.
    pub fn discard(&self, path: &str, scope: Scope, in_base: bool) -> Result<PathBuf, String> {
        let backup = self.backup(path)?;
        match (scope, in_base) {
            (Scope::Uncommitted, true) => self.run(&["restore", "--source=HEAD", "--staged", "--worktree", "--", path], None).map(drop)?,
            (Scope::Unstaged, true) => self.run(&["restore", "--worktree", "--", path], None).map(drop)?,
            (Scope::Uncommitted | Scope::Unstaged, false) => {
                if scope == Scope::Uncommitted {
                    self.run(&["rm", "-q", "--cached", "--ignore-unmatch", "--", path], None)?;
                }
                let p = self.root.join(path);
                if p.exists() {
                    std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                }
            }
            _ => return Err("Only uncommitted changes can be discarded.".into()),
        }
        Ok(backup)
    }

    /// Refreshes the index's cached file stats so later `status` calls are cheap again. It
    /// takes the index lock for a moment; if another git holds it, nothing happens.
    pub fn refresh_index(&self) {
        let _ = self.run(&["update-index", "-q", "--refresh"], None);
    }

    /// The paths among `paths` that `.gitattributes` marks `linguist-generated`.
    pub fn generated<'a>(&self, paths: impl Iterator<Item = &'a str>) -> std::collections::HashSet<String> {
        let input: String = paths.map(|p| format!("{p}\0")).collect();
        if input.is_empty() {
            return Default::default();
        }
        let out = self.run(&["check-attr", "-z", "--stdin", "linguist-generated"], Some(&input)).unwrap_or_default();
        let fields: Vec<&str> = out.split('\0').collect();
        fields.chunks(3).filter(|c| c.len() == 3 && matches!(c[2], "set" | "true")).map(|c| c[0].to_owned()).collect()
    }

    /// Puts the files in a backup made by [`Repo::backup`] back in the working tree; returns
    /// how many it restored.
    pub fn restore(&self, backup: &Path) -> Result<usize, String> {
        let mut n = 0;
        let mut stack = vec![backup.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
                let p = entry.map_err(|e| e.to_string())?.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(backup).map_err(|e| e.to_string())?;
                    let dst = self.root.join(rel);
                    std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
                    std::fs::copy(&p, &dst).map_err(|e| format!("restoring {}: {e}", rel.display()))?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Copies `path`'s working copy into `.git/collate-backup/<time>/` before it's changed, so a
    /// discard can be undone.
    pub fn backup(&self, path: &str) -> Result<PathBuf, String> {
        let git_dir = PathBuf::from(self.run(&["rev-parse", "--absolute-git-dir"], None)?);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let dir = git_dir.join("collate-backup").join(stamp.to_string());
        let src = self.root.join(path);
        if src.is_file() {
            let dst = dir.join(path);
            std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::copy(&src, &dst).map_err(|e| format!("backing up {path}: {e}"))?;
        }
        Ok(dir)
    }

    /// Commits what's staged, or with `amend` rewrites the last commit with it; returns git's
    /// one-line summary.
    pub fn commit(&self, message: &str, amend: bool) -> Result<String, String> {
        let mut args = vec!["commit", "-q", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        self.run(&args, Some(message))?;
        self.run(&["log", "-1", "--format=%h %s"], None)
    }

    /// The whole working tree as a commit on HEAD, untracked files too (not ignored ones),
    /// without moving HEAD, the index or a file: it's staged into a throwaway copy of the index.
    pub fn snapshot_tree(&self, message: &str) -> Result<String, String> {
        let index = PathBuf::from(self.run(&["rev-parse", "--path-format=absolute", "--git-path", "index"], None)?);
        let temp = index.with_file_name(format!("collate-index-{}-{:?}", std::process::id(), std::thread::current().id()));
        // A copy, not an empty one: git trusts its saved file times and rereads only what changed.
        if index.exists() {
            std::fs::copy(&index, &temp).map_err(|e| format!("couldn't copy the index: {e}"))?;
        }
        let tree = self.run_env(&["add", "-A"], None, &[("GIT_INDEX_FILE", &temp)]).and_then(|_| self.run_env(&["write-tree"], None, &[("GIT_INDEX_FILE", &temp)]));
        let _ = std::fs::remove_file(&temp);
        let tree = tree?;
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
        let head = self.run(&["rev-parse", "--verify", "-q", "HEAD"], None).ok();
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        self.run(&args, None)
    }

    /// Saves the working tree as a checkpoint named `label`, under `refs/collate/checkpoints`
    /// so it outlives garbage collection; returns its commit.
    pub fn checkpoint(&self, label: &str) -> Result<String, String> {
        let sha = self.snapshot_tree(label)?;
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.run(&["update-ref", &format!("refs/collate/checkpoints/{secs}-{}", &sha[..12.min(sha.len())]), &sha], None)?;
        Ok(sha)
    }

    /// The checkpoints, newest first: commit, seconds since 1970, label.
    pub fn checkpoints(&self) -> Result<Vec<(String, i64, String)>, String> {
        let out = self.run(&["for-each-ref", "--sort=-committerdate", "--format=%(objectname)%09%(committerdate:unix)%09%(subject)", "refs/collate/checkpoints"], None)?;
        Ok(out.lines().filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            Some((f.next()?.to_owned(), f.next()?.parse().ok()?, f.next().unwrap_or("").to_owned()))
        }).collect())
    }

    /// Puts the working tree back as checkpoint `sha` had it, after saving it as it is now as
    /// another checkpoint (returned), so the restore can be undone. HEAD and the index stay:
    /// what was staged shows as changed again if the files differ from it.
    pub fn restore_checkpoint(&self, sha: &str) -> Result<String, String> {
        let now = self.checkpoint("Before restoring a checkpoint")?;
        // Files made since the checkpoint go; git restore only writes the checkpoint's own.
        let added = self.run(&["diff", "--name-only", "--no-renames", "--diff-filter=A", "-z", sha, &now], None)?;
        for path in added.split('\0').filter(|p| !p.is_empty()) {
            match std::fs::remove_file(self.root.join(path)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("couldn't remove {path}: {e}")),
                _ => {}
            }
        }
        if !self.run(&["ls-tree", "-r", "--name-only", sha], None)?.is_empty() {
            self.run(&["restore", "--source", sha, "--worktree", "--", ":/"], None)?;
        }
        Ok(now)
    }

    /// What a commit would record: the staged diff against HEAD, or with `amend` against the
    /// last commit's parent, as the amended commit would be.
    pub fn staged_diff(&self, amend: bool) -> Result<String, String> {
        let mut args = vec!["diff", "--cached", "--no-color", "--no-ext-diff"];
        if amend {
            args.push("HEAD^");
        }
        self.run(&args, None)
    }

    /// The last `n` commit subjects, newest first: the repository's style for a new one.
    pub fn recent_subjects(&self, n: usize) -> Result<Vec<String>, String> {
        Ok(self.run(&["log", &format!("-{n}"), "--format=%s"], None)?.lines().map(str::to_owned).collect())
    }

    /// The last commit's whole message, to start an amend from.
    pub fn last_message(&self) -> Option<String> {
        self.run(&["log", "-1", "--format=%B"], None).ok()
    }

    /// Stashes every change, untracked files included.
    pub fn stash(&self, message: &str) -> Result<String, String> {
        let mut args = vec!["stash", "push", "-u"];
        if !message.is_empty() {
            args.extend(["-m", message]);
        }
        self.run(&args, None)
    }

    /// The stashes, newest first: their name (`stash@{0}`), commit and message.
    pub fn stashes(&self) -> Vec<Stash> {
        let out = self.run(&["stash", "list", "--format=%gd%x1f%H%x1f%gs"], None).unwrap_or_default();
        out.lines().filter_map(|l| {
            let mut f = l.splitn(3, '\x1f');
            Some(Stash { name: f.next()?.to_owned(), id: f.next()?.to_owned(), message: f.next()?.to_owned() })
        }).collect()
    }

    /// Applies a stash (`pop` also drops it), or with `how` "drop" only drops it.
    pub fn stash_do(&self, how: &str, name: &str) -> Result<String, String> {
        self.run(&["stash", how, "-q", name], None)
    }

    /// Fetches every remote, dropping branches deleted there.
    pub fn fetch(&self) -> Result<String, String> {
        self.run(&["fetch", "--all", "--prune", "-q"], None)
    }

    /// Pulls the upstream, only when that needs no merge.
    pub fn pull(&self) -> Result<String, String> {
        self.run(&["pull", "--ff-only", "-q"], None)
    }

    /// Push over the remote branch after a rebase or amend, but only if it's still where this
    /// clone last saw it: someone else's push in between makes it fail rather than vanish.
    pub fn force_push(&self) -> Result<String, String> {
        self.run(&["push", "-q", "--force-with-lease"], None)
    }

    /// Pushes the branch to its upstream; a branch without one goes to `origin` under its own
    /// name and tracks it from then on.
    pub fn push(&self) -> Result<String, String> {
        if self.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"], None).is_ok() {
            self.run(&["push", "-q"], None)
        } else {
            self.run(&["push", "-q", "-u", "origin", "HEAD"], None)
        }
    }

    /// The upstream's name and the commit this branch left it at: from there to HEAD is what a
    /// push sends, without undoing what others pushed since (as `@{u}..HEAD` as trees would).
    pub fn unpushed(&self) -> Result<(String, String), String> {
        let upstream = self.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"], None).map_err(|_| "This branch has no upstream yet; push it once to track one.".to_owned())?;
        let base = self.merge_base(&upstream)?;
        Ok((upstream, base))
    }

    /// Switches to `branch`; a remote one (`origin/x`) checks out a local `x` that tracks it.
    pub fn switch(&self, branch: &str) -> Result<String, String> {
        let remote = self.run(&["remote"], None).unwrap_or_default();
        let local = remote.lines().find_map(|r| branch.strip_prefix(r).and_then(|b| b.strip_prefix('/'))).unwrap_or(branch);
        self.run(&["switch", "-q", local], None)
    }

    /// Makes branch `name` at `at` (HEAD when `None`) and, with `switch`, moves onto it.
    pub fn new_branch(&self, name: &str, at: Option<&str>, switch: bool) -> Result<String, String> {
        let mut args = if switch { vec!["switch", "-q", "-c", name] } else { vec!["branch", name] };
        args.extend(at);
        self.run(&args, None)
    }

    /// Applies commit `id`'s change on top of HEAD as a new commit.
    /// Merge `branch` into the current one. Stopped on conflicts, the error says so; the
    /// app then shows the merge waiting.
    pub fn merge(&self, branch: &str) -> Result<String, String> {
        self.stopped(self.run(&["merge", "--no-edit", branch], None))
    }

    /// Tags, newest first.
    pub fn tags(&self) -> Vec<String> {
        self.run(&["tag", "--sort=-creatordate"], None).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    /// Tag commit `at`: annotated when there's a message, else a plain name.
    pub fn tag(&self, name: &str, at: &str, message: &str) -> Result<String, String> {
        if message.is_empty() { self.run(&["tag", name, at], None) } else { self.run(&["tag", "-a", name, "-m", message, at], None) }
    }

    pub fn push_tag(&self, name: &str) -> Result<String, String> {
        self.run(&["push", "-q", "origin", &format!("refs/tags/{name}")], None)
    }

    /// Delete a tag here, or from `origin` too.
    pub fn delete_tag(&self, name: &str, remote: bool) -> Result<String, String> {
        if remote {
            self.run(&["push", "-q", "origin", "--delete", &format!("refs/tags/{name}")], None)?;
        }
        self.run(&["tag", "-d", name], None)
    }

    pub fn rename_branch(&self, from: &str, to: &str) -> Result<String, String> {
        self.run(&["branch", "-m", from, to], None)
    }

    /// Delete a local branch, merged or not: the app asks first, saying which.
    pub fn delete_branch(&self, name: &str) -> Result<String, String> {
        self.run(&["branch", "-D", name], None)
    }

    /// Whether every commit of `branch` is also in HEAD, so deleting it loses nothing.
    pub fn merged(&self, branch: &str) -> bool {
        self.run(&["merge-base", "--is-ancestor", branch, "HEAD"], None).is_ok()
    }

    /// Replay the current branch's own commits on top of `onto`.
    pub fn rebase(&self, onto: &str) -> Result<String, String> {
        self.stopped(self.run(&["rebase", onto], None))
    }

    /// `result`, unless git stopped for conflicts, which is the reader's next step rather than
    /// a failure to report in git's words.
    fn stopped(&self, result: Result<String, String>) -> Result<String, String> {
        match (result, self.operation()) {
            (Err(_), Some(op)) => Err(format!("The {} stopped on conflicts: settle them, then continue", op.noun())),
            (r, _) => r,
        }
    }

    pub fn cherry_pick(&self, id: &str) -> Result<String, String> {
        self.run(&["cherry-pick", id], None)
    }

    /// Commits the inverse of commit `id`.
    pub fn revert(&self, id: &str) -> Result<String, String> {
        self.run(&["revert", "--no-edit", id], None)
    }

    /// Moves the branch to `id`: `how` is "soft" (keep the changes staged), "mixed" (keep them
    /// unstaged) or "hard" (throw them away).
    pub fn reset(&self, how: &str, id: &str) -> Result<String, String> {
        self.run(&["reset", "-q", &format!("--{how}"), id], None)
    }

    /// Folds what's staged into commit `id`, an earlier commit of this branch: commits it as a
    /// fixup and rebases it in, setting the unstaged changes aside meanwhile.
    pub fn fixup(&self, id: &str) -> Result<String, String> {
        if self.run(&["merge-base", "--is-ancestor", id, "HEAD"], None).is_err() {
            return Err("That commit isn't on this branch".to_owned());
        }
        if self.run(&["rev-list", "--merges", &format!("{id}..HEAD")], None).is_ok_and(|m| !m.is_empty()) {
            return Err("A merge came after that commit; fixing it up would flatten the merge".to_owned());
        }
        let pushed = self.run(&["merge-base", "--is-ancestor", id, "@{u}"], None).is_ok();
        self.run(&["commit", "-q", "--fixup", id], None)?;
        let parent = format!("{id}^");
        let onto = if self.run(&["rev-parse", "--verify", "-q", &parent], None).is_ok() { parent.as_str() } else { "--root" };
        // The todo list as git wrote it, fixup already in place: no editor to stop in.
        let rebase = self.run_env(&["rebase", "-q", "-i", "--autosquash", "--autostash", onto], None, &[("GIT_SEQUENCE_EDITOR", Path::new("true"))]);
        self.stopped(rebase)?;
        Ok(if pushed { "It was pushed already: Force Push sends the new version".to_owned() } else { String::new() })
    }

    /// The branch HEAD is on (`None` when detached) and its commit; `None` before the first commit.
    pub fn head_at(&self) -> Option<(Option<String>, String)> {
        let id = self.run(&["rev-parse", "--verify", "-q", "HEAD"], None).ok()?;
        Some((self.run(&["symbolic-ref", "-q", "--short", "HEAD"], None).ok(), id))
    }

    /// Runs `f`, which moves HEAD, and says how to undo it: nothing when HEAD didn't move,
    /// `f` failed, or it stopped midway on conflicts. With `save`, the working tree is saved
    /// as a checkpoint first, for an operation that throws changes away (a hard reset).
    pub fn undoable(&self, what: &str, back: Back, save: bool, f: impl FnOnce(&Repo) -> Result<String, String>) -> (Result<String, String>, Option<Moved>) {
        let from = self.head_at();
        let saved = match save {
            true => match self.checkpoint(&format!("Before {what}")) {
                Ok(sha) => Some(sha),
                Err(e) => return (Err(format!("Couldn't save the changes first, so nothing was done: {e}")), None),
            },
            false => None,
        };
        let result = f(self);
        let moved = match (&result, from, self.head_at()) {
            (Ok(_), Some(from), Some(to)) if from != to && self.operation().is_none() => Some(Moved { what: what.to_owned(), back, from, to, saved }),
            _ => None,
        };
        (result, moved)
    }

    /// Takes back operation `m`: puts HEAD where it was, and the saved changes back. Refuses
    /// when HEAD has moved since, as undoing then would throw the later work away.
    pub fn undo(&self, m: &Moved) -> Result<String, String> {
        if let Some(op) = self.operation() {
            return Err(format!("Finish or abort the {} first", op.noun()));
        }
        if self.head_at().as_ref() != Some(&m.to) {
            return Err(format!("HEAD has moved since {}, so it can't be undone", m.what));
        }
        let (branch, id) = &m.from;
        match (m.back, branch) {
            (Back::Switch, Some(b)) => self.run(&["switch", "-q", b], None)?,
            (Back::Switch, None) => self.run(&["switch", "-q", "--detach", id], None)?,
            (Back::Soft, _) => self.reset("soft", id)?,
            (Back::Mixed, _) => self.reset("mixed", id)?,
            // Moves the files that differ, and refuses if one of them has changes of its own.
            (Back::Keep, _) => self.reset("keep", id)?,
        };
        if let Some(sha) = &m.saved {
            self.restore_checkpoint(sha)?;
        }
        Ok(format!("Undid {}", m.what))
    }

    /// Local branches, remote branches and tags: what a compare can start or end at.
    pub fn refs(&self) -> Vec<String> {
        let mut all = self.branches();
        let tags = self.run(&["tag", "--sort=-creatordate"], None).unwrap_or_default();
        all.extend(tags.lines().filter(|t| !t.is_empty()).map(str::to_owned));
        all
    }

    /// The commits that changed `path`, following it through renames, newest first.
    pub fn file_log(&self, path: &str) -> Result<Vec<crate::graph::Commit>, String> {
        let format = format!("--format={}", crate::graph::FORMAT);
        let out = self.run(&["log", "--follow", "-n", "2000", "--date=format-local:%d %b %Y %H:%M", &format, "--", path], None)?;
        Ok(crate::graph::parse(&out))
    }

    /// Who last changed each line of `path`: as of commit `at`, or in the working tree.
    pub fn blame(&self, path: &str, at: Option<&str>) -> Result<Vec<Blamed>, String> {
        let mut args = vec!["blame", "--porcelain"];
        args.extend(at);
        args.extend(["--", path]);
        Ok(parse_blame(&self.run(&args, None)?))
    }

    /// [`Repo::blame`] of the file as staged, its lines numbered as in the index.
    pub fn blame_staged(&self, path: &str) -> Result<Vec<Blamed>, String> {
        let staged = self.run(&["show", &format!(":{path}")], None)?;
        Ok(parse_blame(&self.run(&["blame", "--porcelain", "--contents", "-", "--", path], Some(&staged))?))
    }

    /// A conflicted file's three versions from the index: the common base (empty when both
    /// sides added the file), this branch's and the incoming one's.
    pub fn merge_sides(&self, path: &str) -> Result<(String, String, String), String> {
        let specs: Vec<String> = (1..=3).map(|n| format!(":{n}:{path}")).collect();
        let mut sides = self.cat_files(&specs)?.into_iter().map(|b| match b {
            None => Ok(None),
            Some(Blob::Text(t)) => Ok(Some(t)),
            Some(_) => Err(format!("{path} isn't text, so it can't be merged by line: keep a side")),
        });
        let mut next = || sides.next().unwrap_or(Ok(None));
        let (base, ours, theirs) = (next()?, next()?, next()?);
        match (ours, theirs) {
            (Some(o), Some(t)) => Ok((base.unwrap_or_default(), o, t)),
            _ => Err(format!("One side deleted {path}: keep a side")),
        }
    }

    /// Writes a file's merged text and stages it, which marks it resolved. The file as it
    /// was is backed up first; returns the backup.
    pub fn save_merge(&self, path: &str, text: &str) -> Result<PathBuf, String> {
        let backup = self.backup(path)?;
        std::fs::write(self.root.join(path), text).map_err(|e| format!("{path}: {e}"))?;
        self.stage(path)?;
        Ok(backup)
    }

    /// Settles a conflicted file with one side's version, `ours` or `theirs`, and stages it.
    pub fn take_side(&self, path: &str, side: &str) -> Result<(), String> {
        self.run(&["checkout", &format!("--{side}"), "--", path], None)?;
        self.stage(path)
    }

    /// A ref's commit id.
    pub fn resolve(&self, name: &str) -> Result<String, String> {
        self.run(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")], None).map_err(|_| format!("{name} names no commit"))
    }

    pub fn merge_base(&self, base: &str) -> Result<String, String> {
        self.run(&["merge-base", base, "HEAD"], None).map_err(|e| format!("No common history with {base}: {e}"))
    }

    /// Local and remote branches, local first, without `origin/HEAD`.
    pub fn branches(&self) -> Vec<String> {
        // Full names: a remote's HEAD is always `refs/remotes/<remote>/HEAD`, where its short
        // name is the bare remote's in newer gits, which would take a `git remote` to tell apart.
        let out = self.run(&["for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes"], None).unwrap_or_default();
        out.lines()
            .filter_map(|r| r.strip_prefix("refs/heads/").or_else(|| r.strip_prefix("refs/remotes/").filter(|r| !r.ends_with("/HEAD"))))
            .map(str::to_owned)
            .collect()
    }

    /// The latest `n` commits of every branch, remote branch and tag (or of `branch` alone),
    /// newest first and each after its children, with HEAD's id.
    pub fn graph(&self, n: usize, branch: Option<&str>) -> (Vec<crate::graph::Commit>, Option<String>) {
        let limit = format!("-{n}");
        let format = format!("--format={}", crate::graph::FORMAT);
        let mut args = vec!["log", &limit, "--date-order", "--decorate=full", "--date=format-local:%d %b %Y %H:%M", &format];
        match branch {
            Some(b) => args.extend([b, "--"]),
            // Not `--all`: that takes in stashes and notes.
            None => args.extend(["--branches", "--remotes", "--tags", "HEAD", "--"]),
        }
        let out = self.run(&args, None).unwrap_or_default();
        (crate::graph::parse(&out), self.head_id())
    }

    /// HEAD's commit, if it has one.
    pub fn head_id(&self) -> Option<String> {
        self.run(&["rev-parse", "-q", "--verify", "HEAD"], None).ok()
    }

    /// The web page of `origin` (or the first remote), from its fetch URL: `git@host:o/r.git`,
    /// `ssh://git@host/o/r` and `https://host/o/r.git` all give `https://host/o/r`.
    pub fn web_url(&self) -> Option<String> {
        let remotes = self.run(&["remote"], None).ok()?;
        let name = remotes.lines().find(|r| *r == "origin").or_else(|| remotes.lines().next())?;
        web_url(&self.run(&["remote", "get-url", name], None).ok()?)
    }

    /// A commit's whole message.
    pub fn message(&self, id: &str) -> String {
        self.run(&["show", "-s", "--format=%B", id], None).unwrap_or_default()
    }
}

/// How HEAD goes back to undo an operation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Back {
    /// A commit or amend: its changes come back staged.
    Soft,
    /// A mixed reset: the index goes back with HEAD.
    Mixed,
    /// A merge, rebase, pull, cherry-pick, revert or hard reset: the files go back too.
    Keep,
    /// A branch switch: onto the branch it left.
    Switch,
}

/// An operation that moved HEAD, and how to take it back.
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    /// What it was, as "Undid …" names it: "the commit", "the merge of main".
    pub what: String,
    pub back: Back,
    /// The branch (`None` detached) and commit before, and after.
    pub from: (Option<String>, String),
    pub to: (Option<String>, String),
    /// The checkpoint of the working tree taken first, put back by the undo.
    pub saved: Option<String>,
}

/// The branch of `all` (as [`Repo::branches`] reads them) a review most likely compares against.
/// `git diff` of two files or folders outside any repository. It exits 1 when they differ,
/// which isn't a failure.
pub fn diff_no_index(a: &Path, b: &Path) -> Result<String, String> {
    let out = Command::new("git").args(["diff", "--no-index", "--no-color", "--no-ext-diff", "--"]).arg(a).arg(b).output().map_err(|e| format!("can't run git: {e}"))?;
    match out.status.code() {
        Some(0 | 1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().lines().take(3).collect::<Vec<_>>().join(" ")),
    }
}

/// Makes `exe` git's diff and merge tool for this user: `git difftool` and `git mergetool`
/// open it, and a merge it saves counts as done.
pub fn register_tool(exe: &Path) -> Result<(), String> {
    let exe = format!("'{}'", exe.display().to_string().replace('\'', r"'\''"));
    let settings = [
        ("difftool.collate.cmd", format!(r#"{exe} "$LOCAL" "$REMOTE" --title "$MERGED""#)),
        ("mergetool.collate.cmd", format!(r#"{exe} --merge "$BASE" "$LOCAL" "$REMOTE" "$MERGED""#)),
        ("mergetool.collate.trustExitCode", "true".into()),
        ("diff.tool", "collate".into()),
        ("merge.tool", "collate".into()),
    ];
    for (key, value) in settings {
        let out = Command::new("git").args(["config", "--global", key, &value]).output().map_err(|e| format!("can't run git: {e}"))?;
        if !out.status.success() {
            return Err(format!("git config {key}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    Ok(())
}

pub fn default_base(all: &[String]) -> Option<String> {
    ["origin/main", "origin/master", "main", "master", "origin/develop", "develop"].iter().find(|b| all.iter().any(|r| r == *b)).map(|b| (*b).to_owned())
}

fn same(a: &Option<Blob>, b: &Option<Blob>) -> bool {
    matches!((a, b), (Some(Blob::Text(x)), Some(Blob::Text(y))) if x == y)
}

fn classify(bytes: Vec<u8>) -> Blob {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xFF, 0xD8, 0xFF]) || bytes.starts_with(b"%PDF-") {
        return Blob::Image(bytes.into());
    }
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Blob::Binary;
    }
    match String::from_utf8(bytes) {
        Ok(s) => Blob::Text(s),
        Err(_) => Blob::Binary,
    }
}

fn hash_blob(b: &Option<Blob>, h: &mut DefaultHasher) {
    match b {
        None => 0u8.hash(h),
        Some(Blob::Text(t)) => (1u8, t).hash(h),
        Some(Blob::Binary) => 2u8.hash(h),
        Some(Blob::Image(b)) => (4u8, &b[..]).hash(h),
        Some(Blob::TooLarge(n)) => (3u8, n).hash(h),
    }
}

/// The status's `# branch.ab +1 -2` line: commits ahead of and behind the upstream.
fn ahead_behind(raw: &[u8]) -> Option<(usize, usize)> {
    let text = String::from_utf8_lossy(raw);
    let ab = text.split('\0').find_map(|r| r.strip_prefix("# branch.ab "))?;
    let mut n = ab.split(' ').map(|n| n.trim_start_matches(['+', '-']).parse().ok());
    Some((n.next()??, n.next()??))
}

/// Parses `git status --porcelain=v2 -z --branch` into the branch name and the changed entries.
fn parse_status(raw: &[u8]) -> (String, Option<String>, Vec<Entry>) {
    let text = String::from_utf8_lossy(raw);
    let mut branch = String::from("HEAD");
    let mut head = None;
    let mut entries = Vec::new();
    let mut records = text.split('\0').filter(|r| !r.is_empty());
    while let Some(r) = records.next() {
        let xy = |s: &str| {
            let b = s.as_bytes();
            (b[0], b[1])
        };
        match r.as_bytes()[0] {
            b'#' => {
                if let Some(name) = r.strip_prefix("# branch.head ") {
                    branch = name.to_owned();
                } else if let Some(oid) = r.strip_prefix("# branch.oid ") {
                    head = (oid != "(initial)").then(|| oid.to_owned());
                }
            }
            b'1' => {
                let f: Vec<&str> = r.splitn(9, ' ').collect();
                if f.len() == 9 {
                    let (index, worktree) = xy(f[1]);
                    entries.push(Entry { index, worktree, path: f[8].to_owned(), orig: None, untracked: false, head_oid: oid(f[6]), index_oid: oid(f[7]) });
                }
            }
            b'2' => {
                // Renames are off, but copies can still come through on some configs.
                let f: Vec<&str> = r.splitn(10, ' ').collect();
                let orig = records.next().map(str::to_owned);
                if f.len() == 10 {
                    let (index, worktree) = xy(f[1]);
                    entries.push(Entry { index, worktree, path: f[9].to_owned(), orig, untracked: false, head_oid: oid(f[6]), index_oid: oid(f[7]) });
                }
            }
            b'u' => {
                let f: Vec<&str> = r.splitn(11, ' ').collect();
                if f.len() == 11 {
                    // A conflict: no single index blob, read by path.
                    entries.push(Entry { index: b'M', worktree: b'M', path: f[10].to_owned(), orig: None, untracked: false, head_oid: None, index_oid: None });
                }
            }
            b'?' => entries.push(Entry { index: b'.', worktree: b'?', path: r[2..].to_owned(), orig: None, untracked: true, head_oid: None, index_oid: None }),
            _ => {}
        }
    }
    entries.sort_by(|a, b| tree_order(&a.path, &b.path));
    (branch, head, entries)
}

/// The file tree's order: at each level folders before files, then names in natural order
/// (`file2` before `file10`), so the diff lists files in the order the sidebar shows them.
fn tree_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
    for i in 0..a.len().min(b.len()) {
        let (a_dir, b_dir) = (i + 1 < a.len(), i + 1 < b.len());
        if a_dir != b_dir {
            return b_dir.cmp(&a_dir);
        }
        let o = natural(a[i], b[i]).then(a[i].cmp(b[i]));
        if o.is_ne() {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Compares names with digit runs as numbers and letters ignoring case.
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let num = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (x, y) = (num(&mut a), num(&mut b));
                let (tx, ty) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                let o = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
                if o.is_ne() {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o.is_ne() {
                    return o;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// See [`Repo::web_url`].
pub fn web_url(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = if let Some(r) = r.strip_prefix("https://").or_else(|| r.strip_prefix("http://")) {
        // Drop any `user:token@` in front of the host.
        r.rsplit_once('@').map_or(r, |(_, h)| h).to_owned()
    } else if let Some(r) = r.strip_prefix("ssh://") {
        let r = r.split_once('@').map_or(r, |(_, h)| h);
        // `host:port/o/r`: the port is the SSH one, not the web one.
        match r.split_once('/') {
            Some((host, path)) => format!("{}/{path}", host.split(':').next().unwrap_or(host)),
            None => return None,
        }
    } else {
        let (host, path) = r.split_once('@').map_or(r, |(_, h)| h).split_once(':')?;
        format!("{host}/{path}")
    };
    rest.contains('/').then(|| format!("https://{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_status() {
        let raw = b"# branch.oid abc\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\0\
1 A. N... 000000 100644 100644 000 bbb new.rs\0\
? notes.txt\0";
        let (branch, head, e) = parse_status(raw);
        assert_eq!(branch, "main");
        assert_eq!(head.as_deref(), Some("abc"));
        let paths: Vec<_> = e.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["src/a b.rs", "new.rs", "notes.txt"]);
        assert_eq!((e[0].index, e[0].worktree), (b'.', b'M'));
        assert_eq!((e[1].index, e[1].worktree), (b'A', b'.'));
        assert!(e[2].untracked);
    }

    #[test]
    fn a_checkpoint_keeps_the_tree_and_restoring_it_puts_it_back() {
        let dir = std::env::temp_dir().join(format!("collate-checkpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"]] {
            git(args);
        }
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // Mine: an edit, staged, and a new file git doesn't track yet.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(&["add", "a.txt"]);
        std::fs::write(dir.join("new.txt"), "mine\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "keep\n").unwrap();
        let repo = Repo::open(&dir).unwrap();
        let (head, status) = (git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"]));
        let cp = repo.checkpoint("Before the agent").unwrap();
        assert_eq!((git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"])), (head.clone(), status.clone()), "nothing moved");
        assert_eq!(repo.checkpoints().unwrap(), [(cp.clone(), repo.checkpoints().unwrap()[0].1, "Before the agent".to_owned())]);
        assert_eq!(git(&["show", &format!("{cp}:new.txt")]), "mine\n", "untracked files are kept");
        assert!(Command::new("git").arg("-C").arg(&dir).args(["cat-file", "-e", &format!("{cp}:ignored.txt")]).output().unwrap().status.code() != Some(0), "ignored ones aren't");
        // The agent's: edits, a deletion, a file of its own.
        std::fs::write(dir.join("a.txt"), "agent\n").unwrap();
        std::fs::remove_file(dir.join("new.txt")).unwrap();
        std::fs::write(dir.join("made.txt"), "agent\n").unwrap();
        let before = repo.restore_checkpoint(&cp).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "mine\n");
        assert!(!dir.join("made.txt").exists(), "what came after is gone");
        assert!(dir.join("ignored.txt").exists());
        assert_eq!((git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"])), (head, status), "the index as it was, so the status too");
        assert_eq!(git(&["show", &format!("{before}:made.txt")]), "agent\n", "the restore saved what it replaced");
        assert_eq!(repo.checkpoints().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_staged_diff_is_what_a_commit_or_an_amend_records() {
        let dir = std::env::temp_dir().join(format!("collate-staged-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"]] {
            git(args);
        }
        for (file, subject) in [("a.txt", "Add a"), ("b.txt", "Add b")] {
            std::fs::write(dir.join(file), file).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-qm", subject]);
        }
        std::fs::write(dir.join("c.txt"), "c").unwrap();
        std::fs::write(dir.join("unstaged.txt"), "u").unwrap();
        git(&["add", "c.txt"]);
        let repo = Repo::open(&dir).unwrap();
        let staged = repo.staged_diff(false).unwrap();
        assert!(staged.contains("+++ b/c.txt") && !staged.contains("b.txt") && !staged.contains("unstaged"), "{staged}");
        let amended = repo.staged_diff(true).unwrap();
        assert!(amended.contains("+++ b/b.txt") && amended.contains("+++ b/c.txt"), "the last commit's change too: {amended}");
        assert_eq!(repo.recent_subjects(5).unwrap(), ["Add b", "Add a"]);
        // Blame of the index numbers its lines as staged, not as in the working tree.
        std::fs::write(dir.join("a.txt"), "staged\na.txt\n").unwrap();
        git(&["add", "a.txt"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nstaged\na.txt\n").unwrap();
        let staged: Vec<(bool, String)> = repo.blame_staged("a.txt").unwrap().into_iter().map(|b| (b.id.bytes().all(|c| c == b'0'), b.text)).collect();
        assert_eq!(staged, [(true, "staged".to_owned()), (false, "a.txt".to_owned())]);
        assert_eq!(repo.blame("a.txt", None).unwrap().len(), 4, "the working tree's lines");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpushed_is_the_branch_since_it_left_its_upstream() {
        let dir = std::env::temp_dir().join(format!("collate-unpushed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let commit = |file: &str| {
            std::fs::write(dir.join(file), file).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-qm", file]);
        };
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"]] {
            git(args);
        }
        commit("base.txt");
        let repo = Repo::open(&dir).unwrap();
        assert!(repo.unpushed().is_err(), "no upstream yet");
        // A local branch serves as the upstream; someone else's commit lands on it after ours left.
        git(&["branch", "up"]);
        git(&["branch", "-q", "--set-upstream-to=up"]);
        let base = repo.resolve("HEAD").unwrap();
        commit("mine.txt");
        git(&["switch", "-q", "up"]);
        commit("theirs.txt");
        git(&["switch", "-q", "main"]);
        let (upstream, from) = repo.unpushed().unwrap();
        assert_eq!((upstream.as_str(), from.as_str()), ("up", base.as_str()));
        let head = repo.resolve("HEAD").unwrap();
        let snap = repo.snapshot(Scope::History, &repo.status_raw().unwrap(), Some(("", &format!("{from}..{head}")))).unwrap();
        let paths: Vec<_> = snap.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["mine.txt"], "only what a push sends, not their commit undone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sorts_like_the_file_tree() {
        let mut paths = vec!["src/file10.rs", "README.md", "src/file2.rs", "src/a/z.rs", "src/File1.rs"];
        paths.sort_by(|a, b| tree_order(a, b));
        assert_eq!(paths, ["src/a/z.rs", "src/File1.rs", "src/file2.rs", "src/file10.rs", "README.md"]);
    }
}

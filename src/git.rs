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
    /// A PNG or JPEG, which the app can draw.
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
        Ok(Snapshot { scope, branch, head, base, staged, unstaged, total, working, changes, ahead_behind: ahead_behind(raw) })
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
        let mut c = Command::new("git");
        // No terminal to ask on: a fetch or push that needs a password fails instead of waiting.
        c.arg("-C").arg(&self.root).args(args).env("GIT_TERMINAL_PROMPT", "0").stdout(Stdio::piped()).stderr(Stdio::piped());
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

    /// The last commit's whole message, to start an amend from.
    pub fn last_message(&self) -> Option<String> {
        self.run(&["log", "-1", "--format=%B"], None).ok()
    }

    /// Fetches every remote, dropping branches deleted there.
    pub fn fetch(&self) -> Result<String, String> {
        self.run(&["fetch", "--all", "--prune", "-q"], None)
    }

    /// Pulls the upstream, only when that needs no merge.
    pub fn pull(&self) -> Result<String, String> {
        self.run(&["pull", "--ff-only", "-q"], None)
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

    /// Makes branch `name` at `at` (HEAD when `None`) and, with `switch`, moves onto it.
    pub fn new_branch(&self, name: &str, at: Option<&str>, switch: bool) -> Result<String, String> {
        let mut args = if switch { vec!["switch", "-q", "-c", name] } else { vec!["branch", name] };
        args.extend(at);
        self.run(&args, None)
    }

    /// Applies commit `id`'s change on top of HEAD as a new commit.
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

    /// Local branches, remote branches and tags: what a compare can start or end at.
    pub fn refs(&self) -> Vec<String> {
        let mut all = self.branches();
        let tags = self.run(&["tag", "--sort=-creatordate"], None).unwrap_or_default();
        all.extend(tags.lines().filter(|t| !t.is_empty()).map(str::to_owned));
        all
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
        let out = self.run(&["for-each-ref", "--format=%(refname:short)", "refs/heads", "refs/remotes"], None).unwrap_or_default();
        // A remote's HEAD shows as `origin/HEAD`, or as the bare remote name in newer gits.
        let remotes = self.run(&["remote"], None).unwrap_or_default();
        out.lines().filter(|r| !r.is_empty() && !r.ends_with("/HEAD") && !remotes.lines().any(|m| m == *r)).map(str::to_owned).collect()
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
        (crate::graph::parse(&out), self.run(&["rev-parse", "-q", "--verify", "HEAD"], None).ok())
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

    /// The branch a review most likely compares against.
    pub fn default_base(&self) -> Option<String> {
        let all = self.branches();
        ["origin/main", "origin/master", "main", "master", "origin/develop", "develop"].iter().find(|b| all.iter().any(|r| r == *b)).map(|b| (*b).to_owned())
    }
}

fn same(a: &Option<Blob>, b: &Option<Blob>) -> bool {
    matches!((a, b), (Some(Blob::Text(x)), Some(Blob::Text(y))) if x == y)
}

fn classify(bytes: Vec<u8>) -> Blob {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
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
    fn sorts_like_the_file_tree() {
        let mut paths = vec!["src/file10.rs", "README.md", "src/file2.rs", "src/a/z.rs", "src/File1.rs"];
        paths.sort_by(|a, b| tree_order(a, b));
        assert_eq!(paths, ["src/a/z.rs", "src/File1.rs", "src/file2.rs", "src/file10.rs", "README.md"]);
    }
}

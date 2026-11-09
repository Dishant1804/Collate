//! Staging, unstaging, discarding, committing and branch review, run against real repositories
//! made in a temporary directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::diff::FileDiff;
use crate::git::{Blob, Change, Repo, Scope, Staged};
use crate::patch;

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository on `main` with `files` committed.
fn repo(name: &str, files: &[(&str, &str)]) -> (PathBuf, Repo) {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("collate-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    sh(&dir, &["init", "-q", "-b", "main"]);
    sh(&dir, &["config", "user.email", "t@t"]);
    sh(&dir, &["config", "user.name", "t"]);
    sh(&dir, &["config", "commit.gpgsign", "false"]);
    for (p, text) in files {
        write(&dir, p, text);
    }
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "init"]);
    let r = Repo::open(&dir).unwrap();
    (dir, r)
}

fn write(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn lines(n: u32) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn snapshot(r: &Repo, scope: Scope) -> Vec<Change> {
    let raw = r.status_raw().unwrap();
    r.snapshot(scope, &raw, None).unwrap().changes
}

fn text(b: &Option<Blob>) -> String {
    match b {
        Some(Blob::Text(t)) => t.clone(),
        _ => String::new(),
    }
}

/// The diff the app would show for `path` in `scope`.
fn file(r: &Repo, scope: Scope, path: &str) -> FileDiff {
    let c = snapshot(r, scope).into_iter().find(|c| c.path == path).unwrap_or_else(|| panic!("{path} in {scope:?}"));
    let old_path = c.old.as_ref().map(|_| c.path.clone());
    let new_path = c.new.as_ref().map(|_| c.path.clone());
    FileDiff::between_with_context(old_path, new_path, &text(&c.old), &text(&c.new), 3)
}

/// `a.txt` with lines 5 and 35 changed: two hunks.
fn two_hunks(name: &str) -> (PathBuf, Repo) {
    let (dir, r) = repo(name, &[("a.txt", &lines(40))]);
    write(&dir, "a.txt", &lines(40).replace("line 5\n", "line five\n").replace("line 35\n", "line thirty-five\n"));
    (dir, r)
}

#[test]
fn staging_one_hunk_stages_only_that_hunk() {
    let (dir, r) = two_hunks("stage-hunk");
    let f = file(&r, Scope::Unstaged, "a.txt");
    assert_eq!(f.hunks.len(), 2);
    r.apply(&patch::hunk(&f, 0).unwrap(), true, false).unwrap();
    let cached = sh(&dir, &["diff", "--cached"]);
    assert!(cached.contains("+line five") && !cached.contains("thirty-five"), "{cached}");
    let rest = sh(&dir, &["diff"]);
    assert!(rest.contains("+line thirty-five") && !rest.contains("five\n+line five"), "{rest}");
    let c = snapshot(&r, Scope::Uncommitted).into_iter().find(|c| c.path == "a.txt").unwrap();
    assert_eq!(c.staged, Staged::Partly);
}

#[test]
fn a_hunk_stages_from_the_all_changes_view_too() {
    let (dir, r) = two_hunks("stage-all");
    let f = file(&r, Scope::Uncommitted, "a.txt");
    r.apply(&patch::hunk(&f, 1).unwrap(), true, false).unwrap();
    let cached = sh(&dir, &["diff", "--cached"]);
    assert!(cached.contains("+line thirty-five") && !cached.contains("+line five"), "{cached}");
}

#[test]
fn a_fully_staged_hunk_unstages_from_the_all_changes_view() {
    let (dir, r) = two_hunks("unstage-all");
    r.stage("a.txt").unwrap();
    let f = file(&r, Scope::Uncommitted, "a.txt");
    r.apply(&patch::hunk(&f, 1).unwrap(), true, true).unwrap();
    let cached = sh(&dir, &["diff", "--cached"]);
    assert!(cached.contains("+line five") && !cached.contains("thirty-five"), "{cached}");
}

#[test]
fn unstaging_a_hunk_puts_it_back() {
    let (dir, r) = two_hunks("unstage-hunk");
    r.stage("a.txt").unwrap();
    let f = file(&r, Scope::Staged, "a.txt");
    r.apply(&patch::hunk(&f, 0).unwrap(), true, true).unwrap();
    let cached = sh(&dir, &["diff", "--cached"]);
    assert!(!cached.contains("+line five") && cached.contains("+line thirty-five"), "{cached}");
    // The working tree keeps both changes.
    let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    assert!(on_disk.contains("line five") && on_disk.contains("line thirty-five"));
}

#[test]
fn discarding_a_hunk_reverts_it_on_disk_and_keeps_a_backup() {
    let (dir, r) = two_hunks("discard-hunk");
    let f = file(&r, Scope::Unstaged, "a.txt");
    let backup = r.backup("a.txt").unwrap();
    r.apply(&patch::hunk(&f, 1).unwrap(), false, true).unwrap();
    let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    assert!(on_disk.contains("line five") && on_disk.contains("line 35\n") && !on_disk.contains("thirty-five"));
    let saved = std::fs::read_to_string(backup.join("a.txt")).unwrap();
    assert!(saved.contains("line thirty-five"), "the backup holds the file as it was");
}

#[test]
fn a_change_at_the_end_without_a_newline_stages() {
    let (dir, r) = repo("eof", &[("b.txt", "one\ntwo")]);
    write(&dir, "b.txt", "one\nTWO");
    let f = file(&r, Scope::Unstaged, "b.txt");
    r.apply(&patch::hunk(&f, 0).unwrap(), true, false).unwrap();
    assert!(sh(&dir, &["diff", "--cached"]).contains("+TWO"));
    assert_eq!(sh(&dir, &["diff"]), "");
}

#[test]
fn a_new_file_stages_by_hunk_and_by_file() {
    let (dir, r) = repo("new", &[("keep.txt", "k\n")]);
    write(&dir, "new.txt", "hello\nworld\n");
    let f = file(&r, Scope::Unstaged, "new.txt");
    r.apply(&patch::hunk(&f, 0).unwrap(), true, false).unwrap();
    assert!(sh(&dir, &["diff", "--cached", "--name-only"]).contains("new.txt"));
    r.unstage("new.txt").unwrap();
    assert_eq!(sh(&dir, &["diff", "--cached", "--name-only"]), "");
    r.stage("new.txt").unwrap();
    assert!(sh(&dir, &["diff", "--cached", "--name-only"]).contains("new.txt"));
}

#[test]
fn discarding_files() {
    let (dir, r) = repo("discard-file", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    // Modified and staged, then modified again: All changes goes back to HEAD, index too.
    write(&dir, "a.txt", "a2\n");
    r.stage("a.txt").unwrap();
    write(&dir, "a.txt", "a3\n");
    let backup = r.discard("a.txt", Scope::Uncommitted, true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read_to_string(backup.join("a.txt")).unwrap(), "a3\n");
    // Unstaged goes back to the index, leaving what's staged.
    write(&dir, "b.txt", "b2\n");
    r.stage("b.txt").unwrap();
    write(&dir, "b.txt", "b3\n");
    r.discard("b.txt", Scope::Unstaged, true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b2\n");
    // An untracked file is removed, with a copy kept.
    write(&dir, "scratch.txt", "notes\n");
    let backup = r.discard("scratch.txt", Scope::Unstaged, false).unwrap();
    assert!(!dir.join("scratch.txt").exists());
    assert_eq!(std::fs::read_to_string(backup.join("scratch.txt")).unwrap(), "notes\n");
    // Nothing in a staged or branch view is discarded.
    assert!(r.discard("b.txt", Scope::Staged, true).is_err());
}

#[test]
fn committing_takes_what_is_staged() {
    let (dir, r) = repo("commit", &[("a.txt", "a\n")]);
    write(&dir, "a.txt", "a2\n");
    write(&dir, "b.txt", "untracked\n");
    r.stage("a.txt").unwrap();
    let summary = r.commit("Change a\n\nWith a body.").unwrap();
    assert!(summary.ends_with("Change a"), "{summary}");
    assert_eq!(sh(&dir, &["log", "-1", "--format=%B"]).trim(), "Change a\n\nWith a body.");
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "?? b.txt\n");
    assert!(r.commit("nothing staged").is_err());
}

#[test]
fn a_branch_review_shows_the_branch_and_not_the_base() {
    let (dir, r) = repo("branch", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    sh(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "a.txt", "a on the branch\n");
    sh(&dir, &["commit", "-qam", "branch work"]);
    // Uncommitted and untracked work counts too.
    write(&dir, "b.txt", "b, not committed\n");
    write(&dir, "c.txt", "new\n");
    // Main moves on after the branch left it; that isn't the branch's change.
    sh(&dir, &["checkout", "-q", "main"]);
    let stash = sh(&dir, &["stash", "list"]);
    assert_eq!(stash, "");
    sh(&dir, &["checkout", "-q", "feature"]);
    let mb = r.merge_base("main").unwrap();
    let raw = r.status_raw().unwrap();
    let snap = r.snapshot(Scope::Branch, &raw, Some(("main", &mb))).unwrap();
    let paths: Vec<&str> = snap.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "b.txt", "c.txt"]);
    assert_eq!(text(&snap.changes[0].old), "a\n");
    assert_eq!(snap.base, Some(("main".to_owned(), mb)));
    // Without a base there's nothing to compare against.
    assert!(r.snapshot(Scope::Branch, &raw, None).is_err());
}

#[test]
fn a_base_that_moved_on_is_measured_from_where_the_branch_left_it() {
    let (dir, r) = repo("moved", &[("a.txt", "a\n")]);
    sh(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "a.txt", "feature\n");
    sh(&dir, &["commit", "-qam", "feature"]);
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "d.txt", "main only\n");
    sh(&dir, &["add", "d.txt"]);
    sh(&dir, &["commit", "-qm", "main moves"]);
    sh(&dir, &["checkout", "-q", "feature"]);
    let mb = r.merge_base("main").unwrap();
    let raw = r.status_raw().unwrap();
    let snap = r.snapshot(Scope::Branch, &raw, Some(("main", &mb))).unwrap();
    let paths: Vec<&str> = snap.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt"], "d.txt landed on main after the branch left; it isn't the branch's");
    assert!(r.branches().contains(&"main".to_owned()));
    assert_eq!(r.default_base().as_deref(), Some("main"));
}


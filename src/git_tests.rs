//! Staging, unstaging, discarding, committing and branch review, run against real repositories
//! made in a temporary directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::diff::FileDiff;
use crate::git::{Blob, Change, Repo, Scope, Staged, default_base};
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
fn discarding_tracked_and_untracked_files_keeps_a_backup_of_each() {
    let (dir, r) = repo("discard-all", &[("a.txt", "one\n"), ("b.txt", "two\n")]);
    write(&dir, "a.txt", "one changed\n");
    write(&dir, "b.txt", "two changed\n");
    r.stage("b.txt").unwrap();
    write(&dir, "new/c.txt", "new\n");
    let backup = r.discard_tracked().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "two\n", "staged changes go too");
    assert!(dir.join("new/c.txt").exists(), "untracked files stay");
    assert_eq!(std::fs::read_to_string(backup.join("b.txt")).unwrap(), "two changed\n");
    let backup = r.discard_untracked().unwrap();
    assert!(!dir.join("new/c.txt").exists());
    assert_eq!(std::fs::read_to_string(backup.join("new/c.txt")).unwrap(), "new\n");
    assert_eq!(r.restore(&backup).unwrap(), 1);
    assert!(dir.join("new/c.txt").exists(), "Undo puts it back");
}

#[test]
fn stashing_tracked_changes_leaves_untracked_files() {
    let (dir, r) = repo("stash-tracked", &[("a.txt", "one\n")]);
    write(&dir, "a.txt", "one\ntwo\n");
    write(&dir, "c.txt", "new\n");
    r.stash_tracked("").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    assert!(dir.join("c.txt").exists());
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
    let summary = r.commit("Change a\n\nWith a body.", false).unwrap();
    assert!(summary.ends_with("Change a"), "{summary}");
    assert_eq!(sh(&dir, &["log", "-1", "--format=%B"]).trim(), "Change a\n\nWith a body.");
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "?? b.txt\n");
    assert!(r.commit("nothing staged", false).is_err());
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
    assert_eq!(default_base(&r.branches()).as_deref(), Some("main"));
}

/// A hunk replacing three lines with three others, and which lines of it are which.
fn three_for_three(name: &str) -> (PathBuf, Repo, FileDiff) {
    let (dir, r) = repo(name, &[("c.txt", "keep\nold a\nold b\nold c\nend\n")]);
    write(&dir, "c.txt", "keep\nnew a\nnew b\nnew c\nend\n");
    let f = file(&r, Scope::Unstaged, "c.txt");
    (dir, r, f)
}

/// Picks the lines of hunk 0 whose text is in `texts`.
fn pick(f: &FileDiff, texts: &[&str]) -> Vec<bool> {
    f.hunks[0].lines.iter().map(|l| l.kind != crate::diff::LineKind::Context && texts.contains(&l.text.as_str())).collect()
}

#[test]
fn staging_chosen_lines_stages_only_them() {
    let (dir, r, f) = three_for_three("lines-stage");
    // Replace "old b" with "new b" and nothing else.
    let p = patch::lines(&f, 0, Some(&pick(&f, &["old b", "new b"])), false).unwrap();
    r.apply(&p, true, false).unwrap();
    let index = sh(&dir, &["show", ":c.txt"]);
    assert_eq!(index, "keep\nold a\nnew b\nold c\nend\n");
    // An addition alone, then a removal alone.
    let (dir, r, f) = three_for_three("lines-add-only");
    r.apply(&patch::lines(&f, 0, Some(&pick(&f, &["new c"])), false).unwrap(), true, false).unwrap();
    assert_eq!(sh(&dir, &["show", ":c.txt"]), "keep\nold a\nold b\nold c\nnew c\nend\n");
    let (dir, r, f) = three_for_three("lines-del-only");
    r.apply(&patch::lines(&f, 0, Some(&pick(&f, &["old a"])), false).unwrap(), true, false).unwrap();
    assert_eq!(sh(&dir, &["show", ":c.txt"]), "keep\nold b\nold c\nend\n");
    // Nothing picked, no patch.
    assert!(patch::lines(&f, 0, Some(&pick(&f, &[])), false).is_none());
}

#[test]
fn unstaging_and_discarding_chosen_lines() {
    let (dir, r, _) = three_for_three("lines-unstage");
    r.stage("c.txt").unwrap();
    let f = file(&r, Scope::Staged, "c.txt");
    r.apply(&patch::lines(&f, 0, Some(&pick(&f, &["old a", "new a"])), true).unwrap(), true, true).unwrap();
    assert_eq!(sh(&dir, &["show", ":c.txt"]), "keep\nold a\nnew b\nnew c\nend\n");

    let (dir, r, f) = three_for_three("lines-discard");
    r.apply(&patch::lines(&f, 0, Some(&pick(&f, &["old c", "new c"])), true).unwrap(), false, true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "keep\nnew a\nnew b\nold c\nend\n");
}

#[test]
fn hunks_of_a_crlf_file_stage() {
    let (dir, r) = repo("crlf", &[("w.txt", "one\r\ntwo\r\nthree\r\n")]);
    sh(&dir, &["config", "core.autocrlf", "false"]);
    write(&dir, "w.txt", "one\r\nTWO\r\nthree\r\n");
    let f = file(&r, Scope::Unstaged, "w.txt");
    r.apply(&patch::hunk(&f, 0).unwrap(), true, false).unwrap();
    assert_eq!(sh(&dir, &["show", ":w.txt"]), "one\r\nTWO\r\nthree\r\n");
}

#[test]
fn ignoring_whitespace_hides_reindented_lines() {
    let old = "fn a() {\n    one();\n    two();\n}\n";
    let new = "fn a() {\n        one();\n        two();\n    three();\n}\n";
    let plain = FileDiff::between_opts(Some("a.rs".into()), Some("a.rs".into()), old, new, 3, false);
    assert_eq!((plain.additions(), plain.deletions()), (3, 2));
    let quiet = FileDiff::between_opts(Some("a.rs".into()), Some("a.rs".into()), old, new, 3, true);
    assert_eq!((quiet.additions(), quiet.deletions()), (1, 0), "only the new call is a change");
}

#[test]
fn moved_blocks_are_told_apart_from_new_code() {
    let block = "fn helper() {\n    let total = compute_total();\n    record_metric(total);\n}\n";
    let old_a = format!("start\n{block}end\n");
    let new_a = "start\nend\n";
    let old_b = "top\nbottom\n";
    let new_b = format!("top\n{block}fn brand_new_function() {{}}\nbottom\n");
    let mut files = vec![
        FileDiff::between_with_context(Some("a.rs".into()), Some("a.rs".into()), &old_a, new_a, 3),
        FileDiff::between_with_context(Some("b.rs".into()), Some("b.rs".into()), old_b, &new_b, 3),
    ];
    crate::diff::mark_moved(&mut files);
    let moved = |f: &FileDiff| f.hunks.iter().enumerate().flat_map(|(h, hk)| hk.lines.iter().enumerate().filter(move |(i, _)| f.twin(h, *i).is_some()).map(|(_, l)| l.text.trim().to_owned())).collect::<Vec<_>>();
    assert!(moved(&files[0]).contains(&"record_metric(total);".to_owned()), "the removed block moved");
    let landed = moved(&files[1]);
    assert!(landed.contains(&"let total = compute_total();".to_owned()));
    assert!(!landed.iter().any(|l| l.contains("brand_new")), "new code isn't a move");
    assert!(landed.contains(&"}".to_owned()), "a closing brace inside a moved block moves with it");
}

#[test]
fn history_shows_each_commit_against_its_parent() {
    let (dir, r) = repo("history", &[("a.txt", "one\n"), ("b.txt", "keep\n")]);
    write(&dir, "a.txt", "two\n");
    write(&dir, "c.txt", "new\n");
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "second"]);
    // Uncommitted work doesn't leak into a past commit.
    write(&dir, "b.txt", "edited later\n");
    let (commits, _) = r.graph(10, None);
    let log: Vec<(String, String)> = commits.into_iter().map(|c| (c.id, c.subject)).collect();
    assert_eq!(log.len(), 2);
    assert!(log[0].1.contains("second") && log[1].1.contains("init"), "{log:?}");
    let raw = r.status_raw().unwrap();
    let snap = r.snapshot(Scope::History, &raw, Some((&log[0].0, &log[0].0))).unwrap();
    let paths: Vec<&str> = snap.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "c.txt"]);
    assert_eq!((text(&snap.changes[0].old), text(&snap.changes[0].new)), ("one\n".into(), "two\n".into()));
    assert!(snap.changes[1].old.is_none(), "c.txt was added by the commit");
    // The first commit has no parent: everything in it is added.
    let root = r.snapshot(Scope::History, &raw, Some((&log[1].0, &log[1].0))).unwrap();
    assert_eq!(root.changes.len(), 2);
    assert!(root.changes.iter().all(|c| c.old.is_none()));
}

#[test]
fn moves_are_whole_blocks_in_order_and_know_their_other_end() {
    // Three common lines removed here and added there, but not as one block: not a move.
    let old_a = "fn a() {\n    return Ok(());\n}\nfn b() {\n    log_value(x);\n}\nfn c() {\n    return Ok(());\n}\n";
    let new_a = "fn a() {\n}\nfn b() {\n}\nfn c() {\n}\n";
    let old_b = "top\n";
    let new_b = "top\nfn d() {\n    return Ok(());\n    other_thing();\n    log_value(x);\n}\n";
    // And a real block move, from c.rs line 2 to d.rs line 2.
    let block = "let total = compute_total();\nrecord_metric(total);\nflush_metrics_now();\n";
    let old_c = format!("head\n{block}tail\n");
    let new_d = format!("start\n{block}end\n");
    let mut files = vec![
        FileDiff::between_with_context(Some("a.rs".into()), Some("a.rs".into()), old_a, new_a, 3),
        FileDiff::between_with_context(Some("b.rs".into()), Some("b.rs".into()), old_b, new_b, 3),
        FileDiff::between_with_context(Some("c.rs".into()), Some("c.rs".into()), &old_c, "head\ntail\n", 3),
        FileDiff::between_with_context(Some("d.rs".into()), Some("d.rs".into()), "start\nend\n", &new_d, 3),
    ];
    crate::diff::mark_moved(&mut files);
    let moved = |f: &FileDiff| f.moves.len();
    assert_eq!((moved(&files[0]), moved(&files[1])), (0, 0), "scattered common lines aren't a move");
    type End = (Option<u32>, Option<u32>, Option<(u32, u32)>);
    let ends = |f: &FileDiff| -> Vec<End> {
        f.hunks.iter().enumerate().flat_map(|(h, hk)| hk.lines.iter().enumerate().filter_map(move |(i, l)| f.twin(h, i).map(|t| (l.old, l.new, Some(t))))).collect()
    };
    let gone: Vec<_> = ends(&files[2]).into_iter().map(|(old, _, t)| (old, t)).collect();
    assert_eq!(gone, [(Some(2), Some((3, 2))), (Some(3), Some((3, 3))), (Some(4), Some((3, 4)))], "each removed line points at its new home");
    let came: Vec<_> = ends(&files[3]).into_iter().map(|(_, _, t)| t).collect();
    assert_eq!(came, [Some((2, 2)), Some((2, 3)), Some((2, 4))], "and back");
}

#[test]
fn word_edits_line_up_and_apply_in_any_subset() {
    use crate::diff::{apply_edits, word_edits};
    let (old, new) = ("let total = price * qty;", "let sum = price * quantity + tax;");
    let e = word_edits(old, new);
    let shown: Vec<(&str, &str)> = e.iter().map(|e| (&old[e.old.clone()], &new[e.new.clone()])).collect();
    assert_eq!(shown, [("total", "sum"), ("qty", "quantity + tax")]);
    assert_eq!(apply_edits(old, new, &e, &[true, true]), new);
    assert_eq!(apply_edits(old, new, &e, &[false, false]), old);
    assert_eq!(apply_edits(old, new, &e, &[true, false]), "let sum = price * qty;");
    assert_eq!(apply_edits(old, new, &e, &[false, true]), "let total = price * quantity + tax;");
}

#[test]
fn staging_unstaging_and_discarding_some_words_of_a_line() {
    use crate::diff::{apply_edits, word_edits};
    let (dir, r) = repo("words", &[("w.rs", "fn a() {\n    let total = price * qty;\n}\n")]);
    write(&dir, "w.rs", "fn a() {\n    let sum = price * quantity;\n}\n");
    let f = file(&r, Scope::Unstaged, "w.rs");
    let lines = &f.hunks[0].lines;
    let ri = lines.iter().position(|l| l.kind == crate::diff::LineKind::Removed).unwrap();
    let ai = lines.iter().position(|l| l.kind == crate::diff::LineKind::Added).unwrap();
    let (old, new) = (lines[ri].text.clone(), lines[ai].text.clone());
    let e = word_edits(&old, &new);
    assert_eq!(e.len(), 2);
    // Stage only the rename of `qty`.
    let target = apply_edits(&old, &new, &e, &[false, true]);
    r.apply(&patch::line_to(&f, 0, ri, ai, &target, false).unwrap(), true, false).unwrap();
    assert_eq!(sh(&dir, &["show", ":w.rs"]), "fn a() {\n    let total = price * quantity;\n}\n");
    // Unstage it again from the staged view.
    let staged = file(&r, Scope::Staged, "w.rs");
    let (sr, sa) = (staged.hunks[0].lines.iter().position(|l| l.kind == crate::diff::LineKind::Removed).unwrap(), staged.hunks[0].lines.iter().position(|l| l.kind == crate::diff::LineKind::Added).unwrap());
    let (so, sn) = (staged.hunks[0].lines[sr].text.clone(), staged.hunks[0].lines[sa].text.clone());
    let se = word_edits(&so, &sn);
    let keep = apply_edits(&so, &sn, &se, &vec![false; se.len()]);
    r.apply(&patch::line_to(&staged, 0, sr, sa, &keep, true).unwrap(), true, true).unwrap();
    assert_eq!(sh(&dir, &["show", ":w.rs"]), "fn a() {\n    let total = price * qty;\n}\n");
    // Discard the `total` → `sum` rename in the working tree, keeping the other.
    let keep = apply_edits(&old, &new, &e, &[false, true]);
    r.apply(&patch::line_to(&f, 0, ri, ai, &keep, true).unwrap(), false, true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("w.rs")).unwrap(), "fn a() {\n    let total = price * quantity;\n}\n");
}

#[test]
fn stage_all_and_unstage_all() {
    let (dir, r) = repo("all", &[("a.txt", "a\n")]);
    write(&dir, "a.txt", "a2\n");
    write(&dir, "b.txt", "new\n");
    r.stage_all().unwrap();
    assert_eq!(sh(&dir, &["diff", "--cached", "--name-only"]), "a.txt\nb.txt\n");
    r.unstage_all().unwrap();
    assert_eq!(sh(&dir, &["diff", "--cached", "--name-only"]), "");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a2\n", "the working tree is untouched");
}

#[test]
fn the_graph_reads_every_branch_with_its_labels_and_lanes() {
    let (dir, r) = repo("graph", &[("a.txt", "one\n")]);
    let main = "main";
    sh(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "f.txt", "feature\n");
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "feature work"]);
    sh(&dir, &["checkout", "-q", main]);
    write(&dir, "a.txt", "two\n");
    sh(&dir, &["commit", "-qam", "main work"]);
    sh(&dir, &["tag", "v1"]);
    sh(&dir, &["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"]);
    let (commits, head) = r.graph(50, None);
    let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects.len(), 4, "{subjects:?}");
    assert_eq!(subjects[0], "Merge feature");
    assert_eq!(head.as_deref(), Some(commits[0].id.as_str()));
    assert_eq!(commits[0].parents.len(), 2);
    let top = &commits[0].refs[0];
    assert!(top.head && top.name == main && top.kind == crate::graph::RefKind::Branch, "{:?}", commits[0].refs);
    assert!(commits.iter().any(|c| c.refs.iter().any(|x| x.name == "v1" && x.kind == crate::graph::RefKind::Tag)));
    assert!(commits.iter().any(|c| c.refs.iter().any(|x| x.name == "feature")));
    let (rows, lanes) = crate::graph::layout(&commits);
    assert_eq!(lanes, 2);
    assert_eq!(rows[0].col, 0);
    assert_eq!(rows[0].down.len(), 2, "the merge opens a lane for feature");
    // One branch alone: the merge and what's under it, not other tips.
    let (only, _) = r.graph(50, Some("feature"));
    assert_eq!(only.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["feature work", "init"]);
}

#[test]
fn remotes_give_their_web_pages() {
    use crate::git::web_url;
    assert_eq!(web_url("git@github.com:ada/repo.git").as_deref(), Some("https://github.com/ada/repo"));
    assert_eq!(web_url("https://github.com/ada/repo.git\n").as_deref(), Some("https://github.com/ada/repo"));
    assert_eq!(web_url("https://ada:token@gitlab.com/g/sub/repo").as_deref(), Some("https://gitlab.com/g/sub/repo"));
    assert_eq!(web_url("ssh://git@example.com:2222/ada/repo.git").as_deref(), Some("https://example.com/ada/repo"));
    assert_eq!(web_url("/local/path/repo"), None);
}

#[test]
fn blame_names_the_commit_of_each_line() {
    let (dir, r) = repo("blame", &[("a.txt", "one\ntwo\n")]);
    write(&dir, "a.txt", "one\nTWO\nthree\n");
    sh(&dir, &["commit", "-qam", "second"]);
    write(&dir, "a.txt", "one\nTWO\nthree\nfour\n");
    let at_head = r.blame("a.txt", Some("HEAD")).unwrap();
    assert_eq!(at_head.iter().map(|b| (b.summary.as_str(), b.text.as_str())).collect::<Vec<_>>(), [("init", "one"), ("second", "TWO"), ("second", "three")]);
    assert_eq!(at_head[0].author, "t");
    let now = r.blame("a.txt", None).unwrap();
    assert_eq!(now.len(), 4);
    assert!(now[3].id.bytes().all(|b| b == b'0'), "an uncommitted line has no commit");
    let log = r.file_log("a.txt").unwrap();
    assert_eq!(log.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["second", "init"]);
}

#[test]
fn a_range_shows_what_changed_between_two_commits() {
    let (dir, r) = repo("range", &[("a.txt", "one\n"), ("b.txt", "same\n")]);
    let first = r.resolve("HEAD").unwrap();
    write(&dir, "a.txt", "one\ntwo\n");
    sh(&dir, &["commit", "-qam", "second"]);
    write(&dir, "c.txt", "new\n");
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "third"]);
    let last = r.resolve("HEAD").unwrap();
    let range = format!("{first}..{last}");
    let raw = r.status_raw().unwrap();
    let snap = r.snapshot(Scope::History, &raw, Some((&range, &range))).unwrap();
    let paths: Vec<&str> = snap.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "c.txt"]);
    assert!(snap.changes[1].old.is_none(), "c.txt is new in the range");
}

#[test]
fn stash_branch_and_reset_move_the_repository() {
    let (dir, r) = repo("moves", &[("a.txt", "one\n")]);
    let first = r.resolve("HEAD").unwrap();
    write(&dir, "a.txt", "one\ntwo\n");
    r.stash("half done").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    let stashes = r.stashes();
    assert_eq!(stashes.len(), 1);
    assert!(stashes[0].message.contains("half done"));
    r.stash_do("pop", &stashes[0].name).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\ntwo\n");
    assert!(r.stashes().is_empty());
    r.stage_all().unwrap();
    r.commit("second", false).unwrap();
    r.commit("second, reworded", true).unwrap();
    assert_eq!(r.last_message().unwrap().trim(), "second, reworded");
    r.new_branch("side", Some(&first), true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    r.switch("main").unwrap();
    r.revert("HEAD").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    r.reset("hard", &first).unwrap();
    assert_eq!(r.resolve("HEAD").unwrap(), first);
}

#[test]
fn conflicts_settle_by_side_by_block_or_both() {
    let text = "a\n<<<<<<< HEAD\nours 1\n=======\ntheirs 1\n>>>>>>> other\nb\n<<<<<<< HEAD\nours 2\n||||||| base\nbase 2\n=======\ntheirs 2\n>>>>>>> other\nc\n";
    assert_eq!(patch::resolve(text, patch::Keep::Ours, None), ("a\nours 1\nb\nours 2\nc\n".to_owned(), 2));
    assert_eq!(patch::resolve(text, patch::Keep::Theirs, None), ("a\ntheirs 1\nb\ntheirs 2\nc\n".to_owned(), 2));
    assert_eq!(patch::resolve(text, patch::Keep::Both, None).0, "a\nours 1\ntheirs 1\nb\nours 2\ntheirs 2\nc\n");
    // Only the block starting on line 8 is settled.
    let (second, n) = patch::resolve(text, patch::Keep::Theirs, Some((7, 14)));
    assert_eq!(n, 1);
    assert!(second.starts_with("a\n<<<<<<< HEAD\n") && second.ends_with("b\ntheirs 2\nc\n"));
    // An unclosed block is left as written.
    assert_eq!(patch::resolve("x\n<<<<<<< HEAD\ny\n", patch::Keep::Ours, None), ("x\n<<<<<<< HEAD\ny\n".to_owned(), 0));

    let (dir, r) = repo("conflict", &[("a.txt", "one\n")]);
    sh(&dir, &["switch", "-qc", "other"]);
    write(&dir, "a.txt", "theirs\n");
    sh(&dir, &["commit", "-qam", "theirs"]);
    sh(&dir, &["switch", "-q", "main"]);
    write(&dir, "a.txt", "ours\n");
    sh(&dir, &["commit", "-qam", "ours"]);
    let merge = Command::new("git").arg("-C").arg(&dir).args(["merge", "-q", "other"]).output().unwrap();
    assert!(!merge.status.success(), "the merge stops on the conflict");
    let changes = snapshot(&r, Scope::Uncommitted);
    assert!(changes[0].conflicted && changes[0].path == "a.txt");
    r.take_side("a.txt", "theirs").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "theirs\n");
    assert!(!snapshot(&r, Scope::Uncommitted)[0].conflicted, "staged, so resolved");
}

/// A repository whose `main` and `other` change the same line, so merging either way conflicts.
fn diverged(name: &str) -> (PathBuf, Repo) {
    let (dir, r) = repo(name, &[("a.txt", "one\n")]);
    sh(&dir, &["switch", "-qc", "other"]);
    write(&dir, "a.txt", "theirs\n");
    sh(&dir, &["commit", "-qam", "theirs"]);
    sh(&dir, &["switch", "-q", "main"]);
    write(&dir, "a.txt", "ours\n");
    sh(&dir, &["commit", "-qam", "ours"]);
    (dir, r)
}

#[test]
fn a_stopped_merge_or_rebase_is_seen_and_aborts() {
    use crate::git::Operation;
    let (dir, r) = diverged("operation");
    assert_eq!(r.operation(), None);
    assert!(Command::new("git").arg("-C").arg(&dir).args(["merge", "other"]).output().unwrap().status.code() != Some(0));
    assert_eq!(r.operation(), Some(Operation::Merge));
    let raw = r.status_raw().unwrap();
    assert_eq!(r.snapshot(Scope::Uncommitted, &raw, None).unwrap().operation, Some(Operation::Merge));
    r.resume(Operation::Merge, "abort").unwrap();
    assert_eq!(r.operation(), None);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "ours\n", "abort puts the file back");
    assert!(Command::new("git").arg("-C").arg(&dir).args(["rebase", "other"]).output().unwrap().status.code() != Some(0));
    assert_eq!(r.operation(), Some(Operation::Rebase));
    // Settled in favour of theirs and staged, the rebase continues without an editor.
    write(&dir, "a.txt", "theirs\n");
    sh(&dir, &["add", "a.txt"]);
    let done = r.resume(Operation::Rebase, "continue");
    assert!(done.is_ok() || r.resume(Operation::Rebase, "skip").is_ok(), "{done:?}");
    assert_eq!(r.operation(), None);
}

#[test]
fn merging_a_branch_brings_its_commits_or_stops_on_conflicts() {
    let (dir, r) = repo("merge", &[("a.txt", "one\n"), ("b.txt", "two\n")]);
    sh(&dir, &["switch", "-qc", "other"]);
    write(&dir, "b.txt", "two, changed\n");
    sh(&dir, &["commit", "-qam", "theirs"]);
    sh(&dir, &["switch", "-q", "main"]);
    r.merge("other").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "two, changed\n");
    let (_, r) = diverged("merge-conflict");
    let e = r.merge("other").unwrap_err();
    assert!(e.contains("stopped on conflicts"), "{e}");
    assert_eq!(r.operation(), Some(crate::git::Operation::Merge));
}

#[test]
fn rebasing_replays_the_branch_on_its_new_base() {
    let (dir, r) = repo("rebase", &[("a.txt", "one\n"), ("b.txt", "two\n")]);
    sh(&dir, &["switch", "-qc", "feature"]);
    write(&dir, "b.txt", "two, mine\n");
    sh(&dir, &["commit", "-qam", "mine"]);
    sh(&dir, &["switch", "-q", "main"]);
    write(&dir, "a.txt", "one, upstream\n");
    sh(&dir, &["commit", "-qam", "upstream"]);
    sh(&dir, &["switch", "-q", "feature"]);
    r.rebase("main").unwrap();
    assert_eq!(sh(&dir, &["log", "--format=%s"]).lines().collect::<Vec<_>>(), ["mine", "upstream", "init"]);
    let (_, r) = diverged("rebase-conflict");
    assert!(r.rebase("other").unwrap_err().contains("stopped on conflicts"));
    assert_eq!(r.operation(), Some(crate::git::Operation::Rebase));
}

#[test]
fn branches_rename_and_delete_saying_whether_anything_is_lost() {
    let (dir, r) = repo("branches", &[("a.txt", "one\n")]);
    sh(&dir, &["branch", "merged"]);
    sh(&dir, &["switch", "-qc", "ahead"]);
    write(&dir, "a.txt", "two\n");
    sh(&dir, &["commit", "-qam", "ahead"]);
    sh(&dir, &["switch", "-q", "main"]);
    assert!(r.merged("merged") && !r.merged("ahead"));
    r.rename_branch("ahead", "renamed").unwrap();
    assert!(r.branches().contains(&"renamed".to_owned()) && !r.branches().contains(&"ahead".to_owned()));
    r.delete_branch("renamed").unwrap();
    r.delete_branch("merged").unwrap();
    assert_eq!(r.branches(), ["main"]);
}

#[test]
fn tags_are_made_listed_and_deleted() {
    let (dir, r) = repo("tags", &[("a.txt", "one\n")]);
    r.tag("v1", "HEAD", "").unwrap();
    write(&dir, "a.txt", "two\n");
    sh(&dir, &["commit", "-qam", "two"]);
    r.tag("v2", "HEAD", "Second release").unwrap();
    assert_eq!(sh(&dir, &["cat-file", "-t", "v2"]).trim(), "tag", "a message makes it annotated");
    assert!(r.tags().contains(&"v1".to_owned()) && r.tags().contains(&"v2".to_owned()));
    r.delete_tag("v1", false).unwrap();
    assert_eq!(r.tags(), ["v2"]);
}

#[test]
fn force_push_replaces_a_rewritten_branch_unless_the_remote_moved() {
    let (remote, _) = repo("force-remote", &[("a.txt", "one\n")]);
    sh(&remote, &["config", "receive.denyCurrentBranch", "ignore"]);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("collate-force-clone-{}-{nanos}", std::process::id()));
    sh(&std::env::temp_dir(), &["clone", "-q", remote.to_str().unwrap(), dir.to_str().unwrap()]);
    sh(&dir, &["config", "user.email", "t@t"]);
    sh(&dir, &["config", "user.name", "t"]);
    let r = Repo::open(&dir).unwrap();
    write(&dir, "a.txt", "two\n");
    sh(&dir, &["commit", "-qam", "two"]);
    r.push().unwrap();
    sh(&dir, &["commit", "-q", "--amend", "-m", "two, reworded"]);
    assert!(r.push().is_err(), "a plain push refuses a rewritten branch");
    r.force_push().unwrap();
    assert_eq!(sh(&remote, &["log", "-1", "--format=%s", "main"]).trim(), "two, reworded");
}

#[test]
fn a_diverged_branch_pulls_by_rebasing_its_own_commits() {
    let (remote, _) = repo("pull-remote", &[("a.txt", "one\n")]);
    sh(&remote, &["config", "receive.denyCurrentBranch", "ignore"]);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("collate-pull-clone-{}-{nanos}", std::process::id()));
    sh(&std::env::temp_dir(), &["clone", "-q", remote.to_str().unwrap(), dir.to_str().unwrap()]);
    sh(&dir, &["config", "user.email", "t@t"]);
    sh(&dir, &["config", "user.name", "t"]);
    let r = Repo::open(&dir).unwrap();
    write(&remote, "theirs.txt", "theirs\n");
    sh(&remote, &["add", "-A"]);
    sh(&remote, &["commit", "-qm", "theirs"]);
    write(&dir, "mine.txt", "mine\n");
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "mine"]);
    write(&dir, "a.txt", "uncommitted\n");
    assert!(r.pull().is_err(), "a fast-forward-only pull refuses");
    r.pull_rebase().unwrap();
    assert_eq!(sh(&dir, &["log", "--format=%s"]), "mine\ntheirs\ninit\n");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "uncommitted\n", "what wasn't committed comes back");
}

#[test]
fn the_first_push_goes_to_the_only_remote() {
    let (remote, _) = repo("push-remote", &[("a.txt", "one\n")]);
    let (dir, r) = repo("push-local", &[("a.txt", "one\n")]);
    assert!(r.push().unwrap_err().contains("no remote"));
    sh(&dir, &["remote", "add", "upstream", remote.to_str().unwrap()]);
    sh(&dir, &["switch", "-qc", "feature"]);
    r.push().unwrap();
    assert_eq!(sh(&dir, &["rev-parse", "--abbrev-ref", "@{u}"]).trim(), "upstream/feature");
}

#[test]
fn an_older_commit_is_reworded_keeping_its_body_and_what_came_after() {
    let (dir, r) = repo("reword", &[("a.txt", "one\n")]);
    write(&dir, "a.txt", "two\n");
    sh(&dir, &["commit", "-qam", "second\n\nwhy it's so"]);
    let second = sh(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    write(&dir, "b.txt", "b\n");
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "third"]);
    write(&dir, "a.txt", "dirty\n");
    r.reword(&second, "second, it's 'quoted'").unwrap();
    assert_eq!(sh(&dir, &["log", "--format=%s"]), "third\nsecond, it's 'quoted'\ninit\n");
    assert_eq!(sh(&dir, &["log", "-1", "--skip=1", "--format=%b"]).trim(), "why it's so");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "dirty\n");
    assert!(dir.join("b.txt").exists());
    // The first commit has no parent to replay from.
    let root = sh(&dir, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_owned();
    r.reword(&root, "start").unwrap();
    assert_eq!(sh(&dir, &["log", "--format=%s"]), "third\nsecond, it's 'quoted'\nstart\n");
}

#[test]
fn one_file_is_stashed_or_ignored_alone() {
    let (dir, r) = repo("stash-one", &[("a.txt", "one\n"), ("b.txt", "one\n")]);
    write(&dir, "a.txt", "two\n");
    write(&dir, "b.txt", "two\n");
    r.stash_paths("just a", &["a.txt"]).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "two\n", "the other file stays");
    write(&dir, "build/out.log", "x\n");
    write(&dir, ".gitignore", "*.tmp");
    r.ignore("build/out.log").unwrap();
    r.ignore("build/out.log").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "*.tmp\n/build/out.log\n", "once, on its own line");
    assert!(!sh(&dir, &["status", "--porcelain"]).contains("out.log"));
}

#[test]
fn operations_that_move_head_undo() {
    use crate::git::Back;
    let (dir, r) = repo("undo", &[("a.txt", "one\n")]);
    let head = |r: &Repo| r.head_at().unwrap();
    let start = head(&r);
    // A commit comes back staged.
    write(&dir, "a.txt", "two\n");
    r.stage("a.txt").unwrap();
    let (done, moved) = r.undoable("the commit", Back::Soft, false, |r| r.commit("second", false));
    assert!(done.is_ok());
    let moved = moved.expect("a commit moves HEAD");
    r.undo(&moved).unwrap();
    assert_eq!(head(&r), start);
    assert_eq!(sh(&dir, &["diff", "--cached", "--name-only"]).trim(), "a.txt");
    // A hard reset gives back the commit and the changes it threw away.
    r.commit("second", false).unwrap();
    let second = head(&r);
    write(&dir, "a.txt", "three\n");
    let (_, moved) = r.undoable("the reset", Back::Keep, true, |r| r.reset("hard", &start.1));
    r.undo(&moved.unwrap()).unwrap();
    assert_eq!(head(&r), second);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "three\n");
    // A switch goes back to the branch it left.
    sh(&dir, &["checkout", "-q", "--", "a.txt"]);
    sh(&dir, &["branch", "other", &start.1]);
    let (_, moved) = r.undoable("the switch", Back::Switch, false, |r| r.switch("other"));
    let moved = moved.unwrap();
    // Not once HEAD has moved since: that would throw the new work away.
    write(&dir, "b.txt", "b\n");
    r.stage("b.txt").unwrap();
    r.commit("on other", false).unwrap();
    assert!(r.undo(&moved).is_err());
    sh(&dir, &["reset", "-q", "--hard", "HEAD^"]);
    r.undo(&moved).unwrap();
    assert_eq!(head(&r), second);
    // Nothing moved, nothing to undo.
    assert!(r.undoable("the reset", Back::Keep, false, |r| r.reset("hard", "HEAD")).1.is_none());
}

#[test]
fn staged_changes_fix_up_an_earlier_commit() {
    let (dir, r) = repo("fixup", &[("a.txt", "one\n"), ("b.txt", "b\n")]);
    let first = r.head_at().unwrap().1;
    write(&dir, "b.txt", "b2\n");
    sh(&dir, &["commit", "-qam", "second"]);
    // Staged goes into the first commit; unstaged stays where it is.
    write(&dir, "a.txt", "ONE\n");
    r.stage("a.txt").unwrap();
    write(&dir, "b.txt", "b3\n");
    r.fixup(&first).unwrap();
    assert_eq!(sh(&dir, &["log", "--format=%s"]).lines().collect::<Vec<_>>(), ["second", "init"]);
    assert_eq!(sh(&dir, &["show", "HEAD^:a.txt"]), "ONE\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b3\n");
    assert!(sh(&dir, &["diff", "--cached"]).is_empty());
    // Not a commit off the branch.
    sh(&dir, &["branch", "side"]);
    sh(&dir, &["checkout", "-q", "side"]);
    sh(&dir, &["commit", "-qam", "side"]);
    let side = r.head_at().unwrap().1;
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "a.txt", "x\n");
    r.stage("a.txt").unwrap();
    assert!(r.fixup(&side).is_err());
}

#[test]
fn a_conflicted_file_merges_from_its_three_versions() {
    let (dir, r) = repo("merge3", &[("m.txt", "a\nb\nc\nd\ne\n")]);
    sh(&dir, &["checkout", "-qb", "side"]);
    write(&dir, "m.txt", "a\nb\nc\nd\nE theirs\n");
    sh(&dir, &["commit", "-qam", "side"]);
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "m.txt", "a\nB\nc\nd\nE ours\n");
    sh(&dir, &["commit", "-qam", "main"]);
    assert!(r.merge("side").is_err(), "the merge stops on the conflict");
    let (base, ours, theirs) = r.merge_sides("m.txt").unwrap();
    assert_eq!((base.as_str(), ours.as_str(), theirs.as_str()), ("a\nb\nc\nd\ne\n", "a\nB\nc\nd\nE ours\n", "a\nb\nc\nd\nE theirs\n"));
    let mut m = crate::merge::Merge::new("m.txt".into(), &base, &ours, &theirs, None);
    assert_eq!((m.auto_merged(), m.conflicts().len()), (1, 1));
    m.take(0, crate::merge::Take::Theirs);
    r.save_merge("m.txt", &m.text().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "a\nB\nc\nd\nE theirs\n");
    assert!(sh(&dir, &["diff", "--name-only", "--diff-filter=U"]).is_empty(), "staged, so resolved");
    // A file one side deleted can't be merged by line.
    sh(&dir, &["commit", "-qm", "merged"]);
    sh(&dir, &["checkout", "-qb", "gone"]);
    sh(&dir, &["rm", "-q", "m.txt"]);
    sh(&dir, &["commit", "-qm", "gone"]);
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "m.txt", "changed\n");
    sh(&dir, &["commit", "-qam", "changed"]);
    assert!(r.merge("gone").is_err());
    assert!(r.merge_sides("m.txt").unwrap_err().contains("deleted"));
}

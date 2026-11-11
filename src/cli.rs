//! The command line: a repository and how to show it, two files or folders to compare, a
//! patch to read, or a merge for `git mergetool`.
use std::path::{Path, PathBuf};

use crate::diff::{DiffMode, FileDiff, FileStatus, parse as parse_patch};
use crate::git::{self, Scope};

/// What Collate was opened on.
#[derive(Debug, PartialEq)]
pub enum Launch {
    /// A repository (the project opened last when none is named), shown as asked.
    Repo(Shown),
    /// Two files or folders, compared without a repository. `title` names the file for both
    /// sides, as `git difftool` passes it.
    Compare { a: PathBuf, b: PathBuf, title: Option<String> },
    /// A patch read from a file, or from standard input as `-`.
    Patch(PathBuf),
    /// `git mergetool`: the three versions, and where the result goes.
    Merge { base: PathBuf, local: PathBuf, remote: PathBuf, merged: PathBuf },
}

/// How to show a repository: the folder, the view and layout, a commit or range, and a filter
/// on the file list.
#[derive(Debug, Default, PartialEq)]
pub struct Shown {
    pub dir: Option<PathBuf>,
    pub scope: Option<Scope>,
    pub layout: Option<DiffMode>,
    pub rev: Option<String>,
    pub filter: Option<String>,
}

pub const USAGE: &str = "usage: collate [<repository>] [<commit> | <from>..<to>] [--staged | --unstaged | --branch | --history] [--split | --unified] [-- <path>]
       collate <file> <file>            compare two files (--title <name> names them)
       collate <folder> <folder>        compare two folders
       collate <file.patch> | -         read a patch from a file or standard input
       collate --merge <base> <local> <remote> <merged>   merge for git mergetool";

/// Reads the arguments Collate itself takes, after the window's own (`--snap` and the like)
/// are taken out. `piped` says standard input is a pipe or a file, so a patch with no other
/// argument comes from it, as in `git diff | collate`.
pub fn parse(args: &[String], piped: bool) -> Result<Launch, String> {
    let mut shown = Shown::default();
    let mut title = None;
    let mut plain: Vec<&str> = Vec::new();
    let mut it = args.iter().map(String::as_str).filter(|a| !a.starts_with("-psn_"));
    while let Some(a) = it.next() {
        let mut value = || it.next().map(str::to_owned).ok_or_else(|| format!("{a} needs a value"));
        match a {
            "--merge" => {
                let mut v = || value().map(PathBuf::from);
                let launch = Launch::Merge { base: v()?, local: v()?, remote: v()?, merged: v()? };
                return match it.next() {
                    None => Ok(launch),
                    Some(extra) => Err(format!("--merge takes four files; {extra} is one too many")),
                };
            }
            "--title" => title = Some(value()?),
            "--staged" => shown.scope = Some(Scope::Staged),
            "--unstaged" => shown.scope = Some(Scope::Unstaged),
            "--uncommitted" => shown.scope = Some(Scope::Uncommitted),
            "--branch" => shown.scope = Some(Scope::Branch),
            "--history" => shown.scope = Some(Scope::History),
            "--split" => shown.layout = Some(DiffMode::Split),
            "--unified" => shown.layout = Some(DiffMode::Unified),
            "--" => {
                let paths: Vec<&str> = it.by_ref().collect();
                match paths[..] {
                    [p] => shown.filter = Some(p.to_owned()),
                    [] => {}
                    _ => return Err("Give one path after --".into()),
                }
            }
            "-" => plain.push(a),
            _ if a.starts_with('-') => return Err(format!("unknown option {a}\n{USAGE}")),
            _ => plain.push(a),
        }
    }
    let path = |p: &str| PathBuf::from(p);
    let is_patch = |p: &str| p == "-" || [".patch", ".diff"].iter().any(|e| p.ends_with(e)) && Path::new(p).is_file();
    match plain[..] {
        [] if piped => Ok(Launch::Patch(path("-"))),
        [] => Ok(Launch::Repo(shown)),
        [p] if is_patch(p) => Ok(Launch::Patch(path(p))),
        [p] if Path::new(p).is_dir() => Ok(Launch::Repo(Shown { dir: Some(path(p)), ..shown })),
        [p] if Path::new(p).exists() => Err(format!("{p} is a file: give two to compare them\n{USAGE}")),
        [rev] => Ok(Launch::Repo(Shown { rev: Some(rev.to_owned()), ..shown })),
        [a, b] if Path::new(a).exists() && Path::new(b).exists() => {
            if Path::new(a).is_dir() != Path::new(b).is_dir() {
                return Err(format!("Compare two files or two folders, not one of each: {a}, {b}"));
            }
            Ok(Launch::Compare { a: path(a), b: path(b), title })
        }
        [dir, rev] if Path::new(dir).is_dir() => Ok(Launch::Repo(Shown { dir: Some(path(dir)), rev: Some(rev.to_owned()), ..shown })),
        [a, b] => Err(format!("No such file: {}", if Path::new(a).exists() { b } else { a })),
        _ => Err(format!("Too many arguments\n{USAGE}")),
    }
}

/// Standard input is a pipe or a redirected file rather than a terminal or nothing (an app
/// opened from the Finder or the Dock reads `/dev/null`).
pub fn piped() -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata("/dev/stdin").is_ok_and(|m| m.file_type().is_fifo() || m.is_file())
}

/// The files two files or folders differ in, named as the reader would: a folder's files by
/// their path inside it, two files by `title` when given.
pub fn compare(a: &Path, b: &Path, title: Option<&str>) -> Result<Vec<FileDiff>, String> {
    let mut files = parse_patch(&git::diff_no_index(a, b)?);
    // git names a file by the path it was given, its leading `/` and `./` dropped.
    let inside = |dir: &Path| format!("{}/", dir.to_string_lossy().trim_start_matches("./").trim_start_matches('/').trim_end_matches('/'));
    let (da, db) = (inside(a), inside(b));
    for f in &mut files {
        for p in [&mut f.old_path, &mut f.new_path].into_iter().flatten() {
            if let Some(t) = title {
                *p = t.to_owned();
            } else if a.is_dir()
                && let Some(rest) = p.strip_prefix(&da).or_else(|| p.strip_prefix(&db))
            {
                *p = rest.to_owned();
            }
        }
        // git took the two names for a rename; named alike, it's one file changed.
        if f.status == FileStatus::Renamed && f.old_path == f.new_path {
            f.status = FileStatus::Modified;
        }
    }
    Ok(files)
}

/// The files in a patch file, or in standard input for `-`.
pub fn read_patch(p: &Path) -> Result<Vec<FileDiff>, String> {
    let text = if p == Path::new("-") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| format!("Can't read standard input: {e}"))?;
        s
    } else {
        std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?
    };
    let files = parse_patch(&text);
    if files.is_empty() && !text.trim().is_empty() {
        return Err(format!("{} holds no diff", if p == Path::new("-") { "Standard input".into() } else { p.display().to_string() }));
    }
    Ok(files)
}

/// A short name for what's compared, for the window and the header.
pub fn title(a: &Path, b: &Path, title: Option<&str>) -> String {
    let name = |p: &Path| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
    match title {
        Some(t) => t.to_owned(),
        None if name(a) == name(b) => format!("{} - {} and {}", name(a), a.display(), b.display()),
        None => format!("{} and {}", name(a), name(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &str) -> Vec<String> {
        a.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_command_line_names_a_repository_files_a_patch_or_a_merge() {
        let dir = std::env::temp_dir().join(format!("collate-cli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("x")).unwrap();
        std::fs::create_dir_all(dir.join("y")).unwrap();
        for f in ["a.txt", "b.txt", "c.patch"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let at = |f: &str| dir.join(f).display().to_string();
        assert_eq!(parse(&[], false), Ok(Launch::Repo(Shown::default())));
        assert_eq!(parse(&[], true), Ok(Launch::Patch("-".into())), "git diff | collate");
        assert_eq!(parse(&args("--staged --split -- src"), false), Ok(Launch::Repo(Shown { scope: Some(Scope::Staged), layout: Some(DiffMode::Split), filter: Some("src".into()), ..Default::default() })));
        assert_eq!(parse(&args("main..topic"), false), Ok(Launch::Repo(Shown { rev: Some("main..topic".into()), ..Default::default() })));
        assert_eq!(parse(&[at("x"), "HEAD~2".into()], false), Ok(Launch::Repo(Shown { dir: Some(dir.join("x")), rev: Some("HEAD~2".into()), ..Default::default() })));
        assert_eq!(parse(&[at("a.txt"), at("b.txt"), "--title".into(), "src/a.rs".into()], false), Ok(Launch::Compare { a: dir.join("a.txt"), b: dir.join("b.txt"), title: Some("src/a.rs".into()) }));
        assert_eq!(parse(&[at("x"), at("y")], false), Ok(Launch::Compare { a: dir.join("x"), b: dir.join("y"), title: None }));
        assert_eq!(parse(&[at("c.patch")], false), Ok(Launch::Patch(dir.join("c.patch"))));
        assert_eq!(parse(&args("--merge b l r m"), false), Ok(Launch::Merge { base: "b".into(), local: "l".into(), remote: "r".into(), merged: "m".into() }));
        assert!(parse(&[at("a.txt"), at("x")], false).is_err(), "a file and a folder");
        assert!(parse(&[at("a.txt")], false).is_err(), "one file");
        assert!(parse(&args("--merge b l r"), false).is_err());
        assert!(parse(&args("--frobnicate"), false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_folders_compare_by_the_paths_inside_them() {
        let dir = std::env::temp_dir().join(format!("collate-compare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (f, text) in [("x/s/f", "a\n"), ("y/s/f", "b\n"), ("x/gone", "old\n"), ("y/new", "new\n")] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), text).unwrap();
        }
        let files = compare(&dir.join("x"), &dir.join("y"), None).unwrap();
        let mut paths: Vec<&str> = files.iter().map(FileDiff::path).collect();
        paths.sort();
        assert_eq!(paths, ["gone", "new", "s/f"]);
        let same = files.iter().find(|f| f.path() == "s/f").unwrap();
        assert_eq!((same.old_path.as_deref(), same.status), (Some("s/f"), FileStatus::Modified), "the same file on both sides, not a rename");
        let named = compare(&dir.join("x/s/f"), &dir.join("y/s/f"), Some("src/f.rs")).unwrap();
        assert_eq!((named.len(), named[0].path(), named[0].additions()), (1, "src/f.rs", 1));
        assert!(compare(&dir.join("x/s"), &dir.join("x/s"), None).unwrap().is_empty(), "nothing differs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

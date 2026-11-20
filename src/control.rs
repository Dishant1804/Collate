//! Collate driven by a coding agent: each window listens on a socket of its own, and
//! `collate --mcp` is a Model Context Protocol server, on standard input and output, that
//! speaks to those sockets. An agent reads what the reader sees (the view, the files, the
//! selection, the comments), moves the review (a view, a file, a line, a commit) and runs any
//! command of the menu bar by its id, as the reader would.
//!
//! The socket protocol is one JSON object a line each way: `{"method": "state", "params": {}}`
//! in, `{"ok": …}` or `{"error": "…"}` out.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use rune::Tx;
use serde_json::{Value, json};

use crate::diff::{self, DiffMode, FileDiff, FileStatus, LineKind, LineRange, LineRef, Side};
use crate::git::Scope;
use crate::{Collate, HANDLE, Screen, commands, current_repo};

/// How long a request waits for the UI thread, which may be busy with a large diff.
const ANSWER_WITHIN: Duration = Duration::from_secs(15);
/// The most files `state` lists and the most of a diff `diff` returns: an agent's context
/// is the limit, not the socket.
const FILES_CAP: usize = 400;
const DIFF_CAP: usize = 200_000;

/// `~/Library/Application Support/Collate/control`: one socket per running window, named by
/// its process id.
fn socket_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Collate/control"))
}

fn own_socket() -> Option<PathBuf> {
    Some(socket_dir()?.join(format!("{}.sock", std::process::id())))
}

/// Starts listening for agents. A failure is said in the window: everything else still works.
pub fn serve() {
    if let Err(e) = listen()
        && let Some(app) = HANDLE.get()
    {
        app.update(move |s, _| s.say(format!("Agents can't control this window: {e}"), true));
    }
}

fn listen() -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let (Some(dir), Some(path)) = (socket_dir(), own_socket()) else { return Err("no home folder".into()) };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Only this user's programs may drive the app: they can commit and push.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::thread::spawn(move || {
        prune(&dir);
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    Ok(())
}

/// Takes away the sockets nothing listens on: a window that crashed or was killed left its
/// own, and only an agent listing the windows would otherwise clear them.
fn prune(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "sock") && UnixStream::connect(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Takes the socket away as the app quits, so agents don't find a window that's gone.
pub fn stop() {
    if let Some(path) = own_socket() {
        let _ = std::fs::remove_file(path);
    }
}

/// Answers one connection's requests, a line each, until it closes.
fn answer(stream: UnixStream) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => on_ui_thread(req["method"].as_str().unwrap_or("").to_owned(), req.get("params").cloned().unwrap_or(json!({}))),
            Err(e) => Err(format!("Not JSON: {e}")),
        };
        let reply = match reply {
            Ok(v) => json!({ "ok": v }),
            Err(e) => json!({ "error": e }),
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
    }
}

fn on_ui_thread(method: String, params: Value) -> Result<Value, String> {
    let app = HANDLE.get().ok_or("The window isn't up yet")?;
    let (send, receive) = mpsc::sync_channel(1);
    app.update(move |s, tx| {
        let _ = send.send(handle(s, &method, &params, tx));
    });
    receive.recv_timeout(ANSWER_WITHIN).map_err(|_| "The window didn't answer in time".to_owned())?
}

/// One request, on the UI thread.
pub fn handle(s: &mut Collate, method: &str, p: &Value, tx: &mut Tx) -> Result<Value, String> {
    match method {
        "hello" => Ok(json!({ "pid": std::process::id(), "repo": repo_root(), "version": env!("CARGO_PKG_VERSION") })),
        "state" => Ok(state(s)),
        "commands" => Ok(commands::COMMANDS.iter().filter(|c| !c.id.is_empty()).map(|c| json!({
            "id": c.id, "title": c.title.trim_end_matches('…'), "menu": if c.menu.is_empty() { "Collate" } else { c.menu },
            "key": commands::keycap(c.key), "enabled": (c.enabled)(s), "checked": (c.checked)(s),
        })).collect()),
        "run" => {
            let id = text(p, "command")?;
            let c = commands::find(id).filter(|c| !c.id.is_empty()).ok_or_else(|| format!("No command {id}: `commands` lists them"))?;
            if !(c.enabled)(s) {
                return Err(format!("{} can't run now{}", c.title.trim_end_matches('…'), modal(s).map_or(String::new(), |m| format!(": the {m} is open"))));
            }
            (c.run)(s, tx);
            Ok(json!({ "ran": id, "state": state(s) }))
        }
        "show" => {
            let view = text(p, "view")?;
            match view {
                "pulls" => s.open_pulls(),
                "issues" => {
                    s.open_issues();
                    if let Some(n) = p["number"].as_u64() {
                        s.open_issue(n as u32);
                    }
                }
                "map" => {
                    s.pulls.open = false;
                    s.open_map();
                }
                _ => s.show_scope(scope(view).ok_or_else(|| format!("No view {view}: all, staged, unstaged, branch, history, pulls, issues or map"))?),
            }
            Ok(state(s))
        }
        "open_file" => {
            let f = file_named(s, text(p, "path")?)?;
            leave_for_diff(s);
            match p["line"].as_u64() {
                Some(n) => {
                    let at = line_ref(s, f, p, n)?;
                    s.diff.set_selection(Some(LineRange::line(f, at)));
                    s.diff.scroll_to_line(f, at, tx);
                }
                None => s.diff.scroll_to_file(f, tx),
            }
            let path = s.diff.files()[f].path().to_owned();
            s.select_in_lists(&path);
            Ok(state(s))
        }
        "select" => {
            let f = file_named(s, text(p, "path")?)?;
            let start = p["start"].as_u64().ok_or("start: the first line to select")?;
            let (a, b) = (line_ref(s, f, p, start)?, line_ref(s, f, p, p["end"].as_u64().unwrap_or(start))?);
            leave_for_diff(s);
            s.diff.set_selection(Some(LineRange { file: f, start: a, end: b }));
            s.diff.scroll_to_line(f, a, tx);
            Ok(state(s))
        }
        "clear_selection" => {
            s.selection_clear();
            Ok(state(s))
        }
        "diff" => {
            let files = s.diff.files();
            let shown: Vec<usize> = match p["path"].as_str() {
                Some(path) => vec![file_named(s, path)?],
                None => (0..files.len()).collect(),
            };
            let mut out = String::new();
            for &f in &shown {
                write_patch(&mut out, &files[f]);
                if out.len() > DIFF_CAP {
                    out.truncate(out.floor_char_boundary(DIFF_CAP));
                    out.push_str("\n… cut short: ask for one file by its path\n");
                    break;
                }
            }
            Ok(json!({ "view": scope_name(s.scope), "commit": s.commit, "patch": out }))
        }
        "comments" => Ok(comments(s)),
        "comment" => {
            let f = file_named(s, text(p, "path")?)?;
            let at = line_ref(s, f, p, p["line"].as_u64().ok_or("line: the line to comment on")?)?;
            let body = text(p, "body")?.trim().to_owned();
            if body.is_empty() {
                return Err("body: the comment's text".into());
            }
            let line = s.diff.side_line(f, at.side, at.line).ok_or("That line isn't in the diff")?.to_owned();
            let anchor = diff::Anchor { path: s.diff.files()[f].path().to_owned(), side: at.side, line: at.line, text: line };
            let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            s.comments.push(crate::Comment { at: anchor, body, time });
            s.store_comments();
            Ok(comments(s))
        }
        "delete_comment" => {
            let n = p["index"].as_u64().ok_or("index: the comment's, as `comments` numbers them")? as usize;
            if n >= s.comments.len() {
                return Err(format!("No comment {n}: there are {}", s.comments.len()));
            }
            s.delete_comment(n);
            Ok(comments(s))
        }
        "open_commit" => {
            let rev = text(p, "rev")?;
            let repo = current_repo().ok_or("No project is open")?;
            let id = match rev.split_once("..") {
                Some((a, b)) => format!("{}..{}", repo.resolve(if a.is_empty() { "HEAD" } else { a })?, repo.resolve(if b.is_empty() { "HEAD" } else { b })?),
                None => repo.resolve(rev)?,
            };
            s.open_commit(&id);
            Ok(state(s))
        }
        "open_project" => {
            let dir = Path::new(text(p, "path")?);
            // Opening says a failure in the window; the agent hears it too.
            crate::git::Repo::open(dir)?;
            s.open_project(dir, tx);
            Ok(state(s))
        }
        "say" => {
            let note = text(p, "text")?;
            s.say(note, p["error"].as_bool().unwrap_or(false));
            Ok(json!({ "said": note }))
        }
        "dismiss" => {
            // What Escape closes, short of the merge editor, which holds unsaved work.
            s.palette = false;
            s.jumping = false;
            s.show_help = false;
            s.project_open = false;
            s.confirm = None;
            s.words = None;
            s.prompt = None;
            s.risky = None;
            s.compare = None;
            s.blame = None;
            s.file_log = None;
            if s.screen == Screen::Settings {
                s.screen = Screen::Review;
            }
            Ok(state(s))
        }
        _ => Err(format!("No method {method}")),
    }
}

fn text<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    p[key].as_str().ok_or_else(|| format!("{key} is missing"))
}

fn repo_root() -> Option<String> {
    current_repo().map(|r| r.root.display().to_string())
}

fn scope(name: &str) -> Option<Scope> {
    Some(match name {
        "all" | "uncommitted" => Scope::Uncommitted,
        "staged" => Scope::Staged,
        "unstaged" => Scope::Unstaged,
        "branch" => Scope::Branch,
        "history" => Scope::History,
        _ => return None,
    })
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Uncommitted => "all",
        Scope::Staged => "staged",
        Scope::Unstaged => "unstaged",
        Scope::Branch => "branch",
        Scope::History => "history",
    }
}

fn side_name(side: Side) -> &'static str {
    if side == Side::Old { "old" } else { "new" }
}

fn status_name(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Modified => "modified",
        FileStatus::Added => "added",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed => "renamed",
        FileStatus::Copied => "copied",
    }
}

/// The shown file at `path`: its new path, or its old one for a file deleted or renamed.
fn file_named(s: &Collate, path: &str) -> Result<usize, String> {
    let path = path.trim_start_matches("./");
    let files = s.diff.files();
    files.iter().position(|f| f.path() == path).or_else(|| files.iter().position(|f| f.old_path.as_deref() == Some(path)))
        .ok_or_else(|| format!("{path} isn't in the diff shown ({} view, {} files)", scope_name(s.scope), files.len()))
}

/// Line `n` of file `f` on the side `p` names: the new side unless it says `old` or the
/// file was deleted. Any line of the file will do: an unchanged one folded away opens up.
fn line_ref(s: &Collate, f: usize, p: &Value, n: u64) -> Result<LineRef, String> {
    let side = match p["side"].as_str() {
        Some("old") => Side::Old,
        Some("new") | None if s.diff.files()[f].new_path.is_some() => Side::New,
        None => Side::Old,
        Some(other) => return Err(format!("side is old or new, not {other}")),
    };
    let n = u32::try_from(n).map_err(|_| "No such line")?;
    if n == 0 || s.diff.side_line(f, side, n).is_none() {
        return Err(format!("{} has no line {n} on its {} side", s.diff.files()[f].path(), side_name(side)));
    }
    Ok(LineRef::new(side, n))
}

/// A file's lines are in the diff, not under the Map, the pull request list or History's
/// commit list.
fn leave_for_diff(s: &mut Collate) {
    s.map.open = false;
    s.pulls.open = false;
    s.issues.open = false;
    s.jumping = false;
    s.palette = false;
    if s.scope == Scope::History {
        s.history_diff = true;
    }
    s.pulls.tab = crate::pulls::PrTab::Files;
}

/// What's up in front of the review, which commands wait behind.
fn modal(s: &Collate) -> Option<&'static str> {
    Some(if s.confirm.is_some() || s.risky.is_some() {
        "confirmation dialog"
    } else if s.words.is_some() {
        "word picker"
    } else if s.prompt.is_some() {
        "prompt"
    } else if s.compare.is_some() {
        "compare picker"
    } else if s.blame.is_some() {
        "blame dialog"
    } else if s.file_log.is_some() {
        "file history dialog"
    } else if s.merge.is_some() {
        "merge editor"
    } else if s.jumping {
        "Go to File palette"
    } else if s.palette {
        "command palette"
    } else if s.show_help {
        "shortcuts sheet"
    } else if s.project_open {
        "project switcher"
    } else if s.screen == Screen::Settings {
        "settings"
    } else {
        return None;
    })
}

/// What the reader sees, as an agent reads it.
fn state(s: &Collate) -> Value {
    let place = if !s.has_project && s.loose.is_none() {
        "welcome"
    } else if s.map.open {
        "map"
    } else if s.issues.open {
        "issues"
    } else if s.pulls.open {
        "pull requests"
    } else if s.pulls.pr.is_some() {
        "pull request"
    } else if s.scope == Scope::History && !s.history_diff {
        "commit list"
    } else {
        "diff"
    };
    let files = s.diff.files();
    let list: Vec<Value> = files.iter().enumerate().take(FILES_CAP).map(|(i, f)| {
        let mut v = json!({ "path": f.path(), "status": status_name(f.status), "additions": f.additions(), "deletions": f.deletions() });
        if let Some(old) = f.old_path.as_deref().filter(|o| *o != f.path()) {
            v["old_path"] = json!(old);
        }
        if f.binary {
            v["binary"] = json!(true);
        }
        if s.is_viewed(i) {
            v["viewed"] = json!(true);
        }
        if s.diff.collapsed(i) {
            v["collapsed"] = json!(true);
        }
        v
    }).collect();
    let selection = s.diff.selection().and_then(|sel| {
        let f = files.get(sel.file)?;
        let (side, a, b) = s.diff.selected_span()?;
        Some(json!({ "path": f.path(), "side": side_name(side), "start": a, "end": b, "text": s.diff.selected_text() }))
    });
    let pr = s.pulls.pr.as_ref().map(|p| json!({ "number": p.pull.number, "title": p.pull.title }));
    let issue = s.issues.number.filter(|_| s.issues.open).map(|n| {
        let d = s.issues.detail.as_ref().and_then(|d| d.as_ref().ok());
        json!({ "number": n, "title": d.map(|d| d.issue.title.as_str()), "open": d.map(|d| d.issue.open) })
    });
    json!({
        "pid": std::process::id(),
        "repo": repo_root(),
        "project": s.name,
        "branch": s.branch,
        "head": s.head,
        "view": scope_name(s.scope),
        "place": place,
        "compared": s.loose,
        "commit": if s.scope == Scope::History { s.commit.clone() } else { None },
        "base": if s.scope == Scope::Branch { s.base.clone() } else { None },
        "pull_request": pr,
        "issue": issue,
        "loaded": s.loaded,
        "error": s.error,
        "modal": modal(s),
        "busy": s.busy,
        "notice": s.notice.as_ref().map(|n| json!({ "text": n.text, "error": n.error })),
        "operation": s.operation.map(|o| o.command()),
        "ahead_behind": s.ahead_behind,
        "counts": { "staged": s.staged, "unstaged": s.unstaged, "changed": s.total, "low_signal_hidden": s.hidden },
        "layout": if s.diff.mode() == DiffMode::Split { "split" } else { "unified" },
        "files": list,
        "files_total": files.len(),
        "file_in_view": files.get(s.diff.visible_file()).map(FileDiff::path),
        "selection": selection,
        "comments": s.comments.len(),
        "terminal_open": s.terminals.open,
    })
}

fn comments(s: &Collate) -> Value {
    s.comments.iter().enumerate().map(|(i, c)| json!({
        "index": i, "path": c.at.path, "side": side_name(c.at.side), "line": c.at.line, "text": c.at.text, "body": c.body,
        "in_view": s.comments_shown.get(i).copied().unwrap_or(false),
    })).collect()
}

/// One file as a unified patch, as `git diff` writes it.
fn write_patch(out: &mut String, f: &FileDiff) {
    let a = f.old_path.as_deref().map_or_else(|| "/dev/null".to_owned(), |p| format!("a/{p}"));
    let b = f.new_path.as_deref().map_or_else(|| "/dev/null".to_owned(), |p| format!("b/{p}"));
    out.push_str(&format!("diff --git a/{} b/{}\n", f.old_path.as_deref().unwrap_or(f.path()), f.path()));
    if f.binary {
        out.push_str(&format!("Binary files {a} and {b} differ\n"));
        return;
    }
    out.push_str(&format!("--- {a}\n+++ {b}\n"));
    for h in f.hunks.iter() {
        out.push_str(&h.header());
        out.push('\n');
        for l in &h.lines {
            out.push(match l.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&l.text);
            out.push('\n');
            if l.no_newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
}

// The MCP server: `collate --mcp`.

/// A running window, as its socket answered.
struct Window {
    pid: u64,
    socket: PathBuf,
    repo: Option<String>,
}

/// The windows up now. A socket nothing answers on is left from one that quit without
/// taking it away, and goes.
fn windows() -> Vec<Window> {
    let Some(dir) = socket_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "sock") {
            continue;
        }
        match ask(&path, "hello", json!({})) {
            Ok(v) => out.push(Window { pid: v["pid"].as_u64().unwrap_or(0), socket: path, repo: v["repo"].as_str().map(str::to_owned) }),
            Err(_) if UnixStream::connect(&path).is_err() => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    out.sort_by_key(|w| w.pid);
    out
}

fn ask(socket: &Path, method: &str, params: Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("Collate isn't answering: {e}"))?;
    stream.set_read_timeout(Some(ANSWER_WITHIN + Duration::from_secs(1))).map_err(|e| e.to_string())?;
    writeln!(stream, "{}", json!({ "method": method, "params": params })).map_err(|e| format!("Couldn't ask Collate: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| format!("Collate didn't answer: {e}"))?;
    let reply: Value = serde_json::from_str(&line).map_err(|e| format!("Collate's answer isn't JSON: {e}"))?;
    match reply.get("error") {
        Some(e) => Err(e.as_str().unwrap_or("Collate refused").to_owned()),
        None => Ok(reply["ok"].clone()),
    }
}

/// The window a tool call means: the one `window` names by process id, else the one on
/// `repo`, else the one on the folder the agent works in, else the only one.
fn pick(args: &Value) -> Result<Window, String> {
    let all = windows();
    if all.is_empty() {
        return Err("Collate isn't running: collate_launch opens it on a repository".into());
    }
    let inside = |w: &Window, dir: &Path| w.repo.as_deref().is_some_and(|r| std::fs::canonicalize(dir).is_ok_and(|d| d.starts_with(r)));
    let listed = |all: &[Window]| all.iter().map(|w| format!("{} ({})", w.pid, w.repo.as_deref().unwrap_or("no project"))).collect::<Vec<_>>().join(", ");
    let found = if let Some(pid) = args["window"].as_u64() {
        all.into_iter().find(|w| w.pid == pid).ok_or_else(|| format!("No Collate window has process id {pid}"))?
    } else if let Some(repo) = args["repo"].as_str() {
        let names = listed(&all);
        all.into_iter().find(|w| inside(w, Path::new(repo))).ok_or_else(|| format!("No Collate window is on {repo}. Open: {names}"))?
    } else if let Some(i) = std::env::current_dir().ok().and_then(|cwd| all.iter().position(|w| inside(w, &cwd))) {
        all.into_iter().nth(i).unwrap()
    } else if all.len() == 1 {
        all.into_iter().next().unwrap()
    } else {
        return Err(format!("Several Collate windows are open; name one with `window` or `repo`: {}", listed(&all)));
    };
    Ok(found)
}

/// Opens Collate on `dir` in a window of its own and waits for it to answer.
fn launch(dir: &str) -> Result<Value, String> {
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("{dir}: {e}"))?;
    let root = crate::git::Repo::open(&dir)?.root;
    if let Some(w) = windows().into_iter().find(|w| w.repo.as_deref() == Some(&root.display().to_string())) {
        return Ok(json!({ "window": w.pid, "repo": w.repo, "already_open": true }));
    }
    let status = std::process::Command::new("open").args(["-n", "-a", "Collate", "--args"]).arg(&root).status().map_err(|e| format!("Couldn't run open: {e}"))?;
    if !status.success() {
        return Err("open couldn't start Collate: is Collate.app installed?".into());
    }
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(w) = windows().into_iter().find(|w| w.repo.as_deref() == Some(&root.display().to_string())) {
            return Ok(json!({ "window": w.pid, "repo": w.repo }));
        }
    }
    Err(format!("Collate started but no window on {} answered within 10 seconds", root.display()))
}

/// Targets a call at one window; every tool but `collate_windows` and `collate_launch` takes them.
const TARGET: &str = r#""window": {"type": "integer", "description": "The process id of the Collate window to act on, from collate_windows. Optional."},
    "repo": {"type": "string", "description": "A folder in the repository whose window to act on. Optional: by default the window on the agent's working folder, or the only one."}"#;

/// The tools: name, the socket method, what it does, and its own arguments as JSON Schema
/// properties, then which of them are required.
const TOOLS: &[(&str, &str, &str, &str, &str)] = &[
    ("collate_windows", "", "Lists the Collate windows running: each one's process id and repository.", "", ""),
    ("collate_launch", "", "Opens Collate on a repository in a window of its own, or finds the one already on it.",
        r#""path": {"type": "string", "description": "A folder in the repository."}"#, r#""path""#),
    ("collate_state", "state", "What the reader sees: the repository, branch and view (all, staged, unstaged, branch, history), where they are (diff, commit list, map, pull requests, issues), the files of the diff with their status and counts, the file in view, the selected lines and their text, a dialog in front, the notice shown, and staged and unstaged counts.", "", ""),
    ("collate_commands", "commands", "Every command of Collate's menu bar: its id, title, menu, key, and whether it can run now and is checked. Run one with collate_run.", "", ""),
    ("collate_run", "run", "Runs a menu bar command by its id, as the reader choosing it would; refused when it can't run now. Commands like stage, unstage, discard, comment, copy-lines and add-to-chat act on the selected lines (collate_select), else on the change in view. Discards and other risky commands ask the reader in a dialog first. Returns the state after.",
        r#""command": {"type": "string", "description": "The command's id, from collate_commands: next-file, stage, stage-file, commit, split, view-history, …"}"#, r#""command""#),
    ("collate_show", "show", "Switches the view: all (every uncommitted change), staged, unstaged, branch (the branch against its base, as a pull request shows it), history (the commit graph), pulls (the pull request list), issues (the issue list, or one issue by number) or map (the repository map).",
        r#""view": {"type": "string", "enum": ["all", "staged", "unstaged", "branch", "history", "pulls", "issues", "map"]},
    "number": {"type": "integer", "description": "With issues: the issue to open. Optional."}"#, r#""view""#),
    ("collate_open_file", "open_file", "Scrolls the diff to a file, and to a line of it when given, selecting that line. The file must be in the diff shown.",
        r#""path": {"type": "string", "description": "The file's path in the repository."},
    "line": {"type": "integer", "description": "A line number, on the new side unless side is old. Optional."},
    "side": {"type": "string", "enum": ["new", "old"], "description": "Which side's numbers line counts in. Default new (old for a deleted file)."}"#, r#""path""#),
    ("collate_select", "select", "Selects lines of a file in the diff, as clicking and shift-clicking their numbers does, and scrolls to them, opening unchanged lines folded away.",
        r#""path": {"type": "string"},
    "start": {"type": "integer", "description": "The first line."},
    "end": {"type": "integer", "description": "The last line. Default start."},
    "side": {"type": "string", "enum": ["new", "old"], "description": "Default new."}"#, r#""path", "start""#),
    ("collate_clear_selection", "clear_selection", "Clears the selected lines.", "", ""),
    ("collate_diff", "diff", "The diff the window shows, as a unified patch: one file's, or every file's (cut at 200 KB).",
        r#""path": {"type": "string", "description": "One file's path. Optional."}"#, ""),
    ("collate_comments", "comments", "The reader's comments on lines: each one's index, file, side, line, the line's text and the comment.", "", ""),
    ("collate_comment", "comment", "Leaves a comment under a line of the diff, as the reader's comments are (kept on this Mac, sent to the agent or copied as Markdown).",
        r#""path": {"type": "string"},
    "line": {"type": "integer"},
    "side": {"type": "string", "enum": ["new", "old"], "description": "Default new."},
    "body": {"type": "string", "description": "The comment, in Markdown."}"#, r#""path", "line", "body""#),
    ("collate_delete_comment", "delete_comment", "Deletes a comment by its index from collate_comments.",
        r#""index": {"type": "integer"}"#, r#""index""#),
    ("collate_open_commit", "open_commit", "Shows a commit's diff in History, or a range's (from..to).",
        r#""rev": {"type": "string", "description": "A commit, branch or tag (HEAD~2, v1.2), or a range a..b."}"#, r#""rev""#),
    ("collate_open_project", "open_project", "Opens another repository in this window.",
        r#""path": {"type": "string", "description": "A folder in the repository."}"#, r#""path""#),
    ("collate_say", "say", "Shows the reader a short notice at the foot of the window, to say what the agent did or wants them to look at.",
        r#""text": {"type": "string"},
    "error": {"type": "boolean", "description": "Show it as an error. Default false."}"#, r#""text""#),
    ("collate_dismiss", "dismiss", "Closes what's in front of the review, as Escape does: a dialog (answering no), a palette, the shortcuts sheet, settings. Leaves the merge editor.", "", ""),
];

fn tool_list() -> Value {
    let tools: Vec<Value> = TOOLS.iter().map(|(name, method, about, props, required)| {
        let props = match (method.is_empty(), props.is_empty()) {
            (true, _) => props.to_string(),
            (false, true) => TARGET.to_owned(),
            (false, false) => format!("{props},\n    {TARGET}"),
        };
        let schema = format!(r#"{{"type": "object", "properties": {{{props}}}, "required": [{required}]}}"#);
        json!({ "name": name, "description": about, "inputSchema": serde_json::from_str::<Value>(&schema).unwrap_or_else(|e| panic!("{name}'s schema: {e}")) })
    }).collect();
    json!({ "tools": tools })
}

fn call(name: &str, args: &Value) -> Result<Value, String> {
    let (_, method, ..) = TOOLS.iter().find(|t| t.0 == name).ok_or_else(|| format!("No tool {name}"))?;
    match name {
        "collate_windows" => Ok(windows().iter().map(|w| json!({ "window": w.pid, "repo": w.repo })).collect()),
        "collate_launch" => launch(args["path"].as_str().ok_or("path is missing")?),
        _ => {
            let w = pick(args)?;
            let mut params = args.clone();
            if let Some(m) = params.as_object_mut() {
                m.remove("window");
                m.remove("repo");
            }
            ask(&w.socket, method, params)
        }
    }
}

const INSTRUCTIONS: &str = "Collate is the reader's git diff viewer. These tools read and drive the window they look at: \
call collate_state first, move with collate_show, collate_open_file and collate_select, and run any menu command by id with collate_run (collate_commands lists them). \
The reader is watching: say what you did with collate_say, and ask them before you commit, push or discard.";

/// One JSON-RPC message, and its answer unless it's a notification.
fn mcp_answer(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "collate", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let p = &msg["params"];
            let name = p["name"].as_str().unwrap_or("");
            // A tool's failure is the agent's to read and act on, not a protocol error.
            Ok(match call(name, p.get("arguments").unwrap_or(&json!({}))) {
                Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            })
        }
        m => Err(json!({ "code": -32601, "message": format!("No method {m}") })),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
}

/// `collate --mcp`: serves the Model Context Protocol on standard input and output until
/// the client closes it.
pub fn mcp() -> ! {
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => mcp_answer(&msg),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Not JSON: {e}") } })),
        };
        if let Some(r) = reply {
            let mut out = stdout.lock();
            if writeln!(out, "{r}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mcp_server_lists_its_tools_and_answers_without_a_window() {
        let init = mcp_answer(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } })).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(mcp_answer(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none(), "a notification has no answer");
        let list = mcp_answer(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        let run = tools.iter().find(|t| t["name"] == "collate_run").unwrap();
        assert_eq!(run["inputSchema"]["required"], json!(["command"]));
        assert!(run["inputSchema"]["properties"]["window"].is_object(), "a window's tools can be aimed at one");
        let unknown = mcp_answer(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "nope", "arguments": {} } })).unwrap();
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(mcp_answer(&json!({ "jsonrpc": "2.0", "id": 4, "method": "frob" })).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn every_tool_names_a_method_the_window_takes() {
        let mut rt = rune::Runtime::new(crate::init(), crate::view, (800.0, 600.0), rune::Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        for (name, method, ..) in TOOLS.iter().filter(|t| !t.1.is_empty()) {
            let method = method.to_string();
            let name = name.to_string();
            rt.update(move |s, tx| {
                if let Err(e) = handle(s, &method, &json!({}), tx) {
                    assert!(!e.starts_with("No method"), "{name}: {e}");
                }
            });
        }
    }
}

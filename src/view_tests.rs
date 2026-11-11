//! The app's tests: views drawn headless, keys pressed and repositories changed, each in a
//! throwaway repository in the temp folder.

use super::*;
use crate::comments::{comments_text, parse_comments};
use crate::diff::{BlameNote, mark_moved};
use crate::git::{Blob, Snapshot};
use crate::history_view::ref_labels;
use crate::settings::Motion;
use crate::watcher::{DiffCache, file_diff, prepare, relevant};
use rune::{Env, Runtime};

#[test]
fn history_graph_renders_and_selects_commits() {
    for width in [760.0, 1280.0] {
        let mut s = init();
        s.has_project = true;
        s.scope = Scope::History;
        s.loaded = true;
        s.graph_commits = (0..600).map(|i| graph::Commit {
            id: format!("{i:040x}"), parents: if i < 599 { vec![format!("{:040x}", i + 1)] } else { vec![] },
            author: "Ada".into(), email: "ada@example.com".into(), date: "09 Nov 2026 10:00".into(),
            refs: vec![], subject: format!("Commit {i}"),
        }).collect();
        (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
        s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
        s.commit = Some(s.commit_ids[0].clone());
        let second = s.commit_ids[1].clone();
        let mut rt = Runtime::new(s, view, (width, 840.0), Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        assert!(rt.presented("history-graph").is_some());
        assert!(rt.presented(&format!("graph-{}", rt.state.commit_ids[599])).is_none(), "far rows are virtualized");
        let (rect, _) = rt.presented(&format!("graph-{second}")).unwrap();
        rt.pointer_down(rect[0] + 40.0, rect[1] + 15.0);
        rt.pointer_up(rect[0] + 40.0, rect[1] + 15.0);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.commit.as_deref(), Some(second.as_str()));
        assert!(!rt.state.loaded, "selection requests a new diff");
        assert!(rt.presented("history-graph").is_some(), "graph stays visible while diff loads");
    }
}

/// Counts heap bytes in use and their peak, for the colouring A/B.
struct Counting;
static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), std::sync::atomic::Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, std::sync::atomic::Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), std::sync::atomic::Ordering::Relaxed);
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;

/// A long TypeScript file of varied code, `n` lines; `edit` changes every 40th line.
fn typescript(n: usize, edit: bool) -> String {
    let shapes = [
        "export async function load{i}(id: string, opts?: Options): Promise<Result<User, Error>> {",
        "  const url = `${BASE}/users/${id}?include=${opts?.include ?? \"profile\"}&v={i}`;",
        "  // Fetch with a timeout so a stuck request never holds the queue ({i}).",
        "  const res = await fetch(url, { headers: { Authorization: `Bearer ${token}` }, signal });",
        "  if (!res.ok) return { ok: false, error: new HttpError(res.status, \"load {i} failed\") };",
        "  const body = (await res.json()) as { items: Array<{ id: number; name: string }> };",
        "  for (const [k, v] of Object.entries(body.items)) { cache.set(`${k}:{i}`, v ?? null); }",
        "  return { ok: true, value: body.items.map((x) => ({ ...x, seen: Date.now() + {i} })) };",
        "}",
        "/* {i}: block comment that the scanner has to track across */ let flag{i} = 0x{i}f && /re{i}+/g.test(s);",
    ];
    (0..n).map(|i| {
        let line = shapes[i % shapes.len()].replace("{i}", &i.to_string());
        if edit && i % 40 == 7 { format!("{line} // changed\n") } else { format!("{line}\n") }
    }).collect()
}

/// Scrolling a 60,000-line TypeScript diff, reading pace then flicking, at 60 Hz: frame
/// times and heap, with code coloured (`on`) or plain (`off`). Run each arm alone:
/// `COLLATE_COLOUR=on|off cargo test --release colour_ab -- --ignored --nocapture`.
/// Colouring off a background thread measured no faster (and drew rows twice), so it's
/// done inline as rows first draw.
#[test]
#[ignore]
fn colour_ab() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let arm = std::env::var("COLLATE_COLOUR").unwrap_or_else(|_| "on".into());
    let split = std::env::var("COLLATE_AB_LAYOUT").is_ok_and(|v| v == "split");
    let (old, new) = (typescript(60_000, false), typescript(60_000, true));
    let base = LIVE.load(Ordering::Relaxed);
    let mut state = init();
    state.settings = Settings::default();
    state.settings.layout = if split { DiffMode::Split } else { DiffMode::Unified };
    state.apply_settings(None);
    if arm == "off" {
        state.diff.set_highlighter(Highlighter::new(|_: Option<&str>, _: &str| Vec::new()).key("off"), |_| false);
    }
    state.diff.set_files(vec![FileDiff::between_with_context(Some("app.ts".into()), Some("app.ts".into()), &old, &new, 25)]);
    drop((old, new));
    state.has_project = true;
    state.loaded = true;
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let start = Instant::now();
    let mut rt = Runtime::new(state, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
    rt.frame(0.0);
    let first = start.elapsed();
    let (x, y) = (700.0, 500.0);
    let mut times = Vec::new();
    // Reading pace (150 pt a frame), then flicking (2,500 pt a frame). `COLLATE_AB_PROFILE=1`:
    // flicking down and back up again without pausing, long enough to profile.
    let profile = std::env::var_os("COLLATE_AB_PROFILE").is_some();
    let phases: Vec<(usize, f32)> = if profile { (0..8).map(|n| (150, if n % 2 == 0 { -2500.0 } else { 2500.0 })).collect() } else { vec![(300, -150.0), (150, -2500.0)] };
    for (frames, dy) in phases {
        let mut phase = Vec::new();
        for _ in 0..frames {
            let t = Instant::now();
            rt.wheel(x, y, dy);
            rt.frame(1.0 / 60.0);
            let spent = t.elapsed();
            phase.push(spent.as_secs_f64() * 1000.0);
            if !profile {
                std::thread::sleep(Duration::from_micros(16_667).saturating_sub(spent));
            }
        }
        times.push(phase);
    }
    let stats = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        let over = v.iter().filter(|&&t| t > 16.7).count();
        format!("p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms, {over} over 16.7", q(0.5), q(0.95), q(0.99), q(1.0))
    };
    let mb = |b: usize| b as f64 / 1048576.0;
    // What colouring the whole file costs on one thread, for scale.
    let t = Instant::now();
    let text = typescript(60_000, true);
    let blocks: usize = text.split_inclusive('\n').collect::<Vec<_>>().chunks(128).map(|c| rune_kit::syntax::builtin(Some("ts"), &c.concat()).len()).sum();
    println!("scanner: all 60,000 lines in {:.1} ms ({blocks} tokens)", t.elapsed().as_secs_f64() * 1000.0);
    println!("arm {arm} ({}): first frame {:.1} ms", if split { "split" } else { "unified" }, first.as_secs_f64() * 1000.0);
    println!("  reading: {}", stats(&times[0]));
    println!("  flicking: {}", stats(&times[1]));
    println!("  heap: peak {:.1} MB, held at end {:.1} MB (above the {:.1} MB before the diff)", mb(PEAK.load(Ordering::Relaxed).saturating_sub(base)), mb(LIVE.load(Ordering::Relaxed).saturating_sub(base)), mb(base));
}

#[test]
fn scrolling_near_the_end_asks_for_more_history() {
    let mut s = init();
    s.has_project = true;
    s.scope = Scope::History;
    s.loaded = true;
    s.graph_commits = (0..HISTORY_PAGE).map(|i| graph::Commit {
        id: format!("{i:040x}"), parents: if i + 1 < HISTORY_PAGE { vec![format!("{:040x}", i + 1)] } else { vec![] },
        author: "Ada".into(), email: "a@x".into(), date: String::new(), refs: vec![], subject: format!("Commit {i}"),
    }).collect();
    (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
    s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
    let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
    rt.frame(0.0);
    assert!(rt.presented("graph-refresh").is_some(), "refresh is an icon button");
    assert_eq!(rt.state.graph_limit, HISTORY_PAGE, "the top of the list asks for nothing");
    let (r, _) = rt.presented("graph-scroll").unwrap();
    for _ in 0..200 {
        rt.wheel(r[0] + 200.0, r[1] + 200.0, -3000.0);
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.graph_limit, 2 * HISTORY_PAGE, "the end of the list asks for the next page, once");
}

#[test]
fn a_commit_opens_from_its_files_and_esc_goes_back() {
    let mut s = init();
    s.has_project = true;
    s.scope = Scope::History;
    s.loaded = true;
    s.graph_commits = (0..3).map(|i| graph::Commit {
        id: format!("{i:040x}"), parents: if i < 2 { vec![format!("{:040x}", i + 1)] } else { vec![] },
        author: "Ada Lovelace".into(), email: "ada@example.com".into(), date: "09 Nov 2026 10:00".into(),
        refs: vec![], subject: format!("Commit {i}"),
    }).collect();
    (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
    s.commit_ids = s.graph_commits.iter().map(|c| c.id.clone()).collect();
    s.commit = Some(s.commit_ids[1].clone());
    s.commit_body = "Why it changed.".into();
    let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
    rt.on_key = Some(on_key);
    rt.frame(0.0);
    assert!(rt.presented("history-graph").is_some(), "the list fills the window first");
    assert!(rt.presented("commit-card").is_some(), "the side panel shows the chosen commit");
    assert!(rt.presented("commit-header").is_none());
    // A file in the side panel's list opens the commit's diff.
    rt.update(|s, tx| reveal(s, "src/a.rs", tx));
    for _ in 0..30 { rt.frame(1.0 / 60.0); }
    assert!(rt.state.history_diff);
    assert!(rt.presented("commit-header").is_some(), "the commit heads its diff");
    assert!(rt.presented("history-graph").is_none(), "the list makes way");
    rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
    for _ in 0..30 { rt.frame(1.0 / 60.0); }
    assert!(!rt.state.history_diff, "Esc goes back to the list");
    assert!(rt.presented("history-graph").is_some());
    // Enter opens the chosen commit too.
    rt.key(KeyEvent { key: Key::Enter, mods: Default::default() });
    for _ in 0..30 { rt.frame(1.0 / 60.0); }
    assert!(rt.state.history_diff);
    let (x, y) = rt.find("history-back").expect("a way back");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    for _ in 0..30 { rt.frame(1.0 / 60.0); }
    assert!(!rt.state.history_diff, "All Commits goes back");
}

#[test]
fn the_uncommitted_row_opens_all_changes() {
    let mut s = init();
    s.has_project = true;
    s.scope = Scope::History;
    s.loaded = true;
    s.total = 2;
    let head = format!("{:040x}", 1);
    let commits = vec![graph::Commit { id: head.clone(), parents: vec![], author: "Ada".into(), email: "a@x".into(), date: String::new(), refs: vec![], subject: "Only".into() }];
    s.commit_ids = vec![head.clone()];
    s.graph_commits = graph::with_uncommitted(commits, Some(&head), true);
    (s.graph_rows, s.graph_lanes) = graph::layout(&s.graph_commits);
    let mut rt = Runtime::new(s, view, (1280.0, 840.0), Env { system_dark: true, scale: 2.0 });
    rt.frame(0.0);
    let (rect, _) = rt.presented(&format!("graph-{}", graph::UNCOMMITTED)).expect("uncommitted row on top");
    rt.pointer_down(rect[0] + 60.0, rect[1] + 14.0);
    rt.pointer_up(rect[0] + 60.0, rect[1] + 14.0);
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.scope, Scope::Uncommitted);
}

#[test]
fn a_branch_and_its_remote_share_a_label() {
    let r = |name: &str, kind, head| graph::Ref { name: name.into(), kind, head };
    let labels = ref_labels(&[
        r("main", graph::RefKind::Branch, true),
        r("origin/main", graph::RefKind::Remote, false),
        r("upstream/main", graph::RefKind::Remote, false),
        r("origin/dev", graph::RefKind::Remote, false),
        r("v1", graph::RefKind::Tag, false),
    ]);
    let text: Vec<&str> = labels.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(text, ["main - origin - upstream", "origin/dev", "v1"]);
    assert!(labels[0].2, "HEAD's branch is marked");
}

/// The repository the view tests read: `COLLATE_TEST_REPO` when set (any repository with many
/// changed files under `src/`), else one made here, once: four folders of twenty files on
/// `main`, each file changed in two places since its one commit, and a `develop` branch.
fn test_repo() -> &'static std::path::Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::var_os("COLLATE_TEST_REPO") {
            return PathBuf::from(dir);
        }
        let dir = std::env::temp_dir().join(format!("collate-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folders = ["api", "core", "ui", "util"];
        let write = |edited: bool| {
            for (at, folder) in folders.iter().enumerate() {
                std::fs::create_dir_all(dir.join("src").join(folder)).unwrap();
                for i in 1..=20 {
                    let mut text = format!("use crate::{}::file{i};\n\n", folders[(at + 1) % folders.len()]);
                    for n in 0..60 {
                        text += &match n {
                            10 => format!("    let value_{n} = {}({n});\n", if edited { "changed" } else { "original" }),
                            40 => format!("    let value_{n} = {}({n});\n", if edited { "also changed" } else { "untouched" }),
                            n if n % 12 == 0 => format!("fn function_{folder}_{i}_{n}() {{\n"),
                            n if n % 12 == 11 => "}\n".to_owned(),
                            n => format!("    let value_{n} = compute({n}, {i});\n"),
                        };
                    }
                    std::fs::write(dir.join(format!("src/{folder}/file{i}.rs")), text).unwrap();
                }
            }
        };
        write(false);
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"], &["add", "-A"], &["commit", "-qm", "init"], &["branch", "develop"]] {
            let out = std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}");
        }
        write(true);
        dir
    })
}

fn snapshot() -> Snapshot {
    let repo = Repo::open(test_repo()).unwrap();
    let raw = repo.status_raw().unwrap();
    repo.snapshot(Scope::Uncommitted, &raw, None).unwrap()
}

fn loaded() -> Runtime<Collate> {
    let env = Env { system_dark: true, scale: 2.0 };
    let mut state = init();
    state.settings = Settings::default();
    state.apply_settings(None);
    let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
    rt.frame(0.0);
    let snap = snapshot();
    rt.update(move |s, tx| s.apply_snapshot(snap, tx));
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    rt
}

/// Whether a row of the staging panel's Unstaged list is in its visible part, not just
/// mounted in the virtual list's overscan.
fn on_screen(rt: &Runtime<Collate>, key: &str) -> bool {
    let Some((list, _)) = rt.presented("unstaged") else { return false };
    rt.presented(key).is_some_and(|(r, _)| r[1] >= list[1] + 30.0 && r[1] + r[3] <= list[1] + list[3])
}

/// The middle of the Unstaged list, where a reader scrolls it.
fn list_point(rt: &Runtime<Collate>) -> (f32, f32) {
    let (r, _) = rt.presented("unstaged").expect("the staging panel's list");
    (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
}

fn click(rt: &mut Runtime<Collate>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
}

/// The file's header sits right under the summary, at the top of the diff.
fn header_at_top(rt: &Runtime<Collate>, i: usize) {
    let summary = rt.presented("summary").expect("summary").0;
    let top = summary[1] + summary[3];
    let y = rt.presented(&format!("df{i}")).map(|p| p.0[1]).unwrap_or_else(|| panic!("file {i}'s header is on screen"));
    assert!((y - top).abs() < 2.0, "file {i}'s header is at y={y}, not at the diff's top {top}");
}

#[test]
fn clicking_files_in_the_sidebar_brings_each_to_the_top() {
    let mut rt = loaded();
    // Down the list, then back up past where we started.
    for target in ["src/core/file5.rs", "src/ui/file2.rs", "src/api/file3.rs", "src/util/file15.rs"] {
        // Scroll the sidebar to it, as a reader would: down for a row below, up for one above.
        let down = rt.presented(target).is_none_or(|(r, _)| r[1] > 400.0);
        for _ in 0..80 {
            if on_screen(&rt, target) {
                break;
            }
            let (x, y) = list_point(&rt);
            rt.wheel(x, y, if down { -60.0 } else { 60.0 });
            rt.frame(1.0 / 60.0);
        }
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert!(on_screen(&rt, target), "{target} scrolled into the sidebar");
        click(&mut rt, target);
        let i = rt.state.diff.file_index(target).unwrap();
        assert_eq!(rt.state.tree.cursor(), Some(target));
        header_at_top(&rt, i);
    }
    // Scrolling up from the last file hands the selection back to the file above.
    let last = rt.state.diff.files().len() - 1;
    for _ in 0..20 {
        rt.wheel(700.0, 500.0, 120.0);
        rt.frame(1.0 / 60.0);
    }
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.state.diff.visible_file() < last, "the selection follows the scroll again");
    let above = rt.state.diff.files()[rt.state.diff.visible_file()].path().to_owned();
    assert_eq!(rt.state.tree.cursor(), Some(above.as_str()));
}

#[test]
fn the_summary_says_which_file_is_in_view_and_how_many_are_viewed() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    let shown = |rt: &Runtime<Collate>| rt.elements().into_iter().find(|e| e.key.as_deref() == Some("viewed-count")).and_then(|e| e.text).expect("the progress text");
    let n = rt.state.diff.files().len();
    assert_eq!(shown(&rt), format!("File 1 of {n} - 0 viewed"));
    for key in ["]", "v"] {
        rt.key(KeyEvent { key: Key::Text(key.into()), mods: Default::default() });
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
    }
    assert_eq!(shown(&rt), format!("File 2 of {n} - 1 viewed"), "] moved to the second file and v marked it");
    assert_eq!(progress(0, 1, 1), "1/1 viewed", "one file has nowhere to be in");
    assert_eq!(progress(40, 3, 0), "File 3 of 3 - 0 viewed", "never past the last file");
}

#[test]
fn shortcuts_work_after_clicking_a_file_in_the_sidebar() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    click(&mut rt, "src/api/file3.rs");
    assert_eq!(rt.state.tree.cursor(), Some("src/api/file3.rs"));
    let at = rt.state.diff.visible_file();
    // Routed as the window routes it: focused element first, then the app.
    rt.key(KeyEvent { key: Key::Text("]".into()), mods: Default::default() });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.diff.visible_file(), at + 1, "] reached the diff, not the file list");
    rt.key(KeyEvent { key: Key::Text("v".into()), mods: Default::default() });
    rt.frame(1.0 / 60.0);
    assert!(rt.state.is_viewed(at + 1), "v marked the file in view");
}

#[test]
fn command_f_then_typing_searches() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    click(&mut rt, "src/api/file2.rs");
    let key = |rt: &mut Runtime<Collate>, key: Key, cmd: bool| {
        rt.key(KeyEvent { key, mods: rune::Mods { cmd, ..Default::default() } });
        rt.frame(1.0 / 60.0);
    };
    key(&mut rt, Key::Command('f'), true);
    assert!(rt.state.search.focused, "⌘F puts the keys in the toolbar's search");
    let (field, _) = rt.presented("search").expect("the search is drawn");
    let (modes, _) = rt.presented("toolbar-leading").map(|_| rt.presented("panel-toggle").unwrap()).unwrap();
    assert!(field[0] < modes[0] && field[1] < 52.0, "in the toolbar, left of its tools");
    for c in "also chang".chars() {
        key(&mut rt, Key::Text(c.to_string()), false);
    }
    key(&mut rt, Key::Backspace, false);
    key(&mut rt, Key::Text("g".into()), false);
    assert_eq!(rt.state.search.text, "also chang");
    assert_eq!(rt.state.hits.len(), rt.state.diff.files().len(), "every file matches");
    assert_eq!(rt.state.hit, Some(0), "typing lands on the first match");
    key(&mut rt, Key::Enter, false);
    assert_eq!(rt.state.hit, Some(1), "Return steps on");
    key(&mut rt, Key::Escape, false);
    assert!(!rt.state.search.focused, "Escape gives the keys back");
    key(&mut rt, Key::Command('k'), true);
    assert!(rt.state.search.focused, "⌘K opens the search too");
    key(&mut rt, Key::Escape, false);
    key(&mut rt, Key::Text("v".into()), false);
    assert_eq!(rt.state.search.text, "also chang", "and shortcuts are shortcuts again");
}

#[test]
fn the_search_finds_files_in_the_map_and_commits_in_history() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    let cmd_f = KeyEvent { key: Key::Command('f'), mods: rune::Mods { cmd: true, ..Default::default() } };
    let typed = |rt: &mut Runtime<Collate>, q: &str| {
        for c in q.chars() {
            rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Default::default() });
            rt.frame(1.0 / 60.0);
        }
    };
    // Another test owns the shared repository, so this one maps the test repository itself.
    let built = map::build(test_repo()).expect("the test repository maps");
    rt.update(move |s, _| {
        s.map.open = true;
        s.map.data = Some(std::sync::Arc::new(built));
    });
    rt.frame(1.0 / 60.0);
    rt.key(cmd_f.clone());
    rt.frame(1.0 / 60.0);
    typed(&mut rt, "file2");
    assert!(rt.state.map_query.focused && rt.state.search.text.is_empty(), "the Map has its own query");
    let pick = rt.state.found.iter().position(|f| f.target == "src/api/file2.rs").expect("the file is found");
    assert!(rt.find("found").is_some(), "the results show under the field");
    click(&mut rt, &format!("found-{pick}"));
    assert_eq!(rt.state.map.zoom, "src/api", "a file opens its folder");
    assert!(rt.state.map.hover.is_some(), "picked there");
    assert!(rt.find("found").is_none(), "and the results close");

    let (commits, head) = Repo::open(test_repo()).unwrap().graph(50, None);
    rt.update(move |s, _| {
        s.map.open = false;
        s.set_scope(Scope::History);
        s.show_history(commits, head);
    });
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.searched(), Searched::History);
    rt.key(cmd_f);
    rt.frame(1.0 / 60.0);
    typed(&mut rt, "develop");
    assert_eq!(rt.state.found.first().map(|f| (f.kind, f.title.as_str())), Some((search::Kind::Branch, "develop")), "a branch by name");
    let id = rt.state.found[0].target.clone();
    rt.key(KeyEvent { key: Key::Enter, mods: Default::default() });
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.commit.as_deref(), Some(id.as_str()), "Return picks its commit");
    assert!(!rt.state.history_query.focused && !rt.state.history_diff, "the graph stays, the keys go back");
}

#[test]
fn m_jumps_between_the_ends_of_a_move() {
    let block = "let total = compute_total();\nrecord_metric(total);\nflush_metrics_now();\n";
    let pad: String = (0..60).map(|i| format!("filler line {i}\n")).collect();
    let mut files = vec![
        FileDiff::between_with_context(Some("c.rs".into()), Some("c.rs".into()), &format!("head\n{block}{pad}"), &format!("head\n{pad}"), 3),
        FileDiff::between_with_context(Some("d.rs".into()), Some("d.rs".into()), &format!("{pad}start\n"), &format!("{pad}start\n{block}"), 3),
    ];
    mark_moved(&mut files);
    let env = Env { system_dark: true, scale: 2.0 };
    let mut state = init();
    state.settings = Settings::default();
    state.apply_settings(None);
    state.diff.set_files(files);
    state.loaded = true;
    let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
    rt.on_key = Some(on_key);
    for _ in 0..5 {
        rt.frame(1.0 / 60.0);
    }
    rt.key(KeyEvent { key: Key::Text("m".into()), mods: Default::default() });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.diff.visible_file(), 1, "m went to where the block landed");
    rt.key(KeyEvent { key: Key::Text("m".into()), mods: Default::default() });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.diff.visible_file(), 0, "and back to where it came from");
}

#[test]
fn the_sidebar_switches_views_and_the_file_menu_acts() {
    let mut rt = loaded();
    // Views are sidebar places now, not a toolbar segmented control.
    click(&mut rt, "view-staged");
    assert_eq!(rt.state.scope, Scope::Staged);
    click(&mut rt, "view-all");
    assert_eq!(rt.state.scope, Scope::Uncommitted);
    rt.update(|s, tx| {
        s.hashes.clear();
        s.apply_snapshot(snapshot(), tx);
    });
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    // A file header's "⋯" opens a native menu with the rest of its commands.
    click(&mut rt, "file-more-0");
    let menu = rt.popup_menu().expect("the file menu opened");
    for id in ["stage", "open", "copy", "viewed", "discard"] {
        assert!(menu.find(id).is_some(), "the file menu has {id}");
    }
    assert_eq!(menu.find("discard").map(|i| i.title().to_owned()), Some("Discard Changes…".into()), "discard asks first, and says so");
    rt.resolve_popup(Some("viewed")).unwrap();
    rt.frame(1.0 / 60.0);
    assert!(rt.state.is_viewed(0), "picking Viewed marks the file");
}

#[test]
fn settings_float_over_the_review_and_close_with_escape() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    rt.key(KeyEvent { key: Key::Command(','), mods: rune::Mods { cmd: true, ..Default::default() } });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.screen, Screen::Settings);
    assert!(rt.find("summary").is_some(), "the review stays underneath");
    click(&mut rt, "tab-diff");
    assert_eq!(rt.state.settings_tab, 1);
    assert!(rt.find("words").is_some(), "the Diff section's settings show");
    rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.screen, Screen::Review, "Escape puts the panel away");
}

#[test]
fn reduced_motion_runs_the_clock_faster_and_default_puts_it_back() {
    let mut rt = loaded();
    assert_eq!(rt.time_scale, 1.0);
    rt.update(|s, _| s.settings.motion = Motion::Reduced);
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.time_scale, Motion::Reduced.speed());
    assert!(rt.time_scale > 1.0, "reduced is shorter, not off");
    rt.update(|s, _| s.settings.motion = Motion::Default);
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.time_scale, 1.0);
}

#[test]
fn staged_and_unstaged_fold_to_their_titles() {
    let mut rt = loaded();
    let tall = |rt: &Runtime<Collate>, key: &str| rt.presented(key).map(|p| p.0[3]).unwrap_or_else(|| panic!("{key} is drawn"));
    assert!(rt.find("src/chevron").is_some(), "the unstaged list shows its files");
    click(&mut rt, "unstaged-toggle");
    assert!(rt.state.folded_lists.contains("unstaged"));
    assert!(rt.find("src/chevron").is_none(), "folded, its rows are gone");
    assert!((tall(&rt, "unstaged") - 28.0).abs() < 0.5, "and it's its title alone");
    click(&mut rt, "unstaged-toggle");
    assert!(rt.find("src/chevron").is_some(), "a second click opens it again");
    let foot = |rt: &Runtime<Collate>| {
        let (panel, commit) = (rt.presented("staging").unwrap().0, rt.presented("commit-box").unwrap().0);
        (panel[1] + panel[3] - (commit[1] + commit[3])).abs()
    };
    assert!(foot(&rt) < 1.0, "the commit box sits at the foot");
    rt.update(|s, _| s.folded_lists.extend(["unstaged", "staged"]));
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
    assert!(foot(&rt) < 1.0, "and stays there with both lists folded: {} off", foot(&rt));
    // One rhythm down the panel: both hairlines run edge to edge, and each title's band
    // starts right under what's above it, the filter or a hairline.
    let at = |key: &str| rt.presented(key).unwrap_or_else(|| panic!("{key} is drawn")).0;
    let (between, rule) = (at("between"), at("rule"));
    assert!(between[0] == rule[0] && between[2] == rule[2], "the two hairlines span alike: {between:?} vs {rule:?}");
    assert_eq!(at("staged-head")[1] - between[1], at("commit-head")[1] - rule[1], "and sit alike over their titles");
    assert_eq!(at("staged-head")[3], at("commit-head")[3]);
    let (filter, unstaged) = (at("filter"), at("unstaged-head"));
    assert_eq!(unstaged[1] - (filter[1] + filter[3]), at("staged-head")[1] - (between[1] + between[3]), "the filter and the hairline sit alike over their titles");
    assert_eq!(between[1], unstaged[1] + unstaged[3], "and the hairline right under the title above it");
    let field = rt.presented("message").expect("the commit box").0;
    assert!(field[3] >= 140.0, "the message is a box, not a line: {} tall", field[3]);
}

#[test]
fn the_staging_panel_lines_up() {
    let rt = loaded();
    let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
    let (panel, filter, search) = (rect("staging"), rect("filter"), rect("search"));
    let bulk = rect("unstaged-bulk");
    let left = panel[0] + PANEL_INSET;
    let right = panel[0] + panel[2] - PANEL_INSET;
    assert!((filter[0] - left).abs() < 1.0, "the filter starts on the inset: {} vs {left}", filter[0]);
    assert!((filter[0] + filter[2] - right).abs() < 1.0, "and ends on it: {} vs {right}", filter[0] + filter[2]);
    let label_end = bulk[0] + bulk[2] - GHOST_PAD;
    assert!((label_end - right).abs() < 1.5, "Stage All's label ends on the same edge: {label_end} vs {right}");
    assert!((filter[3] - search[3]).abs() < 0.5, "the filter is as tall as the diff's search");
    // A row's chevron: its ink (4.5 pt into its 16 pt box) starts on the inset. `find`
    // takes a path (the key alone also matches the diff's chevrons) and gives the centre.
    let (cx, _) = rt.find("src/chevron").expect("the list's first chevron");
    let ink = cx - 8.0 + 4.5;
    assert!((ink - left).abs() < 1.0, "the list's chevron ink at {ink} vs {left}");
    // Row buttons end on the inset too.
    let quick = rect("quick");
    assert!((quick[0] + quick[2] - right).abs() < 1.0, "a row's + ends on the inset: {} vs {right}", quick[0] + quick[2]);
    for key in ["message", "commit"] {
        let r = rect(key);
        assert!((r[0] - left).abs() < 1.0 && (r[0] + r[2] - right).abs() < 1.0, "the commit {key} spans the inset: {r:?} vs {left}..{right}");
    }
}

#[test]
fn the_diff_has_one_edge_each_side() {
    let rt = loaded();
    let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
    let summary = rect("summary");
    let (left, right) = (summary[0] + 16.0, summary[0] + summary[2] - 16.0);
    // The summary's text starts on the edge and its last item ends on it.
    let what = rect("what");
    assert!((what[0] - left).abs() < 1.0, "summary text at {} vs {left}", what[0]);
    let viewed = rect("viewed-count");
    assert!((viewed[0] + viewed[2] - right).abs() < 1.0, "viewed count ends at {} vs {right}", viewed[0] + viewed[2]);
    // A file header: the chevron glyph (4 pt into its 24 pt box) and the viewed box.
    let toggle = rect("toggle");
    assert!((toggle[0] + 8.0 - left).abs() < 1.0, "the header triangle's ink at {} vs {left}", toggle[0] + 8.0);
    let check = rect("viewed");
    assert!((check[0] + check[2] - right).abs() < 1.0, "viewed box ends at {} vs {right}", check[0] + check[2]);
    // A hunk row's last action: its label (inside the ghost padding) ends on the edge.
    let discard = rect("discard");
    assert!((discard[0] + discard[2] - 8.0 - right).abs() < 1.5, "Discard Hunk… ends at {} vs {right}", discard[0] + discard[2] - 8.0);
}

#[test]
fn the_minimap_shows_the_view_and_takes_it_where_its_pressed() {
    let mut rt = loaded();
    let rect = |rt: &Runtime<Collate>, key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
    let map = rect(&rt, "minimap");
    let summary = rect(&rt, "summary");
    assert!((map[0] + map[2] - (summary[0] + summary[2])).abs() < 1.0, "the minimap sits on the diff's right edge");
    let before = rect(&rt, "minimap-view");
    assert!((before[1] - map[1]).abs() < 1.0, "the box starts at the top, where the view is");
    assert!(before[3] < map[3], "the box covers only the part on screen");
    let (x, y) = (map[0] + map[2] / 2.0, map[1] + map[3] - 2.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    let n = rt.state.diff.files().len();
    assert!(rt.state.diff.visible_file() + 3 >= n, "pressing its foot goes to the end, not file {} of {n}", rt.state.diff.visible_file());
    let after = rect(&rt, "minimap-view");
    assert!(after[1] > before[1] + map[3] / 2.0, "the box follows, to {} from {}", after[1], before[1]);
}

#[test]
fn inline_blame_names_who_changed_a_line_once_per_run() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    let code_x = |rt: &Runtime<Collate>| rt.presented("code").expect("a line's code").0[0];
    let before = code_x(&rt);
    rt.key(KeyEvent { key: Key::Text("b".into()), mods: Default::default() });
    rt.frame(1.0 / 60.0);
    assert!(rt.state.diff.blame_shown(), "b shows the blame column");
    let path = rt.state.diff.files()[0].path().to_owned();
    let note = |id: &str, who: &str| BlameNote { id: id.into(), who: who.into(), help: format!("{who} help").into() };
    let lines: Vec<BlameNote> = (1..=70).map(|n| if n == 11 { note("0000", "Not committed yet") } else { note("abc", "t, 2026-11-11") }).collect();
    rt.state.diff.set_blame(Some((path, lines)));
    for _ in 0..5 {
        rt.frame(1.0 / 60.0);
    }
    let blame = rt.presented("blame").expect("the blame column").0;
    assert!((blame[2] - 200.0).abs() < 1.0, "the column is {} wide", blame[2]);
    assert!((code_x(&rt) - before - 200.0).abs() < 1.0, "the code moves over for it");
    let st = &rt.state.diff;
    assert_eq!(st.blame_note(0, 11).map(|n| &*n.who), Some("Not committed yet"));
    assert_eq!(st.blame_missing().len(), st.files().len() - 1, "the other files wait for theirs");
    rt.key(KeyEvent { key: Key::Text("b".into()), mods: Default::default() });
    for _ in 0..5 {
        rt.frame(1.0 / 60.0);
    }
    assert!(!rt.state.diff.blame_shown(), "b again hides it");
    assert!((code_x(&rt) - before).abs() < 1.0, "and the code moves back");
}

#[test]
fn the_merge_editor_settles_conflicts_by_side_by_line_and_by_key() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    let out = std::env::temp_dir().join(format!("collate-merged-{}.txt", std::process::id()));
    let m = |s: &str| s.replace(' ', "\n") + "\n";
    let merge = crate::merge::Merge::new("m.txt".into(), &m("a 1 b 2 c"), &m("a x b y c"), &m("a X b Y c"), Some(out.clone()));
    rt.update(move |s, _| s.merge = Some(merge));
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("merge-save").is_some(), "the editor is open");
    let w = |key: &str| rt.presented(key).map(|p| p.0[2]).unwrap_or_else(|| panic!("{key} is drawn"));
    let (o, b, t) = (w("mo0"), w("mb0"), w("mt0"));
    assert!(o > 300.0 && (o - b).abs() < 1.0 && (o - t).abs() < 1.0, "ours, base and theirs share the width: {o} {b} {t}");
    // A line pressed is kept; the first conflict now has theirs' line.
    click(&mut rt, "mt0-0");
    // 1 takes ours for the conflict being read, the second.
    rt.update(|s, _| s.merge.as_mut().unwrap().current = 1);
    rt.key(KeyEvent { key: Key::Text("1".into()), mods: Default::default() });
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.merge.as_ref().unwrap().unsettled(), 0);
    click(&mut rt, "merge-save");
    assert!(rt.state.merge.is_none(), "saving closes it");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), m("a X b y c"));
    let _ = std::fs::remove_file(out);
}

#[test]
fn the_map_zooms_into_folders_and_opens_a_changed_file() {
    let mut rt = loaded();
    click(&mut rt, "view-map");
    assert!(rt.state.map.open, "Map opens");
    // Another test owns the shared repository, so this one maps the test repository itself.
    let built = map::build(test_repo()).expect("the test repository maps");
    assert!(built.files > 0);
    rt.update(move |s, _| s.map.data = Some(std::sync::Arc::new(built)));
    for _ in 0..3 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.scope_title(), "All Changes", "the view's changes stay what the map shows");
    let canvas = rt.presented("map-canvas").expect("the map is drawn").0;
    // A folder takes clicks on its name band; inside it, its files do.
    let press = |rt: &mut Runtime<Collate>, path: &str, folder: bool| {
        let r = rt.state.map_rect(path).unwrap_or_else(|| panic!("{path} is on the map"));
        let (x, y) = (canvas[0] + r[0] + r[2] / 2.0, canvas[1] + r[1] + if folder { 6.0 } else { r[3] / 2.0 });
        rt.pointer_move(x, y);
        rt.frame(1.0 / 60.0);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
    };
    // Everything in the test repository is under src/: a click opens that folder.
    press(&mut rt, "src", true);
    assert_eq!(rt.state.map.zoom, "src");
    let changed = rt.state.diff.files()[0].path().to_owned();
    press(&mut rt, &changed, false);
    assert!(!rt.state.map.open, "a changed file opens its diff");
    assert!(rt.find("summary").is_some(), "the diff shows again");
}

#[test]
fn the_branch_menu_previews_the_pull_request() {
    let mut rt = loaded();
    assert!(rt.find("view-branch").is_none(), "no Branch place in the sidebar, as in GitHub Desktop");
    // On a feature branch, so the repository's other branches are bases to pick. (The menu
    // rereads the branches from whichever repository the tests share, when one is open.)
    rt.update(|s, _| {
        s.branch = "feature".into();
        s.branches = vec!["main".into()];
    });
    click(&mut rt, "branch");
    let base = rt.state.branches.iter().find(|b| *b != "feature").cloned().expect("a branch to compare against");
    let menu = rt.popup_menu().expect("the branch menu opened");
    assert!(menu.find("preview").is_some() && menu.find(&format!("base:{base}")).is_some(), "preview and the bases to compare against");
    rt.resolve_popup(Some(&format!("base:{base}"))).unwrap();
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.scope, Scope::Branch);
    assert_eq!(rt.state.base.as_deref(), Some(base.as_str()));
    assert_eq!(rt.state.scope_title(), "Pull Request Preview");
}

#[test]
fn the_branch_menu_offers_the_unpushed_commits_when_there_are_some() {
    let mut rt = loaded();
    let item = |rt: &mut Runtime<Collate>| {
        click(rt, "branch");
        let menu = rt.popup_menu().expect("the branch menu opened");
        let item = menu.find("unpushed").map(|i| (i.title().to_owned(), i.is_enabled())).expect("the unpushed item");
        rt.resolve_popup(None).unwrap();
        item
    };
    rt.update(|s, _| s.ahead_behind = Some((0, 2)));
    assert_eq!(item(&mut rt), ("Show Unpushed Commits".to_owned(), false), "nothing to push, nothing to show");
    rt.update(|s, _| s.ahead_behind = Some((3, 0)));
    assert_eq!(item(&mut rt), ("Show 3 Unpushed Commits".to_owned(), true));
}

#[test]
fn the_three_columns_start_on_one_line() {
    let rt = loaded();
    let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
    let mid = |r: [f32; 4]| r[1] + r[3] / 2.0;
    // One top inset: each column's first row starts COLUMN_TOP below the card's edge and
    // centres its text in the same band.
    let (card, summary_row, filter, label) = (rect("main-pane"), rect("totals"), rect("filter"), rect("section-title"));
    for (name, r) in [("summary row", summary_row), ("filter", filter)] {
        assert!((r[1] - card[1] - COLUMN_TOP).abs() < 0.5, "the {name} starts {COLUMN_TOP} pt under the card's edge: {} vs {}", r[1], card[1]);
    }
    assert!((mid(filter) - mid(summary_row)).abs() < 0.5, "filter and summary centred alike: {} vs {}", mid(filter), mid(summary_row));
    assert!((mid(label) - mid(summary_row)).abs() < 0.5, "Working Copy centred alike: {} vs {}", mid(label), mid(summary_row));
    let mut rt = rt;
    rt.update(|s, _| s.show_low_signal = true);
    rt.frame(0.0);
    let rect = |key: &str| rt.presented(key).map(|p| p.0).unwrap_or_else(|| panic!("{key} is drawn"));
    let (notes, first_row, list_title) = (rect("notes"), rect("view-all"), rect("unstaged-head"));
    assert!((mid(notes) - mid(first_row)).abs() < 0.5 && (mid(notes) - mid(list_title)).abs() < 0.5, "second rows centred alike: {} {} {}", mid(notes), mid(first_row), mid(list_title));
    let (first_row, list_title) = (rect("view-all"), rect("unstaged-head"));
    assert!((mid(list_title) - mid(first_row)).abs() < 1.0, "the panel's first list title level with the sidebar's first row: {} vs {}", mid(list_title), mid(first_row));
}

#[test]
fn a_partly_staged_file_is_in_both_lists() {
    let mut rt = loaded();
    rt.update(|s, tx| {
        let mut snap = snapshot();
        snap.changes[0].staged = Staged::Partly;
        snap.changes[1].staged = Staged::Fully;
        s.hashes.clear();
        s.apply_snapshot(snap, tx);
    });
    let st = &rt.state;
    let (a, b, c) = (st.diff.files()[0].path(), st.diff.files()[1].path(), st.diff.files()[2].path());
    assert!(st.tree.contains(a) && st.staged_tree.contains(a), "partly staged: both lists");
    assert!(!st.tree.contains(b) && st.staged_tree.contains(b), "fully staged: Staged only");
    assert!(st.tree.contains(c) && !st.staged_tree.contains(c), "not staged: Unstaged only");
}

#[test]
fn recent_projects_keep_the_latest_first_once() {
    let mut list = vec![PathBuf::from("/a"), PathBuf::from("/b")];
    remember_project(&mut list, &[], PathBuf::from("/b"));
    assert_eq!(list, [PathBuf::from("/b"), PathBuf::from("/a")]);
    remember_project(&mut list, &[], PathBuf::from("/c"));
    assert_eq!(list[0], PathBuf::from("/c"));
    let labels = project_labels(&[PathBuf::from("/w/api"), PathBuf::from("/p/api"), PathBuf::from("/w/web")]);
    assert_eq!(labels, ["api - w", "api - p", "web"], "short, and distinct when names repeat");
}

#[test]
fn starred_projects_come_first_and_stay() {
    let list: Vec<PathBuf> = ["/w/web", "/w/core", "/p/voiceprint", "/p/demo"].iter().map(PathBuf::from).collect();
    let stars = [PathBuf::from("/p/demo")];
    assert_eq!(project_order(&list, &stars, ""), [3, 0, 1, 2], "starred first, the rest most recent first");
    assert_eq!(project_order(&list, &stars, "vp"), [2], "the search keeps only matches");
    let mut many: Vec<PathBuf> = (0..30).map(|i| PathBuf::from(format!("/r{i}"))).collect();
    let starred = [PathBuf::from("/r29")];
    remember_project(&mut many, &starred, PathBuf::from("/new"));
    assert_eq!(many.len(), 31, "the oldest goes, unless it's starred");
    assert_eq!(many[30], PathBuf::from("/r29"));
}

#[test]
fn only_changes_that_matter_wake_the_watcher() {
    let root = std::path::Path::new("/r");
    for p in ["/r/src/a.rs", "/r/.git/index", "/r/.git/HEAD", "/r/.git/refs/heads/main"] {
        assert!(relevant(std::path::Path::new(p), root), "{p}");
    }
    for p in ["/r/.git/objects/ab/cdef", "/r/.git/index.lock", "/r/.git/collate-backup/1/a.rs", "/r/.git/logs/HEAD"] {
        assert!(!relevant(std::path::Path::new(p), root), "{p}");
    }
}

#[test]
fn a_viewed_mark_holds_across_views() {
    let mut rt = loaded();
    rt.update(|s, tx| s.toggle_viewed(0, tx));
    let path = rt.state.diff.files()[0].path().to_owned();
    // The same file read for another view: its old side differs, its new side doesn't.
    rt.update(move |s, tx| {
        let mut snap = snapshot();
        for c in &mut snap.changes {
            c.hash ^= 0xdead_beef;
        }
        s.hashes.clear();
        s.apply_snapshot(snap, tx);
    });
    let i = rt.state.diff.file_index(&path).unwrap();
    assert!(rt.state.is_viewed(i), "still viewed");
    assert!(rt.state.stale.is_empty(), "and not flagged as changed");
}

#[test]
fn the_diff_and_the_sidebar_list_files_in_the_same_order() {
    let rt = loaded();
    let diff: Vec<&str> = rt.state.diff.files().iter().map(|f| f.path()).collect();
    let tree: Vec<&str> = (0..rt.state.tree.visible_len())
        .filter_map(|r| rt.state.tree.visible_id(r))
        .filter(|id| rt.state.tree.get(id).is_some_and(|n| !n.dir))
        .collect();
    assert_eq!(diff, tree);
}

#[test]
fn the_diff_scrolls_to_its_last_file() {
    let mut rt = loaded();
    for _ in 0..200 {
        rt.wheel(700.0, 500.0, -400.0);
        rt.frame(1.0 / 60.0);
    }
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    let last = rt.state.diff.files().len() - 1;
    assert!(rt.presented(&format!("df{last}")).is_some(), "the last file's header was reached");
}

#[test]
fn marking_a_file_viewed_folds_it_and_counts_it() {
    let mut rt = loaded();
    click(&mut rt, "src/api/file1.rs");
    let i = rt.state.diff.file_index("src/api/file1.rs").unwrap();
    rt.update(move |s, tx| s.toggle_viewed(i, tx));
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.state.is_viewed(i));
    assert!(rt.state.diff.collapsed(i));
    rt.update(move |s, tx| s.toggle_viewed(i, tx));
    rt.frame(1.0 / 60.0);
    assert!(!rt.state.is_viewed(i) && !rt.state.diff.collapsed(i));
}

#[test]
fn settings_restyle_the_diff() {
    let mut rt = loaded();
    rt.update(|s, _| s.settings.font_size = 16.0);
    rt.update(|s, _| {
        let was = Settings::default();
        s.apply_settings(Some(&was));
    });
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.diff.style().font_size, 16.0);
    assert_eq!(rt.state.diff.style().line_h, (16.0f32 * 1.6).round());
}

/// Stage a hunk, discard another behind the dialog, commit: the review loop end to end,
/// through the app's own handlers, on a throwaway repository.
#[test]
fn the_review_loop_stages_discards_and_commits() {
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let dir = std::env::temp_dir().join(format!("collate-flow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let text: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.join("a.txt"), &text).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"], &["config", "commit.gpgsign", "false"], &["add", "-A"], &["commit", "-qm", "init"]] {
        git(&dir, args);
    }
    std::fs::write(dir.join("a.txt"), text.replace("line 5\n", "line five\n").replace("line 35\n", "line thirty-five\n")).unwrap();
    // The one test that changes a repository owns REPO.
    set_repo(Some(Repo::open(&dir).unwrap()));
    let repo = current_repo().unwrap();
    let reread = |rt: &mut Runtime<Collate>| {
        let raw = repo.status_raw().unwrap();
        let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
        rt.update(move |s, tx| s.apply_snapshot(snap, tx));
        for _ in 0..5 {
            rt.frame(1.0 / 60.0);
        }
    };
    let env = Env { system_dark: true, scale: 2.0 };
    let mut state = init();
    state.settings = Settings::default();
    state.scope = Scope::Uncommitted;
    state.apply_settings(None);
    let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
    rt.frame(0.0);
    reread(&mut rt);
    assert_eq!(rt.state.diff.files()[0].hunks.len(), 2);

    // Stage the first hunk.
    rt.update(|s, _| s.stage_hunk(0, 0, None));
    reread(&mut rt);
    assert!(git(&dir, &["diff", "--cached"]).contains("+line five"));
    assert_eq!(rt.state.info[0].staged, Staged::Partly);
    assert_eq!(rt.state.staged, 1);
    assert!(rt.state.notice.as_ref().is_some_and(|n| !n.error), "a confirmation shows");

    // Discard the second: nothing happens until the dialog is confirmed.
    rt.update(|s, _| s.ask_discard(0, Some(1)));
    // Let the dialog finish opening before reaching for its button.
    for _ in 0..40 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("confirm-discard").is_some(), "the dialog asks first");
    assert!(std::fs::read_to_string(dir.join("a.txt")).unwrap().contains("thirty-five"));
    click(&mut rt, "confirm-discard");
    let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    assert!(!on_disk.contains("thirty-five") && on_disk.contains("line five"), "only the second hunk is undone");
    assert!(rt.state.confirm.is_none());

    // Commit what's staged.
    reread(&mut rt);
    rt.update(|s, _| s.message = TextEdit::new("Spell out five"));
    let summary = repo.commit("Spell out five", false).unwrap();
    assert!(summary.ends_with("Spell out five"));
    reread(&mut rt);
    assert_eq!(rt.state.diff.files().len(), 0, "nothing left to review");

    // Pick one line of a three-line replacement and stage just it, from the keyboard.
    let press = |rt: &mut Runtime<Collate>, t: &str| {
        let ev = KeyEvent { key: Key::Text(t.into()), mods: Default::default() };
        rt.update(move |s, tx| {
            on_key(s, &ev, tx);
        });
        rt.frame(1.0 / 60.0);
    };
    let text: String = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    std::fs::write(dir.join("a.txt"), text.replace("line 20\nline 21\nline 22\n", "line XX\nline XXI\nline XXII\n")).unwrap();
    reread(&mut rt);
    rt.update(|s, _| {
        // Side by side, old line 21 and its replacement share a row: one click picks both.
        s.diff.set_mode(DiffMode::Split);
        s.diff.set_selection(Some(diff::LineRange { file: 0, start: LineRef::new(diff::Side::Old, 21), end: LineRef::new(diff::Side::Old, 21) }));
    });
    rt.frame(1.0 / 60.0);
    assert!(rt.state.diff.selected_changes().is_some(), "lines are picked");
    press(&mut rt, "a");
    let index = git(&dir, &["show", ":a.txt"]);
    assert!(index.contains("line 20\nline XXI\nline 22\n"), "only line 21 is staged, in place: {}", &index[index.find("line 19").unwrap()..index.find("line 23").unwrap()]);
    assert!(rt.state.diff.selection().is_none(), "the pick is spent");

    // Discard a file, then take it back.
    reread(&mut rt);
    let before = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    rt.update(|s, _| s.discard_file(0));
    assert_ne!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), before);
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.undo.is_some()), "undo is offered");
    rt.update(|s, _| s.undo_notice());
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), before, "undo puts the file back");

    // Viewed, then edited: flagged, not silently forgotten.
    reread(&mut rt);
    rt.update(|s, tx| s.toggle_viewed(0, tx));
    assert!(rt.state.is_viewed(0));
    std::fs::write(dir.join("a.txt"), before.replace("line 30\n", "line thirty\n")).unwrap();
    reread(&mut rt);
    assert!(!rt.state.is_viewed(0) && rt.state.stale.contains("a.txt"));

    // Stage one of two word changes in a line: pick the row, p, untick one, Stage picked.
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "checkpoint"]);
    std::fs::write(dir.join("w.rs"), "fn a() {\n    let total = price * qty;\n}\n").unwrap();
    git(&dir, &["add", "w.rs"]);
    git(&dir, &["commit", "-qm", "w"]);
    std::fs::write(dir.join("w.rs"), "fn a() {\n    let sum = price * quantity;\n}\n").unwrap();
    reread(&mut rt);
    rt.on_key = Some(on_key);
    let f = rt.state.diff.file_index("w.rs").unwrap();
    rt.update(move |s, _| {
        s.diff.set_mode(DiffMode::Split);
        s.diff.set_selection(Some(diff::LineRange { file: f, start: LineRef::new(diff::Side::Old, 2), end: LineRef::new(diff::Side::Old, 2) }));
    });
    rt.key(KeyEvent { key: Key::Text("p".into()), mods: Default::default() });
    for _ in 0..40 {
        rt.frame(1.0 / 60.0);
    }
    let shown: Vec<String> = rt.state.words.as_ref().expect("the word picker opened").edits.iter().map(|e| format!("{:?}", e.old)).collect();
    assert_eq!(shown.len(), 2);
    rt.update(|s, _| s.words.as_mut().unwrap().picked[0] = false);
    rt.frame(1.0 / 60.0);
    click(&mut rt, "words-stage");
    assert!(rt.state.words.is_none());
    assert_eq!(git(&dir, &["show", ":w.rs"]), "fn a() {\n    let total = price * quantity;\n}\n", "only qty → quantity is staged");

    // Switch to another project: the review starts over there, and the list remembers both.
    let other = std::env::temp_dir().join(format!("collate-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(other.join("deep/folder")).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"]] {
        git(&other, args);
    }
    rt.update(|s, _| s.viewed.insert("w.rs".into(), 1).map(drop).unwrap_or(()));
    let deep = other.join("deep/folder");
    rt.update(move |s, tx| s.open_project(&deep, tx));
    let root = current_repo().unwrap().root.clone();
    assert_eq!(root.canonicalize().unwrap(), other.canonicalize().unwrap(), "a folder inside a repository opens the repository");
    assert_eq!(rt.state.name, format!("collate-other-{}", std::process::id()));
    assert!(!rt.state.loaded && rt.state.diff.files().is_empty() && rt.state.viewed.is_empty(), "nothing carries over");
    assert_eq!(rt.state.projects[0], root, "the opened project goes first");
    // The tab carries over: the same scope, and Pull Requests stays open.
    rt.update(|s, _| {
        s.set_scope(Scope::Unstaged);
        s.open_pulls();
    });
    let back = dir.clone();
    rt.update(move |s, tx| s.open_project(&back, tx));
    assert!(rt.state.scope == Scope::Unstaged && rt.state.pulls.open, "the switch keeps the tab");
    rt.update(|s, _| {
        s.pulls.open = false;
        s.set_scope(Scope::Uncommitted);
    });
    assert_eq!(rt.state.projects.len().min(2), 2);
    assert_eq!(rt.state.projects[1], root, "and the other stays in the list");
    // The switcher: starring the other project lifts it to the top; the search narrows the list.
    click(&mut rt, "project-trigger");
    assert!(rt.state.project_open && rt.find("project-search").is_some(), "the switcher opens on its search");
    let other_i = rt.state.projects.iter().position(|p| *p == root).unwrap();
    click(&mut rt, &format!("project-star-{other_i}"));
    assert!(rt.state.project_open, "a star leaves the switcher open");
    assert_eq!(rt.state.favorites, std::slice::from_ref(&root));
    assert_eq!(project_order(&rt.state.projects, &rt.state.favorites, "")[0], other_i, "the starred project goes first");
    let name = format!("collate-other-{}", std::process::id());
    rt.update(move |s, _| s.project_search = TextEdit::new(&name));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("project-option-0").is_some() && rt.find("project-option-2").is_none(), "only the match and Add show");
    rt.update(|s, _| s.project_search = TextEdit::new("zzzz"));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("project-none").is_some(), "no match says so");
    rt.update(|s, _| s.close_projects());
    rt.frame(1.0 / 60.0);
    rt.update(|s, tx| s.open_project(std::path::Path::new("/"), tx));
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error), "a folder outside git says so and changes nothing");
    assert_eq!(current_repo().unwrap().root.canonicalize().unwrap(), dir.canonicalize().unwrap());
    let _ = std::fs::remove_dir_all(&other);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn command_j_opens_a_terminal_under_the_diff() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    rt.update(|s, _| s.terminals.connect(std::sync::Arc::new(|_, _| {})));
    let cmd_j = KeyEvent { key: Key::Command('j'), mods: rune::Mods { cmd: true, ..Default::default() } };
    rt.key(cmd_j.clone());
    for _ in 0..5 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.state.terminals.open && rt.state.terminals.list.len() == 1, "⌘J starts a shell");
    let id = rt.state.terminals.list[0].id;
    let (_, grid_y) = rt.find(&format!("terminal-grid-{id}")).expect("the grid is drawn");
    let (_, diff_y) = rt.find("summary").expect("the diff stays");
    assert!(grid_y > diff_y, "the terminal sits under the diff");
    assert!(rt.state.terminals.list[0].keys.focused, "it takes the keys");
    // Typing reaches the shell through the app's own key path.
    for c in "echo collate-$((6*7))".chars() {
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Default::default() });
    }
    rt.key(KeyEvent { key: Key::Enter, mods: Default::default() });
    let start = std::time::Instant::now();
    while !rt.state.terminals.list[0].shown().contains("collate-42") {
        assert!(start.elapsed().as_secs() < 20, "the shell ran what was typed:\n{}", rt.state.terminals.list[0].shown());
        rt.frame(1.0 / 60.0);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    rt.frame(1.0 / 60.0);
    assert!(rt.find("terminal-cursor").is_some(), "the cursor shows");
    rt.key(cmd_j);
    rt.frame(1.0 / 60.0);
    assert!(!rt.state.terminals.open && rt.state.terminals.list.len() == 1, "⌘J again hides it; the shell keeps running");
}

#[test]
fn command_w_asks_before_ending_a_terminal_that_runs_a_program() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    rt.update(|s, _| {
        s.terminals.connect(std::sync::Arc::new(|_, _| {}));
        s.new_terminal();
        s.new_terminal();
    });
    let cmd_w = KeyEvent { key: Key::Command('w'), mods: rune::Mods { cmd: true, ..Default::default() } };
    // An idle shell ends at once.
    rt.key(cmd_w.clone());
    rt.frame(1.0 / 60.0);
    assert!(rt.state.terminals.list.len() == 1 && rt.state.notice.is_none(), "an idle shell ends without asking");
    for c in "sleep 30".chars() {
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Default::default() });
    }
    rt.key(KeyEvent { key: Key::Enter, mods: Default::default() });
    let start = std::time::Instant::now();
    while rt.state.terminals.list[0].running().is_none() {
        assert!(start.elapsed().as_secs() < 20, "sleep runs in front of the shell");
        rt.frame(1.0 / 60.0);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(rt.state.terminals.list[0].running().as_deref(), Some("sleep"));
    rt.key(cmd_w.clone());
    rt.frame(1.0 / 60.0);
    assert!(rt.state.terminals.list.len() == 1 && rt.state.notice.as_ref().is_some_and(|n| n.end.is_some()), "it asks first");
    click(&mut rt, "end-cancel");
    assert!(rt.state.terminals.list.len() == 1 && rt.state.notice.is_none(), "Cancel keeps it");
    rt.key(cmd_w.clone());
    rt.frame(1.0 / 60.0);
    click(&mut rt, "end-terminal");
    assert!(rt.state.terminals.list.is_empty() && !rt.state.terminals.open, "End Terminal ends it and the drawer goes");
}

/// Frames while a shell floods the terminal with coloured lines, every row changing.
/// `cargo test --release bench_terminal_frames -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_terminal_frames() {
    let mut rt = loaded();
    rt.update(|s, _| {
        s.terminals.connect(std::sync::Arc::new(|_, _| {}));
        s.terminals.height = 600.0;
        s.new_terminal();
    });
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    let id = rt.state.terminals.list[0].id;
    // What the shell's thread sends the app when output lands.
    let wake = move |rt: &mut Runtime<Collate>| {
        rt.update(move |s, tx| {
            s.terminals.event(id, alacritty_terminal::event::Event::Wakeup, tx);
        })
    };
    // The prompt drawn first, as on any open.
    std::thread::sleep(std::time::Duration::from_millis(800));
    wake(&mut rt);
    rt.frame(1.0 / 60.0);
    // A keystroke, then its frame, as typing goes.
    let mut keys = Vec::new();
    for c in "echo typing".chars() {
        let t = std::time::Instant::now();
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Default::default() });
        rt.frame(1.0 / 60.0);
        keys.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    keys.sort_by(f64::total_cmp);
    eprintln!("keystroke to frame: median {:.2} ms, max {:.2} ms", keys[keys.len() / 2], keys[keys.len() - 1]);
    rt.key(KeyEvent { key: Key::Text("u".into()), mods: rune::Mods { ctrl: true, ..Default::default() } });
    // Every line different, as real output is, so no frame reuses another's shaped text.
    let flood = "awk 'BEGIN{for(i=0;i<300000;i++) printf \"\\033[31mred \\033[1;32mgreen\\033[0m plain \\033[44mblue bg\\033[0m %d %d\\n\", i, i*7}'\r";
    for c in flood.chars() {
        let key = if c == '\r' { Key::Enter } else { Key::Text(c.to_string()) };
        rt.key(KeyEvent { key, mods: Default::default() });
    }
    rt.frame(1.0 / 60.0);
    let mut times = Vec::new();
    for _ in 0..240 {
        std::thread::sleep(std::time::Duration::from_millis(4));
        wake(&mut rt);
        let t = std::time::Instant::now();
        rt.frame(1.0 / 60.0);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let shown = rt.state.terminals.list[0].shown();
    assert!(shown.contains("blue bg"), "the flood was running:\n{shown}");
    let rows = shown.lines().count();
    eprintln!("terminal frames, {rows} rows: median {:.2} ms, p90 {:.2} ms, max {:.2} ms", times[times.len() / 2], times[times.len() * 9 / 10], times[times.len() - 1]);
}

#[test]
fn search_options_narrow_the_matches() {
    let mut rt = loaded();
    let hits = |rt: &mut Runtime<Collate>, q: &str, flip: Option<fn(&mut Collate)>| {
        rt.update(|s, _| {
            s.search = TextEdit::new(q);
            if let Some(flip) = flip {
                s.search_option(flip);
            }
            s.find();
        });
        let n = rt.state.hits.len();
        rt.update(|s, _| (s.find_case, s.find_word, s.find_regex) = (false, false, false));
        n
    };
    let files = rt.state.diff.files().len();
    assert_eq!(hits(&mut rt, "CHANGED", None), files * 2, "both added lines of each file, in any case");
    assert_eq!(hits(&mut rt, "CHANGED", Some(|s| s.find_case = true)), 0, "match case");
    assert_eq!(hits(&mut rt, "chang", Some(|s| s.find_word = true)), 0, "whole words only");
    assert_eq!(hits(&mut rt, r"also\s+changed\(4\d\)", Some(|s| s.find_regex = true)), files, "a regular expression");
    rt.update(|s, _| {
        s.find_regex = true;
        s.search = TextEdit::new("(");
        s.find();
    });
    assert!(rt.state.search_error.is_some() && rt.state.hits.is_empty(), "a bad pattern says so and finds nothing");
}

#[test]
fn command_p_goes_to_a_file_and_a_line() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    let key = |rt: &mut Runtime<Collate>, key: Key, cmd: bool| {
        rt.key(KeyEvent { key, mods: rune::Mods { cmd, ..Default::default() } });
        rt.frame(1.0 / 60.0);
    };
    key(&mut rt, Key::Command('p'), true);
    assert!(rt.state.jumping && rt.find("jump-field").is_some(), "the palette opens");
    for c in "util/file17".chars() {
        key(&mut rt, Key::Text(c.to_string()), false);
    }
    let (matches, _) = rt.state.jump_matches();
    assert_eq!(rt.state.diff.files()[matches[0]].path(), "src/util/file17.rs", "the closest path first");
    key(&mut rt, Key::Enter, false);
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(!rt.state.jumping);
    assert_eq!(rt.state.diff.files()[rt.state.diff.visible_file()].path(), "src/util/file17.rs");
    key(&mut rt, Key::Command('l'), true);
    assert_eq!(rt.state.jump, ":");
    for c in "41".chars() {
        key(&mut rt, Key::Text(c.to_string()), false);
    }
    assert_eq!(rt.state.jump_matches(), (vec![rt.state.diff.visible_file()], Some(41)), ":line is the file in view");
    key(&mut rt, Key::Escape, false);
    assert!(!rt.state.jumping, "Escape closes it");
    assert!(fuzzy("fl17", "src/util/file17.rs").is_some());
    assert!(fuzzy("file17", "src/util/file17.rs") > fuzzy("file17", "src/file1/x7.rs"), "a run in the name beats letters spread out");
    assert_eq!(fuzzy("zz", "src/util/file17.rs"), None);
}

#[test]
fn a_comment_goes_under_its_line_and_follows_it() {
    let mut rt = loaded();
    rt.on_key = Some(on_key);
    // The first file's changed line 43 (new side), picked as a click on its number does.
    rt.update(|s, _| s.diff.set_selection(Some(diff::LineRange::line(0, LineRef::new(diff::Side::New, 43)))));
    rt.key(KeyEvent { key: Key::Text("c".into()), mods: Default::default() });
    rt.frame(1.0 / 60.0);
    assert!(matches!(rt.state.prompt, Some(Ask::Comment(_, None))), "c asks for the comment");
    assert!(rt.find("prompt-field").is_some());
    rt.update(|s, _| {
        s.prompt_text = TextEdit::new("Why also?\nSecond line");
        s.answer_prompt();
    });
    assert_eq!(rt.state.comments.len(), 1);
    assert_eq!(rt.state.comments[0].at.text, "    let value_40 = also changed(40);");
    assert_eq!(rt.state.comments_shown, [true]);
    let row = rt.state.diff.rows.iter().position(|r| matches!(r, diff::Row::Comment { .. })).expect("a comment row");
    assert!(matches!(rt.state.diff.rows[row - 1], diff::Row::Line { .. }), "right under its line");
    // Lines added above it move it down; it stays with its text.
    let mut moved = rt.state.comments[0].at.clone();
    moved.line = 30;
    rt.update(|s, _| {
        s.comments[0].at = moved;
        s.place_comments();
    });
    assert_eq!(rt.state.comments_shown, [true], "found by its text near the old number");
    let text = comments_text(&rt.state.comments);
    assert_eq!(parse_comments(&text), rt.state.comments, "saved and read back the same, breaks and all");
    rt.update(|s, _| s.delete_comment(0));
    assert!(rt.state.diff.rows.iter().all(|r| !matches!(r, diff::Row::Comment { .. })));
}

#[test]
fn the_selection_copies_as_a_reference_or_as_lines_for_an_agent() {
    let mut rt = loaded();
    let path = rt.state.diff.files()[0].path().to_owned();
    // Line 13 changed (`original(10)` to `changed(10)`); 12 and 14 are its context.
    rt.update(|s, _| s.diff.set_selection(Some(diff::LineRange { file: 0, start: LineRef::new(diff::Side::New, 12), end: LineRef::new(diff::Side::New, 14) })));
    assert_eq!(rt.state.selection_reference().as_deref(), Some(format!("{path}#L12-L14").as_str()));
    let agent = rt.state.selection_for_agent().unwrap();
    assert!(agent.starts_with(&format!("`{path}#L12-L14`, in the uncommitted changes (HEAD to the working tree):\n\n```diff\n")), "{agent}");
    let lines: Vec<&str> = agent.lines().skip(3).take_while(|l| *l != "```").collect();
    assert_eq!(lines.len(), 4, "{agent}");
    assert!(lines[0].starts_with(" ") && lines[3].starts_with(" "), "context either side: {agent}");
    assert_eq!((lines[1], lines[2]), ("-    let value_10 = original(10);", "+    let value_10 = changed(10);"), "removed before added");
    rt.update(|s, _| s.diff.set_selection(Some(diff::LineRange::line(0, LineRef::new(diff::Side::Old, 13)))));
    assert_eq!(rt.state.selection_reference().as_deref(), Some(format!("{path}#L13 (before the change)").as_str()));
    // The hunk header offers both, in a menu, beside Comment.
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    click(&mut rt, "copy-sel");
    let menu = rt.popup_menu().expect("the Copy menu");
    assert!(menu.find("ref").is_some() && menu.find("agent").is_some());
    rt.resolve_popup(Some("ref")).unwrap();
    rt.frame(1.0 / 60.0);
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.text.contains("#L13")), "says what it copied");
    rt.update(|s, _| s.diff.set_selection(None));
    assert_eq!(rt.state.selection_reference(), None);
}

#[test]
fn comments_go_to_the_agent_as_one_prompt() {
    let mut rt = loaded();
    rt.update(|s, _| {
        s.diff.set_selection(Some(diff::LineRange::line(0, LineRef::new(diff::Side::New, 43))));
        s.ask_comment();
        s.prompt_text = TextEdit::new("Name this better");
        s.answer_prompt();
        s.loaded = true;
    });
    let prompt = rt.state.comments_prompt();
    assert!(prompt.starts_with("Address my review comments on the uncommitted changes (HEAD to the working tree)."), "{prompt}");
    assert!(prompt.ends_with(&rt.state.comments_markdown()) && prompt.contains("Name this better"));
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("send-comments").is_some(), "offered beside Copy as Markdown");
    // The tests have no terminal (and may have no project), so nothing starts; it says why.
    click(&mut rt, "send-comments");
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error), "{:?}", rt.state.notice.as_ref().map(|n| &n.text));
    assert!(rt.state.terminals.list.is_empty());
}

#[test]
fn the_checklist_asks_the_agent_about_the_change_in_view() {
    let mut rt = loaded();
    let prompt = rt.state.checklist_prompt(&["Is it safe?".to_owned(), "Who calls it?".to_owned()]);
    assert!(prompt.starts_with("Review the uncommitted changes (HEAD to the working tree). Read the change with `git diff HEAD`, plus the untracked files"), "{prompt}");
    assert!(prompt.ends_with("\n\n1. Is it safe?\n2. Who calls it?") && prompt.contains("Don't change any files."));
    rt.update(|s, _| {
        s.scope = Scope::History;
        s.commit = Some("0123456789abcdef^..0123456789abcdef".into());
    });
    assert!(rt.state.checklist_prompt(&["Why?".to_owned()]).starts_with("Review commit 0123456. Read the change with `git show 0123456`, and the code"));
    rt.update(|s, _| s.scope = Scope::Uncommitted);
    // In the "⋯" menu: every default question, the comments (none yet) and the list itself.
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    click(&mut rt, "more");
    let menu = rt.popup_menu().expect("the more menu");
    assert_eq!(menu.find("ask:0").map(|i| i.title()), Some("What does this change do?"));
    assert!(menu.find("ask:all").is_some() && menu.find("ask:edit").is_some());
    assert!(menu.find("ask:comments").is_some_and(|i| !i.is_enabled()));
    // The tests have no terminal (and may have no project), so the agent doesn't start.
    rt.resolve_popup(Some("ask:all")).unwrap();
    rt.frame(1.0 / 60.0);
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error), "{:?}", rt.state.notice.as_ref().map(|n| &n.text));
    assert!(rt.state.terminals.list.is_empty());
}

#[test]
fn the_agent_writes_a_commit_message_only_for_what_is_staged() {
    let mut rt = loaded();
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("write-message").is_some(), "in the commit box's head");
    assert_eq!(rt.state.staged, 0, "the fixture stages nothing");
    rt.update(|s, _| s.write_message());
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error && n.text.starts_with("Stage files first")));
    assert!(!rt.state.writing_message);
}

#[test]
fn checkpoints_are_in_the_more_menu_and_restoring_one_asks_first() {
    let mut rt = loaded();
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("agent-checkpoints").is_some(), "in the sidebar's Agent section");
    rt.update(|s, _| s.risky = Some(Risky::RestoreCheckpoint("0123456789".into(), 1_700_000_000)));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("risky-ok").is_some());
    rt.update(|s, _| s.risky = None);
    // Without a terminal the agent doesn't start, so no checkpoint is saved for it.
    rt.update(|s, _| s.run_agent("Fix it."));
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error));
    assert!(rt.state.terminals.list.is_empty());
}

#[test]
fn the_agent_has_its_own_sidebar_section_and_picker() {
    let mut rt = loaded();
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    for key in ["agent-review", "agent-comments", "agent-checkpoints", "agent-pick"] {
        assert!(rt.find(key).is_some(), "{key} in the sidebar");
    }
    assert!(rt.find("agent-settle").is_none(), "nothing to settle");
    click(&mut rt, "agent-pick");
    let menu = rt.popup_menu().expect("the agent picker");
    assert!(menu.find("agent:0").is_some_and(|i| i.is_checked()) && menu.find("ask:edit").is_some());
    rt.resolve_popup(Some("agent:1")).unwrap();
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.settings.agent, crate::settings::Agent::Codex);
    rt.update(|s, _| s.ahead_behind = Some((1, 1)));
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("agent-settle").is_some(), "a diverged branch has something to settle");
    click(&mut rt, "agent-pick");
    rt.resolve_popup(Some("settings")).unwrap();
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!((rt.state.screen, rt.state.settings_tab), (Screen::Settings, crate::settings_view::AGENT_TAB), "the picker opens the Agent settings");
    for key in ["agent", "agent-runs", "edit-questions", "edit-commit-rules"] {
        assert!(rt.find(key).is_some(), "{key} in the Agent settings");
    }
    assert!(rt.find("tab-agent").is_some());
}

#[test]
fn a_diverged_pull_or_a_stopped_merge_goes_to_the_agent() {
    let mut rt = loaded();
    let item = |rt: &mut Runtime<Collate>| {
        click(rt, "branch");
        let enabled = rt.popup_menu().expect("the branch menu opened").find("agent-rebase").map(|i| i.is_enabled()).expect("the agent's rebase");
        rt.resolve_popup(None).unwrap();
        enabled
    };
    rt.update(|s, _| s.ahead_behind = Some((2, 0)));
    assert!(!item(&mut rt), "only when the upstream moved too");
    rt.update(|s, _| s.ahead_behind = Some((2, 3)));
    assert!(item(&mut rt));
    // A pull can't fast-forward then, and says what can.
    click(&mut rt, "branch");
    rt.resolve_popup(Some("pull")).unwrap();
    rt.frame(1.0 / 60.0);
    assert!(rt.state.notice.as_ref().is_some_and(|n| n.error && n.text.ends_with("Rebase Onto Upstream with Claude Code")));
    // A merge stopped on conflicts: the bar offers the agent, which gets the conflicted files.
    rt.update(|s, _| {
        s.operation = Some(git::Operation::Merge);
        if let Some(i) = s.info.first_mut() {
            i.conflicted = true;
        }
    });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("op-agent").is_some());
    let first = rt.state.diff.files()[0].path().to_owned();
    assert_eq!(rt.state.stopped_settle(), Some(agent::Settle::Stopped(git::Operation::Merge, vec![first])));
}

#[test]
fn a_changed_png_is_drawn_not_called_binary() {
    // A 1×1 PNG, then another.
    let png = |byte: u8| -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend([0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, byte]);
        b
    };
    let blob = |b: Vec<u8>| Some(Blob::Image(b.into()));
    let c = Change { path: "icon.png".into(), old_path: None, old: blob(png(1)), new: blob(png(2)), hash: 1, staged: Staged::No, untracked: false, new_hash: 2, generated: false, reused: false, conflicted: false };
    let f = file_diff(&c, 3, false);
    assert!(f.binary);
    let sides = f.images.as_ref().expect("both sides are images");
    assert!(sides[0].is_some() && sides[1].is_some() && sides[0].as_ref().unwrap().0 != sides[1].as_ref().unwrap().0);
    let added = Change { old: None, ..c.clone() };
    assert!(file_diff(&added, 3, false).images.as_ref().is_some_and(|s| s[0].is_none()), "an added image has no before");
    let against_binary = Change { old: Some(Blob::Binary), ..c };
    assert!(file_diff(&against_binary, 3, false).images.is_none(), "an image against other bytes stays a note");
}

#[test]
fn pull_requests_ask_to_connect_github_instead_of_an_error() {
    let mut rt = loaded();
    for e in [github::NO_CLI, github::SIGNED_OUT] {
        rt.update(move |s, _| {
            s.pulls.list = Some(Err(e.into()));
            s.pulls.open = true;
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.find("connect-github").is_some(), "{e}: a Connect button");
        assert!(rt.find("pulls-filter").is_none(), "{e}: no filter that can't work");
    }
    rt.update(|s, _| s.pulls.list = Some(Err("HTTP 502".into())));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("connect-github").is_none() && rt.find("pulls-filter").is_some(), "other failures still say what went wrong");
}

#[test]
fn pull_requests_list_by_what_they_want_and_one_opens_with_its_card() {
    let mut rt = loaded();
    let pull = |number: u32, author: &str, requested: &[&str]| github::Pull {
        number, title: format!("Change {number}"), author: author.into(), head: format!("b{number}"), base: "main".into(), state: github::PrState::Open, draft: false,
        decision: "REVIEW_REQUIRED".into(), requested: requested.iter().map(|r| (*r).to_owned()).collect(), additions: 3, deletions: 1,
        updated: "2026-11-10T09:00:00Z".into(), labels: vec![], checks: vec![], ci: None, url: String::new(),
    };
    let merged = github::Pull { state: github::PrState::Merged, ..pull(6, "me", &[]) };
    rt.update(move |s, _| {
        s.pulls.list = Some(Ok(("me".into(), vec![pull(7, "ana", &["me"]), pull(8, "me", &[]), pull(9, "bo", &[]), merged])));
        s.pulls.open = true;
    });
    rt.frame(1.0 / 60.0);
    for key in ["pulls", "group-Review Requested", "group-Yours", "group-Everyone Else", "group-Merged", "pull-7", "pull-8", "pull-9"] {
        assert!(rt.find(key).is_some(), "{key}");
    }
    assert!(rt.find("pull-6").is_none(), "the merged start folded");
    click(&mut rt, "group-Merged");
    click(&mut rt, "group-Yours");
    assert!(rt.find("pull-6").is_some() && rt.find("pull-8").is_none(), "a group's title folds and opens it");
    click(&mut rt, "group-Yours");
    assert_eq!(rt.state.pulls.asked(), 1, "the sidebar counts what asks for the reader");
    rt.update(move |s, _| {
        let detail = github::Detail {
            pull: pull(7, "ana", &["me"]), body: "Why".into(), state: "OPEN".into(), mergeable: "MERGEABLE".into(), merge_state: "BLOCKED".into(),
            head_oid: "c3".into(), reviewers: vec![("me".into(), "REQUESTED".into())], assignees: vec![], conversation: vec![], threads: vec![], commentable: vec![],
            merge_base: "m0".into(), commits: vec![("a1".into(), "First".into()), ("c3".into(), "Second".into())], reviewed: vec![("me".into(), "a1".into())],
        };
        s.pulls.open = false;
        s.open_commit("m0..c3");
        s.pulls.pr = Some(detail);
        // The test has no watcher to read the range: the loaded diff stays the one shown.
        s.loaded = true;
    });
    rt.frame(1.0 / 60.0);
    for key in ["pr-card", "pr-approve", "pr-changes", "pr-merge", "reviewer-me", "pr-part"] {
        assert!(rt.find(key).is_some(), "{key}");
    }
    assert!(rt.state.pr_whole());
    // The summary fades in with the diff; a press waits for it.
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    click(&mut rt, "pr-part");
    let menu = rt.popup_menu().expect("the part menu opened");
    let titles: Vec<String> = ["all", "since", "a1", "c3"].iter().map(|id| menu.find(id).map(|i| i.title().to_owned()).expect(id)).collect();
    assert_eq!(titles, ["All Changes", "Changes Since Your Last Review", "1 of 2 - First", "2 of 2 - Second"]);
    rt.resolve_popup(Some("c3")).unwrap();
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.commit.as_deref(), Some("c3^..c3"), "one commit, against its parent");
    assert!(rt.state.pulls.pr.is_some() && !rt.state.pr_whole(), "still the pull request, but not all of it");
    assert!(rt.state.pending().is_empty(), "no line comment goes to GitHub from part of it");
    rt.update(|s, _| s.show_pr_part("all"));
    assert!(rt.state.pr_whole());
    // Reviewed on a commit the branch no longer has: a force-push to look at, not a range.
    rt.update(|s, _| {
        s.pulls.pr.as_mut().unwrap().reviewed = vec![("me".into(), "gone".into())];
        s.loaded = true;
    });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    click(&mut rt, "pr-part");
    let menu = rt.popup_menu().expect("the part menu opened");
    assert!(menu.find("rewrite").is_some() && menu.find("since").is_none(), "the force-push, not a range from a missing commit");
    rt.resolve_popup(None).unwrap();
    rt.update(|s, _| s.pulls.rewrite = Some("1:  a1 = 1:  b1 First\n2:  a2 ! 2:  c3 Second\n    -    old\n    +    new\n".into()));
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.find("rewrite-3").is_some(), "the dialog lists the range-diff");
    click(&mut rt, "rewrite-done");
    assert!(rt.state.pulls.rewrite.is_none());
    rt.update(|s, _| s.set_scope(Scope::Staged));
    assert!(rt.state.pulls.pr.is_none(), "another view leaves the pull request");
}

#[test]
fn a_changed_svg_is_drawn_above_its_lines() {
    let svg = |fill: &str| format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\">\n<rect width=\"8\" height=\"8\" fill=\"{fill}\"/>\n</svg>\n");
    let text = |t: String| Some(Blob::Text(t));
    let c = Change { path: "logo.svg".into(), old_path: None, old: text(svg("red")), new: text(svg("blue")), hash: 1, staged: Staged::No, untracked: false, new_hash: 2, generated: false, reused: false, conflicted: false };
    let f = file_diff(&c, 3, false);
    let sides = f.images.as_ref().expect("an SVG has pictures");
    assert!(sides[0].is_some() && sides[1].is_some());
    assert!(!f.hunks.is_empty(), "and its lines");
    let st = DiffState::new(vec![f]);
    assert!(matches!(st.rows[..2], [diff::Row::File(0), diff::Row::Note(0)]), "the pictures come first");
    assert!(st.rows.len() > 2);
    let plain = Change { path: "notes.txt".into(), ..c };
    assert!(file_diff(&plain, 3, false).images.is_none());
}

#[test]
fn a_changed_pdf_is_drawn_by_its_first_page() {
    // A blank 100 × 50 page, with the cross-reference table readers need.
    let objects = ["<</Type/Catalog/Pages 2 0 R>>", "<</Type/Pages/Kids[3 0 R]/Count 1>>", "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 50]>>"];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut xref = String::from("xref\n0 4\n0000000000 65535 f \n");
    for (i, o) in objects.iter().enumerate() {
        xref.push_str(&format!("{:010} 00000 n \n", pdf.len()));
        pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let at = pdf.len();
    pdf.push_str(&format!("{xref}trailer\n<</Size 4/Root 1 0 R>>\nstartxref\n{at}\n%%EOF\n"));
    let pdf = pdf.into_bytes();
    let c = Change { path: "doc.pdf".into(), old_path: None, old: None, new: Some(Blob::Image(pdf.clone().into())), hash: 1, staged: Staged::No, untracked: false, new_hash: 2, generated: false, reused: false, conflicted: false };
    let f = file_diff(&c, 3, false);
    let (hash, page) = f.images.as_ref().and_then(|s| s[1].clone()).expect("a PDF is a picture");
    assert!(page.starts_with(&[0xFF, 0xD8, 0xFF]), "rendered to a JPEG");
    assert_eq!(hash, hash_bytes(&pdf), "keyed by the PDF, not the rendering");
}

#[test]
fn dialogs_for_names_compares_and_risky_changes_open_and_close() {
    let mut rt = loaded();
    rt.update(|s, _| s.ask(Ask::Branch(None)));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("prompt-field").is_some() && rt.find("prompt-ok").is_some());
    rt.update(|s, _| s.prompt = None);
    rt.update(|s, _| s.compare = Some(("main".into(), "develop".into())));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("compare-from").is_some() && rt.find("compare-ok").is_some());
    rt.update(|s, _| s.compare = None);
    rt.update(|s, _| s.risky = Some(Risky::ResetHard("0123456789".into())));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("risky-ok").is_some());
    rt.update(|s, _| s.risky = None);
    rt.update(|s, _| s.blame = Some(("a.rs".into(), vec![git::Blamed { id: "a".repeat(40), author: "Ada".into(), time: 1_700_000_000, summary: "Add a".into(), text: "fn a() {}".into() }])));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("blame-rows").is_some());
    assert_eq!(day(1_700_000_000), "2023-11-14");
}

#[test]
fn search_finds_lines_and_steps_through_them() {
    let mut rt = loaded();
    rt.update(|s, _| {
        s.search = TextEdit::new("also changed");
        s.find();
    });
    let n = rt.state.hits.len();
    assert_eq!(n, rt.state.diff.files().len(), "one hit per file");
    rt.update(|s, tx| s.next_hit(true, tx));
    rt.update(|s, tx| s.next_hit(true, tx));
    assert_eq!(rt.state.hit, Some(1));
    rt.update(|s, tx| s.next_hit(false, tx));
    rt.update(|s, tx| s.next_hit(false, tx));
    assert_eq!(rt.state.hit, Some(n - 1), "stepping back from the first wraps to the last");
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    let last = n - 1;
    assert!(rt.presented(&format!("df{last}")).is_some() || rt.state.diff.visible_file() == last, "the last match's file is in view");
}

#[test]
fn large_files_start_folded_and_folds_survive_a_refresh() {
    let mut rt = loaded();
    // Every file in the fixture changes 4 lines; call that large.
    rt.update(|s, tx| {
        s.settings.fold_large = 3;
        s.seen.clear();
        s.hashes.clear();
        let snap = snapshot();
        s.apply_snapshot(snap, tx);
    });
    assert!((0..rt.state.diff.files().len()).all(|i| rt.state.diff.collapsed(i)), "large files start folded");
    rt.update(|s, _| s.diff.set_collapsed(2, false));
    rt.update(|s, tx| {
        s.hashes.clear();
        let snap = snapshot();
        s.apply_snapshot(snap, tx);
    });
    assert!(!rt.state.diff.collapsed(2), "a file the reader opened stays open");
    assert!(rt.state.diff.collapsed(3), "and one left folded stays folded");
}


/// How long each animation runs, Default against Reduced, on the headless clock at 120 Hz.
/// "Moved" is when the element watched lands within half a point (and 1% opacity) of where
/// it ends; "settled" is when the window stops asking for frames.
/// `cargo test --release bench_motion -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_motion() {
    const DT: f64 = 1.0 / 120.0;
    /// Steps until the window is idle; the times the watched element landed and the window settled.
    fn timeline(rt: &mut Runtime<Collate>, watch: &str) -> (f64, f64) {
        let mut seen = Vec::new();
        let mut t = 0.0;
        loop {
            let busy = rt.frame(DT);
            t += DT;
            seen.push((t, rt.presented(watch)));
            if !busy || t > 3.0 {
                break;
            }
        }
        let last = seen.last().and_then(|s| s.1);
        let near = |p: Option<([f32; 4], f32)>| match (p, last) {
            (Some((r, o)), Some((e, eo))) => r.iter().zip(e).all(|(a, b)| (a - b).abs() <= 0.5) && (o - eo).abs() <= 0.01,
            (a, b) => a.is_none() == b.is_none(),
        };
        let moved = seen.iter().rposition(|s| !near(s.1)).map_or(0.0, |i| seen[i].0 + DT);
        (moved * 1000.0, t * 1000.0)
    }
    let key = |c: char, alt: bool| KeyEvent { key: Key::Command(c), mods: rune::Mods { cmd: true, alt, ..Default::default() } };
    type Step = Box<dyn Fn(&mut Runtime<Collate>)>;
    let steps: Vec<(&str, &str, Step)> = vec![
        ("Sidebar hides", "summary", Box::new(|rt| rt.update(|s, _| s.sidebar.toggle()))),
        ("Sidebar shows", "summary", Box::new(|rt| rt.update(|s, _| s.sidebar.toggle()))),
        ("Staging panel hides", "summary", Box::new(move |rt| rt.key(key('0', true)))),
        ("Staging panel shows", "summary", Box::new(move |rt| rt.key(key('0', true)))),
        ("Settings open", "settings-nav", Box::new(move |rt| rt.key(key(',', false)))),
        ("Settings tab", "settings-rows", Box::new(|rt| click_once(rt, "tab-diff"))),
        ("Settings close", "settings-nav", Box::new(|rt| rt.key(KeyEvent { key: Key::Escape, mods: Default::default() }))),
        ("Split layout", "summary", Box::new(|rt| rt.key(KeyEvent { key: Key::Text("s".into()), mods: Default::default() }))),
        ("Unified layout", "summary", Box::new(|rt| rt.key(KeyEvent { key: Key::Text("s".into()), mods: Default::default() }))),
    ];
    fn click_once(rt: &mut Runtime<Collate>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
    }
    let mut rows = Vec::new();
    for motion in [Motion::Default, Motion::Reduced] {
        let mut rt = loaded();
        rt.on_key = Some(on_key);
        rt.update(move |s, _| s.settings.motion = motion);
        // Lands whatever loading started.
        timeline(&mut rt, "summary");
        for (name, watch, step) in &steps {
            step(&mut rt);
            rows.push((*name, motion, timeline(&mut rt, watch)));
        }
    }
    println!("{:<22}{:>16}{:>16}{:>16}{:>16}", "", "Default moved", "Reduced moved", "Default settled", "Reduced settled");
    for (name, _, _) in &steps {
        let of = |m: Motion| rows.iter().find(|r| r.0 == *name && r.1 == m).unwrap().2;
        let (d, r) = (of(Motion::Default), of(Motion::Reduced));
        // Nothing watched moves on a layout switch; only its settle time says anything.
        let ms = |v: f64| if v > 0.0 { format!("{v:.0}ms") } else { "-".into() };
        println!("{name:<22}{:>16}{:>16}{:>16}{:>16}", ms(d.0), ms(r.0), ms(d.1), ms(r.1));
    }
}

/// Refresh cost on a heavy review, by phase. `COLLATE_BENCH_REPO=<repo> cargo test --release
/// bench_refresh -- --ignored --nocapture`; medians of 7 runs, in milliseconds.
#[test]
#[ignore]
fn bench_refresh() {
    use std::time::Instant;
    let dir = std::env::var("COLLATE_BENCH_REPO").expect("COLLATE_BENCH_REPO");
    let repo = Repo::open(std::path::Path::new(&dir)).unwrap();
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[0], v[v.len() - 1])
    };
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let runs = 7;
    let (mut status, mut fp, mut snap_t, mut cold, mut warm, mut frame) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut prep_cold, mut prep_warm) = (vec![], vec![]);
    let (mut memo_cold, mut memo_edit) = (vec![], vec![]);
    for _ in 0..runs {
        let t = Instant::now();
        let raw = repo.status_raw().unwrap();
        status.push(ms(t));
        let t = Instant::now();
        let _ = repo.fingerprint(&raw, "");
        fp.push(ms(t));
        let t = Instant::now();
        let snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
        snap_t.push(ms(t));
        let env = Env { system_dark: true, scale: 2.0 };
        let mut state = init();
        state.settings = Settings::default();
        state.apply_settings(None);
        let mut rt = Runtime::new(state, view, (1280.0, 840.0), env);
        rt.frame(0.0);
        // Cold: every file diffed, as on launch (watcher thread), then shown (UI thread).
        let opts = rt.state.opts();
        let mut cache = DiffCache::new();
        let t = Instant::now();
        let p = prepare(snap, opts, &mut cache).unwrap();
        prep_cold.push(ms(t));
        let t = Instant::now();
        rt.update(move |s, tx| s.apply(p, tx));
        cold.push(ms(t));
        let t = Instant::now();
        rt.frame(1.0 / 60.0);
        frame.push(ms(t));
        // Warm: a refresh where one file changed (the cache serves the rest).
        let mut snap = repo.snapshot(Scope::Uncommitted, &raw, None).unwrap();
        snap.changes[0].hash ^= 1;
        let t = Instant::now();
        let p = prepare(snap, opts, &mut cache).unwrap();
        prep_warm.push(ms(t));
        let t = Instant::now();
        rt.update(move |s, tx| s.apply(p, tx));
        warm.push(ms(t));
    }
    // A real refresh: the watcher's memory in place, one file edited on disk.
    let readme = ["README.md", "README"].iter().map(|n| std::path::Path::new(&dir).join(n)).find(|p| p.exists()).expect("a README");
    let original = std::fs::read(&readme).unwrap();
    for i in 0..runs {
        let mut memo = git::Memo::default();
        let raw = repo.status_raw().unwrap();
        let t = Instant::now();
        let _ = repo.snapshot_with(Scope::Uncommitted, &raw, None, &mut memo).unwrap();
        memo_cold.push(ms(t));
        std::fs::write(&readme, [&original[..], format!("\nbench edit {i}\n").as_bytes()].concat()).unwrap();
        let raw = repo.status_raw().unwrap();
        let t = Instant::now();
        let snap = repo.snapshot_with(Scope::Uncommitted, &raw, None, &mut memo).unwrap();
        memo_edit.push(ms(t));
        assert_eq!(snap.changes.iter().filter(|c| !c.reused).count(), 1, "only the edited file is read");
    }
    std::fs::write(&readme, &original).unwrap();
    let files = repo.snapshot(Scope::Uncommitted, &repo.status_raw().unwrap(), None).unwrap().changes.len();
    eprintln!("files {files}");
    for (name, v) in [
        ("git status", status),
        ("fingerprint", fp),
        ("snapshot, no memory (blobs)", snap_t),
        ("snapshot, first (memo)", memo_cold),
        ("snapshot, 1 file edited (memo)", memo_edit),
        ("diff, cold (watcher thread)", prep_cold),
        ("apply cold (UI thread)", cold),
        ("first frame", frame),
        ("diff, 1 file changed (watcher)", prep_warm),
        ("apply, 1 file changed (UI thread)", warm),
    ] {
        let (m, lo, hi) = median(v);
        eprintln!("{name:36} median {m:8.1} ms   min {lo:8.1}   max {hi:8.1}");
    }
}

/// Opening a project and scrolling its review. `COLLATE_BENCH_REPO=<repo> cargo test
/// --release bench_start_scroll -- --ignored --nocapture`. Start is each step from opening
/// the repository to the first frame of its diff, medians of 5 runs in milliseconds; scroll
/// is frame times at 60 Hz, reading pace (150 pt a frame) then flicking (2,500 pt a frame),
/// with no file folded however large.
#[test]
#[ignore]
fn bench_start_scroll() {
    use std::time::{Duration, Instant};
    let dir = std::env::var("COLLATE_BENCH_REPO").expect("COLLATE_BENCH_REPO");
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let env = Env { system_dark: true, scale: 2.0 };
    let steps = ["open", "git status", "snapshot", "diff (watcher thread)", "apply (UI thread)", "first frame"];
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); steps.len() + 1];
    let mut rt = None;
    for _ in 0..5 {
        let mut t = [0.0; 6];
        let start = Instant::now();
        let at = Instant::now();
        let repo = Repo::open(std::path::Path::new(&dir)).unwrap();
        t[0] = ms(at);
        let at = Instant::now();
        let raw = repo.status_raw().unwrap();
        t[1] = ms(at);
        let at = Instant::now();
        let snap = repo.snapshot_with(Scope::Uncommitted, &raw, None, &mut git::Memo::default()).unwrap();
        t[2] = ms(at);
        let mut state = init();
        state.settings = Settings::default();
        // Every file open, large ones too: the worst case to scroll.
        state.settings.fold_large = 0;
        state.apply_settings(None);
        let at = Instant::now();
        let p = prepare(snap, state.opts(), &mut DiffCache::new()).unwrap();
        t[3] = ms(at);
        let mut r = Runtime::new(state, view, (1280.0, 840.0), env);
        let at = Instant::now();
        r.update(move |s, tx| s.apply(p, tx));
        t[4] = ms(at);
        let at = Instant::now();
        r.frame(0.0);
        t[5] = ms(at);
        let total = ms(start);
        for (v, x) in times.iter_mut().zip(t.into_iter().chain([total])) {
            v.push(x);
        }
        rt = Some(r);
    }
    let mut rt = rt.unwrap();
    let files = rt.state.diff.files().len();
    let lines: usize = rt.state.diff.files().iter().map(|f| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>()).sum();
    eprintln!("{dir}: {files} files, {lines} diff lines");
    let median = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    for (name, v) in steps.iter().chain(&["start, in all"]).zip(&times) {
        eprintln!("{name:36} median {:8.1} ms", median(v));
    }
    let stats = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        let over = v.iter().filter(|&&t| t > 16.7).count();
        format!("p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms, {over} of {} over 16.7", q(0.5), q(0.95), q(0.99), q(1.0), v.len())
    };
    for (name, frames, dy) in [("scroll, reading", 300, -150.0), ("scroll, flicking", 300, -2500.0)] {
        let mut phase = Vec::new();
        for _ in 0..frames {
            let t = Instant::now();
            rt.wheel(700.0, 500.0, dy);
            rt.frame(1.0 / 60.0);
            let spent = t.elapsed();
            phase.push(spent.as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_micros(16_667).saturating_sub(spent));
        }
        eprintln!("{name:36} {}", stats(&phase));
    }
}

#[test]
fn a_dropped_patch_shows_on_its_own_until_closed() {
    let mut rt = loaded();
    let files = rt.state.diff.files().len();
    assert!(files > 1);
    let patch = std::env::temp_dir().join(format!("collate-dropped-{}.patch", std::process::id()));
    std::fs::write(&patch, "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n").unwrap();
    rt.drop_files(vec![patch.clone()], Some((600.0, 400.0)));
    for _ in 0..3 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.loose.as_deref(), Some(patch.file_name().unwrap().to_str().unwrap()));
    assert_eq!(rt.state.diff.files().iter().map(|f| f.path()).collect::<Vec<_>>(), ["notes.txt"]);
    assert!(rt.find("loose-summary").is_some() && rt.find("unstaged").is_none(), "the patch, without the repository's lists");
    // A refresh of the repository doesn't replace it.
    let snap = snapshot();
    rt.update(move |s, tx| s.apply_snapshot(snap, tx));
    assert_eq!(rt.state.diff.files().len(), 1);
    click(&mut rt, "loose-close");
    assert!(rt.state.loose.is_none());
    let snap = snapshot();
    rt.update(move |s, tx| s.apply_snapshot(snap, tx));
    assert_eq!(rt.state.diff.files().len(), files, "back to the changes");
    let _ = std::fs::remove_file(patch);
}

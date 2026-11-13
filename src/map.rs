//! The repository map: every file a rectangle sized by its lines, nested in its folders (a
//! squarified treemap), and the imports between files. Built off the UI thread from the working
//! tree; a file is read again only when its size or modification time changed since the last
//! build, so a refresh costs a `stat` per file.
//!
//! Imports are found for JavaScript and TypeScript (relative paths, `index` files, `tsconfig`
//! `paths` and `baseUrl`, workspace packages by their `package.json` name) and Rust (`mod`,
//! `crate::` and `super::`). An import that can't be resolved draws no line rather than a
//! wrong one.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// A file or folder. Folders hold the sum of their files' lines.
pub struct Node {
    pub name: String,
    /// Relative to the repository root; empty for the root.
    pub path: String,
    pub parent: Option<usize>,
    /// Largest first, the order the treemap lays them out in.
    pub children: Vec<usize>,
    pub lines: u64,
    pub dir: bool,
}

pub struct RepoMap {
    /// The root is node 0.
    pub nodes: Vec<Node>,
    pub by_path: HashMap<String, usize>,
    /// By node: the files a file imports, and the files importing it. Empty for folders.
    pub imports: Vec<Vec<usize>>,
    pub imported_by: Vec<Vec<usize>>,
    pub files: usize,
}

impl RepoMap {
    /// The nodes from the root down to `node`, the root first.
    pub fn trail(&self, node: usize) -> Vec<usize> {
        let mut t = vec![node];
        while let Some(p) = self.nodes[*t.last().unwrap()].parent {
            t.push(p);
        }
        t.reverse();
        t
    }

    /// Whether `node` is `ancestor` or inside it.
    pub fn within(&self, mut node: usize, ancestor: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            match self.nodes[node].parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }
}

/// What reading a file found, kept between builds.
#[derive(Clone)]
struct Scan {
    len: u64,
    modified: Option<SystemTime>,
    lines: u64,
    specs: Vec<String>,
}

/// Every scanned file, by absolute path, across builds and projects.
static SCANS: Mutex<Option<HashMap<PathBuf, Scan>>> = Mutex::new(None);

/// Untracked folders left off the map.
const GENERATED: [&str; 8] = ["target", "node_modules", "dist", ".next", ".turbo", ".cache", ".venv", "__pycache__"];

/// Larger files are weighed by size and not searched for imports.
const READ_LIMIT: u64 = 2 << 20;

/// The map of the repository at `root`: tracked and untracked (not ignored) files that exist
/// in the working tree, less low-signal ones (lockfiles, bundles), which would crowd it.
pub fn build(root: &Path) -> Result<RepoMap, String> {
    let list = |which: &str| -> Result<Vec<String>, String> {
        let out = crate::git::command().args(["ls-files", "-z", which, "--exclude-standard"]).current_dir(root).output().map_err(|e| format!("couldn't run git: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        Ok(out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
    };
    let mut paths = list("--cached")?;
    // New files belong on the map, but not a build or dependency folder a project forgot to
    // ignore (Cargo's `target`, `node_modules`): those would bury the source.
    paths.extend(list("--others")?.into_iter().filter(|p| !p.split('/').any(|part| GENERATED.contains(&part))));
    paths.sort();
    paths.dedup();
    paths.retain(|p| !crate::settings::low_signal(p));
    let scans = scan_all(root, &paths);
    // A file listed but gone from the working tree (deleted, not yet staged) has no scan.
    let present: Vec<(String, Scan)> = paths.into_iter().zip(scans).filter_map(|(p, s)| Some((p, s?))).collect();
    let mut map = tree(&present);
    let specs: Vec<(usize, &Vec<String>)> = present.iter().map(|(p, s)| (map.by_path[p], &s.specs)).collect();
    link(&mut map, root, &specs);
    Ok(map)
}

/// Reads the files that changed since they were last read, on every core.
fn scan_all(root: &Path, paths: &[String]) -> Vec<Option<Scan>> {
    let cached = SCANS.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let chunk = paths.len().div_ceil(threads).max(1);
    let results: Vec<Option<Scan>> = std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| {
                let cached = &cached;
                scope.spawn(move || part.iter().map(|p| scan(&root.join(p), p, cached)).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    // Keep this project's scans, and other projects' too: switching back is then cheap.
    let mut keep = cached;
    for (p, s) in paths.iter().zip(&results) {
        match s {
            Some(s) => keep.insert(root.join(p), s.clone()),
            None => keep.remove(&root.join(p)),
        };
    }
    *SCANS.lock().unwrap_or_else(|e| e.into_inner()) = Some(keep);
    results
}

fn scan(abs: &Path, rel: &str, cached: &HashMap<PathBuf, Scan>) -> Option<Scan> {
    let meta = std::fs::metadata(abs).ok().filter(|m| m.is_file())?;
    let (len, modified) = (meta.len(), meta.modified().ok());
    if let Some(c) = cached.get(abs).filter(|c| c.len == len && c.modified == modified) {
        return Some(c.clone());
    }
    if len > READ_LIMIT {
        return Some(Scan { len, modified, lines: len / 40, specs: Vec::new() });
    }
    let bytes = std::fs::read(abs).ok()?;
    // Binary files (images, fonts) keep a token size.
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Some(Scan { len, modified, lines: 1, specs: Vec::new() });
    }
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64 + u64::from(bytes.last().is_some_and(|&b| b != b'\n'));
    let text = String::from_utf8_lossy(&bytes);
    let specs = match lang(rel) {
        Lang::Js => js_specs(&text),
        Lang::Rust => rust_specs(&text),
        Lang::Other => Vec::new(),
    };
    Some(Scan { len, modified, lines: lines.max(1), specs })
}

#[derive(PartialEq)]
enum Lang {
    Js,
    Rust,
    Other,
}

fn lang(path: &str) -> Lang {
    match path.rsplit_once('.').map(|(_, e)| e) {
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "vue" | "svelte" | "astro") => Lang::Js,
        Some("rs") => Lang::Rust,
        _ => Lang::Other,
    }
}

/// The module names a JS/TS file imports: `from "x"`, `import "x"`, `import("x")`,
/// `require("x")`.
fn js_specs(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let quoted = |mut i: usize| -> Option<String> {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        let q = *b.get(i)?;
        if q != b'"' && q != b'\'' && q != b'`' {
            return None;
        }
        let end = b[i + 1..].iter().take(300).position(|&c| c == q || c == b'\n')?;
        (b[i + 1 + end] == q).then(|| text[i + 1..i + 1 + end].to_owned())
    };
    let word_at = |i: usize, w: &[u8]| b[i..].starts_with(w) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$' || b[i - 1] == b'.'));
    let mut i = 0;
    while i < b.len() {
        let found = if word_at(i, b"from") && b.get(i + 4).is_some_and(|c| !c.is_ascii_alphanumeric()) {
            quoted(i + 4)
        } else if word_at(i, b"import") {
            let mut j = i + 6;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            if b.get(j) == Some(&b'(') { quoted(j + 1) } else { quoted(i + 6).filter(|_| j > i + 6 || matches!(b.get(j), Some(b'"' | b'\''))) }
        } else if word_at(i, b"require(") {
            quoted(i + 8)
        } else {
            None
        };
        if let Some(s) = found.filter(|s| !s.is_empty() && !s.contains("${")) {
            out.push(s);
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

/// A Rust file's module references: `mod:name` for `mod name;`, `crate:name` and `super:name`
/// for the first module named after `crate::` and `super::`.
fn rust_specs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let ident = |s: &str| s.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>();
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        let decl = t.strip_prefix("pub(crate) ").or_else(|| t.strip_prefix("pub(super) ")).or_else(|| t.strip_prefix("pub ")).unwrap_or(t);
        if let Some(rest) = decl.strip_prefix("mod ") {
            let name = ident(rest);
            if rest[name.len()..].trim_start().starts_with(';') && !name.is_empty() {
                out.push(format!("mod:{name}"));
            }
        }
        for (key, tag) in [("crate::", "crate"), ("super::", "super")] {
            let mut rest = t;
            while let Some(at) = rest.find(key) {
                let after = &rest[at + key.len()..];
                // `use crate::{a, b}` names several.
                let names: Vec<String> = match after.strip_prefix('{') {
                    Some(list) => list.split([',', '}']).map(|n| ident(n.trim())).take_while(|n| !n.is_empty()).collect(),
                    None => vec![ident(after)],
                };
                out.extend(names.into_iter().filter(|n| !n.is_empty()).map(|n| format!("{tag}:{n}")));
                rest = after;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The folder tree over `files` (sorted paths), each folder summing its files' lines.
fn tree(files: &[(String, Scan)]) -> RepoMap {
    let mut nodes = vec![Node { name: String::new(), path: String::new(), parent: None, children: Vec::new(), lines: 0, dir: true }];
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for (path, scan) in files {
        let mut parent = 0;
        let mut at = 0;
        let parts: Vec<&str> = path.split('/').collect();
        for (n, part) in parts.iter().enumerate() {
            at += part.len() + usize::from(n > 0);
            let here = &path[..at];
            let last = n + 1 == parts.len();
            let id = match by_path.get(here) {
                Some(&id) => id,
                None => {
                    let id = nodes.len();
                    nodes.push(Node { name: (*part).to_owned(), path: here.to_owned(), parent: Some(parent), children: Vec::new(), lines: if last { scan.lines } else { 0 }, dir: !last });
                    nodes[parent].children.push(id);
                    by_path.insert(here.to_owned(), id);
                    id
                }
            };
            parent = id;
        }
    }
    // Children always come after their parent, so one pass from the end sums every folder.
    for i in (1..nodes.len()).rev() {
        let (lines, parent) = (nodes[i].lines, nodes[i].parent.unwrap_or(0));
        nodes[parent].lines += lines;
    }
    for i in 0..nodes.len() {
        let mut kids = std::mem::take(&mut nodes[i].children);
        kids.sort_by(|&a, &b| nodes[b].lines.cmp(&nodes[a].lines).then_with(|| nodes[a].name.cmp(&nodes[b].name)));
        nodes[i].children = kids;
    }
    let n = nodes.len();
    RepoMap { nodes, by_path, imports: vec![Vec::new(); n], imported_by: vec![Vec::new(); n], files: files.len() }
}

/// Resolves every file's imports to files in the map.
fn link(map: &mut RepoMap, root: &Path, specs: &[(usize, &Vec<String>)]) {
    let r = Resolver::new(map, root);
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for &(from, list) in specs {
        let path = map.nodes[from].path.as_str();
        let rust = lang(path) == Lang::Rust;
        for spec in list.iter() {
            let to = if rust { r.rust(path, spec) } else { r.js(path, spec) };
            if let Some(to) = to.and_then(|p| map.by_path.get(&p).copied()).filter(|&to| to != from) {
                edges.push((from, to));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    for (from, to) in edges {
        map.imports[from].push(to);
        map.imported_by[to].push(from);
    }
}

/// A `tsconfig.json`'s module lookup: its folder, `baseUrl`, and `paths` patterns.
struct TsConfig {
    dir: String,
    base: Option<String>,
    paths: Vec<(String, Vec<String>)>,
}

/// A workspace package: where it is, its entry files, and its `exports` map.
struct Package {
    dir: String,
    entries: Vec<String>,
    exports: Vec<(String, String)>,
}

struct Resolver<'a> {
    files: HashSet<&'a str>,
    configs: Vec<TsConfig>,
    packages: Vec<(String, Package)>,
}

const JS_EXTS: [&str; 14] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".d.ts", ".vue", ".svelte", ".astro", ".json", ".css"];

impl<'a> Resolver<'a> {
    fn new(map: &'a RepoMap, root: &Path) -> Resolver<'a> {
        let files: HashSet<&str> = map.nodes.iter().filter(|n| !n.dir).map(|n| n.path.as_str()).collect();
        let read = |p: &str| std::fs::read_to_string(root.join(p)).ok().and_then(|t| json::parse(&t));
        let mut configs = Vec::new();
        let mut packages = Vec::new();
        for &p in &files {
            let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
            if name == "tsconfig.json" || name == "jsconfig.json" {
                if let Some(c) = ts_config(root, dir, p, 0) {
                    configs.push(c);
                }
            } else if name == "package.json"
                && let Some(v) = read(p)
                && let Some(pkg) = v.get("name").and_then(json::Value::str)
            {
                let mut entries: Vec<String> = ["module", "main", "types", "typings", "source"].iter().filter_map(|k| v.get(k).and_then(json::Value::str)).map(str::to_owned).collect();
                let mut exports = Vec::new();
                match v.get("exports") {
                    Some(json::Value::Str(s)) => entries.insert(0, s.clone()),
                    Some(e @ json::Value::Obj(fields)) => {
                        if fields.iter().all(|(k, _)| k.starts_with('.')) {
                            for (k, target) in fields {
                                if let Some(t) = target.first_string() {
                                    exports.push((k.clone(), t));
                                }
                            }
                        } else if let Some(t) = e.first_string() {
                            // Conditions only (`{"import": …, "require": …}`): the package's root.
                            entries.insert(0, t);
                        }
                    }
                    _ => {}
                }
                packages.push((pkg.to_owned(), Package { dir: dir.to_owned(), entries, exports }));
            }
        }
        // The longest name first, so `@a/b-c` wins over `@a/b`.
        packages.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
        configs.sort_by_key(|c| std::cmp::Reverse(c.dir.len()));
        Resolver { files, configs, packages }
    }

    /// The file `base` names, trying the extensions and `index` files a bundler would.
    fn file(&self, base: &str) -> Option<String> {
        let base = normalize(base)?;
        let found = |p: &str| self.files.contains(p).then(|| p.to_owned());
        if let Some(f) = found(&base) {
            return Some(f);
        }
        // TypeScript's ESM style names the output: `./a.js` for `a.ts`.
        for (js, ts) in [(".js", ".ts"), (".js", ".tsx"), (".jsx", ".tsx"), (".mjs", ".mts"), (".cjs", ".cts")] {
            if let Some(stem) = base.strip_suffix(js)
                && let Some(f) = found(&format!("{stem}{ts}"))
            {
                return Some(f);
            }
        }
        JS_EXTS.iter().find_map(|e| found(&format!("{base}{e}"))).or_else(|| JS_EXTS.iter().find_map(|e| found(&format!("{base}/index{e}"))))
    }

    fn js(&self, from: &str, spec: &str) -> Option<String> {
        let spec = spec.split(['?', '#']).next().unwrap_or(spec);
        let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            return self.file(&join(dir, spec));
        }
        if let Some(cfg) = self.configs.iter().find(|c| c.dir.is_empty() || from.starts_with(&format!("{}/", c.dir))) {
            let base = cfg.base.as_deref().unwrap_or(&cfg.dir);
            for (pattern, targets) in &cfg.paths {
                let rest = match pattern.strip_suffix('*') {
                    Some(prefix) => spec.strip_prefix(prefix),
                    None => (spec == pattern).then_some(""),
                };
                if let Some(rest) = rest
                    && let Some(f) = targets.iter().find_map(|t| self.file(&join(base, &t.replace('*', rest))))
                {
                    return Some(f);
                }
            }
            if let Some(b) = &cfg.base
                && let Some(f) = self.file(&join(b, spec))
            {
                return Some(f);
            }
        }
        let (name, pkg) = self.packages.iter().find(|(n, _)| spec == n || spec.starts_with(&format!("{n}/")))?;
        let sub = spec[name.len()..].trim_start_matches('/');
        let key = if sub.is_empty() { ".".to_owned() } else { format!("./{sub}") };
        for (k, target) in &pkg.exports {
            let hit = match k.split_once('*') {
                Some((pre, post)) => key.strip_prefix(pre).and_then(|r| r.strip_suffix(post)).map(|r| target.replace('*', r)),
                None => (*k == key).then(|| target.clone()),
            };
            if let Some(f) = hit.and_then(|t| self.file(&join(&pkg.dir, &t))) {
                return Some(f);
            }
        }
        if sub.is_empty() {
            pkg.entries.iter().find_map(|e| self.file(&join(&pkg.dir, e))).or_else(|| ["src/index", "index"].iter().find_map(|e| self.file(&join(&pkg.dir, e))))
        } else {
            self.file(&join(&pkg.dir, sub)).or_else(|| self.file(&join(&pkg.dir, &format!("src/{sub}"))))
        }
    }

    fn rust(&self, from: &str, spec: &str) -> Option<String> {
        let (tag, name) = spec.split_once(':')?;
        let (dir, file) = from.rsplit_once('/').unwrap_or(("", from));
        let module = |d: &str| self.files.get(join(d, &format!("{name}.rs")).as_str()).or_else(|| self.files.get(join(d, &format!("{name}/mod.rs")).as_str())).map(|s| (*s).to_owned());
        let owns_dir = matches!(file, "main.rs" | "lib.rs" | "mod.rs");
        match tag {
            // `mod x;` in a.rs lives in a/x.rs; in main.rs, lib.rs or mod.rs beside it.
            "mod" => module(&if owns_dir { dir.to_owned() } else { join(dir, file.trim_end_matches(".rs")) }),
            // The parent module's folder: this one's, or the one above for mod.rs.
            "super" => module(if file == "mod.rs" { dir.rsplit_once('/').map_or("", |(d, _)| d) } else { dir }),
            "crate" => {
                // The crate root: the nearest folder up holding main.rs or lib.rs.
                let mut d = dir.to_owned();
                loop {
                    if self.files.contains(join(&d, "main.rs").as_str()) || self.files.contains(join(&d, "lib.rs").as_str()) {
                        return module(&d);
                    }
                    d = d.rsplit_once('/').map(|(p, _)| p.to_owned()).filter(|_| !d.is_empty())?;
                }
            }
            _ => None,
        }
    }
}

/// A tsconfig's lookup, following a relative `extends` for `paths` and `baseUrl` it doesn't set.
fn ts_config(root: &Path, dir: &str, path: &str, depth: u8) -> Option<TsConfig> {
    let v = json::parse(&std::fs::read_to_string(root.join(path)).ok()?)?;
    let opts = v.get("compilerOptions");
    let base = opts.and_then(|o| o.get("baseUrl")).and_then(json::Value::str).map(|b| join(dir, b));
    let paths: Vec<(String, Vec<String>)> = match opts.and_then(|o| o.get("paths")) {
        Some(json::Value::Obj(fields)) => fields.iter().map(|(k, t)| (k.clone(), t.strings())).collect(),
        _ => Vec::new(),
    };
    let mut cfg = TsConfig { dir: dir.to_owned(), base, paths };
    if (cfg.paths.is_empty() || cfg.base.is_none())
        && depth < 3
        && let Some(ext) = v.get("extends").and_then(json::Value::str).filter(|e| e.starts_with('.'))
    {
        let parent_path = normalize(&join(dir, ext))?;
        let parent_path = if parent_path.ends_with(".json") { parent_path } else { format!("{parent_path}.json") };
        let parent_dir = parent_path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
        if let Some(parent) = ts_config(root, &parent_dir, &parent_path, depth + 1) {
            if cfg.paths.is_empty() {
                cfg.paths = parent.paths;
                // Inherited paths resolve against the config that set them.
                cfg.base = cfg.base.or(parent.base).or(Some(parent.dir));
            }
            cfg.base = cfg.base.or(None);
        }
    }
    Some(cfg)
}

fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./");
    if dir.is_empty() || rel.starts_with('/') { rel.to_owned() } else { format!("{dir}/{rel}") }
}

/// `a/./b/../c` as `a/c`; `None` when it climbs out of the repository.
fn normalize(p: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// One laid-out rectangle: `[x, y, width, height]` in points.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub node: usize,
    pub rect: [f32; 4],
    /// A folder with room for its name along its top.
    pub titled: bool,
}

/// Height of a folder's name band.
pub const TITLE_H: f32 = 16.0;

/// Lays out `root`'s contents in a `w` × `h` box, folders holding their files, down to where
/// rectangles get too small to see. Parents come before their children.
pub fn layout(map: &RepoMap, root: usize, w: f32, h: f32) -> Vec<Cell> {
    let mut out = Vec::new();
    place_children(map, root, [0.0, 0.0, w, h], &mut out);
    out
}

fn place_children(map: &RepoMap, node: usize, area: [f32; 4], out: &mut Vec<Cell>) {
    if area[2] < 3.0 || area[3] < 3.0 {
        return;
    }
    let kids: Vec<usize> = map.nodes[node].children.iter().copied().filter(|&c| map.nodes[c].lines > 0).collect();
    let sizes: Vec<f64> = kids.iter().map(|&c| map.nodes[c].lines as f64).collect();
    for (&child, r) in kids.iter().zip(squarify(&sizes, area)) {
        // A 2-point gutter between neighbours.
        let rect = [r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0];
        if rect[2] < 1.5 || rect[3] < 1.5 {
            continue;
        }
        let n = &map.nodes[child];
        let titled = n.dir && rect[2] >= 44.0 && rect[3] >= TITLE_H + 14.0;
        out.push(Cell { node: child, rect, titled });
        if n.dir {
            let top = if titled { TITLE_H } else { 2.0 };
            place_children(map, child, [rect[0] + 2.0, rect[1] + top, rect[2] - 4.0, rect[3] - top - 2.0], out);
        }
    }
}

/// Squarified treemap (Bruls, Huizing and van Wijk): rectangles for `sizes` (largest first)
/// filling `area`, each as close to square as rows allow.
fn squarify(sizes: &[f64], area: [f32; 4]) -> Vec<[f32; 4]> {
    let total: f64 = sizes.iter().sum();
    let (mut x, mut y, mut w, mut h) = (area[0] as f64, area[1] as f64, area[2] as f64, area[3] as f64);
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return vec![[0.0; 4]; sizes.len()];
    }
    let scale = w * h / total;
    let areas: Vec<f64> = sizes.iter().map(|s| s * scale).collect();
    let worst = |row: &[f64], sum: f64, side: f64| {
        let (lo, hi) = row.iter().fold((f64::MAX, 0.0f64), |(lo, hi), &a| (lo.min(a), hi.max(a)));
        let (s2, side2) = (sum * sum, side * side);
        (side2 * hi / s2).max(s2 / (side2 * lo))
    };
    let mut out = Vec::with_capacity(sizes.len());
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h);
        let mut j = i + 1;
        let mut sum = areas[i];
        let mut best = worst(&areas[i..j], sum, side);
        while j < areas.len() {
            let next = worst(&areas[i..=j], sum + areas[j], side);
            if next > best {
                break;
            }
            best = next;
            sum += areas[j];
            j += 1;
        }
        if w >= h {
            // A column along the left edge.
            let cw = if h > 0.0 { sum / h } else { 0.0 };
            let mut cy = y;
            for a in &areas[i..j] {
                let ch = if cw > 0.0 { a / cw } else { 0.0 };
                out.push([x as f32, cy as f32, cw as f32, ch as f32]);
                cy += ch;
            }
            x += cw;
            w -= cw;
        } else {
            // A row along the top edge.
            let rh = if w > 0.0 { sum / w } else { 0.0 };
            let mut cx = x;
            for a in &areas[i..j] {
                let cw = if rh > 0.0 { a / rh } else { 0.0 };
                out.push([cx as f32, y as f32, cw as f32, rh as f32]);
                cx += cw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

/// The deepest cell under `[x, y]`.
pub fn hit(cells: &[Cell], x: f32, y: f32) -> Option<usize> {
    cells.iter().rev().find(|c| x >= c.rect[0] && y >= c.rect[1] && x < c.rect[0] + c.rect[2] && y < c.rect[1] + c.rect[3]).map(|c| c.node)
}

/// Just enough JSON for `package.json` and `tsconfig.json`, which allow comments and trailing
/// commas.
mod json {
    pub enum Value {
        Null,
        Bool,
        Num,
        Str(String),
        Arr(Vec<Value>),
        Obj(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Obj(f) => f.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }
        /// The first string inside, depth first: an `exports` entry's target through its
        /// conditions (`{"types": …, "import": …}`).
        pub fn first_string(&self) -> Option<String> {
            match self {
                Value::Str(s) => Some(s.clone()),
                Value::Arr(a) => a.iter().find_map(Value::first_string),
                Value::Obj(f) => f.iter().find_map(|(_, v)| v.first_string()),
                Value::Null | Value::Bool | Value::Num => None,
            }
        }
        pub fn strings(&self) -> Vec<String> {
            match self {
                Value::Str(s) => vec![s.clone()],
                Value::Arr(a) => a.iter().filter_map(|v| v.str().map(str::to_owned)).collect(),
                _ => Vec::new(),
            }
        }
    }

    pub fn parse(text: &str) -> Option<Value> {
        let mut p = Parser { b: text.as_bytes(), i: 0 };
        p.value()
    }

    struct Parser<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn skip(&mut self) {
            loop {
                while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                    self.i += 1;
                }
                if self.b[self.i..].starts_with(b"//") {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                } else if self.b[self.i..].starts_with(b"/*") {
                    self.i = self.b[self.i + 2..].windows(2).position(|w| w == b"*/").map_or(self.b.len(), |p| self.i + 2 + p + 2);
                } else {
                    return;
                }
            }
        }

        fn value(&mut self) -> Option<Value> {
            self.skip();
            match *self.b.get(self.i)? {
                b'{' => {
                    self.i += 1;
                    let mut fields = Vec::new();
                    loop {
                        self.skip();
                        if *self.b.get(self.i)? == b'}' {
                            self.i += 1;
                            return Some(Value::Obj(fields));
                        }
                        let Value::Str(k) = self.value()? else { return None };
                        self.skip();
                        if *self.b.get(self.i)? != b':' {
                            return None;
                        }
                        self.i += 1;
                        fields.push((k, self.value()?));
                        self.skip();
                        if *self.b.get(self.i)? == b',' {
                            self.i += 1;
                        }
                    }
                }
                b'[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.skip();
                        if *self.b.get(self.i)? == b']' {
                            self.i += 1;
                            return Some(Value::Arr(items));
                        }
                        items.push(self.value()?);
                        self.skip();
                        if *self.b.get(self.i)? == b',' {
                            self.i += 1;
                        }
                    }
                }
                b'"' => {
                    self.i += 1;
                    let mut s = Vec::new();
                    while let Some(&c) = self.b.get(self.i) {
                        self.i += 1;
                        match c {
                            b'"' => return Some(Value::Str(String::from_utf8_lossy(&s).into_owned())),
                            b'\\' => {
                                let e = *self.b.get(self.i)?;
                                self.i += 1;
                                match e {
                                    b'n' => s.push(b'\n'),
                                    b't' => s.push(b'\t'),
                                    b'u' => self.i += 4,
                                    other => s.push(other),
                                }
                            }
                            _ => s.push(c),
                        }
                    }
                    None
                }
                _ => {
                    let start = self.i;
                    while self.i < self.b.len() && !matches!(self.b[self.i], b',' | b'}' | b']') && !self.b[self.i].is_ascii_whitespace() {
                        self.i += 1;
                    }
                    match &self.b[start..self.i] {
                        b"null" => Some(Value::Null),
                        b"true" | b"false" => Some(Value::Bool),
                        b"" => None,
                        _ => Some(Value::Num),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_js_imports() {
        let src = "import a from './a';\nimport { b,\n  c } from \"../b/c\";\nimport './side.css';\nexport * from '@acme/db/schema';\nconst x = await import('./lazy');\nconst y = require('lodash');\nconst s = 'from the start';\n";
        assert_eq!(js_specs(src), ["../b/c", "./a", "./lazy", "./side.css", "@acme/db/schema", "lodash"]);
    }

    #[test]
    fn finds_rust_modules() {
        let src = "mod diff;\npub mod git;\nuse crate::{graph, settings::Settings};\nuse super::model;\nlet x = crate::avatars::Avatars::default();\n// mod not;\n";
        assert_eq!(rust_specs(src), ["crate:avatars", "crate:graph", "crate:settings", "mod:diff", "mod:git", "super:model"]);
    }

    fn map_of(paths: &[&str]) -> RepoMap {
        let files: Vec<(String, Scan)> = paths.iter().map(|p| ((*p).to_owned(), Scan { len: 0, modified: None, lines: 10, specs: Vec::new() })).collect();
        tree(&files)
    }

    #[test]
    fn resolves_relative_index_and_rust() {
        let map = map_of(&["src/a.ts", "src/b/index.tsx", "src/c.ts", "src/main.rs", "src/diff/mod.rs", "src/diff/view.rs", "src/diff/model.rs", "src/git.rs"]);
        let r = Resolver { files: map.nodes.iter().filter(|n| !n.dir).map(|n| n.path.as_str()).collect(), configs: Vec::new(), packages: Vec::new() };
        assert_eq!(r.js("src/a.ts", "./b").as_deref(), Some("src/b/index.tsx"));
        assert_eq!(r.js("src/b/index.tsx", "../c.js").as_deref(), Some("src/c.ts"));
        assert_eq!(r.js("src/a.ts", "react"), None);
        assert_eq!(r.rust("src/main.rs", "mod:diff").as_deref(), Some("src/diff/mod.rs"));
        assert_eq!(r.rust("src/diff/mod.rs", "mod:view").as_deref(), Some("src/diff/view.rs"));
        assert_eq!(r.rust("src/diff/view.rs", "super:model").as_deref(), Some("src/diff/model.rs"));
        assert_eq!(r.rust("src/diff/view.rs", "crate:git").as_deref(), Some("src/git.rs"));
    }

    #[test]
    fn resolves_tsconfig_paths_and_packages() {
        let map = map_of(&["apps/web/src/lib/x.ts", "apps/web/src/app.tsx", "packages/db/src/schema.ts", "packages/db/src/index.ts"]);
        let r = Resolver {
            files: map.nodes.iter().filter(|n| !n.dir).map(|n| n.path.as_str()).collect(),
            configs: vec![TsConfig { dir: "apps/web".into(), base: Some("apps/web".into()), paths: vec![("@/*".into(), vec!["./src/*".into()])] }],
            packages: vec![("@acme/db".into(), Package { dir: "packages/db".into(), entries: Vec::new(), exports: vec![("./schema".into(), "./src/schema.ts".into())] })],
        };
        assert_eq!(r.js("apps/web/src/app.tsx", "@/lib/x").as_deref(), Some("apps/web/src/lib/x.ts"));
        assert_eq!(r.js("apps/web/src/app.tsx", "@acme/db/schema").as_deref(), Some("packages/db/src/schema.ts"));
        assert_eq!(r.js("apps/web/src/app.tsx", "@acme/db").as_deref(), Some("packages/db/src/index.ts"));
    }

    #[test]
    fn treemap_fills_its_box_without_overlap() {
        let r = squarify(&[6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0], [0.0, 0.0, 600.0, 400.0]);
        let area: f32 = r.iter().map(|c| c[2] * c[3]).sum();
        assert!((area - 240_000.0).abs() < 1.0, "fills the box: {area}");
        for (i, a) in r.iter().enumerate() {
            for b in &r[i + 1..] {
                let overlap_w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
                let overlap_h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
                assert!(overlap_w <= 0.01 || overlap_h <= 0.01, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn json_allows_comments_and_trailing_commas() {
        let v = json::parse("{ // c\n \"compilerOptions\": { \"paths\": { \"@/*\": [\"./src/*\",], }, /* x */ }, }").unwrap();
        let paths = v.get("compilerOptions").and_then(|o| o.get("paths")).and_then(|p| p.get("@/*")).map(json::Value::strings);
        assert_eq!(paths, Some(vec!["./src/*".to_owned()]));
    }
}

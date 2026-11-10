//! Syntax colours for the languages rune-kit's built-in scanners don't know, from tree-sitter
//! grammars fetched and built on demand. The app ships only the parser runtime; a grammar is a
//! repository at a pinned commit, fetched with `git`, compiled with the system C compiler
//! (Xcode's command line tools) into a dynamic library, and kept with its highlight queries
//! under `~/Library/Application Support/Collate/grammars/<id>/`.
//!
//! [`prepare`] installs (if asked) and loads one grammar, off the UI thread. [`Languages`] is
//! the UI's side: what is loaded, being prepared, or failed. [`colour`] highlights code with a
//! loaded grammar, on the UI thread, for the diff's [`Highlighter`](rune_kit::syntax::Highlighter).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rune_kit::syntax::{Paint, Syntax};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent};

/// A grammar Collate can fetch: where its source is and which files it colours.
#[derive(Debug)]
pub struct Grammar {
    /// Its folder name, and how settings and the code refer to it.
    pub id: &'static str,
    pub title: &'static str,
    /// Extensions, without the dot, or whole file names (`Dockerfile`), as
    /// [`FileDiff::language`](crate::diff::FileDiff::language) gives them.
    pub files: &'static [&'static str],
    repo: &'static str,
    /// The commit fetched: a grammar's queries and generated parser change together, and a
    /// pinned commit is the same code for every reader.
    rev: &'static str,
    /// The folder holding `src/parser.c`, for repositories with more than one grammar.
    dir: &'static str,
    /// A grammar whose highlight queries this one's build on (`; inherits: c`).
    base: Option<&'static str>,
}

const fn g(id: &'static str, title: &'static str, files: &'static [&'static str], repo: &'static str, rev: &'static str) -> Grammar {
    Grammar { id, title, files, repo, rev, dir: "", base: None }
}

impl Grammar {
    const fn dir(mut self, dir: &'static str) -> Grammar {
        self.dir = dir;
        self
    }

    const fn base(mut self, base: &'static str) -> Grammar {
        self.base = Some(base);
        self
    }
}

/// The grammars on offer, by title. Every one is checked by `every_grammar_builds_and_colours`.
pub const CATALOGUE: &[Grammar] = &[
    g("c", "C", &["c", "h"], "tree-sitter/tree-sitter-c", "b780e47fc780ddc8da13afa35a3f4ed5c157823d"),
    g("c_sharp", "C#", &["cs"], "tree-sitter/tree-sitter-c-sharp", "8c0abe0b84a3681d3e6852ce2e2a1eecb4f731c5"),
    g("cpp", "C++", &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "ipp"], "tree-sitter/tree-sitter-cpp", "c009222808634c1014f82438d4883753516a2c24").base("c"),
    g("clojure", "Clojure", &["clj", "cljs", "cljc", "edn"], "sogaiu/tree-sitter-clojure", "e43eff80d17cf34852dcd92ca5e6986d23a7040f"),
    g("cmake", "CMake", &["cmake", "CMakeLists.txt"], "uyha/tree-sitter-cmake", "a3ec2b2144daf7132e7ee824d8cab9610523d10f"),
    g("d", "D", &["d"], "gdamore/tree-sitter-d", "64f27931b4e6fdd75af1102c79bacbca68a8dacc"),
    g("dart", "Dart", &["dart"], "UserNobody14/tree-sitter-dart", "0038280b774b65c1b89ba7fec50624d8baed9fe1"),
    g("dockerfile", "Dockerfile", &["Dockerfile", "dockerfile", "Containerfile"], "camdencheek/tree-sitter-dockerfile", "971acdd908568b4531b0ba28a445bf0bb720aba5"),
    g("elixir", "Elixir", &["ex", "exs"], "elixir-lang/tree-sitter-elixir", "cb32ccbe17c67037b09e31bf9beb22cdae6ed8e9"),
    g("elm", "Elm", &["elm"], "elm-tooling/tree-sitter-elm", "e1e8fea161a1e66f3997855d316be2a43e4e956f"),
    g("erlang", "Erlang", &["erl", "hrl"], "WhatsApp/tree-sitter-erlang", "62c82e67b73e13f3ef06e4c8c07ff035a8235d30"),
    g("fish", "Fish", &["fish"], "ram02z/tree-sitter-fish", "b7f1d682941e0c62dfcd6bf9ef481638351bab16"),
    g("fortran", "Fortran", &["f", "f90", "f95", "f03", "f08"], "stadelmanma/tree-sitter-fortran", "72f8759dea28ff6671e0868a9ed5ad4a18413917"),
    g("gleam", "Gleam", &["gleam"], "gleam-lang/tree-sitter-gleam", "fa6d0d94804f1b342e175865a3dbeb316ee5115e"),
    g("glsl", "GLSL", &["glsl", "vert", "frag", "comp", "geom"], "tree-sitter-grammars/tree-sitter-glsl", "24a6c8ef698e4480fecf8340d771fbcb5de8fbb4").base("c"),
    g("graphql", "GraphQL", &["graphql", "gql"], "bkegley/tree-sitter-graphql", "5e66e961eee421786bdda8495ed1db045e06b5fe"),
    g("groovy", "Groovy", &["groovy", "gradle"], "murtaza64/tree-sitter-groovy", "2a6ddd558b6aa39c5b77d8db9fe9baf817486b2c"),
    g("haskell", "Haskell", &["hs"], "tree-sitter/tree-sitter-haskell", "97288e585b0bd44199720280d067fa35bacd81ff"),
    g("ini", "INI", &["ini"], "justinmk/tree-sitter-ini", "e4018b5176132b4f3c5d6e61cea383f42288d0f5"),
    g("java", "Java", &["java"], "tree-sitter/tree-sitter-java", "e10607b45ff745f5f876bfa3e94fbcc6b44bdc11"),
    g("julia", "Julia", &["jl"], "tree-sitter/tree-sitter-julia", "e0f9dcd180fdcfcfa8d79a3531e11d99e79321d3"),
    g("just", "Just", &["just", "Justfile", "justfile"], "IndianBoy42/tree-sitter-just", "5685543a6e64f66335e25518c9ae8ffa1dae3d01"),
    g("kdl", "KDL", &["kdl"], "tree-sitter-grammars/tree-sitter-kdl", "e8ff98a113e175b84b92ec2096820517c1e10a66"),
    g("kotlin", "Kotlin", &["kt", "kts"], "fwcd/tree-sitter-kotlin", "d6af96d2e23ef0a8ff5ab512864378cf08fba311"),
    g("lua", "Lua", &["lua"], "tree-sitter-grammars/tree-sitter-lua", "10fe0054734eec83049514ea2e718b2a56acd0c9"),
    g("make", "Make", &["mk", "Makefile", "makefile", "GNUmakefile"], "tree-sitter-grammars/tree-sitter-make", "70613f3d812cbabbd7f38d104d60a409c4008b43"),
    g("markdown", "Markdown", &["md", "markdown"], "tree-sitter-grammars/tree-sitter-markdown", "41ff24a6c775492c543b09228aeb98d1cd2b3a0f").dir("tree-sitter-markdown"),
    g("nim", "Nim", &["nim", "nims"], "alaviss/tree-sitter-nim", "ac72ba30d16edf0be021588a9301ede4accd6cf4"),
    g("nix", "Nix", &["nix"], "nix-community/tree-sitter-nix", "21daff7ec37b01539eddd1e0ac468d2070c2ed5c"),
    g("objc", "Objective-C", &["m"], "tree-sitter-grammars/tree-sitter-objc", "181a81b8f23a2d593e7ab4259981f50122909fda").base("c"),
    g("ocaml", "OCaml", &["ml"], "tree-sitter/tree-sitter-ocaml", "a1c562e73ab787dcc69fdc2e5510c53bbce5364f").dir("grammars/ocaml"),
    g("ocaml_interface", "OCaml Interface", &["mli"], "tree-sitter/tree-sitter-ocaml", "a1c562e73ab787dcc69fdc2e5510c53bbce5364f").dir("grammars/interface"),
    g("php", "PHP", &["php"], "tree-sitter/tree-sitter-php", "58d5643086ae60e93a3746cca648363ee97c8761").dir("php"),
    g("powershell", "PowerShell", &["ps1", "psm1", "psd1"], "airbus-cert/tree-sitter-powershell", "65ea6919e816a7ac142acec7ee72757f89db8439"),
    g("prisma", "Prisma", &["prisma"], "victorhqc/tree-sitter-prisma", "3556b2c1f20ec9ac91e92d32c43d9d2a0ca3cc49"),
    g("proto", "Protocol Buffers", &["proto"], "treywood/tree-sitter-proto", "e9f6b43f6844bd2189b50a422d4e2094313f6aa3"),
    g("r", "R", &["r", "R"], "r-lib/tree-sitter-r", "58a22794466c0fc15b0d3b40531db751593721e8"),
    g("ruby", "Ruby", &["rb", "rake", "gemspec", "Gemfile", "Rakefile"], "tree-sitter/tree-sitter-ruby", "ad907a69da0c8a4f7a943a7fe012712208da6dee"),
    g("scala", "Scala", &["scala", "sc", "sbt"], "tree-sitter/tree-sitter-scala", "9e930d6f3a3b3626778b53549a661fbaef643d8e"),
    g("toml", "TOML", &["toml"], "tree-sitter-grammars/tree-sitter-toml", "64b56832c2cffe41758f28e05c756a3a98d16f41"),
    g("typst", "Typst", &["typ"], "uben0/tree-sitter-typst", "46cf4ded12ee974a70bf8457263b67ad7ee0379d"),
    g("wgsl", "WGSL", &["wgsl"], "szebniok/tree-sitter-wgsl", "40259f3c77ea856841a4e0c4c807705f3e4a2b65"),
    g("zig", "Zig", &["zig"], "tree-sitter-grammars/tree-sitter-zig", "6479aa13f32f701c383083d8b28360ebd682fb7d"),
];

/// The built-in scanners' languages, for the settings to name.
pub const BUILT_IN: &str = "Rust, TypeScript, JavaScript, JSON, Python, Go, Swift, shell, SQL, HTML, XML, CSS, YAML and diffs";

/// The grammar for a file's language as [`FileDiff::language`](crate::diff::FileDiff::language)
/// gives it, unless the built-in scanners colour it already.
pub fn for_language(lang: &str) -> Option<&'static Grammar> {
    if rune_kit::syntax::known(lang) {
        return None;
    }
    CATALOGUE.iter().find(|g| g.files.contains(&lang)).or_else(|| {
        // Extensions in any case (`.C`, `.PRISMA`); whole names only as written.
        let lower = lang.to_ascii_lowercase();
        CATALOGUE.iter().find(|g| g.files.contains(&lower.as_str()))
    })
}

pub fn by_id(id: &str) -> Option<&'static Grammar> {
    CATALOGUE.iter().find(|g| g.id == id)
}

/// Where grammars are kept; `None` in tests, which never touch the reader's.
pub fn root() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Collate/grammars"))
}

const LIBRARY: &str = "grammar.dylib";
const HIGHLIGHTS: &str = "highlights.scm";
const LOCALS: &str = "locals.scm";
/// The commit the folder was built from, written last: a folder without it is half-built.
const REV: &str = "rev";
/// The parser's entry point, as `tree_sitter_<name>`.
const SYMBOL: &str = "symbol";

/// Whether `g` is built in `root` from the commit the catalogue pins.
pub fn installed(root: &Path, g: &Grammar) -> bool {
    std::fs::read_to_string(root.join(g.id).join(REV)).is_ok_and(|r| r.trim() == g.rev)
}

/// A loaded grammar with its highlight queries.
pub type Config = HighlightConfiguration;

/// Install `g` if it's missing or outdated, then load it. Blocking: fetching and compiling
/// take seconds, so run it off the UI thread.
pub fn prepare(root: &Path, g: &Grammar) -> Result<Config, String> {
    if !installed(root, g) {
        install(root, g)?;
    }
    load(root, g)
}

/// Fetch `g`'s repository at its commit, compile its parser and keep it with its queries in
/// `root/<id>`, replacing what was there only once the new build has loaded.
pub fn install(root: &Path, g: &Grammar) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let work = root.join(format!(".build-{}-{}", g.id, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let result = build(root, g, &work);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn build(root: &Path, g: &Grammar, work: &Path) -> Result<(), String> {
    let src = work.join("src");
    fetch(g, &src)?;
    let grammar = src.join(g.dir);
    let out = work.join("out");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let parser = grammar.join("src/parser.c");
    let code = std::fs::read_to_string(&parser).map_err(|e| format!("{} has no parser: {e}", g.title))?;
    // The function returning the language, not the external scanner's, also `(void)`.
    let symbol = regex::Regex::new(r"TSLanguage\s*\*\s*(tree_sitter_\w+)\s*\(\s*void\s*\)")
        .unwrap()
        .captures(&code)
        .map(|c| c[1].to_owned())
        .ok_or_else(|| format!("{}'s parser has no entry point", g.title))?;
    compile(g, &grammar, &out.join(LIBRARY))?;
    let mut highlights = String::new();
    if let Some(base) = g.base {
        let b = by_id(base).ok_or_else(|| format!("{} builds on {base}, which isn't in the catalogue", g.title))?;
        let bsrc = work.join("base");
        fetch(b, &bsrc)?;
        highlights.push_str(&query(b, &bsrc, HIGHLIGHTS).ok_or_else(|| format!("{} has no highlight queries", b.title))?);
        highlights.push('\n');
    }
    highlights.push_str(&query(g, &src, HIGHLIGHTS).ok_or_else(|| format!("{} has no highlight queries", g.title))?);
    std::fs::write(out.join(HIGHLIGHTS), highlights).map_err(|e| e.to_string())?;
    if let Some(locals) = query(g, &src, LOCALS) {
        std::fs::write(out.join(LOCALS), locals).map_err(|e| e.to_string())?;
    }
    std::fs::write(out.join(SYMBOL), symbol).map_err(|e| e.to_string())?;
    // Load it before it replaces anything, so a grammar that doesn't work never does.
    open(g, &out)?;
    std::fs::write(out.join(REV), g.rev).map_err(|e| e.to_string())?;
    let dest = root.join(g.id);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&out, &dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// One commit of `g`'s repository into `dir`, without its history.
fn fetch(g: &Grammar, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let url = format!("https://github.com/{}", g.repo);
    let steps: [&[&str]; 3] = [
        &["init", "-q"],
        // A stalled connection fails after 30 seconds rather than hanging the install.
        &["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=30", "fetch", "-q", "--depth", "1", &url, g.rev],
        &["-c", "advice.detachedHead=false", "checkout", "-q", "FETCH_HEAD"],
    ];
    for args in steps {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| format!("can't run git: {e}"))?;
        if !out.status.success() {
            return Err(format!("Couldn't fetch {} from {url}: {}", g.title, String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    Ok(())
}

/// `g`'s parser (and its external scanner, if it has one) as a dynamic library at `out`.
fn compile(g: &Grammar, grammar: &Path, out: &Path) -> Result<(), String> {
    let src = grammar.join("src");
    let mut c = Command::new("cc");
    c.args(["-shared", "-fPIC", "-O2", "-w", "-std=c11", "-I"]).arg(&src).arg(src.join("parser.c"));
    if src.join("scanner.c").exists() {
        c.arg(src.join("scanner.c"));
    }
    let o = c.arg("-o").arg(out).output().map_err(|e| format!("Building {} needs a C compiler: run xcode-select --install ({e})", g.title))?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        if err.contains("xcode-select") || err.contains("developer tools") {
            return Err(format!("Building {} needs Xcode's command line tools: run xcode-select --install", g.title));
        }
        let first = err.lines().find(|l| l.contains("error")).unwrap_or(err.trim());
        return Err(format!("Couldn't build {}: {first}", g.title));
    }
    Ok(())
}

/// A query file of `g`'s from its checkout at `src`: beside the grammar, at the repository's
/// root, or in a folder named for the language, as repositories variously keep them.
fn query(g: &Grammar, src: &Path, name: &str) -> Option<String> {
    let lang = g.id.split('_').next().unwrap_or(g.id);
    [src.join(g.dir).join("queries").join(name), src.join("queries").join(name), src.join("queries").join(lang).join(name)]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
}

/// Load an installed grammar.
pub fn load(root: &Path, g: &Grammar) -> Result<HighlightConfiguration, String> {
    open(g, &root.join(g.id))
}

fn open(g: &Grammar, dir: &Path) -> Result<HighlightConfiguration, String> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name));
    let symbol = read(SYMBOL).map_err(|e| format!("{} is incomplete: {e}", g.title))?;
    let highlights = read(HIGHLIGHTS).map_err(|e| format!("{} is incomplete: {e}", g.title))?;
    let locals = read(LOCALS).unwrap_or_default();
    // SAFETY: the library is a tree-sitter parser this module compiled; loading runs no code
    // of its own (a parser has no initialisers), and it is never unloaded, so the language
    // it returns stays valid.
    let language = unsafe {
        let lib = libloading::Library::new(dir.join(LIBRARY)).map_err(|e| format!("Couldn't load {}: {e}", g.title))?;
        let f: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            lib.get(symbol.trim().as_bytes()).map_err(|e| format!("Couldn't load {}: {e}", g.title))?;
        let language = tree_sitter::Language::new(tree_sitter_language::LanguageFn::from_raw(*f));
        std::mem::forget(lib);
        language
    };
    let mut config = HighlightConfiguration::new(language, g.id, &highlights, "", &locals).map_err(|e| format!("{}'s highlight queries don't fit it: {e}", g.title))?;
    config.configure(&CAPTURES.map(|(name, _)| name));
    Ok(config)
}

/// Remove `g` from `root`. A library already loaded stays loaded until the app quits.
pub fn remove(root: &Path, g: &Grammar) -> Result<(), String> {
    let dir = root.join(g.id);
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", dir.display())),
        _ => Ok(()),
    }
}

/// The capture names grammars' queries use (tree-sitter's, Neovim's and Helix's), and the kind
/// of token each colours as. A capture takes the longest name whose parts it has
/// (`function.method.call` is `function`); one not listed stays plain, as `variable` does.
const CAPTURES: [(&str, Syntax); 41] = [
    ("keyword", Syntax::Keyword),
    ("conditional", Syntax::Keyword),
    ("repeat", Syntax::Keyword),
    ("include", Syntax::Keyword),
    ("exception", Syntax::Keyword),
    ("storageclass", Syntax::Keyword),
    ("tag", Syntax::Keyword),
    ("variable.builtin", Syntax::Keyword),
    ("markup.heading", Syntax::Keyword),
    ("text.title", Syntax::Keyword),
    ("string", Syntax::String),
    ("character", Syntax::String),
    ("markup.raw", Syntax::String),
    ("text.literal", Syntax::String),
    ("number", Syntax::Number),
    ("float", Syntax::Number),
    ("boolean", Syntax::Number),
    ("constant", Syntax::Number),
    ("comment", Syntax::Comment),
    ("type", Syntax::Type),
    ("constructor", Syntax::Type),
    ("namespace", Syntax::Type),
    ("module", Syntax::Type),
    ("function", Syntax::Function),
    ("method", Syntax::Function),
    ("markup.link", Syntax::Function),
    ("text.uri", Syntax::Function),
    ("property", Syntax::Property),
    ("field", Syntax::Property),
    ("attribute", Syntax::Property),
    ("label", Syntax::Property),
    ("variable.member", Syntax::Property),
    ("variable.other.member", Syntax::Property),
    ("punctuation", Syntax::Punctuation),
    ("operator", Syntax::Punctuation),
    ("delimiter", Syntax::Punctuation),
    ("variable.parameter", Syntax::Parameter),
    ("parameter", Syntax::Parameter),
    ("markup.strong", Syntax::Keyword),
    ("markup.italic", Syntax::Keyword),
    ("text.emphasis", Syntax::Keyword),
];

thread_local! {
    static HIGHLIGHTER: RefCell<tree_sitter_highlight::Highlighter> = RefCell::new(tree_sitter_highlight::Highlighter::new());
}

/// `code`'s highlights by a loaded grammar. A grammar that fails partway (it was checked when
/// it loaded, so only a cancelled parse can) leaves the rest plain.
pub fn colour(config: &HighlightConfiguration, code: &str) -> Vec<rune_kit::syntax::Highlight> {
    HIGHLIGHTER.with_borrow_mut(|h| {
        let mut out = Vec::new();
        let Ok(events) = h.highlight(config, code.as_bytes(), None, None, |_| None) else { return out };
        let mut open: Vec<usize> = Vec::new();
        for e in events {
            match e {
                Ok(HighlightEvent::HighlightStart(k)) => open.push(k.0),
                Ok(HighlightEvent::HighlightEnd) => {
                    open.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    if let Some(&k) = open.last() {
                        out.push((start..end, Paint::Kind(CAPTURES[k].1)));
                    }
                }
                Err(_) => break,
            }
        }
        out
    })
}

/// How a grammar stands, for the settings.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Neither installed nor wanted yet.
    Available,
    Installed,
    /// Installed from a commit this version doesn't pin.
    Outdated,
    Working,
    Failed(String),
}

/// The UI thread's view of the grammars: loaded ones colour the diff.
pub struct Languages {
    pub loaded: HashMap<&'static str, Arc<Config>>,
    /// Being installed or loaded off the UI thread.
    pub working: Vec<&'static str>,
    /// Why a grammar couldn't be installed or loaded this session; not retried until asked.
    pub failed: HashMap<&'static str, String>,
    /// What's on disk, per grammar: true when it's built from the pinned commit. Read once
    /// and after each change rather than by every frame of the settings.
    on_disk: HashMap<&'static str, bool>,
}

impl Languages {
    pub fn new(root: Option<&Path>) -> Languages {
        let mut l = Languages { loaded: HashMap::new(), working: Vec::new(), failed: HashMap::new(), on_disk: HashMap::new() };
        l.rescan(root);
        l
    }

    pub fn rescan(&mut self, root: Option<&Path>) {
        self.on_disk.clear();
        let Some(root) = root else { return };
        for g in CATALOGUE {
            if root.join(g.id).join(REV).exists() {
                self.on_disk.insert(g.id, installed(root, g));
            }
        }
    }

    pub fn status(&self, g: &Grammar) -> Status {
        if self.working.contains(&g.id) {
            return Status::Working;
        }
        if let Some(e) = self.failed.get(g.id) {
            return Status::Failed(e.clone());
        }
        match self.on_disk.get(g.id) {
            Some(true) => Status::Installed,
            Some(false) => Status::Outdated,
            None => Status::Available,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_find_their_grammar_but_built_in_languages_dont() {
        assert_eq!(for_language("prisma").map(|g| g.id), Some("prisma"));
        assert_eq!(for_language("PRISMA").map(|g| g.id), Some("prisma"));
        assert_eq!(for_language("hpp").map(|g| g.id), Some("cpp"));
        assert_eq!(for_language("Dockerfile").map(|g| g.id), Some("dockerfile"));
        assert_eq!(for_language("CMakeLists.txt").map(|g| g.id), Some("cmake"));
        assert!(for_language("rs").is_none());
        assert!(for_language("ts").is_none());
        assert!(for_language("txt").is_none());
    }

    #[test]
    fn the_catalogue_is_sorted_with_unique_ids_and_known_bases() {
        let titles: Vec<String> = CATALOGUE.iter().map(|g| g.title.to_lowercase()).collect();
        assert!(titles.is_sorted(), "{titles:?}");
        for (i, g) in CATALOGUE.iter().enumerate() {
            assert!(CATALOGUE[..i].iter().all(|o| o.id != g.id), "{}", g.id);
            assert!(g.rev.len() == 40, "{}", g.id);
            assert!(g.base.is_none_or(|b| by_id(b).is_some()), "{}", g.id);
            for f in g.files {
                assert!(!rune_kit::syntax::known(f), "{} claims {f}, which the built-in scanners colour", g.id);
            }
        }
    }

    /// Fetches and builds every grammar, so it needs the network and a C compiler:
    /// `cargo test --release every_grammar -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn every_grammar_builds_and_colours() {
        let root = std::env::temp_dir().join(format!("collate-grammars-{}", std::process::id()));
        let mut bad = Vec::new();
        for g in CATALOGUE {
            let t = std::time::Instant::now();
            match prepare(&root, g) {
                Ok(config) => {
                    let built = t.elapsed();
                    // A diff's block is 128 lines, coloured on the UI thread when it scrolls in.
                    let block = "x = 1 // a\nfn main() { \"s\" }\n".repeat(64);
                    let t = std::time::Instant::now();
                    let n = colour(&config, &block).len();
                    println!("{:<16} built in {:>4.1} s, a block in {:>5.2} ms, {n} highlights", g.id, built.as_secs_f32(), t.elapsed().as_secs_f64() * 1e3);
                    assert!(installed(&root, g));
                }
                Err(e) => {
                    println!("{:<16} FAILED {e}", g.id);
                    bad.push(g.id);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(bad.is_empty(), "{bad:?}");
    }
}

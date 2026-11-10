//! Collate's settings: what they are, their defaults, and a small `key = value` file under
//! `~/Library/Application Support/Collate/settings.conf`. Unknown keys and bad values are
//! skipped, so a file from a newer or older version still loads.

use std::path::PathBuf;

use crate::diff::{DiffMode, Style};
use crate::git::Scope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    System,
    Light,
    Dark,
}

/// How much room each line gets, as a multiple of the font size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Compact,
    Comfortable,
    Spacious,
}

impl Density {
    fn factor(self) -> f32 {
        match self {
            Density::Compact => 1.4,
            Density::Comfortable => 1.6,
            Density::Spacious => 1.85,
        }
    }
}

/// How fast animations run. Reduced keeps every animation, only shorter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Default,
    Reduced,
}

impl Motion {
    /// The window's animation clock rate: Reduced runs it faster, so each spring and fade
    /// finishes sooner but keeps its shape. Measured by `bench_motion`.
    pub fn speed(self) -> f32 {
        match self {
            Motion::Default => 1.0,
            Motion::Reduced => REDUCED_SPEED,
        }
    }
}

/// Reduced motion's clock rate.
const REDUCED_SPEED: f32 = 2.0;

/// Where "Open" sends a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Editor {
    /// The app macOS opens the file type with.
    System,
    Zed,
    VsCode,
    Cursor,
}

impl Editor {
    pub const ALL: [Editor; 4] = [Editor::System, Editor::Zed, Editor::VsCode, Editor::Cursor];

    /// The application `open -a` takes; `None` for the system default.
    pub fn app(self) -> Option<&'static str> {
        match self {
            Editor::System => None,
            Editor::Zed => Some("Zed"),
            Editor::VsCode => Some("Visual Studio Code"),
            Editor::Cursor => Some("Cursor"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Editor::System => "system",
            Editor::Zed => "zed",
            Editor::VsCode => "vscode",
            Editor::Cursor => "cursor",
        }
    }
}

/// The coding agent Collate hands work to, in a terminal under the diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
    Cursor,
}

impl Agent {
    pub const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Gemini, Agent::Cursor];

    /// The command that starts it in a session, its prompt the argument that follows.
    pub fn command(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini -i",
            Agent::Cursor => "cursor-agent",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Gemini => "Gemini",
            Agent::Cursor => "Cursor",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Cursor => "cursor",
        }
    }
}

/// Where updates come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The newest GitHub release, downloaded ready-built.
    Release,
    /// The newest commit on the main branch, built here (needs Rust).
    Source,
}

pub const FONT_SIZES: [f32; 5] = [11.0, 12.0, 13.0, 14.0, 16.0];
/// Context lines to keep; the last is the whole file.
pub const CONTEXT_LINES: [u32; 4] = [3, 10, 25, FULL_FILE];
pub const FULL_FILE: u32 = 1_000_000;
/// Changed lines past which a file starts folded; 0 never folds.
pub const FOLD_LARGE: [u32; 4] = [0, 300, 1000, 3000];
pub const REFRESH_MS: [u64; 4] = [500, 1000, 2000, 5000];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub theme: Theme,
    pub font_size: f32,
    pub density: Density,
    pub motion: Motion,
    pub layout: DiffMode,
    pub wrap: bool,
    /// One line-number column in unified view instead of old and new.
    pub one_number: bool,
    pub markers: bool,
    pub word_diff: bool,
    pub syntax: bool,
    /// Fetch and build a grammar the first time a diff shows a language without one.
    pub download_languages: bool,
    pub context: u32,
    pub default_scope: Scope,
    /// Marking a file viewed folds it away.
    pub collapse_viewed: bool,
    /// Hide lockfiles, minified bundles and snapshots until asked.
    pub hide_low_signal: bool,
    /// Read source first, then tests, then lockfiles and generated files.
    pub source_first: bool,
    /// A desktop notification when someone asks for the reader's review.
    pub notify_reviews: bool,
    pub auto_refresh: bool,
    pub refresh_ms: u64,
    pub editor: Editor,
    pub agent: Agent,
    /// Compare lines without their whitespace, as `git diff -w`.
    pub ignore_whitespace: bool,
    /// Show lines that moved in their own colour instead of as removed and added.
    pub show_moved: bool,
    pub fold_large: u32,
    /// Look for a new version at launch and every few hours.
    pub check_updates: bool,
    /// Install a new version as soon as it's found.
    pub auto_update: bool,
    pub update_from: Channel,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            theme: Theme::System,
            font_size: 12.0,
            density: Density::Comfortable,
            motion: Motion::Default,
            layout: DiffMode::Unified,
            wrap: false,
            one_number: true,
            markers: true,
            word_diff: true,
            syntax: true,
            download_languages: true,
            context: 3,
            default_scope: Scope::Uncommitted,
            collapse_viewed: true,
            hide_low_signal: true,
            source_first: true,
            notify_reviews: true,
            auto_refresh: true,
            refresh_ms: 1000,
            editor: Editor::System,
            agent: Agent::Claude,
            ignore_whitespace: false,
            show_moved: true,
            fold_large: 1000,
            check_updates: true,
            auto_update: true,
            update_from: Channel::Release,
        }
    }
}

impl Settings {
    pub fn style(&self) -> Style {
        Style {
            font_size: self.font_size,
            line_h: (self.font_size * self.density.factor()).round(),
            one_number: self.one_number,
            markers: self.markers,
            word_diff: self.word_diff,
        }
    }

    pub fn dark(&self, system_dark: bool) -> bool {
        match self.theme {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }

    fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/Collate/settings.conf"))
    }

    pub fn load() -> Settings {
        let text = Settings::path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Settings::parse(&text)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Settings::path().ok_or("HOME is not set")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, self.serialize()).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        let bool = |v: &str| match v {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "theme" => {
                    s.theme = match v {
                        "light" => Theme::Light,
                        "dark" => Theme::Dark,
                        _ => Theme::System,
                    }
                }
                "font_size" => {
                    if let Some(n) = v.parse::<f32>().ok().filter(|n| FONT_SIZES.contains(n)) {
                        s.font_size = n;
                    }
                }
                "density" => {
                    s.density = match v {
                        "compact" => Density::Compact,
                        "spacious" => Density::Spacious,
                        _ => Density::Comfortable,
                    }
                }
                "update_from" => s.update_from = if v == "source" { Channel::Source } else { Channel::Release },
                "motion" => s.motion = if v == "reduced" { Motion::Reduced } else { Motion::Default },
                "layout" => s.layout = if v == "split" { DiffMode::Split } else { DiffMode::Unified },
                "default_scope" => {
                    s.default_scope = match v {
                        "staged" => Scope::Staged,
                        "unstaged" => Scope::Unstaged,
                        "branch" => Scope::Branch,
                        "history" => Scope::History,
                        _ => Scope::Uncommitted,
                    }
                }
                "editor" => {
                    if let Some(e) = Editor::ALL.into_iter().find(|e| e.name() == v) {
                        s.editor = e;
                    }
                }
                "agent" => {
                    if let Some(a) = Agent::ALL.into_iter().find(|a| a.name() == v) {
                        s.agent = a;
                    }
                }
                "context" => {
                    if let Some(n) = v.parse::<u32>().ok().filter(|n| CONTEXT_LINES.contains(n)) {
                        s.context = n;
                    }
                }
                "fold_large" => {
                    if let Some(n) = v.parse::<u32>().ok().filter(|n| FOLD_LARGE.contains(n)) {
                        s.fold_large = n;
                    }
                }
                "refresh_ms" => {
                    if let Some(n) = v.parse::<u64>().ok().filter(|n| REFRESH_MS.contains(n)) {
                        s.refresh_ms = n;
                    }
                }
                _ => {
                    let Some(b) = bool(v) else { continue };
                    match k {
                        "wrap" => s.wrap = b,
                        "one_number" => s.one_number = b,
                        "markers" => s.markers = b,
                        "word_diff" => s.word_diff = b,
                        "syntax" => s.syntax = b,
                        "download_languages" => s.download_languages = b,
                        "collapse_viewed" => s.collapse_viewed = b,
                        "hide_low_signal" => s.hide_low_signal = b,
                        "source_first" => s.source_first = b,
                        "notify_reviews" => s.notify_reviews = b,
                        "auto_refresh" => s.auto_refresh = b,
                        "ignore_whitespace" => s.ignore_whitespace = b,
                        "show_moved" => s.show_moved = b,
                        "check_updates" => s.check_updates = b,
                        "auto_update" => s.auto_update = b,
                        _ => {}
                    }
                }
            }
        }
        s
    }

    fn serialize(&self) -> String {
        let theme = match self.theme {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        };
        let density = match self.density {
            Density::Compact => "compact",
            Density::Comfortable => "comfortable",
            Density::Spacious => "spacious",
        };
        let update_from = if self.update_from == Channel::Source { "source" } else { "release" };
        let motion = if self.motion == Motion::Reduced { "reduced" } else { "default" };
        let layout = if self.layout == DiffMode::Split { "split" } else { "unified" };
        let scope = match self.default_scope {
            Scope::Uncommitted => "all",
            Scope::Staged => "staged",
            Scope::Unstaged => "unstaged",
            Scope::Branch => "branch",
            Scope::History => "history",
        };
        format!(
            "theme = {theme}\nfont_size = {}\ndensity = {density}\nmotion = {motion}\nlayout = {layout}\nwrap = {}\none_number = {}\nmarkers = {}\nword_diff = {}\nsyntax = {}\ndownload_languages = {}\ncontext = {}\ndefault_scope = {scope}\ncollapse_viewed = {}\nhide_low_signal = {}\nsource_first = {}\nnotify_reviews = {}\nauto_refresh = {}\nrefresh_ms = {}\neditor = {}\nagent = {}\nignore_whitespace = {}\nshow_moved = {}\nfold_large = {}\ncheck_updates = {}\nauto_update = {}\nupdate_from = {update_from}\n",
            self.font_size,
            self.wrap,
            self.one_number,
            self.markers,
            self.word_diff,
            self.syntax,
            self.download_languages,
            self.context,
            self.collapse_viewed,
            self.hide_low_signal,
            self.source_first,
            self.notify_reviews,
            self.auto_refresh,
            self.refresh_ms,
            self.editor.name(),
            self.agent.name(),
            self.ignore_whitespace,
            self.show_moved,
            self.fold_large,
            self.check_updates,
            self.auto_update,
        )
    }
}

/// Files a review usually skips: lockfiles, minified or generated bundles, snapshots.
pub fn low_signal(path: &str) -> bool {
    const NAMES: [&str; 12] = [
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "Gemfile.lock",
        "poetry.lock",
        "composer.lock",
        "go.sum",
        "Podfile.lock",
        "uv.lock",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    NAMES.contains(&name) || [".min.js", ".min.css", ".map", ".snap"].iter().any(|e| name.ends_with(e))
}

/// Where a file comes in a review read in order: 0 for source, 1 for tests, 2 for what a
/// review usually skips. Tests are told by their folder or their name's convention.
pub fn review_rank(path: &str, generated: bool) -> u8 {
    if generated || low_signal(path) {
        return 2;
    }
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = name.split('.').next().unwrap_or(name);
    let in_tests = dirs.split('/').any(|d| matches!(d, "test" | "tests" | "__tests__" | "spec" | "specs" | "testdata"));
    let named = stem.starts_with("test_")
        || [".test.", ".spec.", "_test.", "_tests.", "_spec."].iter().any(|t| name.contains(t))
        || ["Test", "Tests", "Spec"].iter().any(|t| stem.len() > t.len() && stem.ends_with(t));
    u8::from(in_tests || named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let s = Settings {
            theme: Theme::Dark,
            font_size: 14.0,
            density: Density::Spacious,
            motion: Motion::Reduced,
            layout: DiffMode::Split,
            wrap: true,
            one_number: false,
            markers: false,
            word_diff: false,
            syntax: false,
            download_languages: false,
            context: FULL_FILE,
            default_scope: Scope::Branch,
            collapse_viewed: false,
            hide_low_signal: false,
            source_first: false,
            notify_reviews: false,
            auto_refresh: false,
            refresh_ms: 5000,
            editor: Editor::Cursor,
            agent: Agent::Gemini,
            ignore_whitespace: true,
            show_moved: false,
            fold_large: 300,
            check_updates: false,
            auto_update: false,
            update_from: Channel::Source,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn bad_values_keep_the_defaults() {
        let s = Settings::parse("font_size = 99\ncontext = abc\nwrap = maybe\nnonsense\nunknown = 1\n");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn source_comes_before_tests_and_lockfiles_last() {
        for p in ["src/main.rs", "src/latest.rs", "lib/contest.py", "app/Testing.swift", "web/spec.md"] {
            assert_eq!(review_rank(p, false), 0, "{p}");
        }
        for p in ["tests/flow.rs", "src/view_tests.rs", "pkg/a_test.go", "test_api.py", "ui/Button.test.tsx", "ui/b.spec.ts", "src/__tests__/x.js", "Sources/AppTests.swift", "src/FooTest.java"] {
            assert_eq!(review_rank(p, false), 1, "{p}");
        }
        assert_eq!(review_rank("Cargo.lock", false), 2);
        assert_eq!(review_rank("tests/snapshots/a.snap", false), 2);
        assert_eq!(review_rank("src/schema.rs", true), 2, "generated, by .gitattributes");
    }

    #[test]
    fn lockfiles_and_bundles_are_low_signal() {
        assert!(low_signal("Cargo.lock"));
        assert!(low_signal("web/package-lock.json"));
        assert!(low_signal("dist/app.min.js"));
        assert!(!low_signal("src/lock.rs"));
    }
}

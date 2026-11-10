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
    pub auto_refresh: bool,
    pub refresh_ms: u64,
    pub editor: Editor,
    /// Compare lines without their whitespace, as `git diff -w`.
    pub ignore_whitespace: bool,
    /// Show lines that moved in their own colour instead of as removed and added.
    pub show_moved: bool,
    pub fold_large: u32,
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
            auto_refresh: true,
            refresh_ms: 1000,
            editor: Editor::System,
            ignore_whitespace: false,
            show_moved: true,
            fold_large: 1000,
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
                        "auto_refresh" => s.auto_refresh = b,
                        "ignore_whitespace" => s.ignore_whitespace = b,
                        "show_moved" => s.show_moved = b,
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
            "theme = {theme}\nfont_size = {}\ndensity = {density}\nmotion = {motion}\nlayout = {layout}\nwrap = {}\none_number = {}\nmarkers = {}\nword_diff = {}\nsyntax = {}\ndownload_languages = {}\ncontext = {}\ndefault_scope = {scope}\ncollapse_viewed = {}\nhide_low_signal = {}\nauto_refresh = {}\nrefresh_ms = {}\neditor = {}\nignore_whitespace = {}\nshow_moved = {}\nfold_large = {}\n",
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
            self.auto_refresh,
            self.refresh_ms,
            self.editor.name(),
            self.ignore_whitespace,
            self.show_moved,
            self.fold_large,
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
            auto_refresh: false,
            refresh_ms: 5000,
            editor: Editor::Cursor,
            ignore_whitespace: true,
            show_moved: false,
            fold_large: 300,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn bad_values_keep_the_defaults() {
        let s = Settings::parse("font_size = 99\ncontext = abc\nwrap = maybe\nnonsense\nunknown = 1\n");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn lockfiles_and_bundles_are_low_signal() {
        assert!(low_signal("Cargo.lock"));
        assert!(low_signal("web/package-lock.json"));
        assert!(low_signal("dist/app.min.js"));
        assert!(!low_signal("src/lock.rs"));
    }
}

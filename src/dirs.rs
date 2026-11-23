//! Where Collate keeps its files. On macOS everything but caches lives in
//! `~/Library/Application Support/Collate`; on Linux it follows the XDG base directories, so
//! settings go in `$XDG_CONFIG_HOME/collate`, the rest in `$XDG_DATA_HOME/collate`, caches in
//! `$XDG_CACHE_HOME/collate` and the control sockets in `$XDG_RUNTIME_DIR/collate`.

use std::path::PathBuf;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `$name` if it's set to an absolute path, as the XDG spec asks; else `~/<fallback>`.
#[cfg(not(target_os = "macos"))]
fn xdg(name: &str, fallback: &str) -> Option<PathBuf> {
    match std::env::var_os(name).map(PathBuf::from) {
        Some(p) if p.is_absolute() => Some(p),
        _ => Some(home()?.join(fallback)),
    }
}

/// The settings file's folder.
pub fn config() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return data();
    #[cfg(not(target_os = "macos"))]
    return Some(xdg("XDG_CONFIG_HOME", ".config")?.join("collate"));
}

/// Recent projects, favorites, viewed marks, comments and grammars.
pub fn data() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(home()?.join("Library/Application Support/Collate"));
    #[cfg(not(target_os = "macos"))]
    return Some(xdg("XDG_DATA_HOME", ".local/share")?.join("collate"));
}

/// Avatars and updates being installed: whatever can be fetched again.
pub fn cache() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(home()?.join("Library/Caches/Collate"));
    #[cfg(not(target_os = "macos"))]
    return Some(xdg("XDG_CACHE_HOME", ".cache")?.join("collate"));
}

/// The running windows' sockets, which `collate --mcp` looks through.
pub fn control() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(data()?.join("control"));
    // The runtime folder is the reader's alone and emptied at logout, as sockets should be.
    #[cfg(not(target_os = "macos"))]
    return match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(p) if p.is_absolute() => Some(p.join("collate")),
        _ => Some(data()?.join("control")),
    };
}

//! What Collate asks of the desktop: open a link or file, show a folder, open a file in an
//! editor, post a notification, start another window. `open` and AppleScript on macOS, the
//! freedesktop tools (`xdg-open`, `notify-send`) on Linux.

use std::path::Path;
use std::process::Command;

use crate::settings::Editor;

/// Open `target`, a link or a path, in the app the system picks for it.
pub fn open(target: &str) -> Result<(), String> {
    let tool = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(tool).arg(target).spawn().map(drop).map_err(|e| format!("Couldn't run {tool}: {e}"))
}

/// Show `path` in the file manager: selected in the Finder on macOS, its folder on Linux,
/// where no command selects a file in every file manager.
pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return Command::new("open").arg("-R").arg(path).spawn().map(drop).map_err(|e| e.to_string());
    #[cfg(not(target_os = "macos"))]
    return Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn().map(drop).map_err(|e| e.to_string());
}

/// `text` with the Mac named as Linux would name it: "this Mac" is "this computer" there, and
/// "macOS" "the desktop".
pub fn local(text: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(target_os = "macos") || !text.contains("Mac") {
        return text.into();
    }
    text.replace("this Mac", "this computer").replace("macOS", "the desktop").into()
}

/// What the file manager is called in a menu.
pub const FILE_MANAGER: &str = if cfg!(target_os = "macos") { "Finder" } else { "Files" };

/// Open `path` in `editor`. With the system's choice, a `text` file opens in the text editor
/// on macOS, whatever its name, and any other in the app its type opens with.
pub fn edit(editor: Editor, path: &Path, text: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        match editor.app() {
            Some(app) => cmd.args(["-a", app]),
            None if text => cmd.arg("-t"),
            None => &mut cmd,
        };
        match cmd.arg(path).status() {
            Ok(st) if st.success() => Ok(()),
            Ok(_) => Err(format!("Couldn't open {} in {}", path.display(), editor.app().unwrap_or("the default app"))),
            Err(e) => Err(format!("Couldn't run open: {e}")),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        // Editors' command-line tools hand the file to a running window and return, so spawn
        // rather than wait: a missing tool is the error worth reporting.
        let tool = editor.command().unwrap_or("xdg-open");
        Command::new(tool).arg(path).spawn().map(drop).map_err(|e| format!("Couldn't run {tool}: {e}"))
    }
}

/// A notification: through AppleScript on macOS, with no permission to ask for and nothing to
/// bundle; `notify-send` on Linux.
pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let quote = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = Command::new("notify-send").args(["--app-name=Collate", title, body]).spawn();
    }
}

/// Start a new Collate window on `root`, apart from this process.
pub fn launch(root: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open").args(["-n", "-a", "Collate", "--args"]).arg(root).status().map_err(|e| format!("Couldn't run open: {e}"))?;
        if !status.success() {
            return Err("open couldn't start Collate: is Collate.app installed?".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let exe = std::env::current_exe().map_err(|e| format!("Couldn't find Collate's own binary: {e}"))?;
        // Its own process group, so the agent's session ending doesn't take the window with it.
        Command::new(&exe).arg(root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn()
            .map(drop)
            .map_err(|e| format!("Couldn't start {}: {e}", exe.display()))
    }
}

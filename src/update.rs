//! Updates: Collate looks for a newer version on GitHub, either the newest release or the
//! newest commit on the main branch, and installs it in place of the running app (Collate.app
//! on macOS, the `collate` binary on Linux). The running copy keeps going; the new one opens
//! on the next launch, or at once with Restart.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::settings::Channel;
use crate::{Collate, HANDLE};

const REPO: &str = "Dishant1804/Collate";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The commit this build came from (build.rs), empty when it wasn't built from a checkout.
const COMMIT: &str = env!("COLLATE_COMMIT");

/// The first check waits for the window to settle, then they repeat this often.
const FIRST_AFTER: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// A version newer than the running one.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    Release { version: String, zip: String, sums: String, name: String },
    Source { commit: String },
}

impl Update {
    /// How the settings and notices name it: "1.2.0", or a commit's short hash.
    pub fn label(&self) -> String {
        match self {
            Update::Release { version, .. } => version.clone(),
            Update::Source { commit } => format!("the latest source ({})", &commit[..commit.len().min(7)]),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Current,
    /// Newer, waiting for Install (installing automatically is off, or the app can't).
    Available(Update),
    Installing(Update),
    /// Installed; it runs from the next launch.
    Ready(Update),
    Failed(String),
}

impl Status {
    pub fn busy(&self) -> bool {
        matches!(self, Status::Checking | Status::Installing(_))
    }
}

/// What the settings show for the running version.
pub fn running() -> String {
    if COMMIT.is_empty() { format!("Collate {VERSION}") } else { format!("Collate {VERSION} ({})", &COMMIT[..COMMIT.len().min(7)]) }
}

/// Check now and then while the app runs, if the reader lets it.
pub fn watch() {
    let Some(app) = HANDLE.get().cloned() else { return };
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_AFTER);
        loop {
            app.update(|s, _| {
                if s.settings.check_updates {
                    s.check_update();
                }
            });
            std::thread::sleep(EVERY);
        }
    });
}

impl Collate {
    /// Look for a newer version off the UI thread, and install it if the reader lets Collate.
    pub fn check_update(&mut self) {
        let Some(app) = HANDLE.get().cloned() else { return };
        if self.update.busy() || matches!(self.update, Status::Ready(_)) {
            return;
        }
        self.update = Status::Checking;
        let channel = self.settings.update_from;
        std::thread::spawn(move || {
            let found = newest(channel);
            app.update(move |s, _| {
                s.update = match found {
                    Ok(None) => Status::Current,
                    Ok(Some(u)) => Status::Available(u),
                    Err(e) => Status::Failed(e),
                };
                if s.settings.auto_update && installed().is_some() {
                    s.install_update();
                }
            });
        });
    }

    /// Install the update found, off the UI thread.
    pub fn install_update(&mut self) {
        let (Status::Available(u), Some(app)) = (self.update.clone(), HANDLE.get().cloned()) else { return };
        self.update = Status::Installing(u.clone());
        std::thread::spawn(move || {
            let done = install(&u);
            app.update(move |s, _| match done {
                Ok(()) => {
                    s.say(format!("Installed {}; it opens when Collate restarts", u.label()), false);
                    s.update = Status::Ready(u);
                }
                Err(e) => {
                    s.say(format!("Couldn't update: {e}"), true);
                    s.update = Status::Failed(e);
                }
            });
        });
    }

    /// Quit and open the installed app once this one has gone.
    pub fn restart(&mut self, tx: &mut rune::Tx) {
        let Some(app) = installed() else { return };
        let start = if cfg!(target_os = "macos") { "open \"$2\"" } else { "\"$2\" >/dev/null 2>&1 &" };
        let wait = format!("while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; {start}");
        match Command::new("/bin/sh").args(["-c", &wait, "sh", &std::process::id().to_string()]).arg(&app).spawn() {
            Ok(_) => tx.quit(),
            Err(e) => self.say(format!("Couldn't restart: {e}"), true),
        }
    }
}

/// The newest version on `channel` if it's newer than this one.
fn newest(channel: Channel) -> Result<Option<Update>, String> {
    match channel {
        Channel::Release => {
            let json = run(CURL, &["-fsSL", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: Collate", &format!("https://api.github.com/repos/{REPO}/releases/latest")], None)
                .map_err(|e| format!("couldn't ask GitHub for the newest release: {e}"))?;
            release(&json, VERSION)
        }
        Channel::Source => {
            let out = run("git", &["ls-remote", &format!("https://github.com/{REPO}.git"), "HEAD"], None).map_err(|e| format!("couldn't read the latest commit: {e}"))?;
            let commit = out.split_whitespace().next().ok_or("GitHub sent no commit for HEAD")?.to_owned();
            Ok((commit != COMMIT).then_some(Update::Source { commit }))
        }
    }
}

/// The release in GitHub's `json` if it's newer than `current`, with its app's download.
fn release(json: &str, current: &str) -> Result<Option<Update>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("GitHub's release isn't JSON: {e}"))?;
    let tag = v["tag_name"].as_str().ok_or("the release has no tag")?;
    let version = tag.trim_start_matches('v').to_owned();
    if !newer(&version, current) {
        return Ok(None);
    }
    let assets = v["assets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let url = |pick: &dyn Fn(&str) -> bool| assets.iter().find(|a| a["name"].as_str().is_some_and(pick)).and_then(|a| Some((a["name"].as_str()?.to_owned(), a["browser_download_url"].as_str()?.to_owned())));
    let (Some((name, zip)), Some((_, sums))) = (url(&|n| n.ends_with(&asset_suffix())), url(&|n| n == "SHA256SUMS")) else {
        return Err(format!("release {version} has no app to download; update from the latest source instead"));
    };
    Ok(Some(Update::Release { version, zip, sums, name }))
}

/// Whether dotted version `a` is past `b`.
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) > parts(b)
}

/// curl, by full path on macOS for an app opened without the shell's PATH.
const CURL: &str = if cfg!(target_os = "macos") { "/usr/bin/curl" } else { "curl" };

/// How this platform's release download is named: the app on macOS, the tarball for this
/// architecture on Linux.
fn asset_suffix() -> String {
    if cfg!(target_os = "macos") { "-macos-universal.zip".into() } else { format!("-linux-{}.tar.gz", std::env::consts::ARCH) }
}

/// What an update replaces: the Collate.app this process runs from on macOS, the `collate`
/// binary on Linux when it's installed in a `bin` folder; `None` from `cargo run`.
pub fn installed() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        return exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf);
    }
    exe.parent().is_some_and(|d| d.file_name().is_some_and(|n| n == "bin")).then_some(exe)
}

/// Download or build `u` and put it in place of the running app.
fn install(u: &Update) -> Result<(), String> {
    let app = installed().ok_or("Collate isn't running from its installed app, so there's nothing to replace")?;
    let cache = crate::dirs::cache().ok_or("HOME is not set")?;
    let work = cache.join("update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let built = match u {
        Update::Release { zip, sums, name, .. } => download(&work, zip, sums, name),
        Update::Source { .. } => build(&cache.join("source"), &work),
    };
    let done = built.and_then(|new| replace(&app, &new));
    let _ = std::fs::remove_dir_all(&work);
    done
}

/// The release's app (or binary), checked against its SHA-256 sum before it's unpacked.
fn download(work: &Path, zip: &str, sums: &str, name: &str) -> Result<PathBuf, String> {
    let file = work.join(name);
    let file_s = file.to_string_lossy();
    run(CURL, &["-fsSL", "-o", &file_s, zip], None).map_err(|e| format!("couldn't download {name}: {e}"))?;
    let list = run(CURL, &["-fsSL", sums], None).map_err(|e| format!("couldn't download SHA256SUMS: {e}"))?;
    let want = list.lines().filter_map(|l| l.split_once(char::is_whitespace)).find(|(_, n)| n.trim() == name).map(|(sum, _)| sum.to_owned());
    let got = if cfg!(target_os = "macos") { run("/usr/bin/shasum", &["-a", "256", &file_s], None)? } else { run("sha256sum", &[&file_s], None)? };
    if want.as_deref() != got.split_whitespace().next() {
        return Err(format!("{name} doesn't match its sum in SHA256SUMS"));
    }
    if cfg!(target_os = "macos") {
        run("/usr/bin/ditto", &["-x", "-k", &file_s, &work.to_string_lossy()], None)?;
        return Ok(work.join("Collate.app"));
    }
    run("tar", &["-xzf", &file_s, "-C", &work.to_string_lossy()], None)?;
    Ok(work.join(name.trim_end_matches(".tar.gz")).join("bin/collate"))
}

/// Pull the latest source into `source` and build it (and on macOS, bundle it) into `work`.
fn build(source: &Path, work: &Path) -> Result<PathBuf, String> {
    // An app opened from the Dock doesn't get the shell's PATH, so look where rustup puts cargo.
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.cargo/bin:{}", std::env::var("PATH").unwrap_or_default());
    if run("cargo", &["--version"], Some(&path)).is_err() {
        return Err("building needs Rust: install it from rustup.rs, or update from releases".into());
    }
    let src = source.to_string_lossy();
    if source.join(".git").exists() {
        run("git", &["-C", &src, "fetch", "--quiet", "origin"], None)?;
        run("git", &["-C", &src, "reset", "--quiet", "--hard", "origin/HEAD"], None)?;
    } else {
        std::fs::create_dir_all(source.parent().unwrap_or(source)).map_err(|e| e.to_string())?;
        run("git", &["clone", "--quiet", &format!("https://github.com/{REPO}.git"), &src], None)?;
    }
    run("cargo", &["build", "--release", "--manifest-path", &source.join("Cargo.toml").to_string_lossy()], Some(&path))?;
    let binary = source.join("target/release/collate");
    if !cfg!(target_os = "macos") {
        return Ok(binary);
    }
    run(&source.join("scripts/bundle.sh").to_string_lossy(), &[&binary.to_string_lossy(), &work.to_string_lossy()], Some(&path)).map_err(|e| format!("bundling failed: {e}"))?;
    Ok(work.join("Collate.app"))
}

/// Put `new` where `bundle` is, keeping the old one until the new one is in place.
#[cfg(target_os = "macos")]
fn replace(bundle: &Path, new: &Path) -> Result<(), String> {
    if !new.join("Contents/MacOS/collate").exists() {
        return Err(format!("{} isn't a complete Collate.app", new.display()));
    }
    let old = bundle.with_file_name(".Collate-previous.app");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(bundle, &old).map_err(|e| format!("couldn't move {} aside: {e}", bundle.display()))?;
    if let Err(e) = run("/usr/bin/ditto", &[&new.to_string_lossy(), &bundle.to_string_lossy()], None) {
        let _ = std::fs::remove_dir_all(bundle);
        if let Err(back) = std::fs::rename(&old, bundle) {
            return Err(format!("{e}; and the old app couldn't be put back ({back}): it is at {}", old.display()));
        }
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&old);
    // So Finder and the Dock see the new version and its icon.
    let lsregister = "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister";
    let _ = run(lsregister, &["-f", &bundle.to_string_lossy()], None);
    Ok(())
}

/// Put binary `new` where `binary` is: copied beside it, then renamed over it in one step, so
/// the running process keeps its old file and nothing ever sees half a binary.
#[cfg(not(target_os = "macos"))]
fn replace(binary: &Path, new: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if !new.is_file() {
        return Err(format!("{} isn't a Collate binary", new.display()));
    }
    let next = binary.with_file_name(".collate-next");
    let placed = std::fs::copy(new, &next)
        .and_then(|_| std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)))
        .and_then(|_| std::fs::rename(&next, binary));
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&next);
        return Err(format!("couldn't replace {}: {e}", binary.display()));
    }
    Ok(())
}

/// Run a command and return what it printed, or its error output when it fails.
fn run(cmd: &str, args: &[&str], path: Option<&str>) -> Result<String, String> {
    let mut c = Command::new(cmd);
    c.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(p) = path {
        c.env("PATH", p);
    }
    let out = c.output().map_err(|e| format!("can't run {cmd}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    Err(format!("{} failed: {}", Path::new(cmd).file_name().map_or(cmd.into(), |n| n.to_string_lossy()), last.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("1.10.0", "1.9.3"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(!newer("1.1.0", "1.1.0"));
        assert!(!newer("1.0.9", "1.1.0"));
    }

    #[test]
    fn a_newer_release_brings_this_platforms_app_and_sums() {
        let json = r#"{"tag_name":"v9.0.0","assets":[
            {"name":"Collate-9.0.0-macos-universal.zip","browser_download_url":"https://x/zip"},
            {"name":"collate-9.0.0-macos-universal.tar.gz","browser_download_url":"https://x/tar"},
            {"name":"collate-9.0.0-linux-x86_64.tar.gz","browser_download_url":"https://x/linux-x86_64"},
            {"name":"collate-9.0.0-linux-aarch64.tar.gz","browser_download_url":"https://x/linux-aarch64"},
            {"name":"SHA256SUMS","browser_download_url":"https://x/sums"}]}"#;
        let (name, zip) = if cfg!(target_os = "macos") {
            ("Collate-9.0.0-macos-universal.zip".to_owned(), "https://x/zip".to_owned())
        } else {
            let arch = std::env::consts::ARCH;
            (format!("collate-9.0.0-linux-{arch}.tar.gz"), format!("https://x/linux-{arch}"))
        };
        let u = release(json, "1.1.0").unwrap().unwrap();
        assert_eq!(u, Update::Release { version: "9.0.0".into(), zip, sums: "https://x/sums".into(), name });
        assert_eq!(release(json, "9.0.0").unwrap(), None);
    }

    /// Downloads the newest release and swaps it in for a stand-in app in the temp folder.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn the_newest_release_downloads_and_replaces_an_app() {
        let Update::Release { zip, sums, name, .. } = release(&run(CURL, &["-fsSL", "-H", "User-Agent: Collate", &format!("https://api.github.com/repos/{REPO}/releases/latest")], None).unwrap(), "0.0.0").unwrap().unwrap() else { unreachable!() };
        let dir = std::env::temp_dir().join(format!("collate-update-{}", std::process::id()));
        let (work, bundle) = (dir.join("work"), dir.join("Apps/Collate.app"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents/old"), "").unwrap();
        let new = download(&work, &zip, &sums, &name).unwrap();
        replace(&bundle, &new).unwrap();
        assert!(bundle.join("Contents/MacOS/collate").exists() && !bundle.join("Contents/old").exists());
        assert!(!bundle.with_file_name(".Collate-previous.app").exists(), "the old app is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_release_without_an_app_says_so() {
        let json = r#"{"tag_name":"v9.0.0","assets":[]}"#;
        assert!(release(json, "1.1.0").unwrap_err().contains("no app"));
    }
}

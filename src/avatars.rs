//! Commit authors' pictures, as Zed shows them: the GitHub avatar of the account behind an
//! email. Each is looked up once, off the UI thread, and kept in `~/Library/Caches/Collate/avatars`;
//! an author GitHub doesn't know keeps their initials.
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime};

use rune::{Image, ImageCache};

use crate::graph;

/// How long an email GitHub had no account for stays unasked.
const MISS_FOR: Duration = Duration::from_secs(24 * 60 * 60);
/// The size fetched: sharp at the side panel's 44 pt on a 2× screen.
const PIXELS: u32 = 96;

struct Job {
    email: String,
    /// A commit by this email, for the GitHub API to name its account.
    sha: String,
    /// `owner/repo` on github.com, when the project is there.
    repo: Option<String>,
}

/// The pictures known so far, by email. Without `start` (headless tests) it stays empty.
#[derive(Default)]
pub struct Avatars {
    cache: Option<ImageCache>,
    jobs: Option<Sender<Job>>,
    /// `None`: asked for, or there's none.
    images: HashMap<String, Option<Image>>,
}

impl Avatars {
    /// Looks pictures up on one worker; `deliver` hands each result back to the UI thread.
    pub fn start(&mut self, cache: ImageCache, deliver: impl Fn(String, Option<Vec<u8>>) + Send + 'static) {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in rx {
                let bytes = resolve(&job);
                deliver(job.email, bytes);
            }
        });
        self.cache = Some(cache);
        self.jobs = Some(tx);
    }

    /// Asks for the picture of each author in `commits` not asked for yet. `web` is the
    /// project's web address; only github.com ones can name an account from a commit.
    pub fn request(&mut self, web: Option<&str>, commits: &[graph::Commit]) {
        let Some(jobs) = &self.jobs else { return };
        let repo = web.and_then(|w| w.strip_prefix("https://github.com/")).map(str::to_owned);
        for c in commits {
            if c.id == graph::UNCOMMITTED || c.email.is_empty() || self.images.contains_key(&c.email) {
                continue;
            }
            self.images.insert(c.email.clone(), None);
            let _ = jobs.send(Job { email: c.email.clone(), sha: c.id.clone(), repo: repo.clone() });
        }
    }

    pub fn landed(&mut self, email: String, bytes: Option<Vec<u8>>) {
        let image = bytes.zip(self.cache.as_ref()).map(|(b, cache)| cache.load(b));
        self.images.insert(email, image);
    }

    /// The picture for `email`, once it's decoded.
    pub fn get(&self, email: &str) -> Option<&Image> {
        self.images.get(email)?.as_ref().filter(|i| i.is_ready() && i.error().is_none())
    }
}

/// The picture's bytes: from the disk cache, else looked up and saved. A failed lookup (no
/// network, rate limited) saves nothing, so a later session asks again.
fn resolve(job: &Job) -> Option<Vec<u8>> {
    let file = cache_file(&job.email)?;
    if let Ok(meta) = std::fs::metadata(&file) {
        if meta.len() > 0 {
            return std::fs::read(&file).ok();
        }
        // An empty file: GitHub had no account for this email, recently.
        if meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age < MISS_FOR) {
            return None;
        }
    }
    let found = noreply_url(&job.email).map(Some).or_else(|| job.repo.as_deref().and_then(|r| commit_avatar(r, &job.sha)))?;
    let bytes = match found {
        Some(url) => {
            let out = Command::new("curl").args(["-sfL", "--max-time", "15", &url]).output().ok()?;
            if !out.status.success() || out.stdout.is_empty() {
                return None;
            }
            out.stdout
        }
        None => Vec::new(),
    };
    let _ = std::fs::create_dir_all(file.parent()?);
    let _ = std::fs::write(&file, &bytes);
    (!bytes.is_empty()).then_some(bytes)
}

/// `~/Library/Caches/Collate/avatars/<hash of the email>`.
fn cache_file(email: &str) -> Option<PathBuf> {
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let h = email.trim().to_lowercase().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    Some(PathBuf::from(std::env::var_os("HOME")?).join(format!("Library/Caches/Collate/avatars/{h:016x}")))
}

/// A GitHub no-reply address names its account outright: `123+name@users.noreply.github.com`
/// by id, the older `name@users.noreply.github.com` by name.
fn noreply_url(email: &str) -> Option<String> {
    let local = email.strip_suffix("@users.noreply.github.com")?;
    Some(match local.split_once('+') {
        Some((id, _)) if id.bytes().all(|b| b.is_ascii_digit()) => format!("https://avatars.githubusercontent.com/u/{id}?s={PIXELS}&v=4"),
        _ => format!("https://github.com/{local}.png?size={PIXELS}"),
    })
}

/// The avatar of the account GitHub links `sha` to: `Some(None)` when it links none, `None`
/// when GitHub couldn't be asked. `gh` is signed in, with a far higher rate limit; without it,
/// the public API.
fn commit_avatar(repo: &str, sha: &str) -> Option<Option<String>> {
    let path = format!("repos/{repo}/commits/{sha}");
    let url = match Command::new("gh").args(["api", &path, "--jq", ".author.avatar_url // \"\""]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(_) => return None,
        Err(_) => {
            let out = Command::new("curl").args(["-sf", "--max-time", "15", "-H", "Accept: application/vnd.github+json", &format!("https://api.github.com/{path}")]).output().ok()?;
            if !out.status.success() {
                return None;
            }
            let body = String::from_utf8_lossy(&out.stdout).replace(": ", ":");
            // The commit's own `author` holds no avatar, so the first is the account's, unless
            // GitHub linked no account (`"author":null`) and it's the committer's.
            if body.contains("\"author\":null") {
                String::new()
            } else {
                body.split_once("\"avatar_url\":\"").and_then(|(_, rest)| rest.split_once('"')).map(|(u, _)| u.to_owned())?
            }
        }
    };
    Some((!url.is_empty()).then(|| format!("{url}{}s={PIXELS}", if url.contains('?') { '&' } else { '?' })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noreply_addresses_name_their_account() {
        assert_eq!(noreply_url("123+octo@users.noreply.github.com").as_deref(), Some("https://avatars.githubusercontent.com/u/123?s=96&v=4"));
        assert_eq!(noreply_url("octo@users.noreply.github.com").as_deref(), Some("https://github.com/octo.png?size=96"));
        assert_eq!(noreply_url("octo@example.com"), None);
    }
}

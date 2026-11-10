//! Commit authors' pictures, as Zed shows them: the GitHub avatar of the account behind an
//! email. Each is looked up once, off the UI thread, and kept in `~/Library/Caches/Collate/avatars`;
//! an author GitHub doesn't know keeps their initials. The authors waiting are looked up
//! together, in one GraphQL query through `gh`, so a long history costs one request.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime};

use rune::{Image, ImageCache};
use serde_json::Value;

use crate::graph;

/// How long an email GitHub had no account for stays unasked.
const MISS_FOR: Duration = Duration::from_secs(24 * 60 * 60);
/// The size fetched: sharp at the side panel's 44 pt on a 2× screen.
const PIXELS: u32 = 96;
/// Pictures downloaded at once.
const DOWNLOADS: usize = 8;
/// Commits asked about in one query; each is one field, and GitHub's queries stay cheap well
/// past this.
const PER_QUERY: usize = 100;

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
    pub fn start(&mut self, cache: ImageCache, deliver: impl Fn(String, Option<Vec<u8>>) + Send + Sync + 'static) {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                // Every job waiting goes together, to ask GitHub about their commits at once.
                let jobs: Vec<Job> = std::iter::once(first).chain(rx.try_iter()).collect();
                resolve(jobs, &deliver);
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

    /// Decodes other pictures (a changed image in the diff) on the same cache; `None` without
    /// one (headless tests).
    pub fn load(&self, bytes: std::sync::Arc<[u8]>) -> Option<Image> {
        self.cache.as_ref().map(|c| c.load(bytes))
    }

    /// The picture for `email`, once it's decoded.
    pub fn get(&self, email: &str) -> Option<&Image> {
        self.images.get(email)?.as_ref().filter(|i| i.is_ready() && i.error().is_none())
    }
}

/// Each job's picture: from the disk cache, else looked up and saved. A failed lookup (no
/// network, rate limited) saves nothing, so a later session asks again.
fn resolve(jobs: Vec<Job>, deliver: &(impl Fn(String, Option<Vec<u8>>) + Sync)) {
    let mut ask: Vec<(Job, PathBuf)> = Vec::new();
    for job in jobs {
        match cache_file(&job.email) {
            Some(file) => match cached(&file) {
                Some(bytes) => deliver(job.email, bytes),
                None => ask.push((job, file)),
            },
            None => deliver(job.email, None),
        }
    }
    // Where each picture is: a no-reply address names its account, else GitHub names the one
    // behind the author's commit, a query per repository.
    let mut found: Vec<Option<Option<String>>> = ask.iter().map(|(j, _)| noreply_url(&j.email).map(Some)).collect();
    let mut repos: Vec<&str> = ask.iter().zip(&found).filter(|(_, f)| f.is_none()).filter_map(|((j, _), _)| j.repo.as_deref()).collect();
    repos.sort_unstable();
    repos.dedup();
    for repo in repos {
        let unknown: Vec<usize> = (0..ask.len()).filter(|&i| found[i].is_none() && ask[i].0.repo.as_deref() == Some(repo)).collect();
        for part in unknown.chunks(PER_QUERY) {
            let shas: Vec<&str> = part.iter().map(|&i| ask[i].0.sha.as_str()).collect();
            for (&i, url) in part.iter().zip(commit_avatars(repo, &shas)) {
                found[i] = url;
            }
        }
    }
    let lookups: Vec<_> = ask.into_iter().zip(found).collect();
    for part in lookups.chunks(DOWNLOADS) {
        std::thread::scope(|t| {
            for ((job, file), found) in part {
                t.spawn(move || deliver(job.email.clone(), found.as_ref().and_then(|url| save(file, url.as_deref()))));
            }
        });
    }
}

/// The cached picture: `Some(None)` when GitHub had no account for the email recently, `None`
/// when it's to be looked up.
fn cached(file: &Path) -> Option<Option<Vec<u8>>> {
    let meta = std::fs::metadata(file).ok()?;
    if meta.len() > 0 {
        return Some(std::fs::read(file).ok());
    }
    // An empty file: GitHub had no account for this email, recently.
    meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age < MISS_FOR).then_some(None)
}

/// Download the picture at `url` into `file`, or with no `url`, note there's none.
fn save(file: &Path, url: Option<&str>) -> Option<Vec<u8>> {
    let bytes = match url {
        Some(url) => {
            let out = Command::new("curl").args(["-sfL", "--max-time", "15", url]).output().ok()?;
            if !out.status.success() || out.stdout.is_empty() {
                return None;
            }
            out.stdout
        }
        None => Vec::new(),
    };
    let _ = std::fs::create_dir_all(file.parent()?);
    let _ = std::fs::write(file, &bytes);
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

/// The avatars of the accounts GitHub links each of `shas` to: `Some(None)` when it links none,
/// `None` when GitHub couldn't be asked or doesn't have the commit. With `gh`, signed in, one
/// GraphQL query for them all; without it, the public API, a request each, limited to 60 an
/// hour, so the first refusal stops the rest.
fn commit_avatars(repo: &str, shas: &[&str]) -> Vec<Option<Option<String>>> {
    let mut urls = vec![None; shas.len()];
    let Some((owner, name)) = repo.split_once('/') else { return urls };
    let Some(gh) = crate::github::find("gh") else {
        for (url, sha) in urls.iter_mut().zip(shas) {
            *url = public_avatar(repo, sha);
            if url.is_none() {
                break;
            }
        }
        return urls;
    };
    let fields: String = shas.iter().enumerate().map(|(i, sha)| format!("c{i}:object(oid:\"{sha}\"){{...on Commit{{author{{user{{avatarUrl(size:{PIXELS})}}}}}}}}")).collect();
    let query = format!("query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{fields}}}}}");
    let Ok(out) = Command::new(gh).args(["api", "graphql", "-f", &format!("owner={owner}"), "-f", &format!("name={name}"), "-f", &format!("query={query}")]).env("GH_PROMPT_DISABLED", "1").output() else { return urls };
    // A commit GitHub hasn't got fails its field alone, and `gh` exits with an error, but the rest still answer.
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return urls };
    for (i, url) in urls.iter_mut().enumerate() {
        let author = &v["data"]["repository"][format!("c{i}")]["author"];
        if author.is_object() {
            *url = Some(author["user"]["avatarUrl"].as_str().map(str::to_owned));
        }
    }
    urls
}

/// `commit_avatars` for one commit, through the public API.
fn public_avatar(repo: &str, sha: &str) -> Option<Option<String>> {
    let out = Command::new("curl").args(["-sf", "--max-time", "15", "-H", "Accept: application/vnd.github+json", &format!("https://api.github.com/repos/{repo}/commits/{sha}")]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    // `author` is the account GitHub linked the commit's email to, or null.
    let url = v["author"]["avatar_url"].as_str();
    Some(url.map(|u| format!("{u}{}s={PIXELS}", if u.contains('?') { '&' } else { '?' })))
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

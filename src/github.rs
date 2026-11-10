//! Pull requests on GitHub, through the `gh` CLI: the reader's own login and tokens, no app
//! of Collate's to authorise, and the repository `gh` resolves from the remotes. Every call
//! blocks on the network, so the app runs them off the UI thread.
//!
//! A pull request's diff is read with `git`, not the API: its head and base are fetched into
//! `refs/collate/`, and the app shows the merge base against the head, as GitHub does.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// One open pull request in the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Pull {
    pub number: u32,
    pub title: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    pub decision: String,
    /// Logins and team names asked to review.
    pub requested: Vec<String>,
    pub additions: u32,
    pub deletions: u32,
    pub updated: String,
    pub labels: Vec<String>,
    pub checks: Vec<Check>,
    pub url: String,
}

/// A status check or commit status on a pull request's head.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Pending,
    /// Skipped, neutral or cancelled: neither holds the pull request back nor passes it.
    Skipped,
}

/// How a pull request's checks add up: the worst of them.
pub fn overall(checks: &[Check]) -> Option<CheckState> {
    if checks.is_empty() {
        return None;
    }
    let has = |s| checks.iter().any(|c| c.state == s);
    Some(if has(CheckState::Failed) {
        CheckState::Failed
    } else if has(CheckState::Pending) {
        CheckState::Pending
    } else {
        CheckState::Passed
    })
}

/// A comment in the conversation, a review's summary, or one comment of a thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Post {
    pub author: String,
    pub body: String,
    pub time: String,
    /// For a review: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`; empty otherwise.
    pub state: String,
}

/// A review thread on a line of the diff.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    /// The GraphQL node id, to resolve it.
    pub id: String,
    /// The first comment's database id, to reply to.
    pub reply_to: u64,
    pub path: String,
    /// The line in the head (`RIGHT`) or base (`LEFT`) the thread is on now; `None` once a
    /// later push moved it out of the diff.
    pub line: Option<u32>,
    pub left: bool,
    /// The commented line's text, from the comment's diff hunk, to find the line by.
    pub text: String,
    pub resolved: bool,
    pub outdated: bool,
    pub comments: Vec<Post>,
}

/// Everything shown for an open pull request.
#[derive(Clone, Debug, PartialEq)]
pub struct Detail {
    pub pull: Pull,
    pub body: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    /// GitHub's merge state: `CLEAN`, `BEHIND`, `BLOCKED`, `DIRTY`, `UNSTABLE`, `HAS_HOOKS`, …
    pub merge_state: String,
    pub head_oid: String,
    /// Reviewers and where each stands: their latest review's state, or `REQUESTED`.
    pub reviewers: Vec<(String, String)>,
    pub assignees: Vec<String>,
    /// Comments and reviews with a summary, oldest first.
    pub conversation: Vec<Post>,
    pub threads: Vec<Thread>,
    /// Where GitHub takes line comments: its own diff's hunks, three lines of context each, as
    /// (path, left side, first line, last line). Lines past them (context Collate shows more of)
    /// can't be commented on. Filled by [`Gh::fetch`].
    pub commentable: Vec<(String, bool, u32, u32)>,
}

impl Detail {
    /// Whether GitHub accepts a comment on `line` of `path`'s base (`left`) or head side.
    pub fn commentable(&self, path: &str, left: bool, line: u32) -> bool {
        self.commentable.iter().any(|(p, l, a, b)| p == path && *l == left && (*a..=*b).contains(&line))
    }
}

/// A comment of a review being submitted, on a line of the diff.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub path: String,
    pub line: u32,
    pub left: bool,
    pub body: String,
}

/// How to land a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Squash,
    Merge,
    Rebase,
}

impl Method {
    pub const ALL: [Method; 3] = [Method::Squash, Method::Merge, Method::Rebase];

    pub fn title(self) -> &'static str {
        match self {
            Method::Squash => "Squash and Merge",
            Method::Merge => "Create a Merge Commit",
            Method::Rebase => "Rebase and Merge",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Method::Squash => "--squash",
            Method::Merge => "--merge",
            Method::Rebase => "--rebase",
        }
    }
}

/// What a review says overall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    fn event(self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Comment => "COMMENT",
        }
    }
}

/// `gh` run in one repository's folder.
#[derive(Clone, Debug)]
pub struct Gh {
    root: PathBuf,
}

const LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,reviewDecision,reviewRequests,additions,deletions,updatedAt,labels,statusCheckRollup,url";

impl Gh {
    pub fn new(root: &Path) -> Gh {
        Gh { root: root.to_owned() }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        let mut c = Command::new("gh");
        c.args(args).current_dir(&self.root).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1").env("GH_NO_UPDATE_NOTIFIER", "1");
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "Pull requests need the GitHub CLI: brew install gh, then gh auth login".to_owned(),
            _ => format!("can't run gh: {e}"),
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            use std::io::Write;
            pipe.write_all(input.as_bytes()).map_err(|e| format!("gh: {e}"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("gh: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            if err.contains("gh auth login") || err.contains("not logged") {
                return Err("Sign in to GitHub first: run gh auth login in a terminal".into());
            }
            return Err(if err.is_empty() { format!("gh {} failed", args.first().unwrap_or(&"")) } else { err });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn json(&self, args: &[&str], stdin: Option<&str>) -> Result<Value, String> {
        let out = self.run(args, stdin)?;
        serde_json::from_str(&out).map_err(|e| format!("Unexpected reply from gh: {e}"))
    }

    /// The signed-in login.
    pub fn me(&self) -> Result<String, String> {
        Ok(self.run(&["api", "user", "--jq", ".login"], None)?.trim().to_owned())
    }

    /// The repository's open pull requests, newest activity first.
    pub fn pulls(&self) -> Result<Vec<Pull>, String> {
        let v = self.json(&["pr", "list", "--state", "open", "--limit", "100", "--json", LIST_FIELDS], None)?;
        Ok(v.as_array().map(|a| a.iter().map(pull).collect()).unwrap_or_default())
    }

    /// One pull request with its conversation, reviews, checks and line threads.
    pub fn detail(&self, number: u32) -> Result<Detail, String> {
        let n = number.to_string();
        let fields = format!("{LIST_FIELDS},body,state,mergeable,mergeStateStatus,headRefOid,reviews,comments,assignees,latestReviews");
        let v = self.json(&["pr", "view", &n, "--json", &fields], None)?;
        let threads = self.json(&["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-F", &format!("n={number}"), "-f", &format!("query={THREADS}")], None)?;
        Ok(detail(&v, &threads))
    }

    /// Fetch the pull request's head and base into `refs/collate/`, and note which lines GitHub
    /// takes comments on; returns the range to diff, `<merge base>..<head>`.
    pub fn fetch(&self, d: &mut Detail) -> Result<String, String> {
        let url = self.run(&["repo", "view", "--json", "url", "--jq", ".url"], None)?.trim().to_owned();
        let n = d.pull.number;
        let (head, base) = (format!("refs/collate/pull/{n}/head"), format!("refs/collate/pull/{n}/base"));
        let git = |args: &[&str]| -> Result<String, String> {
            let out = Command::new("git").args(args).current_dir(&self.root).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("can't run git: {e}"))?;
            if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()) }
        };
        git(&["fetch", "--no-tags", "--quiet", &url, &format!("+refs/pull/{n}/head:{head}"), &format!("+refs/heads/{}:{base}", d.pull.base)])
            .map_err(|e| format!("Couldn't fetch #{n}: {e}"))?;
        let mb = git(&["merge-base", &base, &head]).map_err(|e| format!("#{n} has no common history with {}: {e}", d.pull.base))?;
        let tip = git(&["rev-parse", &head])?;
        let patch = git(&["diff", "--unified=3", "--no-color", "--no-ext-diff", &mb, &tip])?;
        d.commentable = hunk_ranges(&patch);
        Ok(format!("{mb}..{tip}"))
    }

    /// Submit a review: its verdict, a summary and comments on lines, in one go.
    pub fn review(&self, number: u32, verdict: Verdict, body: &str, drafts: &[Draft]) -> Result<(), String> {
        let comments: Vec<Value> = drafts
            .iter()
            .map(|d| serde_json::json!({ "path": d.path, "line": d.line, "side": if d.left { "LEFT" } else { "RIGHT" }, "body": d.body }))
            .collect();
        let mut req = serde_json::json!({ "event": verdict.event(), "comments": comments });
        if !body.trim().is_empty() {
            req["body"] = Value::String(body.to_owned());
        }
        self.run(&["api", "-X", "POST", &format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews"), "--input", "-"], Some(&req.to_string())).map(|_| ())
    }

    /// Reply in a line thread.
    pub fn reply(&self, number: u32, to: u64, body: &str) -> Result<(), String> {
        let req = serde_json::json!({ "body": body });
        self.run(&["api", "-X", "POST", &format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments/{to}/replies"), "--input", "-"], Some(&req.to_string())).map(|_| ())
    }

    pub fn resolve(&self, thread: &str, resolved: bool) -> Result<(), String> {
        let m = if resolved { "resolveReviewThread" } else { "unresolveReviewThread" };
        let q = format!("mutation($id:ID!){{{m}(input:{{threadId:$id}}){{thread{{isResolved}}}}}}");
        self.run(&["api", "graphql", "-F", &format!("id={thread}"), "-f", &format!("query={q}")], None).map(|_| ())
    }

    /// A comment in the conversation.
    pub fn comment(&self, number: u32, body: &str) -> Result<(), String> {
        self.run(&["pr", "comment", &number.to_string(), "--body-file", "-"], Some(body)).map(|_| ())
    }

    pub fn merge(&self, number: u32, method: Method, delete_branch: bool) -> Result<(), String> {
        let n = number.to_string();
        let mut args = vec!["pr", "merge", &n, method.flag()];
        if delete_branch {
            args.push("--delete-branch");
        }
        self.run(&args, None).map(|_| ())
    }

    /// `close`, `reopen` or `ready` (and `ready --undo` back to a draft).
    pub fn set(&self, number: u32, what: &[&str]) -> Result<(), String> {
        let n = number.to_string();
        let mut args = vec!["pr"];
        args.push(what[0]);
        args.push(&n);
        args.extend(&what[1..]);
        self.run(&args, None).map(|_| ())
    }

    /// Change the title and description.
    pub fn retitle(&self, number: u32, title: &str, body: &str) -> Result<(), String> {
        self.run(&["pr", "edit", &number.to_string(), "--title", title, "--body-file", "-"], Some(body)).map(|_| ())
    }

    /// Ask (`reviewer`) or label (`label`): `add` and `remove`, each a list of logins, teams
    /// (`org/team`) or label names.
    pub fn people(&self, number: u32, what: &str, add: &[String], remove: &[String]) -> Result<(), String> {
        let n = number.to_string();
        let (a, r) = (add.join(","), remove.join(","));
        let (add_flag, remove_flag) = (format!("--add-{what}"), format!("--remove-{what}"));
        let mut args = vec!["pr", "edit", n.as_str()];
        if !add.is_empty() {
            args.extend([add_flag.as_str(), a.as_str()]);
        }
        if !remove.is_empty() {
            args.extend([remove_flag.as_str(), r.as_str()]);
        }
        if args.len() == 3 {
            return Ok(());
        }
        self.run(&args, None).map(|_| ())
    }

    /// Switch the working tree to the pull request's branch.
    pub fn checkout(&self, number: u32) -> Result<(), String> {
        self.run(&["pr", "checkout", &number.to_string()], None).map(|_| ())
    }

    /// Open a pull request from the current branch, pushing it first if it isn't. Returns its
    /// web address.
    pub fn create(&self, base: &str, title: &str, body: &str, draft: bool) -> Result<String, String> {
        let mut args = vec!["pr", "create", "--base", base, "--title", title, "--body-file", "-"];
        if draft {
            args.push("--draft");
        }
        Ok(self.run(&args, Some(body))?.lines().last().unwrap_or("").trim().to_owned())
    }
}

/// Each hunk's lines on both sides, by the file's path (the new one, or the old for a deletion).
fn hunk_ranges(patch: &str) -> Vec<(String, bool, u32, u32)> {
    let mut out = Vec::new();
    let (mut old, mut path) = (String::new(), String::new());
    for l in patch.lines() {
        if let Some(p) = l.strip_prefix("--- ") {
            old = p.strip_prefix("a/").unwrap_or(p).to_owned();
        } else if let Some(p) = l.strip_prefix("+++ ") {
            path = if p == "/dev/null" { old.clone() } else { p.strip_prefix("b/").unwrap_or(p).to_owned() };
        } else if let Some(h) = l.strip_prefix("@@ -") {
            // `@@ -a,b +c,d @@`: a count left out is 1.
            let mut parts = h.split(' ');
            let range = |r: Option<&str>| -> Option<(u32, u32)> {
                let r = r?.trim_start_matches(['-', '+']);
                let (a, n) = r.split_once(',').unwrap_or((r, "1"));
                Some((a.parse().ok()?, n.parse().ok()?))
            };
            for (left, r) in [(true, range(parts.next())), (false, range(parts.next()))] {
                if let Some((a, n)) = r.filter(|(_, n)| *n > 0) {
                    out.push((path.clone(), left, a, a + n - 1));
                }
            }
        }
    }
    out
}

/// Line threads, with the commented line's hunk to find the line's text.
const THREADS: &str = "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){pullRequest(number:$n){reviewThreads(first:100){nodes{id isResolved isOutdated path line originalLine diffSide comments(first:50){nodes{databaseId author{login} body createdAt diffHunk}}}}}}}";

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_owned()
}

fn login(v: &Value) -> String {
    v["login"].as_str().unwrap_or("ghost").to_owned()
}

fn pull(v: &Value) -> Pull {
    let list = |k: &str, f: fn(&Value) -> String| v[k].as_array().map(|a| a.iter().map(f).collect()).unwrap_or_default();
    Pull {
        number: v["number"].as_u64().unwrap_or(0) as u32,
        title: s(v, "title"),
        author: login(&v["author"]),
        head: s(v, "headRefName"),
        base: s(v, "baseRefName"),
        draft: v["isDraft"].as_bool().unwrap_or(false),
        decision: s(v, "reviewDecision"),
        requested: list("reviewRequests", |r| r["login"].as_str().or(r["name"].as_str()).unwrap_or("").to_owned()),
        additions: v["additions"].as_u64().unwrap_or(0) as u32,
        deletions: v["deletions"].as_u64().unwrap_or(0) as u32,
        updated: s(v, "updatedAt"),
        labels: list("labels", |l| s(l, "name")),
        checks: v["statusCheckRollup"].as_array().map(|a| a.iter().map(check).collect()).unwrap_or_default(),
        url: s(v, "url"),
    }
}

/// A check run (`name`, `status`, `conclusion`) or a commit status (`context`, `state`).
fn check(v: &Value) -> Check {
    let up = |k: &str| s(v, k).to_ascii_uppercase();
    let state = if v["__typename"] == "StatusContext" || v.get("context").is_some() {
        match up("state").as_str() {
            "SUCCESS" => CheckState::Passed,
            "FAILURE" | "ERROR" => CheckState::Failed,
            _ => CheckState::Pending,
        }
    } else if up("status") != "COMPLETED" {
        CheckState::Pending
    } else {
        match up("conclusion").as_str() {
            "SUCCESS" => CheckState::Passed,
            "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => CheckState::Failed,
            _ => CheckState::Skipped,
        }
    };
    let name = [s(v, "name"), s(v, "context")].into_iter().find(|n| !n.is_empty()).unwrap_or_default();
    let url = [s(v, "detailsUrl"), s(v, "targetUrl")].into_iter().find(|n| !n.is_empty()).unwrap_or_default();
    Check { name, state, url }
}

fn detail(v: &Value, threads: &Value) -> Detail {
    let mut conversation: Vec<Post> = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Post { author: login(&c["author"]), body: s(c, "body"), time: s(c, "createdAt"), state: String::new() })
        .collect();
    // A review that only left line comments has no summary to show here.
    conversation.extend(v["reviews"].as_array().into_iter().flatten().filter(|r| !s(r, "body").trim().is_empty() || s(r, "state") != "COMMENTED").map(|r| Post {
        author: login(&r["author"]),
        body: s(r, "body"),
        time: s(r, "submittedAt"),
        state: s(r, "state"),
    }));
    conversation.sort_by(|a, b| a.time.cmp(&b.time));
    let mut reviewers: Vec<(String, String)> = v["latestReviews"].as_array().into_iter().flatten().map(|r| (login(&r["author"]), s(r, "state"))).collect();
    for r in v["reviewRequests"].as_array().into_iter().flatten() {
        let who = r["login"].as_str().or(r["name"].as_str()).unwrap_or("").to_owned();
        if !reviewers.iter().any(|(w, _)| *w == who) {
            reviewers.push((who, "REQUESTED".into()));
        }
    }
    Detail {
        pull: pull(v),
        body: s(v, "body"),
        state: s(v, "state"),
        mergeable: s(v, "mergeable"),
        merge_state: s(v, "mergeStateStatus"),
        head_oid: s(v, "headRefOid"),
        reviewers,
        assignees: v["assignees"].as_array().into_iter().flatten().map(login).collect(),
        conversation,
        threads: threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"].as_array().into_iter().flatten().filter_map(thread).collect(),
        commentable: Vec::new(),
    }
}

fn thread(t: &Value) -> Option<Thread> {
    let comments: Vec<&Value> = t["comments"]["nodes"].as_array()?.iter().collect();
    let first = comments.first()?;
    let left = s(t, "diffSide") == "LEFT";
    // The hunk ends at the commented line; drop its `+`, `-` or ` ` column.
    let text = s(first, "diffHunk").lines().last().map(|l| l.get(1..).unwrap_or("").to_owned()).unwrap_or_default();
    Some(Thread {
        id: s(t, "id"),
        reply_to: first["databaseId"].as_u64().unwrap_or(0),
        path: s(t, "path"),
        line: t["line"].as_u64().map(|n| n as u32),
        left,
        text,
        resolved: t["isResolved"].as_bool().unwrap_or(false),
        outdated: t["isOutdated"].as_bool().unwrap_or(false),
        comments: comments.iter().map(|c| Post { author: login(&c["author"]), body: s(c, "body"), time: s(c, "createdAt"), state: String::new() }).collect(),
    })
}

/// The pull requests that want the reader's attention first: asked of them, then theirs, then the rest.
pub fn group<'a>(pulls: &'a [Pull], me: &str) -> [(&'static str, Vec<&'a Pull>); 3] {
    let asked: Vec<&Pull> = pulls.iter().filter(|p| p.requested.iter().any(|r| r == me)).collect();
    let mine: Vec<&Pull> = pulls.iter().filter(|p| p.author == me).collect();
    let rest: Vec<&Pull> = pulls.iter().filter(|p| p.author != me && !p.requested.iter().any(|r| r == me)).collect();
    [("Review Requested", asked), ("Yours", mine), ("Everyone Else", rest)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_request_reads_from_ghs_json() {
        let v: Value = serde_json::from_str(r#"{"number":42,"title":"Add billing","author":{"login":"ana"},"headRefName":"billing","baseRefName":"main",
            "isDraft":false,"reviewDecision":"REVIEW_REQUIRED","reviewRequests":[{"login":"me"},{"name":"core-team"}],"additions":120,"deletions":3,
            "updatedAt":"2026-11-10T09:00:00Z","labels":[{"name":"db"}],"url":"https://github.com/o/r/pull/42",
            "statusCheckRollup":[{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"u"},
              {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":""},
              {"__typename":"StatusContext","context":"ci/circle","state":"FAILURE","targetUrl":"c"}]}"#).unwrap();
        let p = pull(&v);
        assert_eq!((p.number, p.author.as_str(), p.head.as_str(), p.base.as_str()), (42, "ana", "billing", "main"));
        assert_eq!(p.requested, ["me", "core-team"]);
        assert_eq!(p.checks.iter().map(|c| c.state).collect::<Vec<_>>(), [CheckState::Passed, CheckState::Pending, CheckState::Failed]);
        assert_eq!(overall(&p.checks), Some(CheckState::Failed));
        assert_eq!(overall(&p.checks[..2]), Some(CheckState::Pending));
        assert_eq!(overall(&[]), None);
        let g = group(std::slice::from_ref(&p), "me");
        assert_eq!((g[0].1.len(), g[1].1.len(), g[2].1.len()), (1, 0, 0));
    }

    #[test]
    fn comments_are_taken_only_inside_githubs_hunks() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,7 +10,8 @@ fn a()\n x\n+y\n@@ -40 +41,0 @@\n-z\ndiff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        let mut d = detail(&serde_json::json!({}), &serde_json::json!({}));
        d.commentable = hunk_ranges(patch);
        assert!(d.commentable("a.rs", false, 10) && d.commentable("a.rs", false, 17) && !d.commentable("a.rs", false, 18));
        assert!(d.commentable("a.rs", true, 16) && d.commentable("a.rs", true, 40) && !d.commentable("a.rs", true, 41));
        assert!(d.commentable("gone.rs", true, 2) && !d.commentable("gone.rs", false, 1), "a deleted file is its old side");
    }

    #[test]
    fn threads_find_their_line_text_and_the_conversation_is_in_order() {
        let v: Value = serde_json::from_str(r#"{"number":1,"body":"Why","state":"OPEN","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","headRefOid":"abc",
            "comments":[{"author":{"login":"b"},"body":"later","createdAt":"2026-11-02T00:00:00Z"}],
            "reviews":[{"author":{"login":"a"},"body":"","state":"APPROVED","submittedAt":"2026-11-01T00:00:00Z"},
              {"author":{"login":"c"},"body":"","state":"COMMENTED","submittedAt":"2026-11-03T00:00:00Z"}],
            "latestReviews":[{"author":{"login":"a"},"state":"APPROVED"}],"reviewRequests":[{"login":"d"}],"assignees":[]}"#).unwrap();
        let t: Value = serde_json::from_str(r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"T1","isResolved":false,"isOutdated":false,
            "path":"src/a.rs","line":12,"originalLine":12,"diffSide":"RIGHT","comments":{"nodes":[{"databaseId":7,"author":{"login":"a"},"body":"Rename?",
            "createdAt":"2026-11-01T00:00:00Z","diffHunk":"@@ -1,2 +1,2 @@\n fn a() {}\n+let x = 1;"}]}}]}}}}}"#).unwrap();
        let d = detail(&v, &t);
        assert_eq!(d.conversation.iter().map(|p| p.author.as_str()).collect::<Vec<_>>(), ["a", "b"], "a bare line-comment review stays out");
        assert_eq!(d.reviewers, [("a".to_owned(), "APPROVED".to_owned()), ("d".to_owned(), "REQUESTED".to_owned())]);
        let th = &d.threads[0];
        assert_eq!((th.path.as_str(), th.line, th.left, th.text.as_str(), th.reply_to), ("src/a.rs", Some(12), false, "let x = 1;", 7));
    }
}

//! Issues on GitHub, through the same `gh` as the pull requests: read with GraphQL, asking
//! only what the page shows, and changed with `gh issue`. Every call blocks on the network, so
//! the app runs them off the UI thread.

use serde_json::Value;

use crate::github::{Gh, Post, login, post, s};

/// One issue in the list.
#[derive(Clone, Debug, Default, PartialEq, Hash)]
pub struct Issue {
    pub number: u32,
    pub title: String,
    pub author: String,
    pub open: bool,
    /// Why it was closed, as GitHub names it (`COMPLETED`, `NOT_PLANNED`, `DUPLICATE`); empty
    /// while open.
    pub reason: String,
    pub assignees: Vec<String>,
    /// Each label's name and colour, as hex without the `#`.
    pub labels: Vec<(String, String)>,
    pub comments: u32,
    pub created: String,
    pub updated: String,
    pub url: String,
    /// It mentions the reader.
    pub mentions: bool,
}

const FIELDS: &str = "number title url state stateReason createdAt updatedAt author{login} assignees(first:10){nodes{login}} labels(first:20){nodes{name color}} comments{totalCount}";

/// The open issues, newest activity first, and the reader's login.
fn open_query() -> String {
    format!("query($owner:String!,$name:String!){{viewer{{login}} repository(owner:$owner,name:$name){{issues(states:OPEN,first:100,orderBy:{{field:UPDATED_AT,direction:DESC}}){{nodes{{{FIELDS}}}}}}}}}")
}

/// The last closed issues: there to look back on, so fewer.
fn closed_query() -> String {
    format!("query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{issues(states:CLOSED,first:30,orderBy:{{field:UPDATED_AT,direction:DESC}}){{nodes{{{FIELDS}}}}}}}}}")
}

/// The open issues that mention `$me`, by number.
const MENTIONS: &str = "query($owner:String!,$name:String!,$me:String!){repository(owner:$owner,name:$name){issues(states:OPEN,first:100,filterBy:{mentioned:$me}){nodes{number}}}}";

/// One issue, opened: what the list knows, and what it says.
#[derive(Clone, Debug, Default)]
pub struct Detail {
    pub issue: Issue,
    /// The GraphQL node id, to subscribe to or react to it.
    pub id: String,
    /// The reader hears of everything on it.
    pub subscribed: bool,
    /// Reactions to the description and how many of each, by GitHub's names.
    pub reactions: Vec<(String, u32)>,
    /// Only those with write access may comment.
    pub locked: bool,
    pub body: String,
    /// The comments, oldest first.
    pub comments: Vec<Post>,
    /// What else happened to it, oldest first.
    pub events: Vec<Event>,
    /// The reader may change its title, description, labels and the rest.
    pub can_edit: bool,
    /// The reader may close it, or reopen it once closed; an issue's author may though they
    /// can't edit the rest.
    pub can_close: bool,
    /// The reader may change who has it, its labels and its milestone, each its own right, so
    /// one who triages has them without the rest.
    pub can_assign: bool,
    pub can_label: bool,
    pub can_milestone: bool,
    /// The milestone it's in, by title.
    pub milestone: Option<String>,
    /// The pull requests that close it when they merge: number, title and state (`OPEN`,
    /// `CLOSED`, `MERGED`).
    pub pulls: Vec<(u32, String, String)>,
}

impl Detail {
    /// A locked issue takes comments only from those who can change it.
    pub fn can_comment(&self) -> bool {
        !self.locked || self.can_edit
    }
}

/// A change in an issue's timeline, said as GitHub says it.
#[derive(Clone, Debug, Default, PartialEq, Hash)]
pub struct Event {
    /// GitHub's name for it, as `ClosedEvent`.
    pub kind: String,
    pub who: String,
    /// What they did, after their name: "closed this as completed".
    pub what: String,
    pub time: String,
    /// The label it added or took off, and its colour.
    pub label: Option<(String, String)>,
    /// Where it points: the issue or pull request that mentioned this one.
    pub url: String,
}

/// A comment: who, what and when, and what the reader may do with it.
const COMMENT: &str = "id body url createdAt viewerDidAuthor author{login} reactionGroups{content users{totalCount}}";

/// The events the timeline shows, each with who and when.
const EVENTS: &str = "... on LabeledEvent{createdAt actor{login} label{name color}} ... on UnlabeledEvent{createdAt actor{login} label{name color}} \
... on AssignedEvent{createdAt actor{login} assignee{... on User{login}}} ... on UnassignedEvent{createdAt actor{login} assignee{... on User{login}}} \
... on ClosedEvent{createdAt actor{login} stateReason} ... on ReopenedEvent{createdAt actor{login}} \
... on RenamedTitleEvent{createdAt actor{login} previousTitle currentTitle} \
... on CrossReferencedEvent{createdAt actor{login} source{... on Issue{number title url} ... on PullRequest{number title url}}} \
... on MilestonedEvent{createdAt actor{login} milestoneTitle} ... on DemilestonedEvent{createdAt actor{login} milestoneTitle}";

/// GitHub's timeline holds the last 100 items; older ones are on GitHub.
fn detail_fields() -> String {
    format!("id body reactionGroups{{content users{{totalCount}}}} viewerCanUpdate viewerCanClose viewerCanReopen viewerCanAssign viewerCanLabel viewerCanSetMilestone viewerSubscription locked milestone{{title}} closedByPullRequestsReferences(first:10,includeClosedPrs:true){{nodes{{number title state}}}} timelineItems(last:100,itemTypes:[ISSUE_COMMENT,LABELED_EVENT,UNLABELED_EVENT,ASSIGNED_EVENT,UNASSIGNED_EVENT,CLOSED_EVENT,REOPENED_EVENT,\
RENAMED_TITLE_EVENT,CROSS_REFERENCED_EVENT,MILESTONED_EVENT,DEMILESTONED_EVENT]){{nodes{{__typename ... on IssueComment{{{COMMENT}}} {EVENTS}}}}}")
}

fn detail_query() -> String {
    format!("query($owner:String!,$name:String!,$n:Int!){{repository(owner:$owner,name:$name){{issue(number:$n){{{FIELDS} {}}}}}}}", detail_fields())
}

/// What an issue's people and labels can be set to in this repository.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// Who can be assigned, by login.
    pub people: Vec<String>,
    /// Each label's name and colour, by name.
    pub labels: Vec<(String, String)>,
    /// The open milestones, by title.
    pub milestones: Vec<String>,
    /// The repository's issue templates, as a new issue starts from them.
    pub templates: Vec<Template>,
}

/// An issue template: what a new issue opens with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Template {
    pub name: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

const OPTIONS: &str = "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){assignableUsers(first:100){nodes{login}} labels(first:100,orderBy:{field:NAME,direction:ASC}){nodes{name color}} milestones(states:OPEN,first:50,orderBy:{field:DUE_DATE,direction:ASC}){nodes{title}} \
issueTemplates{name title body labels(first:10){nodes{name}} assignees(first:10){nodes{login}}}}}";

impl Gh {
    pub fn issue_options(&self) -> Result<Options, String> {
        let v = self.query(OPTIONS)?;
        let r = &v["data"]["repository"];
        Ok(Options {
            people: r["assignableUsers"]["nodes"].as_array().into_iter().flatten().map(login).collect(),
            labels: r["labels"]["nodes"].as_array().into_iter().flatten().map(|l| (s(l, "name"), s(l, "color"))).collect(),
            milestones: r["milestones"]["nodes"].as_array().into_iter().flatten().map(|m| s(m, "title")).collect(),
            templates: r["issueTemplates"].as_array().into_iter().flatten().map(|t| Template {
                name: s(t, "name"),
                title: s(t, "title"),
                body: s(t, "body"),
                labels: t["labels"]["nodes"].as_array().into_iter().flatten().map(|l| s(l, "name")).collect(),
                assignees: t["assignees"]["nodes"].as_array().into_iter().flatten().map(login).collect(),
            }).collect(),
        })
    }

    /// Issue `n`, opened.
    pub fn issue(&self, n: u32) -> Result<Detail, String> {
        let v = self.json(&["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-F", &format!("n={n}"), "-f", &format!("query={}", detail_query())], None)?;
        Ok(detail(&v["data"]["repository"]["issue"]))
    }

    pub fn issue_comment(&self, n: u32, body: &str) -> Result<String, String> {
        self.run(&["issue", "comment", &n.to_string(), "--body-file", "-"], Some(body))
    }

    pub fn edit_issue(&self, n: u32, title: &str, body: &str) -> Result<String, String> {
        self.run(&["issue", "edit", &n.to_string(), &format!("--title={title}"), "--body-file", "-"], Some(body))
    }

    /// Add `add` to issue `n`'s assignees and take `remove` off, by login (`@me` for the reader).
    pub fn issue_assignees(&self, n: u32, add: &[&str], remove: &[&str]) -> Result<String, String> {
        self.issue_edit_list(n, "assignee", add, remove)
    }

    /// Add label `add` to issue `n`, or take `remove` off, by name.
    pub fn issue_labels(&self, n: u32, add: &[&str], remove: &[&str]) -> Result<String, String> {
        self.issue_edit_list(n, "label", add, remove)
    }

    /// Put issue `n` in milestone `title`, or take it out of its own with `None`.
    pub fn issue_milestone(&self, n: u32, title: Option<&str>) -> Result<String, String> {
        let n = n.to_string();
        let set = title.map(|t| format!("--milestone={t}"));
        self.run(&["issue", "edit", &n, set.as_deref().unwrap_or("--remove-milestone")], None)
    }

    /// Open a new issue; its number, from the link `gh` answers with.
    pub fn create_issue(&self, title: &str, body: &str, labels: &[String], assignees: &[String]) -> Result<u32, String> {
        let mut args = vec!["issue".to_owned(), "create".to_owned(), format!("--title={title}"), "--body-file".to_owned(), "-".to_owned()];
        args.extend(labels.iter().map(|l| format!("--label={l}")));
        args.extend(assignees.iter().map(|a| format!("--assignee={a}")));
        let url = self.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), Some(body))?;
        url.trim().rsplit('/').next().and_then(|n| n.parse().ok()).ok_or_else(|| format!("Unexpected reply from gh: {}", url.trim()))
    }

    /// Lock issue `n`'s conversation to collaborators, or unlock it.
    pub fn lock_issue(&self, n: u32, lock: bool) -> Result<String, String> {
        self.run(&["issue", if lock { "lock" } else { "unlock" }, &n.to_string()], None)
    }

    /// `gh issue edit`'s `--add-<what>` and `--remove-<what>`.
    fn issue_edit_list(&self, n: u32, what: &str, add: &[&str], remove: &[&str]) -> Result<String, String> {
        let mut args = vec!["issue".to_owned(), "edit".to_owned(), n.to_string()];
        args.extend(add.iter().map(|a| format!("--add-{what}={a}")));
        args.extend(remove.iter().map(|a| format!("--remove-{what}={a}")));
        self.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)
    }

    /// Close issue `n` for `reason` (`completed` or `not planned`), or reopen it with `None`,
    /// saying `comment` as it goes if there's one.
    pub fn set_issue_open(&self, n: u32, reason: Option<&str>, comment: &str) -> Result<String, String> {
        let n = n.to_string();
        let mut args = vec!["issue", if reason.is_some() { "close" } else { "reopen" }, n.as_str()];
        let reason = reason.map(|r| format!("--reason={r}"));
        args.extend(reason.as_deref());
        // `=` so a comment that starts with a dash isn't taken for a flag.
        let comment = format!("--comment={comment}");
        if comment.len() > "--comment=".len() {
            args.push(&comment);
        }
        self.run(&args, None)
    }

    fn query(&self, q: &str) -> Result<Value, String> {
        self.json(&["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("query={q}")], None)
    }

    /// The signed-in login, the repository's open issues and its last closed ones, with those
    /// that mention the reader marked. The queries run at once; the mentions need the login
    /// first, which is a call of its own, so they're read beside the rest rather than after.
    pub fn issues(&self) -> Result<(String, Vec<Issue>), String> {
        let failed = |_| Err("Reading issues failed".to_owned());
        let (open, closed, mentions) = std::thread::scope(|t| {
            let closed = t.spawn(|| self.query(&closed_query()));
            let mentions = t.spawn(|| {
                let me = self.me()?;
                self.json(&["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("me={me}"), "-f", &format!("query={MENTIONS}")], None)
            });
            (self.query(&open_query()), closed.join().unwrap_or_else(failed), mentions.join().unwrap_or_else(failed))
        });
        let (open, closed, mentions) = (open?, closed?, mentions?);
        let me = open["data"]["viewer"]["login"].as_str().ok_or("Unexpected reply from gh: no login")?.to_owned();
        let mentioned: Vec<u64> = mentions["data"]["repository"]["issues"]["nodes"].as_array().into_iter().flatten().filter_map(|n| n["number"].as_u64()).collect();
        let mut all = [nodes(&open), nodes(&closed)].concat();
        for i in &mut all {
            i.mentions = mentioned.contains(&(i.number as u64));
        }
        Ok((me, all))
    }
}

fn nodes(v: &Value) -> Vec<Issue> {
    v["data"]["repository"]["issues"]["nodes"].as_array().map(|a| a.iter().map(issue).collect()).unwrap_or_default()
}

/// An issue from GraphQL: its connections are under `nodes`.
fn issue(v: &Value) -> Issue {
    Issue {
        number: v["number"].as_u64().unwrap_or(0) as u32,
        title: s(v, "title"),
        author: login(&v["author"]),
        open: v["state"] != "CLOSED",
        reason: if v["state"] == "CLOSED" { s(v, "stateReason") } else { String::new() },
        assignees: v["assignees"]["nodes"].as_array().into_iter().flatten().map(login).collect(),
        labels: v["labels"]["nodes"].as_array().into_iter().flatten().map(|l| (s(l, "name"), s(l, "color"))).collect(),
        comments: v["comments"]["totalCount"].as_u64().unwrap_or(0) as u32,
        created: s(v, "createdAt"),
        updated: s(v, "updatedAt"),
        url: s(v, "url"),
        mentions: false,
    }
}

fn detail(v: &Value) -> Detail {
    let items = v["timelineItems"]["nodes"].as_array().into_iter().flatten();
    let (comments, events): (Vec<&Value>, Vec<&Value>) = items.partition(|e| e["__typename"] == "IssueComment");
    let comments = comments.into_iter().map(|c| Post { time: s(c, "createdAt"), ..post(c) }).collect();
    Detail { issue: issue(v), id: s(v, "id"), subscribed: v["viewerSubscription"] == "SUBSCRIBED", locked: v["locked"].as_bool().unwrap_or(false), reactions: post(v).reactions, body: s(v, "body"), comments, events: events.into_iter().filter_map(event).collect(), can_edit: v["viewerCanUpdate"].as_bool().unwrap_or(false),
        can_close: v[if v["state"] != "CLOSED" { "viewerCanClose" } else { "viewerCanReopen" }].as_bool().unwrap_or(false),
        can_assign: v["viewerCanAssign"].as_bool().unwrap_or(false), can_label: v["viewerCanLabel"].as_bool().unwrap_or(false),
        can_milestone: v["viewerCanSetMilestone"].as_bool().unwrap_or(false),
        milestone: v["milestone"]["title"].as_str().map(str::to_owned),
        pulls: v["closedByPullRequestsReferences"]["nodes"].as_array().into_iter().flatten()
            .map(|p| (p["number"].as_u64().unwrap_or(0) as u32, s(p, "title"), s(p, "state"))).collect() }
}

/// An event as GitHub words it; `None` for a kind the timeline doesn't show.
fn event(e: &Value) -> Option<Event> {
    let kind = s(e, "__typename");
    let who = login(&e["actor"]);
    let whom = login(&e["assignee"]);
    let mut url = String::new();
    let mut label = None;
    let what = match kind.as_str() {
        "LabeledEvent" | "UnlabeledEvent" => {
            label = Some((s(&e["label"], "name"), s(&e["label"], "color")));
            if kind == "LabeledEvent" { "added the label" } else { "removed the label" }.to_owned()
        }
        "AssignedEvent" if whom == who => "self-assigned this".to_owned(),
        "AssignedEvent" => format!("assigned {whom}"),
        "UnassignedEvent" if whom == who => "removed their assignment".to_owned(),
        "UnassignedEvent" => format!("unassigned {whom}"),
        "ClosedEvent" => match e["stateReason"].as_str() {
            Some("COMPLETED") => "closed this as completed",
            Some("NOT_PLANNED") => "closed this as not planned",
            Some("DUPLICATE") => "closed this as a duplicate",
            _ => "closed this",
        }.to_owned(),
        "ReopenedEvent" => "reopened this".to_owned(),
        "RenamedTitleEvent" => format!("changed the title from \u{201c}{}\u{201d} to \u{201c}{}\u{201d}", s(e, "previousTitle"), s(e, "currentTitle")),
        "CrossReferencedEvent" => {
            url = s(&e["source"], "url");
            format!("mentioned this in #{} {}", e["source"]["number"].as_u64().unwrap_or(0), s(&e["source"], "title"))
        }
        "MilestonedEvent" => format!("added this to the {} milestone", s(e, "milestoneTitle")),
        "DemilestonedEvent" => format!("removed this from the {} milestone", s(e, "milestoneTitle")),
        _ => return None,
    };
    Some(Event { kind, who, what, time: s(e, "createdAt"), label, url })
}

/// The issues on the reader, assigned to them, then the ones they opened, then those that
/// mention them, then the rest; then the closed. Each issue is in the first group it fits.
pub fn group<'a>(issues: &[&'a Issue], me: &str) -> [(&'static str, Vec<&'a Issue>); 5] {
    let assigned = |i: &Issue| i.assignees.iter().any(|a| a == me);
    let open = issues.iter().copied().filter(|i| i.open);
    let pick = |f: &dyn Fn(&Issue) -> bool| open.clone().filter(|i| f(i)).collect::<Vec<_>>();
    [
        ("Assigned to You", pick(&|i| assigned(i))),
        ("Created by You", pick(&|i| !assigned(i) && i.author == me)),
        ("Mentioning You", pick(&|i| !assigned(i) && i.author != me && i.mentions)),
        ("Everyone Else", pick(&|i| !assigned(i) && i.author != me && !i.mentions)),
        ("Closed", issues.iter().copied().filter(|i| !i.open).collect()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issue_reads_from_graphql() {
        let v = serde_json::json!({"data": {"repository": {"issues": {"nodes": [
            {"number": 12, "title": "Crash on launch", "state": "OPEN", "stateReason": null, "author": {"login": "ana"},
             "assignees": {"nodes": [{"login": "me"}]}, "labels": {"nodes": [{"name": "bug", "color": "d73a4a"}]},
             "comments": {"totalCount": 3}, "createdAt": "2026-11-01T09:00:00Z", "updatedAt": "2026-11-10T09:00:00Z", "url": "https://github.com/o/r/issues/12"},
            {"number": 9, "title": "Old idea", "state": "CLOSED", "stateReason": "NOT_PLANNED", "author": null}
        ]}}}});
        let issues = nodes(&v);
        let a = &issues[0];
        assert_eq!((a.number, a.title.as_str(), a.author.as_str(), a.open, a.reason.as_str()), (12, "Crash on launch", "ana", true, ""));
        assert_eq!((a.assignees.as_slice(), a.labels.as_slice(), a.comments), (&["me".to_owned()][..], &[("bug".to_owned(), "d73a4a".to_owned())][..], 3));
        let b = &issues[1];
        assert_eq!((b.open, b.reason.as_str(), b.author.as_str(), b.comments), (false, "NOT_PLANNED", "ghost", 0), "a deleted account reads as GitHub's ghost");
    }

    #[test]
    fn an_opened_issue_reads_its_comments() {
        let v = serde_json::json!({"number": 7, "title": "Add sync", "state": "OPEN", "author": {"login": "me"}, "body": "It should sync.", "timelineItems": {"nodes": [
            {"__typename": "IssueComment", "id": "IC_1", "body": "Agreed", "createdAt": "2026-11-02T10:00:00Z", "viewerDidAuthor": true, "author": {"login": "me"},
             "reactionGroups": [{"content": "HEART", "users": {"totalCount": 2}}, {"content": "LAUGH", "users": {"totalCount": 0}}]}
        ]}});
        let d = detail(&v);
        assert_eq!((d.issue.number, d.body.as_str(), d.comments.len()), (7, "It should sync.", 1));
        let c = &d.comments[0];
        assert_eq!((c.id.as_str(), c.body.as_str(), c.time.as_str(), c.mine), ("IC_1", "Agreed", "2026-11-02T10:00:00Z", true));
        assert_eq!(c.reactions, [("HEART".to_owned(), 2)], "only the reactions someone gave");
        let can = |state: &str| detail(&serde_json::json!({"state": state, "viewerCanClose": true, "viewerCanReopen": false})).can_close;
        assert!(can("OPEN") && !can("CLOSED"), "closing asks one right, reopening another");
    }

    #[test]
    fn timeline_events_read_as_github_words_them() {
        let e = |v: serde_json::Value| event(&v).map(|e| (e.who, e.what, e.label, e.url));
        let me = serde_json::json!({"login": "me"});
        assert_eq!(e(serde_json::json!({"__typename": "ClosedEvent", "actor": me, "stateReason": "NOT_PLANNED"})), Some(("me".into(), "closed this as not planned".into(), None, String::new())));
        assert_eq!(e(serde_json::json!({"__typename": "AssignedEvent", "actor": me, "assignee": me})).map(|e| e.1), Some("self-assigned this".into()));
        assert_eq!(e(serde_json::json!({"__typename": "AssignedEvent", "actor": me, "assignee": {"login": "ana"}})).map(|e| e.1), Some("assigned ana".into()));
        assert_eq!(e(serde_json::json!({"__typename": "LabeledEvent", "actor": me, "label": {"name": "bug", "color": "d73a4a"}})).map(|e| (e.1, e.2)),
            Some(("added the label".into(), Some(("bug".into(), "d73a4a".into())))));
        assert_eq!(e(serde_json::json!({"__typename": "CrossReferencedEvent", "actor": me, "source": {"number": 9, "title": "Fix it", "url": "https://github.com/o/r/pull/9"}})).map(|e| (e.1, e.3)),
            Some(("mentioned this in #9 Fix it".into(), "https://github.com/o/r/pull/9".into())));
        assert_eq!(e(serde_json::json!({"__typename": "SubscribedEvent"})), None, "a kind the timeline doesn't show");
    }

    #[test]
    fn issues_group_by_what_they_want_of_the_reader() {
        let i = |number: u32, author: &str, assignees: &[&str], mentions: bool, open: bool| Issue {
            number, author: author.into(), assignees: assignees.iter().map(|a| (*a).to_owned()).collect(), mentions, open, ..Issue::default()
        };
        let all = [i(1, "ana", &["me"], true, true), i(2, "me", &["me"], false, true), i(3, "me", &[], true, true), i(4, "bo", &[], true, true),
            i(5, "bo", &["ana"], false, true), i(6, "me", &["me"], false, false)];
        let g = group(&all.iter().collect::<Vec<_>>(), "me");
        let numbers: Vec<Vec<u32>> = g.iter().map(|(_, is)| is.iter().map(|i| i.number).collect()).collect();
        assert_eq!(numbers, [vec![1, 2], vec![3], vec![4], vec![5], vec![6]], "each in the first group it fits; a closed one asks nothing");
        assert_eq!(g.map(|(t, _)| t), ["Assigned to You", "Created by You", "Mentioning You", "Everyone Else", "Closed"]);
    }
}

//! Issues on GitHub, through the same `gh` as the pull requests: read with GraphQL, asking
//! only what the page shows, and changed with `gh issue`. Every call blocks on the network, so
//! the app runs them off the UI thread.

use serde_json::Value;

use crate::github::{Gh, login, s};

/// One issue in the list.
#[derive(Clone, Debug, Default, PartialEq)]
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

impl Gh {
    fn query(&self, q: &str) -> Result<Value, String> {
        self.json(&["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("query={q}")], None)
    }

    /// The signed-in login, the repository's open issues and its last closed ones. The two
    /// queries run at once.
    pub fn issues(&self) -> Result<(String, Vec<Issue>), String> {
        let (open, closed) = std::thread::scope(|t| {
            let closed = t.spawn(|| self.query(&closed_query()));
            (self.query(&open_query()), closed.join().unwrap_or_else(|_| Err("Reading issues failed".into())))
        });
        let (open, closed) = (open?, closed?);
        let me = open["data"]["viewer"]["login"].as_str().ok_or("Unexpected reply from gh: no login")?.to_owned();
        Ok((me, [nodes(&open), nodes(&closed)].concat()))
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
    }
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
}

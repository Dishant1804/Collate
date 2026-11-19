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

impl Gh {
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

/// The issues on the reader, assigned to them, then the ones they opened, then those that
/// mention them, then the rest; then the closed. Each issue is in the first group it fits.
pub fn group<'a>(issues: &'a [Issue], me: &str) -> [(&'static str, Vec<&'a Issue>); 5] {
    let assigned = |i: &Issue| i.assignees.iter().any(|a| a == me);
    let open = issues.iter().filter(|i| i.open);
    let pick = |f: &dyn Fn(&Issue) -> bool| open.clone().filter(|i| f(i)).collect::<Vec<_>>();
    [
        ("Assigned to You", pick(&|i| assigned(i))),
        ("Created by You", pick(&|i| !assigned(i) && i.author == me)),
        ("Mentioning You", pick(&|i| !assigned(i) && i.author != me && i.mentions)),
        ("Everyone Else", pick(&|i| !assigned(i) && i.author != me && !i.mentions)),
        ("Closed", issues.iter().filter(|i| !i.open).collect()),
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
    fn issues_group_by_what_they_want_of_the_reader() {
        let i = |number: u32, author: &str, assignees: &[&str], mentions: bool, open: bool| Issue {
            number, author: author.into(), assignees: assignees.iter().map(|a| (*a).to_owned()).collect(), mentions, open, ..Issue::default()
        };
        let all = [i(1, "ana", &["me"], true, true), i(2, "me", &["me"], false, true), i(3, "me", &[], true, true), i(4, "bo", &[], true, true),
            i(5, "bo", &["ana"], false, true), i(6, "me", &["me"], false, false)];
        let g = group(&all, "me");
        let numbers: Vec<Vec<u32>> = g.iter().map(|(_, is)| is.iter().map(|i| i.number).collect()).collect();
        assert_eq!(numbers, [vec![1, 2], vec![3], vec![4], vec![5], vec![6]], "each in the first group it fits; a closed one asks nothing");
        assert_eq!(g.map(|(t, _)| t), ["Assigned to You", "Created by You", "Mentioning You", "Everyone Else", "Closed"]);
    }
}

//! Fuzzy matching, and what the toolbar's search finds outside the diff: the Map's files and
//! folders, and History's commits (by subject, author or hash) with their branches and tags.
//! Each keystroke scores every candidate without allocating and builds rows for the best only.

use crate::graph::{Commit, RefKind, UNCOMMITTED};
use crate::map::RepoMap;

/// Rows shown at most.
pub const LIMIT: usize = 50;

/// How well `q` matches `text` as a subsequence, ignoring ASCII case and spaces in `q`; `None`
/// when it doesn't. Runs of adjacent letters, word starts and letters in the last path
/// segment count for more.
pub fn fuzzy(q: &str, text: &str) -> Option<i32> {
    let p = text.as_bytes();
    let name = text.rfind('/').map_or(0, |i| i + 1);
    let (mut score, mut at, mut last) = (0, 0, None::<usize>);
    for c in q.bytes().filter(|c| !c.is_ascii_whitespace()).map(|c| c.to_ascii_lowercase()) {
        let i = at + p[at..].iter().position(|x| x.to_ascii_lowercase() == c)?;
        score += 1;
        if last.is_some_and(|l| l + 1 == i) {
            score += 5;
        }
        if i == 0 || matches!(p[i - 1], b'/' | b'_' | b'-' | b'.' | b' ') {
            score += 4;
        }
        if i >= name {
            score += 3;
        }
        last = Some(i);
        at = i + 1;
    }
    Some(score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    Commit,
    Branch,
    Tag,
}

/// One row of results. `target` is a path in the Map, a commit id in History: both outlive a
/// rebuild, where an index wouldn't.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub kind: Kind,
    pub target: String,
    pub title: String,
    pub detail: String,
}

/// The best `LIMIT` of `scored` (score, then a tie-break, then the candidate), best first.
fn best<T: Copy>(mut scored: Vec<(i32, usize, T)>) -> Vec<(i32, usize, T)> {
    let order = |a: &(i32, usize, T), b: &(i32, usize, T)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
    if scored.len() > LIMIT {
        scored.select_nth_unstable_by(LIMIT, order);
        scored.truncate(LIMIT);
    }
    scored.sort_unstable_by(order);
    scored
}

/// The Map's files and folders matching `q`, by their path; shorter paths first on a tie.
pub fn files(m: &RepoMap, q: &str) -> Vec<Found> {
    if q.trim().is_empty() {
        return Vec::new();
    }
    let scored = m.nodes.iter().enumerate().skip(1).filter_map(|(i, n)| fuzzy(q, &n.path).map(|sc| (sc, n.path.len(), i))).collect();
    best(scored).into_iter().map(|(_, _, i)| {
        let n = &m.nodes[i];
        let folder = n.path.rsplit_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default();
        Found { kind: if n.dir { Kind::Folder } else { Kind::File }, target: n.path.clone(), title: n.name.clone(), detail: folder }
    }).collect()
}

/// What a History candidate is: a commit, or one of its refs.
#[derive(Clone, Copy)]
enum Hit {
    Commit(usize),
    Ref(usize, usize),
}

/// History's commits matching `q` by subject or author, a hash starting with it, and the
/// branches and tags whose name matches. Newer first on a tie; refs ahead of commits.
pub fn commits(list: &[Commit], q: &str) -> Vec<Found> {
    let q = q.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let hex = q.len() >= 4 && q.bytes().all(|b| b.is_ascii_hexdigit());
    let mut scored = Vec::new();
    for (i, c) in list.iter().enumerate().filter(|(_, c)| c.id != UNCOMMITTED) {
        for (r, rf) in c.refs.iter().enumerate() {
            if let Some(sc) = fuzzy(q, &rf.name) {
                // A ref names one place; ahead of commits that merely mention it.
                scored.push((sc + 20, i, Hit::Ref(i, r)));
            }
        }
        let by_hash = (hex && q.len() <= c.id.len() && c.id.as_bytes().iter().zip(q.bytes()).all(|(a, b)| *a == b.to_ascii_lowercase())).then_some(1000);
        if let Some(sc) = by_hash.or_else(|| fuzzy(q, &c.subject).max(fuzzy(q, &c.author))) {
            scored.push((sc, i, Hit::Commit(i)));
        }
    }
    best(scored).into_iter().map(|(_, _, hit)| match hit {
        Hit::Commit(i) => {
            let c = &list[i];
            Found { kind: Kind::Commit, target: c.id.clone(), title: c.subject.clone(), detail: format!("{} - {} - {}", &c.id[..c.id.len().min(7)], c.author, c.date) }
        }
        Hit::Ref(i, r) => {
            let (c, rf) = (&list[i], &list[i].refs[r]);
            let kind = if rf.kind == RefKind::Tag { Kind::Tag } else { Kind::Branch };
            Found { kind, target: c.id.clone(), title: rf.name.clone(), detail: format!("{} - {}", &c.id[..c.id.len().min(7)], c.subject) }
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Ref;

    fn commit(id: &str, subject: &str, author: &str, refs: &[(&str, RefKind)]) -> Commit {
        Commit {
            id: id.into(),
            parents: vec![],
            author: author.into(),
            email: String::new(),
            date: "09 Nov 2026 10:00".into(),
            refs: refs.iter().map(|(n, k)| Ref { name: (*n).into(), kind: *k, head: false }).collect(),
            subject: subject.into(),
        }
    }

    #[test]
    fn fuzzy_ranks_runs_and_names_and_ignores_case() {
        assert!(fuzzy("main", "src/main.rs") > fuzzy("main", "src/m_a_i_n.rs"));
        assert!(fuzzy("MAIN", "src/main.rs").is_some());
        assert!(fuzzy("view", "src/diff/view.rs") > fuzzy("view", "views/old.rs"), "the file name counts for more");
        assert_eq!(fuzzy("zz", "src/main.rs"), None);
        assert_eq!(fuzzy("", "anything"), Some(0));
    }

    #[test]
    fn history_finds_refs_hashes_subjects_and_authors() {
        let list = vec![
            commit(UNCOMMITTED, "Uncommitted changes", "", &[]),
            commit("abc1234def", "Add the map", "Ada", &[("main", RefKind::Branch), ("v1.0", RefKind::Tag)]),
            commit("ffe0001aaa", "Fix the main loop", "Grace", &[]),
        ];
        let titles = |q: &str| commits(&list, q).into_iter().map(|f| (f.kind, f.title)).collect::<Vec<_>>();
        assert_eq!(titles("main")[0], (Kind::Branch, "main".into()), "the branch first");
        assert!(titles("main").contains(&(Kind::Commit, "Fix the main loop".into())));
        assert_eq!(titles("ABC123"), [(Kind::Commit, "Add the map".into())], "a hash prefix, any case");
        assert_eq!(titles("grace"), [(Kind::Commit, "Fix the main loop".into())]);
        assert_eq!(titles("v1.0")[0], (Kind::Tag, "v1.0".into()));
        assert!(titles("Uncommitted").is_empty(), "the uncommitted row isn't a commit");
        assert!(titles("  ").is_empty());
    }

    #[test]
    fn results_stop_at_the_limit_best_first() {
        let list: Vec<Commit> = (0..500).map(|i| commit(&format!("{i:040x}"), &format!("change {i}"), "Ada", &[])).collect();
        let found = commits(&list, "change");
        assert_eq!(found.len(), LIMIT);
        assert_eq!(found[0].title, "change 0", "newest first on a tie");
    }
}

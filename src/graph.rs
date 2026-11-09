//! The commit graph: `git log` read into commits, and each commit given a lane, with the lines
//! between consecutive rows, as Git Graph (mhutchie) draws them.

/// The id the uncommitted changes go by as the graph's first row.
pub const UNCOMMITTED: &str = "*";

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub date: String,
    pub refs: Vec<Ref>,
    pub subject: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
    /// HEAD points at this branch.
    pub head: bool,
}

/// The `--format` that [`parse`] reads, with `--decorate=full`.
pub const FORMAT: &str = "%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%D%x1f%s%x1e";

/// Commits from `git log --format=FORMAT --decorate=full`, newest first.
pub fn parse(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').split('\x1f');
            let id = f.next().filter(|id| !id.is_empty())?.to_owned();
            let parents = f.next()?.split_whitespace().map(str::to_owned).collect();
            let author = f.next()?.to_owned();
            let email = f.next()?.to_owned();
            let date = f.next()?.to_owned();
            let refs = refs(f.next()?);
            let subject = f.next().unwrap_or_default().to_owned();
            Some(Commit { id, parents, author, email, date, refs, subject })
        })
        .collect()
}

/// `%D` with full names: `HEAD -> refs/heads/main, refs/remotes/origin/main, tag: refs/tags/v1`.
fn refs(d: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    for part in d.split(", ").filter(|p| !p.is_empty()) {
        let (head, name) = match part.strip_prefix("HEAD -> ") {
            Some(n) => (true, n),
            None => (false, part),
        };
        let name = name.strip_prefix("tag: ").unwrap_or(name);
        let r = if let Some(n) = name.strip_prefix("refs/heads/") {
            Ref { name: n.to_owned(), kind: RefKind::Branch, head }
        } else if let Some(n) = name.strip_prefix("refs/remotes/") {
            if n.ends_with("/HEAD") {
                continue;
            }
            Ref { name: n.to_owned(), kind: RefKind::Remote, head }
        } else if let Some(n) = name.strip_prefix("refs/tags/") {
            Ref { name: n.to_owned(), kind: RefKind::Tag, head }
        } else {
            // A detached HEAD, or a ref outside heads, remotes and tags.
            continue;
        };
        out.push(r);
    }
    // Branches first (HEAD's leading), then remotes, then tags.
    out.sort_by_key(|r| (r.kind as u8, !r.head));
    out
}

/// Where one row's commit sits and which lines leave it for the next row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// The commit's lane.
    pub col: usize,
    /// Lines from this row's middle to the next row's: (lane here, lane there).
    pub down: Vec<(usize, usize)>,
}

/// Lanes for `commits` (newest first, each after its children). A commit takes the leftmost
/// lane waiting for it, or the first free one; its first parent continues its lane, other
/// parents get lanes of their own unless one is already waiting for them.
pub fn layout(commits: &[Commit]) -> (Vec<Row>, usize) {
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows: Vec<Row> = Vec::with_capacity(commits.len());
    let mut widest = 0;
    for c in commits {
        let waiting: Vec<usize> = lanes.iter().enumerate().filter(|(_, l)| **l == Some(c.id.as_str())).map(|(j, _)| j).collect();
        let col = match waiting.first() {
            Some(&j) => j,
            None => free(&mut lanes, 0),
        };
        // Lanes that waited for this commit too end in it: their last line bends into it.
        if let Some(prev) = rows.last_mut() {
            for seg in &mut prev.down {
                if waiting.iter().skip(1).any(|&lane| lane == seg.1) {
                    seg.1 = col;
                }
            }
        }
        for &j in &waiting {
            lanes[j] = None;
        }
        let mut down: Vec<(usize, usize)> = lanes.iter().enumerate().filter(|(_, l)| l.is_some()).map(|(j, _)| (j, j)).collect();
        for (n, p) in c.parents.iter().enumerate() {
            match lanes.iter().position(|l| *l == Some(p.as_str())) {
                Some(k) if n == 0 && col < k => {
                    lanes[k] = None;
                    lanes[col] = Some(p.as_str());
                    for seg in &mut down {
                        if seg.1 == k { seg.1 = col; }
                    }
                    down.push((col, col));
                }
                Some(k) => down.push((col, k)),
                None => {
                    // The first parent keeps the lane; a merged branch opens one to the right.
                    let k = if n == 0 { col } else { free(&mut lanes, col + 1) };
                    lanes[k] = Some(p.as_str());
                    down.push((col, k));
                }
            }
        }
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
        widest = widest.max(col + 1).max(lanes.len());
        rows.push(Row { col, down });
    }
    (rows, widest)
}

/// The first free lane at or after `from`, made if there's none.
fn free(lanes: &mut Vec<Option<&str>>, from: usize) -> usize {
    if let Some(j) = (from..lanes.len()).find(|&j| lanes[j].is_none()) {
        return j;
    }
    while lanes.len() < from {
        lanes.push(None);
    }
    lanes.push(None);
    lanes.len() - 1
}

/// The commits with the working tree's changes as a row on top of HEAD, when there are any.
pub fn with_uncommitted(mut commits: Vec<Commit>, head: Option<&str>, dirty: bool) -> Vec<Commit> {
    // On top of everything, not just above HEAD: its lane runs down to HEAD like a branch's.
    if let (true, Some(h)) = (dirty, head) {
        commits.insert(
            0,
            Commit { id: UNCOMMITTED.to_owned(), parents: vec![h.to_owned()], author: String::new(), email: String::new(), date: String::new(), refs: Vec::new(), subject: "Uncommitted Changes".to_owned() },
        );
    }
    commits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, parents: &[&str]) -> Commit {
        Commit { id: id.into(), parents: parents.iter().map(|p| (*p).into()).collect(), author: String::new(), email: String::new(), date: String::new(), refs: Vec::new(), subject: String::new() }
    }

    #[test]
    fn a_straight_history_is_one_lane() {
        let (rows, w) = layout(&[c("c", &["b"]), c("b", &["a"]), c("a", &[])]);
        assert_eq!(w, 1);
        assert_eq!(rows.iter().map(|r| r.col).collect::<Vec<_>>(), [0, 0, 0]);
        assert_eq!(rows[0].down, [(0, 0)]);
        assert!(rows[2].down.is_empty());
    }

    #[test]
    fn a_merge_opens_a_lane_and_the_branch_closes_it() {
        // m merges f into the line b; f and b both come from a.
        let (rows, w) = layout(&[c("m", &["b", "f"]), c("f", &["a"]), c("b", &["a"]), c("a", &[])]);
        assert_eq!(w, 2);
        assert_eq!(rows.iter().map(|r| r.col).collect::<Vec<_>>(), [0, 1, 0, 0]);
        assert_eq!(rows[0].down, [(0, 0), (0, 1)]);
        assert_eq!(rows[1].down, [(0, 0), (1, 1)]);
        // b's row: the f lane bends into a.
        assert_eq!(rows[2].down, [(1, 0), (0, 0)]);
    }

    #[test]
    fn two_tips_on_one_parent_join() {
        // Two branch tips x and y, both on a.
        let (rows, _) = layout(&[c("x", &["a"]), c("y", &["a"]), c("a", &[])]);
        assert_eq!(rows.iter().map(|r| r.col).collect::<Vec<_>>(), [0, 1, 0]);
        // y's first parent is already waited for in lane 0: its line bends left.
        assert_eq!(rows[1].down, [(0, 0), (1, 0)]);
    }

    #[test]
    fn a_merge_crossing_another_branch_keeps_both_lines() {
        // As `git log --graph` draws it: main merges login while bugfix (from b) is still open.
        // m2 merges bugfix; m1 merges login; t is main's own commit; r the root.
        let cs = [c("m2", &["v", "x2"]), c("v", &["m1"]), c("x2", &["x1"]), c("m1", &["t", "l2"]), c("x1", &["t"]), c("l2", &["l1"]), c("t", &["r"]), c("l1", &["r"]), c("r", &[])];
        let (rows, w) = layout(&cs);
        assert_eq!(w, 3);
        assert_eq!(rows.iter().map(|r| r.col).collect::<Vec<_>>(), [0, 0, 1, 0, 1, 2, 0, 2, 0]);
        // m1 opens the login lane to the right of bugfix's, and bugfix's line runs on past it.
        assert_eq!(rows[3].down, [(1, 1), (0, 0), (0, 2)]);
        // x1 ends in t, which main is already waiting for: its line bends into lane 0.
        assert_eq!(rows[4].down, [(0, 0), (2, 2), (1, 0)]);
        // The root takes every lane still open.
        assert_eq!(rows[7].down, [(0, 0), (2, 0)]);
    }

    #[test]
    fn uncommitted_changes_sit_on_top_and_run_to_head() {
        let cs = with_uncommitted(vec![c("x", &["h"]), c("h", &[])], Some("h"), true);
        assert_eq!(cs[0].id, UNCOMMITTED);
        let (rows, _) = layout(&cs);
        // Its own lane, then a line into HEAD's.
        assert_eq!(rows[0].col, 0);
        assert_eq!(rows[2].col, 0);
        assert!(with_uncommitted(vec![c("h", &[])], Some("h"), false).len() == 1, "a clean tree adds no row");
    }

    #[test]
    fn refs_parse_with_head_first() {
        let r = refs("HEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1, refs/heads/dev");
        let names: Vec<(&str, RefKind, bool)> = r.iter().map(|r| (r.name.as_str(), r.kind, r.head)).collect();
        assert_eq!(names, [("main", RefKind::Branch, true), ("dev", RefKind::Branch, false), ("origin/main", RefKind::Remote, false), ("v1", RefKind::Tag, false)]);
    }

    #[test]
    fn log_output_parses() {
        let out = "a1\x1fb2 c3\x1fAda\x1fada@x\x1f09 Nov 2026 10:00\x1fHEAD -> refs/heads/main\x1fMerge dev\x1e\nb2\x1f\x1fAda\x1fada@x\x1f08 Nov 2026 09:00\x1f\x1fFirst\x1e";
        let cs = parse(out);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].parents, ["b2", "c3"]);
        assert_eq!(cs[0].refs[0].name, "main");
        assert!(cs[1].parents.is_empty());
        assert_eq!(cs[1].subject, "First");
    }
}

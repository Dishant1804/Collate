//! A three-way merge of a file: what the two sides changed from their common base, the
//! changes only one side made taken as they are, and where both changed the same lines, a
//! conflict the reader settles line by line.

use crate::diff::myers::{self, Op};

/// Where a run of the merged file came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum From {
    /// Unchanged on both sides, or changed the same way on both.
    Both,
    /// Only this branch changed it.
    Ours,
    /// Only the incoming side changed it.
    Theirs,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    /// Lines the merge settled by itself.
    Clean { lines: Vec<String>, from: From },
    /// Both sides changed these lines differently: the base's, each side's, and which of each
    /// side's lines the reader keeps (`None` until they pick).
    Conflict { base: Vec<String>, ours: Vec<String>, theirs: Vec<String>, pick: Option<Pick> },
}

/// The lines kept from each side of a conflict, ours before theirs unless `theirs_first`;
/// or with `base`, the base's lines instead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pick {
    pub ours: Vec<bool>,
    pub theirs: Vec<bool>,
    pub theirs_first: bool,
    pub base: bool,
}

/// A whole side, or both, for a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// Theirs, then ours.
    BothTheirsFirst,
    /// Neither: the base's lines.
    Base,
}

/// A file being merged: where the result goes and the merge so far.
#[derive(Clone, Debug)]
pub struct Merge {
    /// The file's path as shown, relative to the repository when it's in one.
    pub path: String,
    pub chunks: Vec<Chunk>,
    /// The conflict being read, by its place among the conflicts.
    pub current: usize,
    /// Where `git mergetool` asked for the result, or `None` for a file in this repository's
    /// merge, staged once saved.
    pub output: Option<std::path::PathBuf>,
}

/// Lines with their endings, so the merge writes back what it read byte for byte.
fn lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_owned).collect()
}

/// For each line of `a`, the line of `b` it matched, if it's unchanged.
fn matches(a: &[String], b: &[String]) -> Vec<Option<usize>> {
    let mut out = vec![None; a.len()];
    for op in myers::diff(a, b) {
        if let Op::Equal { old, new, len } = op {
            for i in 0..len {
                out[old + i] = Some(new + i);
            }
        }
    }
    out
}

/// Merges `ours` and `theirs`, both changed from `base`: lines unchanged on both sides are
/// anchors, and between two anchors a run changed on one side only takes that side's lines,
/// the same change on both is taken once, and anything else is a conflict.
pub fn merge(base: &str, ours: &str, theirs: &str) -> Vec<Chunk> {
    let (base, ours, theirs) = (lines(base), lines(ours), lines(theirs));
    let (to_ours, to_theirs) = (matches(&base, &ours), matches(&base, &theirs));
    let mut chunks: Vec<Chunk> = Vec::new();
    let push = |lines: &[String], from: From, chunks: &mut Vec<Chunk>| {
        if lines.is_empty() {
            return;
        }
        if let Some(Chunk::Clean { lines: l, from: f }) = chunks.last_mut()
            && *f == from
        {
            l.extend_from_slice(lines);
        } else {
            chunks.push(Chunk::Clean { lines: lines.to_vec(), from });
        }
    };
    let (mut i, mut o, mut t) = (0, 0, 0);
    while i < base.len() || o < ours.len() || t < theirs.len() {
        if i < base.len() && to_ours[i] == Some(o) && to_theirs[i] == Some(t) {
            push(&base[i..=i], From::Both, &mut chunks);
            (i, o, t) = (i + 1, o + 1, t + 1);
            continue;
        }
        // The next base line both sides kept ends this run.
        let next = (i..base.len()).find_map(|j| Some((j, to_ours[j]?, to_theirs[j]?)));
        let (j, oj, tj) = next.unwrap_or((base.len(), ours.len(), theirs.len()));
        let (b, ou, th) = (&base[i..j], &ours[o..oj], &theirs[t..tj]);
        if ou == b {
            push(th, if th == b { From::Both } else { From::Theirs }, &mut chunks);
        } else if th == b || ou == th {
            push(ou, if th == b { From::Ours } else { From::Both }, &mut chunks);
        } else {
            chunks.push(Chunk::Conflict { base: b.to_vec(), ours: ou.to_vec(), theirs: th.to_vec(), pick: None });
        }
        (i, o, t) = (j, oj, tj);
    }
    chunks
}

impl Pick {
    pub fn take(take: Take, ours: usize, theirs: usize) -> Pick {
        let (o, t) = match take {
            Take::Ours => (true, false),
            Take::Theirs => (false, true),
            Take::Both | Take::BothTheirsFirst => (true, true),
            Take::Base => (false, false),
        };
        Pick { ours: vec![o; ours], theirs: vec![t; theirs], theirs_first: take == Take::BothTheirsFirst, base: take == Take::Base }
    }
}

impl Chunk {
    /// The lines this chunk puts in the merged file; `None` for a conflict not settled yet.
    pub fn result(&self) -> Option<Vec<&str>> {
        match self {
            Chunk::Clean { lines, .. } => Some(lines.iter().map(String::as_str).collect()),
            Chunk::Conflict { base, ours, theirs, pick } => {
                let p = pick.as_ref()?;
                if p.base {
                    return Some(base.iter().map(String::as_str).collect());
                }
                fn kept<'a>(lines: &'a [String], keep: &[bool]) -> Vec<&'a str> {
                    lines.iter().zip(keep).filter(|(_, k)| **k).map(|(l, _)| l.as_str()).collect()
                }
                let (o, t) = (kept(ours, &p.ours), kept(theirs, &p.theirs));
                Some(if p.theirs_first { [t, o].concat() } else { [o, t].concat() })
            }
        }
    }
}

impl Merge {
    pub fn new(path: String, base: &str, ours: &str, theirs: &str, output: Option<std::path::PathBuf>) -> Merge {
        Merge { path, chunks: merge(base, ours, theirs), current: 0, output }
    }

    /// The chunk index of each conflict, in order.
    pub fn conflicts(&self) -> Vec<usize> {
        self.chunks.iter().enumerate().filter(|(_, c)| matches!(c, Chunk::Conflict { .. })).map(|(i, _)| i).collect()
    }

    /// How many conflicts are still waiting for a pick.
    pub fn unsettled(&self) -> usize {
        self.chunks.iter().filter(|c| matches!(c, Chunk::Conflict { pick: None, .. })).count()
    }

    /// How many runs the merge took from one side by itself.
    pub fn auto_merged(&self) -> usize {
        self.chunks.iter().filter(|c| matches!(c, Chunk::Clean { from: From::Ours | From::Theirs, .. })).count()
    }

    /// Settles conflict `n` (by its place among the conflicts) with a whole side, or both.
    pub fn take(&mut self, n: usize, take: Take) {
        if let Some(&c) = self.conflicts().get(n)
            && let Chunk::Conflict { ours, theirs, pick, .. } = &mut self.chunks[c]
        {
            *pick = Some(Pick::take(take, ours.len(), theirs.len()));
        }
    }

    /// Keeps or drops one line of conflict `n`: line `i` of ours, or of theirs. The first
    /// line picked in an unsettled conflict starts from nothing kept.
    pub fn toggle(&mut self, n: usize, theirs_side: bool, i: usize) {
        if let Some(&c) = self.conflicts().get(n)
            && let Chunk::Conflict { ours, theirs, pick, .. } = &mut self.chunks[c]
        {
            let p = pick.get_or_insert_with(|| Pick::take(Take::Base, ours.len(), theirs.len()));
            p.base = false;
            if let Some(k) = if theirs_side { p.theirs.get_mut(i) } else { p.ours.get_mut(i) } {
                *k = !*k;
            }
        }
    }

    /// The merged file, or `None` while a conflict is unsettled.
    pub fn text(&self) -> Option<String> {
        let mut out = String::new();
        for c in &self.chunks {
            for l in c.result()? {
                out.push_str(l);
            }
        }
        Some(out)
    }

    /// Moves to the next (or previous) conflict, wrapping, preferring unsettled ones.
    pub fn step(&mut self, forward: bool) {
        let n = self.conflicts().len();
        if n == 0 {
            return;
        }
        let order = (1..=n).map(|d| if forward { (self.current + d) % n } else { (self.current + n * 2 - d) % n });
        let unsettled = |k: usize| self.conflicts().get(k).is_some_and(|&c| matches!(self.chunks[c], Chunk::Conflict { pick: None, .. }));
        let order: Vec<usize> = order.collect();
        self.current = order.iter().copied().find(|&k| unsettled(k)).unwrap_or(order[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_on_one_side_merge_and_the_same_lines_changed_twice_conflict() {
        let base = "a\nb\nc\nd\nx\ne\n";
        let ours = "a\nB\nc\nd\nx\nE1\n";
        let theirs = "a\nb\nc\nD\nx\nE2\n";
        let m = Merge::new("f".into(), base, ours, theirs, None);
        assert_eq!(m.auto_merged(), 2, "{:?}", m.chunks);
        assert_eq!(m.conflicts().len(), 1);
        assert_eq!(m.text(), None, "a conflict waits for a pick");
        let mut m2 = m.clone();
        m2.take(0, Take::Ours);
        assert_eq!(m2.text().unwrap(), "a\nB\nc\nD\nx\nE1\n");
        m2.take(0, Take::BothTheirsFirst);
        assert_eq!(m2.text().unwrap(), "a\nB\nc\nD\nx\nE2\nE1\n");
        m2.take(0, Take::Base);
        assert_eq!(m2.text().unwrap(), "a\nB\nc\nD\nx\ne\n");
        // Picking lines starts from none kept.
        let mut m3 = m;
        m3.toggle(0, true, 0);
        assert_eq!(m3.text().unwrap(), "a\nB\nc\nD\nx\nE2\n");
        m3.toggle(0, false, 0);
        assert_eq!(m3.text().unwrap(), "a\nB\nc\nD\nx\nE1\nE2\n");
    }

    #[test]
    fn the_same_change_on_both_sides_is_taken_once() {
        let m = Merge::new("f".into(), "a\nb\n", "a\nx\n", "a\nx\n", None);
        assert!(m.conflicts().is_empty());
        assert_eq!(m.text().unwrap(), "a\nx\n");
        let m = Merge::new("f".into(), "", "new\n", "other\n", None);
        assert_eq!(m.conflicts().len(), 1, "both added a file differently");
    }

    #[test]
    fn next_conflict_prefers_unsettled_ones_and_wraps() {
        let m = |s: &str| s.replace(' ', "\n") + "\n";
        let mut g = Merge::new("f".into(), &m("a 1 b 2 c 3 d"), &m("a x b y c z d"), &m("a X b Y c Z d"), None);
        assert_eq!(g.conflicts().len(), 3);
        g.take(1, Take::Ours);
        g.step(true);
        assert_eq!(g.current, 2, "skips the settled one");
        g.step(true);
        assert_eq!(g.current, 0, "wraps");
        g.step(false);
        assert_eq!(g.current, 2);
    }
}

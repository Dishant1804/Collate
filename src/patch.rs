//! Patches for one hunk of a [`FileDiff`], or chosen lines of it, for `git apply`: what
//! stages, unstages or discards a single change.

use crate::diff::{FileDiff, LineKind, Side};

/// A patch holding hunk `h` of `file` alone, against the file's old side.
#[cfg(test)]
pub fn hunk(file: &FileDiff, h: usize) -> Option<String> {
    lines(file, h, None, false)
}

/// A patch for the `picked` lines of hunk `h` (all of them when `None`), as `git add -p`'s
/// line editing makes one.
///
/// Applied forwards (staging), an unpicked removal stays as context and an unpicked addition
/// is left out. Applied in `reverse` (unstaging, discarding), the patch is read from the new
/// side, so the opposite: an unpicked addition stays as context and an unpicked removal is
/// left out. `None` when nothing is picked.
pub fn lines(file: &FileDiff, h: usize, picked: Option<&[bool]>, reverse: bool) -> Option<String> {
    let hunk = file.hunks.get(h)?;
    // Lines are kept without their breaks; a CRLF file needs its `\r` back or git won't match.
    let crlf = [Side::New, Side::Old].into_iter().filter_map(|s| file.text(s)).any(|t| t.contains("\r\n"));
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut body = String::new();
    let (mut old_count, mut new_count, mut changes) = (0u32, 0u32, 0);
    let on = |i: usize| picked.is_none_or(|p| p.get(i).copied().unwrap_or(false));
    let mut emit = |mark: char, i: usize| {
        let l = &hunk.lines[i];
        match mark {
            ' ' => {
                old_count += 1;
                new_count += 1;
            }
            '-' => {
                old_count += 1;
                changes += 1;
            }
            _ => {
                new_count += 1;
                changes += 1;
            }
        }
        body.push(mark);
        body.push_str(&l.text);
        body.push_str(if l.no_newline { "\n" } else { eol });
        if l.no_newline {
            body.push_str("\\ No newline at end of file\n");
        }
    };
    let lines = &hunk.lines;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            emit(' ', i);
            i += 1;
            continue;
        }
        // A run of changes: its removals and additions, each in order. They go out in pairs
        // (the k-th removal, then the k-th addition) so a picked replacement lands where the
        // line it replaces was, not after the lines left unpicked around it.
        let start = i;
        while i < lines.len() && lines[i].kind != LineKind::Context {
            i += 1;
        }
        let removed: Vec<usize> = (start..i).filter(|&j| lines[j].kind == LineKind::Removed).collect();
        let added: Vec<usize> = (start..i).filter(|&j| lines[j].kind == LineKind::Added).collect();
        for k in 0..removed.len().max(added.len()) {
            if let Some(&j) = removed.get(k) {
                match (on(j), reverse) {
                    (true, _) => emit('-', j),
                    // Kept: forwards it stays as context; in reverse it isn't on the side read.
                    (false, false) => emit(' ', j),
                    (false, true) => {}
                }
            }
            if let Some(&j) = added.get(k) {
                match (on(j), reverse) {
                    (true, _) => emit('+', j),
                    (false, false) => {}
                    (false, true) => emit(' ', j),
                }
            }
        }
    }
    if changes == 0 {
        return None;
    }
    // The side the patch is matched against keeps every one of its lines, so its start is the
    // hunk's; the other side's start follows from it (a count of 0 names the line before).
    let (old_start, new_start) = if reverse {
        let n = hunk.new_start;
        let o = match (old_count, new_count) {
            (0, _) => n.saturating_sub(1),
            (_, 0) => n + 1,
            _ => n,
        };
        (o, n)
    } else {
        let o = hunk.old_start;
        let n = match (old_count, new_count) {
            (0, _) => o + 1,
            (_, 0) => o.saturating_sub(1),
            _ => o,
        };
        (o, n)
    };
    let path = file.path();
    let old = file.old_path.as_deref().unwrap_or(path);
    let new = file.new_path.as_deref().unwrap_or(path);
    let mut p = format!("diff --git a/{old} b/{new}\n");
    match (&file.old_path, &file.new_path) {
        (None, _) => p.push_str(&format!("new file mode 100644\n--- /dev/null\n+++ b/{new}\n")),
        (_, None) => p.push_str(&format!("deleted file mode 100644\n--- a/{old}\n+++ /dev/null\n")),
        _ => p.push_str(&format!("--- a/{old}\n+++ b/{new}\n")),
    }
    let range = |start: u32, count: u32| if count == 1 { format!("{start}") } else { format!("{start},{count}") };
    p.push_str(&format!("@@ -{} +{} @@\n", range(old_start, old_count), range(new_start, new_count)));
    p.push_str(&body);
    Some(p)
}

/// A patch that changes one line of hunk `h` part of the way: removed line `r` and added line
/// `a` become one replacement, with the line's text set to `target`. Forwards (staging) the
/// old line becomes `target`; in `reverse` (unstaging, discarding) the new line does.
pub fn line_to(file: &FileDiff, h: usize, r: usize, a: usize, target: &str, reverse: bool) -> Option<String> {
    let mut f = file.clone();
    let hunk = std::sync::Arc::make_mut(&mut f.hunks).get_mut(h)?;
    let edited = if reverse { r } else { a };
    hunk.lines.get_mut(edited)?.text = target.to_owned();
    let picked: Vec<bool> = (0..hunk.lines.len()).map(|i| i == r || i == a).collect();
    lines(&f, h, Some(&picked), reverse)
}

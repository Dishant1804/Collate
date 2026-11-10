//! Fuzzy matching: how well what's typed matches a name or a path, scored without
//! allocating, for the file palette and the project switcher.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranks_runs_and_names_and_ignores_case() {
        assert!(fuzzy("main", "src/main.rs") > fuzzy("main", "src/m_a_i_n.rs"));
        assert!(fuzzy("MAIN", "src/main.rs").is_some());
        assert!(fuzzy("view", "src/diff/view.rs") > fuzzy("view", "views/old.rs"), "the file name counts for more");
        assert_eq!(fuzzy("zz", "src/main.rs"), None);
        assert_eq!(fuzzy("", "anything"), Some(0));
    }
}

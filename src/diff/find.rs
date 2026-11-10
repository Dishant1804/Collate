//! The diff's search: plain text or a regular expression, matched with or without case, as
//! whole words or anywhere.

use std::ops::Range;

/// What the search looks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub text: String,
    /// Match case exactly.
    pub case: bool,
    /// Only matches with no letter, digit or `_` on either side.
    pub word: bool,
    /// `text` is a regular expression.
    pub regex: bool,
}

/// A [`Query`] ready to run.
#[derive(Clone, Debug)]
pub struct Finder {
    query: Query,
    re: Option<regex::Regex>,
    /// The text, lowercased when case doesn't matter.
    needle: String,
}

impl Finder {
    /// `None` for an empty query; an error for a regular expression that doesn't parse.
    pub fn new(query: Query) -> Result<Option<Finder>, String> {
        if query.text.is_empty() {
            return Ok(None);
        }
        let re = if query.regex {
            let pattern = if query.word { format!(r"\b(?:{})\b", query.text) } else { query.text.clone() };
            Some(regex::RegexBuilder::new(&pattern).case_insensitive(!query.case).build().map_err(|e| e.to_string().lines().last().unwrap_or("Invalid pattern").trim().to_owned())?)
        } else {
            None
        };
        let needle = if query.case { query.text.clone() } else { query.text.to_ascii_lowercase() };
        Ok(Some(Finder { query, re, needle }))
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// Byte ranges of the matches in `text`.
    pub fn find_all(&self, text: &str) -> Vec<Range<usize>> {
        if let Some(re) = &self.re {
            return re.find_iter(text).filter(|m| !m.is_empty()).map(|m| m.range()).collect();
        }
        if self.needle.len() > text.len() {
            return Vec::new();
        }
        let hay = if self.query.case { std::borrow::Cow::Borrowed(text) } else { std::borrow::Cow::Owned(text.to_ascii_lowercase()) };
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        hay.match_indices(&self.needle)
            .map(|(i, m)| i..i + m.len())
            .filter(|r| !self.query.word || (!word(text[..r.start].chars().next_back()) && !word(text[r.end..].chars().next())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, q: &str, case: bool, word: bool, regex: bool) -> Vec<Range<usize>> {
        Finder::new(Query { text: q.into(), case, word, regex }).unwrap().unwrap().find_all(text)
    }

    #[test]
    fn plain_text_matches_with_or_without_case_and_as_words() {
        let t = "Value value values";
        assert_eq!(find(t, "value", false, false, false), [0..5, 6..11, 12..17]);
        assert_eq!(find(t, "value", true, false, false), [6..11, 12..17]);
        assert_eq!(find(t, "value", false, true, false), [0..5, 6..11]);
        assert_eq!(find("a_value value", "value", false, true, false), vec![8..13], "an underscore joins words");
    }

    #[test]
    fn regular_expressions_take_the_same_options() {
        let t = "let x1 = X2;";
        assert_eq!(find(t, r"x\d", false, false, true), [4..6, 9..11]);
        assert_eq!(find(t, r"x\d", true, false, true), vec![4..6]);
        assert_eq!(find("ab abc", "ab", false, true, true), vec![0..2]);
        assert!(Finder::new(Query { text: "(".into(), regex: true, ..Default::default() }).is_err(), "a bad pattern is an error");
        assert!(Finder::new(Query::default()).unwrap().is_none());
    }
}

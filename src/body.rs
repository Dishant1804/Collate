//! GitHub comment bodies as Collate draws them. GitHub renders the HTML a body may hold, and
//! bots (CodeRabbit, Linear, Vercel) write much of theirs in it; the kit's Markdown leaves HTML
//! out. So the HTML GitHub would draw becomes the Markdown that draws the same, `<details>`
//! becomes a fold of its own, and GitHub's alerts (`> [!NOTE]`) get their title.

/// A piece of a body: Markdown, or a `<details>` fold with its summary and what it hides.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Fold { summary: String, open: bool, inner: Vec<Part> },
}

/// `body` as Markdown and folds.
pub fn parse(body: &str) -> Vec<Part> {
    parts(&unquote_folds(&strip_comments(body)))
}

/// Quoted lines from a quote's first `<details>` on, unquoted. A fold is drawn as a part of its
/// own, so the `> ` its lines carry inside a quote (CodeRabbit's "Outside diff range comments")
/// would stay behind as marks; the quote's lines before it keep theirs.
fn unquote_folds(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut folded = false;
    for line in s.split_inclusive('\n') {
        let t = line.trim_start();
        if !t.starts_with('>') {
            folded = false;
            out.push_str(line);
            continue;
        }
        folded = folded || t.to_ascii_lowercase().contains("<details");
        if folded {
            let rest = &t[1..];
            out.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Whether `parts` read whole at a glance: a few lines of text and no folds.
pub fn short(parts: &[Part]) -> bool {
    match parts {
        [] => true,
        [Part::Text(t)] => t.len() <= 280 && t.lines().filter(|l| !l.trim().is_empty()).count() <= 3,
        _ => false,
    }
}

/// What `parts` say, in a line: the first line of words, past alert titles and headings that
/// only name a section, or else the first fold's summary.
pub fn gist(parts: &[Part]) -> String {
    let mut summary = None;
    for part in parts {
        match part {
            Part::Text(t) => {
                for line in t.lines() {
                    let line = words(line);
                    if line.split_whitespace().count() >= 3 {
                        return line;
                    }
                }
            }
            Part::Fold { summary: s, .. } => {
                summary.get_or_insert_with(|| s.clone());
            }
        }
    }
    summary.unwrap_or_default()
}

/// A Markdown line as plain words: quote, heading and list marks, emphasis and link addresses
/// left out.
fn words(line: &str) -> String {
    let t = line.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '>' | '#' | '-' | '+'));
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' | '~' | '[' => {}
            // `](address)`: the link's words stay, its address goes.
            ']' if chars.peek() == Some(&'(') => {
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The byte ranges of `s`'s fenced code blocks, where HTML is code to show, not to draw.
fn fences(s: &str) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut open: Option<(usize, &str)> = None;
    let mut at = 0;
    for line in s.split_inclusive('\n') {
        let t = line.trim_start();
        let mark = if t.starts_with("```") { Some("```") } else if t.starts_with("~~~") { Some("~~~") } else { None };
        match (open, mark) {
            (None, Some(m)) => open = Some((at, m)),
            (Some((start, m)), Some(n)) if m == n => {
                out.push((start, at + line.len()));
                open = None;
            }
            _ => {}
        }
        at += line.len();
    }
    if let Some((start, _)) = open {
        out.push((start, s.len()));
    }
    out
}

/// The first `needle` (lowercase) in `s` from `from` on, ignoring case and fenced code.
fn find(lower: &str, from: usize, needle: &str, fenced: &[(usize, usize)]) -> Option<usize> {
    let mut i = from;
    while let Some(n) = lower.get(i..)?.find(needle) {
        let at = i + n;
        if !fenced.iter().any(|&(a, b)| at >= a && at < b) {
            return Some(at);
        }
        i = at + needle.len();
    }
    None
}

/// `<!-- … -->` left out, outside code: GitHub never draws them.
fn strip_comments(s: &str) -> String {
    let fenced = fences(s);
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    while let Some(open) = find(s, at, "<!--", &fenced) {
        out.push_str(&s[at..open]);
        at = s[open..].find("-->").map_or(s.len(), |n| open + n + 3);
    }
    out.push_str(&s[at..]);
    out
}

fn parts(s: &str) -> Vec<Part> {
    let lower = s.to_ascii_lowercase();
    let fenced = fences(s);
    let mut out = vec![];
    let mut at = 0;
    let text = |out: &mut Vec<Part>, t: &str| {
        let md = markdown(t);
        // Marks with no words, like the `<h6>` CodeRabbit wraps its folds in, draw nothing.
        if md.chars().any(|c| !c.is_whitespace() && !matches!(c, '#' | '>' | '*' | '_' | '~')) {
            out.push(Part::Text(md));
        }
    };
    while let Some(open) = find(&lower, at, "<details", &fenced) {
        let Some(gt) = s[open..].find('>') else { break };
        let attrs = &lower[open + 8..open + gt];
        // Not `<detailsfoo>`.
        if !(attrs.is_empty() || attrs.starts_with(char::is_whitespace)) {
            text(&mut out, &s[at..open + gt + 1]);
            at = open + gt + 1;
            continue;
        }
        let start = open + gt + 1;
        // Its own `</details>`, past any folds inside it; an unclosed one runs to the end.
        let (mut depth, mut i, mut end, mut after) = (1, start, s.len(), s.len());
        loop {
            let (o, c) = (find(&lower, i, "<details", &fenced), find(&lower, i, "</details>", &fenced));
            match (o, c) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    i = o + 8;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    i = c + 10;
                    if depth == 0 {
                        (end, after) = (c, c + 10);
                        break;
                    }
                }
                (_, None) => break,
            }
        }
        text(&mut out, &s[at..open]);
        let inner = &s[start..end];
        let inner_lower = &lower[start..end];
        let summary_at = inner_lower.find("<summary").filter(|&n| inner[..n].trim().is_empty());
        let (summary, rest) = match summary_at.and_then(|n| Some((n, inner[n..].find('>')? + n + 1))) {
            Some((_, from)) => match inner_lower[from..].find("</summary>") {
                Some(len) => (&inner[from..from + len], &inner[from + len + 10..]),
                None => (&inner[from..], ""),
            },
            None => ("Details", inner),
        };
        out.push(Part::Fold { summary: plain(summary), open: attrs.contains("open"), inner: parts(rest) });
        at = after;
    }
    text(&mut out, &s[at..]);
    out
}

/// A summary as one line of text: its tags and Markdown marks left out.
fn plain(s: &str) -> String {
    let md = markdown(s);
    let t: String = md.chars().filter(|c| !matches!(c, '*' | '`' | '_')).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { "Details".to_owned() } else { t }
}

/// GitHub's alert kinds, by the word in `> [!WORD]`.
const ALERTS: [(&str, &str); 5] = [("note", "Note"), ("tip", "Tip"), ("important", "Important"), ("warning", "Warning"), ("caution", "Caution")];

/// The HTML GitHub draws, as Markdown; code (fenced or in backticks) as it is.
fn markdown(s: &str) -> String {
    let fenced = fences(s);
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for &(a, b) in fenced.iter().chain([(s.len(), s.len())].iter()) {
        out.push_str(&prose(&s[at..a]));
        out.push_str(&s[a..b]);
        at = b;
    }
    out
}

/// [`markdown`] for text with no fences in it.
fn prose(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Links open with `<a href>` and close with `</a>`; the address waits for the close.
    let mut links: Vec<Option<String>> = vec![];
    // Just after an opening `**`, `*` or `~~`: Markdown won't open one that a space follows, and
    // `<strong><img> Major</strong>` leaves one there.
    let mut opened = false;
    let b = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = b[i];
        // An inline code span, kept whole.
        if c == b'`' {
            let run = s[i..].bytes().take_while(|&x| x == b'`').count();
            let ticks = &s[i..i + run];
            let close = s[i + run..].find(ticks).map(|n| i + run + n + run);
            let end = close.unwrap_or(i + run);
            out.push_str(&s[i..end]);
            i = end;
            continue;
        }
        // An alert's first line.
        if c == b'['
            && s[i..].starts_with("[!")
            && out.trim_end_matches([' ', '\t']).ends_with('>')
            && let Some(close) = s[i..].find(']')
            && let Some((_, title)) = ALERTS.iter().find(|(w, _)| w.eq_ignore_ascii_case(&s[i + 2..i + close]))
        {
            out.push_str(&format!("**{title}**"));
            i += close + 1;
            continue;
        }
        if c == b'<'
            && let Some((len, md)) = tag(&s[i..], &mut links)
        {
            let mark = matches!(md.as_str(), "**" | "*" | "~~");
            if mark && s[i..].starts_with("</") {
                // Nor close one a space comes before.
                out.truncate(out.trim_end_matches([' ', '\t']).len());
            }
            if md.is_empty() {
                // `**<img> Major**`: a picture left out after a written opening `**`.
                let before = out.trim_end_matches(['*', '_']);
                opened |= before.len() < out.len() && before.chars().next_back().is_none_or(char::is_whitespace);
            } else {
                opened = mark && !s[i..].starts_with("</");
            }
            out.push_str(&md);
            i += len;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        if opened && (ch == ' ' || ch == '\t') {
            i += 1;
            continue;
        }
        opened = false;
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The HTML tag at the start of `s`, if it's one GitHub draws: its length and its Markdown.
/// Anything else, `Vec<String>` say, stays text.
fn tag(s: &str, links: &mut Vec<Option<String>>) -> Option<(usize, String)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    if inner.contains('<') || inner.contains('\n') && inner.len() > 400 {
        return None;
    }
    let closing = inner.starts_with('/');
    let body = inner.trim_start_matches('/').trim_end_matches('/');
    let name: String = body.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    let attr = |key: &str| -> Option<String> {
        let lower = body.to_ascii_lowercase();
        let at = lower.find(&format!("{key}="))? + key.len() + 1;
        let rest = &body[at..];
        let q = rest.chars().next()?;
        if q == '"' || q == '\'' {
            rest[1..].find(q).map(|n| rest[1..1 + n].to_owned())
        } else {
            Some(rest.split(|c: char| c.is_whitespace()).next()?.to_owned())
        }
    };
    let md = match (name.as_str(), closing) {
        ("br", _) => "  \n".to_owned(),
        ("hr", _) => "\n\n---\n\n".to_owned(),
        ("p" | "div" | "blockquote" | "table" | "tr" | "ul" | "ol" | "summary" | "details", _) => "\n\n".to_owned(),
        ("li", false) => "\n- ".to_owned(),
        ("li", true) => "\n".to_owned(),
        ("td" | "th", false) => " ".to_owned(),
        ("b" | "strong", _) => "**".to_owned(),
        ("i" | "em", _) => "*".to_owned(),
        ("s" | "del" | "strike", _) => "~~".to_owned(),
        ("code" | "kbd", _) => "`".to_owned(),
        ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => format!("\n\n{} ", "#".repeat(name[1..].parse().unwrap_or(3))),
        ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => "\n\n".to_owned(),
        ("a", false) => {
            let href = attr("href");
            let md = if href.is_some() { "[" } else { "" };
            links.push(href);
            md.to_owned()
        }
        ("a", true) => match links.pop().flatten() {
            Some(href) => format!("]({href})"),
            None => String::new(),
        },
        // Pictures the app has no way to fetch: their words, where they have any.
        ("img", _) => attr("alt").filter(|a| !a.trim().is_empty()).unwrap_or_default(),
        ("span" | "sub" | "sup" | "small" | "picture" | "source" | "thead" | "tbody" | "td" | "th" | "u" | "ins" | "font" | "center" | "dl" | "dt" | "dd", _) => String::new(),
        _ => return None,
    };
    Some((end + 1, md))
}

#[cfg(test)]
mod tests {
    use super::{Part, parse};

    fn text(s: &str) -> Part {
        Part::Text(s.to_owned())
    }

    #[test]
    fn details_fold_with_their_summary_and_nest() {
        let body = "Actionable comments posted: 2\n\n<details>\n<summary>🧹 Nitpick comments (1)</summary><blockquote>\n\n<details>\n<summary><b>a.rs</b> (1)</summary>\n\nUse `<T>` here.\n\n</details>\n\n</blockquote></details>\n\nEnd";
        let parts = parse(body);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], text("Actionable comments posted: 2\n\n"));
        let Part::Fold { summary, open, inner } = &parts[1] else { panic!("{parts:?}") };
        assert_eq!((summary.as_str(), *open), ("🧹 Nitpick comments (1)", false));
        let Some(Part::Fold { summary, inner, .. }) = inner.iter().find(|p| matches!(p, Part::Fold { .. })) else { panic!("{inner:?}") };
        assert_eq!(summary, "a.rs (1)");
        assert!(matches!(&inner[..], [Part::Text(t)] if t.trim() == "Use `<T>` here."), "{inner:?}");
        assert!(matches!(&parts[2], Part::Text(t) if t.trim() == "End"));
    }

    #[test]
    fn html_becomes_the_markdown_that_draws_the_same() {
        let parts = parse("<!-- hidden -->Hi<br>there <b>bold</b> <a href=\"https://x.dev\">link</a> <img src=\"a.png\" alt=\"badge\">\n\n```html\n<b>kept</b>\n```\n");
        assert_eq!(parts, [text("Hi  \nthere **bold** [link](https://x.dev) badge\n\n```html\n<b>kept</b>\n```\n")]);
        assert_eq!(parse("> [!CAUTION]\n> Careful"), [text("> **Caution**\n> Careful")]);
        assert_eq!(parse("a Vec<String> b"), [text("a Vec<String> b")], "only the tags GitHub draws");
        assert!(matches!(&parse("<details open><summary>S</summary>x</details>")[..], [Part::Fold { open: true, .. }]));
        assert!(matches!(&parse("<details><summary>S</summary>never closed")[..], [Part::Fold { inner, .. }] if inner == &[text("never closed")]));
        assert_eq!(parse("<strong><picture><img alt=\"\"></picture> Major</strong> - **<img alt=\"\"> Minor**"), [text("**Major** - **Minor**")]);
    }

    #[test]
    fn a_long_body_reads_as_its_first_line_of_words() {
        use super::{gist, short};
        let review = parse("> [!WARNING]\n> GitHub reported that **this** review needs [access](https://x.dev).\n\n<details><summary>Prompt</summary>x</details>");
        assert!(!short(&review));
        assert_eq!(gist(&review), "GitHub reported that this review needs access.");
        assert_eq!(gist(&parse("## Summary\n\n<details><summary>Walkthrough</summary>x</details>")), "Walkthrough");
        assert!(short(&parse("Looks good, thanks!")));
    }

    #[test]
    fn a_fold_in_a_quote_and_an_empty_heading_leave_no_marks() {
        let body = "> [!CAUTION]\n> Outside the diff\n>\n> <details>\n> <summary>a.yml</summary>\n>\n> Fix it.\n>\n> </details>\n\n<h6>\n<details><summary>Review info</summary>x</details>\n</h6>";
        let parts = parse(body);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], text("> **Caution**\n> Outside the diff\n>\n"));
        assert!(matches!(&parts[1], Part::Fold { summary, inner, .. } if summary == "a.yml" && inner == &[text("\n\nFix it.\n\n")]), "{parts:?}");
        assert!(matches!(&parts[2], Part::Fold { summary, .. } if summary == "Review info"), "{parts:?}");
    }
}


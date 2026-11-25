//! Editing the diff's lines in place: a click on a new-side line opens it in a field, and
//! leaving the field writes it back to the file in the working tree. Only where the new side
//! is that file (All Changes, Unstaged).
use crate::git::Scope;
use crate::{Collate, current_repo, refresh};

impl Collate {
    /// Whether the diff shown has the working tree as its new side, so its lines can be edited.
    pub fn can_edit_lines(&self) -> bool {
        self.loose.is_none() && matches!(self.scope, Scope::Uncommitted | Scope::Unstaged)
    }

    /// Saves the line being edited once its field has lost focus; true when it did.
    pub fn settle_line_edit(&mut self) -> bool {
        let settle = self.diff.editing().is_some() && !self.diff.line_edit.focused;
        if settle {
            self.save_line_edit();
        }
        settle
    }

    /// Writes the edited line over the one the edit started from, if the file still holds it
    /// there, and closes the field. Left open and focused when it can't, so the text isn't lost.
    pub fn save_line_edit(&mut self) {
        let Some(e) = self.diff.editing() else { return };
        let text = self.diff.line_edit.text.clone();
        if text == e.base {
            self.diff.end_edit();
            return;
        }
        let (path, line) = (e.path.clone(), e.line);
        let file = current_repo().map(|repo| repo.root.join(&path));
        let spliced = file
            .as_ref()
            .ok_or_else(|| "No project is open, so nothing was written".to_owned())
            .and_then(|f| std::fs::read_to_string(f).map_err(|err| format!("Couldn't read {path}: {err}")))
            .and_then(|old| splice(&old, line, &e.base, &text).ok_or_else(|| format!("{path} changed on disk since line {line} was opened, so nothing was written")));
        match spliced.and_then(|new| std::fs::write(file.as_ref().expect("read above"), new).map_err(|err| format!("Couldn't write {path}: {err}"))) {
            Ok(()) => {
                self.diff.end_edit();
                self.say(format!("Saved line {line} of {path}"), false);
                refresh();
            }
            Err(e) => {
                self.diff.line_edit.focused = true;
                self.say(e, true);
            }
        }
    }
}

/// `old` with its line `n` (from 1) replaced by the lines of `text`, each ending as the file's
/// lines do and the last as the replaced one did. `None` when that line isn't `base` any more.
fn splice(old: &str, n: u32, base: &str, text: &str) -> Option<String> {
    let lines: Vec<&str> = old.split_inclusive('\n').collect();
    let a = (n as usize).checked_sub(1)?;
    let held = *lines.get(a)?;
    let body = held.strip_suffix('\n').map_or(held, |l| l.strip_suffix('\r').unwrap_or(l));
    if body != base {
        return None;
    }
    let end = &held[body.len()..];
    let eol = std::iter::once(held).chain(lines.iter().copied()).map(|l| &l[l.trim_end_matches(['\r', '\n']).len()..]).find(|e| !e.is_empty()).unwrap_or("\n");
    let mut out = String::with_capacity(old.len() + text.len());
    out.extend(lines[..a].iter().copied());
    for (k, line) in text.split('\n').enumerate() {
        if k > 0 {
            out.push_str(eol);
        }
        out.push_str(line.strip_suffix('\r').unwrap_or(line));
    }
    out.push_str(end);
    out.extend(lines[a + 1..].iter().copied());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::splice;

    #[test]
    fn an_edited_line_replaces_its_own_and_keeps_the_files_line_ends() {
        assert_eq!(splice("a\nb\nc\n", 2, "b", "B\nB2").as_deref(), Some("a\nB\nB2\nc\n"));
        assert_eq!(splice("a\r\nb\r\nc\r\n", 1, "a", "x\ny").as_deref(), Some("x\r\ny\r\nb\r\nc\r\n"));
        // The last line has no line break: neither does what replaces it.
        assert_eq!(splice("a\nb", 2, "b", "b\nmore").as_deref(), Some("a\nb\nmore"));
        assert_eq!(splice("a\nb\n", 1, "a", "").as_deref(), Some("\nb\n"));
    }

    #[test]
    fn a_line_changed_since_the_edit_began_is_left_alone() {
        assert_eq!(splice("a\nX\nc\n", 2, "b", "B"), None);
        assert_eq!(splice("a\n", 3, "c", "C"), None);
        assert_eq!(splice("a\n", 0, "a", "A"), None);
    }
}

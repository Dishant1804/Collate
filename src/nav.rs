//! Back and forward through the places the reader has been, as a browser does: a view, the
//! map at a folder, a commit's diff. Every way of moving (the sidebar, shortcuts, the map, a
//! file list) counts, because a move is noticed after it happens rather than where it's made.
use crate::{Scope, Collate};

#[derive(Clone, PartialEq)]
pub struct Place {
    scope: Scope,
    /// The map's folder, while the map is open.
    map: Option<String>,
    /// The commit whose diff fills History.
    commit: Option<String>,
}

#[derive(Default)]
pub struct Nav {
    back: Vec<Place>,
    forward: Vec<Place>,
    here: Option<Place>,
}

/// Places remembered behind the current one.
const KEEP: usize = 100;

impl Collate {
    fn place(&self) -> Place {
        let commit = if self.scope == Scope::History && self.history_diff && !self.map.open { self.commit.clone() } else { None };
        Place { scope: self.scope, map: self.map.open.then(|| self.map.zoom.clone()), commit }
    }

    /// Notes a move since the last call: the place left goes on the back list, and forward is
    /// cleared. True when there was one.
    pub fn note_place(&mut self) -> bool {
        let now = self.place();
        match self.nav.here.replace(now.clone()) {
            Some(was) if was != now => {
                self.nav.back.push(was);
                if self.nav.back.len() > KEEP {
                    self.nav.back.remove(0);
                }
                self.nav.forward.clear();
                true
            }
            _ => false,
        }
    }

    pub fn can_go(&self, back: bool) -> bool {
        !if back { &self.nav.back } else { &self.nav.forward }.is_empty()
    }

    pub fn go(&mut self, back: bool) {
        // A move not yet noted is where this one leaves from.
        self.note_place();
        let Some(to) = (if back { self.nav.back.pop() } else { self.nav.forward.pop() }) else { return };
        if let Some(here) = self.nav.here.take() {
            if back { self.nav.forward.push(here) } else { self.nav.back.push(here) }
        }
        self.map.open = to.map.is_some();
        if let Some(zoom) = &to.map {
            self.map.zoom = zoom.clone();
            self.map.hover = None;
            if self.map.data.is_none() {
                self.build_map();
            }
        }
        self.set_scope(to.scope);
        if to.scope == Scope::History {
            if let Some(c) = &to.commit {
                self.set_commit(c);
            }
            self.history_diff = to.commit.is_some();
        }
        self.nav.here = Some(self.place());
    }

    /// A project switch: the places were in the last one.
    pub fn forget_places(&mut self) {
        self.nav = Nav::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_forward_retrace_moves() {
        let mut s = crate::init();
        s.scope = Scope::Uncommitted;
        s.note_place();
        s.set_scope(Scope::Staged);
        s.note_place();
        s.map.open = true;
        s.map.zoom = "src".into();
        s.note_place();
        assert!(s.can_go(true) && !s.can_go(false));
        s.go(true);
        assert!(!s.map.open && s.scope == Scope::Staged, "back to Staged");
        s.go(true);
        assert_eq!(s.scope, Scope::Uncommitted, "back to where it started");
        assert!(!s.can_go(true));
        s.go(false);
        s.go(false);
        assert!(s.map.open && s.map.zoom == "src", "forward to the map's folder");
        // A new move drops what was ahead.
        s.go(true);
        s.set_scope(Scope::Unstaged);
        s.note_place();
        assert!(!s.can_go(false));
    }
}

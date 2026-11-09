//! The Map place: the whole repository as nested rectangles sized by lines, the view's changed
//! files outlined in their status colour, and, under the pointer, lines to what a file imports
//! and what imports it. A folder zooms in on a click; a changed file opens its diff.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use rune::{Cursor, El, LineCap, Path, PointerEvent, Shape, Tx, canvas, col, row, text};
use rune_kit::Kit;

use crate::diff::FileStatus;
use crate::map::{self, Cell, RepoMap, TITLE_H};
use crate::{COLUMN_TOP, HANDLE, PANEL_INSET, Collate, current_repo, message, reveal};

/// The map's state in the window.
#[derive(Default)]
pub struct MapState {
    pub open: bool,
    pub data: Option<Arc<RepoMap>>,
    pub error: Option<String>,
    /// The latest build asked for; an older one landing late is dropped.
    generation: u64,
    pub building: bool,
    /// The folder shown, by path (empty: the root), so it survives a rebuild.
    pub zoom: String,
    pub hover: Option<usize>,
    /// The last layout, for drawing and for hit-testing the pointer.
    cells: RefCell<Option<Laid>>,
}

struct Laid {
    key: (u64, usize, i32, i32),
    cells: Rc<Vec<Cell>>,
}

/// Labels drawn at most: past this the small ones are left to hovering.
const MAX_LABELS: usize = 500;

impl Collate {
    pub fn open_map(&mut self) {
        self.map.open = true;
        self.map.hover = None;
        self.build_map();
    }

    /// Reads the repository again, off the UI thread when there's a window. Files unchanged
    /// since the last build aren't read again, so this is cheap after the first time.
    pub fn build_map(&mut self) {
        let Some(repo) = current_repo() else { return };
        self.map.generation += 1;
        self.map.building = true;
        let generation = self.map.generation;
        let root = repo.root.clone();
        let land = move |s: &mut Collate, result: Result<RepoMap, String>| {
            if s.map.generation != generation {
                return;
            }
            s.map.building = false;
            s.map.hover = None;
            match result {
                Ok(m) => {
                    s.map.data = Some(Arc::new(m));
                    s.map.error = None;
                }
                Err(e) => s.map.error = Some(e),
            }
        };
        match HANDLE.get().cloned() {
            Some(app) => {
                std::thread::spawn(move || {
                    let result = map::build(&root);
                    app.update(move |s, _| land(s, result));
                });
            }
            None => land(self, map::build(&root)),
        }
    }

    /// A project switch: what was read belongs to the last one.
    pub fn reset_map(&mut self) {
        self.map.data = None;
        self.map.error = None;
        self.map.zoom.clear();
        self.map.hover = None;
        if self.map.open {
            self.build_map();
        }
    }

    fn map_cells(&self) -> Rc<Vec<Cell>> {
        self.map.cells.borrow().as_ref().map(|l| l.cells.clone()).unwrap_or_default()
    }

    fn map_zoom_node(&self, m: &RepoMap) -> usize {
        m.by_path.get(&self.map.zoom).copied().filter(|&i| m.nodes[i].dir).unwrap_or(0)
    }

    fn map_point(&mut self, p: PointerEvent) {
        let hover = map::hit(&self.map_cells(), p.x, p.y);
        if hover != self.map.hover {
            self.map.hover = hover;
        }
    }

    /// A folder zooms in; a file with changes in this view opens its diff.
    fn map_press(&mut self, tx: &mut Tx) {
        let (Some(m), Some(node)) = (self.map.data.clone(), self.map.hover) else { return };
        let n = &m.nodes[node];
        if n.dir {
            self.map.zoom = n.path.clone();
            self.map.hover = None;
            return;
        }
        if self.diff.file_index(&n.path).is_some() {
            self.map.open = false;
            let path = n.path.clone();
            self.select_in_lists(&path);
            reveal(self, &path, tx);
        } else {
            self.say(format!("{} has no changes in {}", n.name, self.scope_title()), false);
        }
    }

    /// Where `path` is drawn, in the map's own points.
    #[cfg(test)]
    pub fn map_rect(&self, path: &str) -> Option<[f32; 4]> {
        let node = *self.map.data.as_ref()?.by_path.get(path)?;
        self.map_cells().iter().find(|c| c.node == node).map(|c| c.rect)
    }

    /// Escape: up one folder. False at the top, so the key goes on.
    pub fn map_up(&mut self) -> bool {
        if self.map.zoom.is_empty() {
            return false;
        }
        self.map.zoom = self.map.zoom.rsplit_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default();
        self.map.hover = None;
        true
    }
}

/// `98k`, `1.2k`, `840`.
fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn map_view(s: &Collate, k: &Kit) -> El {
    let Some(m) = s.map.data.clone() else {
        return match &s.map.error {
            Some(e) => message(k, "Can't map this repository", e),
            None => message(k, "Mapping the repository…", ""),
        };
    };
    let zoom = s.map_zoom_node(&m);
    // This view's changes, by path: added and removed lines and what happened to the file.
    let changes: HashMap<&str, (usize, usize, FileStatus)> = s.diff.files().iter().map(|f| (f.path(), (f.additions(), f.deletions(), f.status))).collect();
    let mut changed_dirs: HashSet<usize> = HashSet::new();
    for p in changes.keys() {
        if let Some(&i) = m.by_path.get(*p) {
            let mut at = m.nodes[i].parent;
            while let Some(d) = at {
                if !changed_dirs.insert(d) {
                    break;
                }
                at = m.nodes[d].parent;
            }
        }
    }
    col().key("map").w_full().grow(1.0).min_h(0.0).fill(k.c.bg_base)
        .child(header(s, k, &m, zoom, &changes))
        .child(stage(s, k, &m, zoom, &changes, &changed_dirs))
}


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

/// Two 28-point bands, as the diff's summary has: where you are and what's in it, then what's
/// under the pointer.
fn header(s: &Collate, k: &Kit, m: &RepoMap, zoom: usize, changes: &HashMap<&str, (usize, usize, FileStatus)>) -> El {
    let mono = rune::TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono };
    let mut crumbs = row().key("map-crumbs").gap(6.0).items_center().min_w(0.0).shrink();
    let trail = m.trail(zoom);
    for (n, &node) in trail.iter().enumerate() {
        let last = n + 1 == trail.len();
        if n > 0 {
            crumbs = crumbs.child(text("/".to_owned()).font(k.ty.ui).color(k.c.text_faint));
        }
        let name = if node == 0 { s.name.clone() } else { m.nodes[node].name.clone() };
        let label = text(name.clone()).font(if last { k.ty.ui_medium } else { k.ty.ui }).color(if last { k.c.text_base } else { k.c.text_muted }).truncate().min_w(0.0);
        crumbs = crumbs.child(if last {
            label
        } else {
            let path = m.nodes[node].path.clone();
            label.cursor(Cursor::Pointer).role("link").label(format!("Show {name}")).on_press(move |s: &mut Collate| {
                s.map.zoom = path.clone();
                s.map.hover = None;
            })
        });
    }
    let here = &m.nodes[zoom];
    let changed_in = changes.keys().filter(|p| m.by_path.get(**p).is_some_and(|&i| m.within(i, zoom))).count();
    let stats = if zoom == 0 {
        format!("{} lines - {} files - {} changed", short(here.lines), m.files, changed_in)
    } else {
        format!("{} lines - {} changed", short(here.lines), changed_in)
    };
    let legend = row().key("map-legend").gap(4.0).items_center()
        .child(text("hover:".to_owned()).font(k.ty.text_small).color(k.c.text_faint))
        .child(text("imports".to_owned()).font(k.ty.text_small).color(k.c.info.icon))
        .child(text("/".to_owned()).font(k.ty.text_small).color(k.c.text_faint))
        .child(text("imported by".to_owned()).font(k.ty.text_small).color(k.c.success.icon));
    let top = row().key("map-top").w_full().h(28.0).gap(12.0).items_center()
        .child(crumbs)
        .child(text(stats).key("map-stats").font(mono).color(k.c.text_muted).truncate().min_w(0.0).shrink())
        .child(row().grow(1.0))
        .child(legend);
    let note = match s.map.hover.filter(|&h| h < m.nodes.len()) {
        Some(h) => {
            let n = &m.nodes[h];
            let mut r = row().gap(10.0).items_center().min_w(0.0).child(text(n.path.clone()).font(mono).color(k.c.text_base).truncate().min_w(0.0).shrink());
            r = r.child(text(format!("{} lines", short(n.lines))).font(k.ty.text_small).color(k.c.text_muted));
            if let Some(&(a, d, _)) = changes.get(n.path.as_str()) {
                r = r.child(text(format!("+{a}")).font(mono).color(k.c.success.icon)).child(text(format!("\u{2212}{d}")).font(mono).color(k.c.danger.icon));
            }
            if n.dir {
                r.child(text("click to open".to_owned()).font(k.ty.text_small).color(k.c.text_faint))
            } else {
                r.child(text(format!("imports {} - imported by {}", m.imports[h].len(), m.imported_by[h].len())).font(k.ty.text_small).color(k.c.text_muted))
            }
        }
        None => row().child(
            text(if zoom == 0 { "Click a folder to open it, a changed file to see its diff" } else { "Click a folder to open it, a changed file to see its diff - Esc goes up" }.to_owned())
                .font(k.ty.text_small)
                .color(k.c.text_faint)
                .truncate()
                .min_w(0.0),
        ),
    };
    // The same inset as the diff's summary, so the map's first band lines up with the
    // sidebar's and the panel's first rows.
    col().key("map-header").w_full().px(16.0).pt(COLUMN_TOP)
        .child(top)
        .child(row().key("map-note").w_full().h(28.0).items_center().child(note.key("map-hover")))
}

fn stage(s: &Collate, k: &Kit, m: &Arc<RepoMap>, zoom: usize, changes: &HashMap<&str, (usize, usize, FileStatus)>, changed_dirs: &HashSet<usize>) -> El {
    let size = rune::memo::layout_with("map-canvas", |r| (r[2], r[3]));
    let mut area = col().key("map-canvas").w_full().grow(1.0).min_h(0.0).clip_rounded(0.0)
        .cursor(Cursor::Pointer)
        .role("img")
        .label("Repository map")
        .on_pointer_move_local(|s: &mut Collate, p: PointerEvent, _: &mut Tx| s.map_point(p))
        .on_hover_change(|s: &mut Collate, inside: bool, _: &mut Tx| {
            if !inside {
                s.map.hover = None;
            }
        })
        .on_press_tx(|s: &mut Collate, tx: &mut Tx| s.map_press(tx));
    let Some((w, h)) = size.filter(|(w, h)| *w > 20.0 && *h > 20.0) else {
        return col().key("map-area").w_full().grow(1.0).min_h(0.0).px(PANEL_INSET).pb(PANEL_INSET).child(area);
    };
    let key = (Arc::as_ptr(m) as u64, zoom, w as i32, h as i32);
    let cells = {
        let mut cache = s.map.cells.borrow_mut();
        match cache.as_ref().filter(|l| l.key == key) {
            Some(l) => l.cells.clone(),
            None => {
                let cells = Rc::new(map::layout(m, zoom, w, h));
                *cache = Some(Laid { key, cells: cells.clone() });
                cells
            }
        }
    };
    let index: HashMap<usize, usize> = cells.iter().enumerate().map(|(i, c)| (c.node, i)).collect();
    let hover = s.map.hover.filter(|h| index.contains_key(h));
    let (imports, importers): (HashSet<usize>, HashSet<usize>) = match hover.filter(|&h| !m.nodes[h].dir) {
        Some(h) => (m.imports[h].iter().copied().collect(), m.imported_by[h].iter().copied().collect()),
        None => Default::default(),
    };
    let c = &k.c;
    let status_color = |st: FileStatus| match st {
        FileStatus::Added | FileStatus::Copied => c.success.icon,
        FileStatus::Deleted => c.danger.icon,
        FileStatus::Modified | FileStatus::Renamed => c.warning.icon,
    };
    let mut shape = Shape::new();
    let mut labels: Vec<El> = Vec::new();
    let mono = rune::TextStyle { size: 10.0, line_height: 13.0, ..k.ty.mono };
    for cell in cells.iter() {
        let n = &m.nodes[cell.node];
        let [x, y, cw, ch] = cell.rect;
        let outline = rounded(cell.rect, if cw > 8.0 && ch > 8.0 { 3.0 } else { 1.0 });
        let change = changes.get(n.path.as_str()).copied();
        if n.dir {
            shape = shape.fill(outline.clone(), k.alpha(c.text_base, 0.025));
            let edge = if changed_dirs.contains(&cell.node) { k.alpha(c.warning.icon, 0.35) } else { k.alpha(c.text_base, 0.09) };
            shape = shape.stroke_with(outline, true, 1.0, LineCap::Round, edge);
        } else {
            let tint = if imports.contains(&cell.node) {
                Some(c.info.icon)
            } else if importers.contains(&cell.node) {
                Some(c.success.icon)
            } else {
                change.map(|ch| status_color(ch.2))
            };
            let fill = match (tint, change) {
                (Some(t), _) if imports.contains(&cell.node) || importers.contains(&cell.node) => k.alpha(t, 0.30),
                (Some(t), Some(_)) => k.alpha(t, 0.16),
                _ => k.alpha(c.text_base, 0.07),
            };
            shape = shape.fill(outline.clone(), fill);
            if let Some(t) = change.map(|ch| status_color(ch.2)) {
                shape = shape.stroke_with(outline, true, 1.0, LineCap::Round, k.alpha(t, 0.9));
            }
        }
        if labels.len() >= MAX_LABELS {
            continue;
        }
        let name_color = if change.is_some() || changed_dirs.contains(&cell.node) { c.warning.icon } else { c.text_muted };
        if n.dir && cell.titled {
            labels.push(
                row().absolute(x + 5.0, y + 1.0).w(cw - 10.0).h(TITLE_H).gap(5.0).items_center().a11y_hidden()
                    .child(text(n.name.clone()).font(k.ty.text_xsmall_medium).color(name_color).truncate().min_w(0.0).shrink())
                    .child(text(short(n.lines)).font(k.ty.text_xsmall).color(c.text_faint)),
            );
        } else if !n.dir && cw >= 46.0 && ch >= 17.0 {
            let mut l = col().absolute(x + 4.0, y + 2.0).w(cw - 8.0).gap(1.0).a11y_hidden()
                .child(text(n.name.clone()).font(k.ty.text_xsmall).color(if change.is_some() { c.text_base } else { c.text_muted }).truncate().w_full());
            if ch >= 32.0 {
                l = l.child(match change {
                    Some((a, d, _)) => text(format!("+{a} \u{2212}{d}")).font(mono).color(name_color).truncate().w_full(),
                    None => text(format!("{} lines", short(n.lines))).font(mono).color(c.text_faint).truncate().w_full(),
                });
            }
            labels.push(l);
        }
    }
    // Lines from the hovered file to what it imports (blue) and what imports it (green). A
    // file too small to draw at this zoom is reached at its nearest drawn folder.
    if let Some(h) = hover {
        let center = |node: usize| -> Option<[f32; 2]> {
            let mut at = Some(node);
            while let Some(n) = at {
                if let Some(&i) = index.get(&n) {
                    let r = cells[i].rect;
                    return Some([r[0] + r[2] / 2.0, r[1] + r[3] / 2.0]);
                }
                at = m.nodes[n].parent.filter(|&p| m.within(p, zoom) && p != zoom);
            }
            None
        };
        let from = center(h).unwrap_or([0.0, 0.0]);
        let r = cells[index[&h]].rect;
        shape = shape.stroke_with(rounded(r, 3.0), true, 1.5, LineCap::Round, c.text_base);
        if !m.nodes[h].dir {
            for (targets, color) in [(&m.imports[h], c.info.icon), (&m.imported_by[h], c.success.icon)] {
                for &t in targets {
                    let Some(to) = center(t) else { continue };
                    // Bowed to one side, so lines to neighbours don't lie on top of each other.
                    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
                    let bow = 0.25;
                    let c1 = [from[0] + dx * 0.25 - dy * bow, from[1] + dy * 0.25 + dx * bow];
                    let c2 = [from[0] + dx * 0.75 - dy * bow, from[1] + dy * 0.75 + dx * bow];
                    shape = shape.stroke_path(&Path::new().move_to(from).cubic_to(c1, c2, to), 1.5, LineCap::Round, k.alpha(color, 0.85));
                    shape = shape.fill(circle(to, 2.5), color);
                }
            }
            shape = shape.fill(circle(from, 5.0), c.bg_base).stroke_with(circle(from, 4.0), true, 2.0, LineCap::Round, c.text_base);
        }
    }
    area = area.child(canvas(shape.view_box(w, h)).pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).a11y_hidden());
    for l in labels {
        area = area.child(l);
    }
    col().key("map-area").w_full().grow(1.0).min_h(0.0).px(PANEL_INSET).pb(PANEL_INSET).child(area)
}

/// A rectangle with corners rounded by `r`, as a polygon.
fn rounded(rect: [f32; 4], r: f32) -> Vec<[f32; 2]> {
    let [x, y, w, h] = rect;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r < 0.5 {
        return vec![[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
    }
    let mut pts = Vec::with_capacity(16);
    let corners = [([x + w - r, y + r], -90.0f32), ([x + w - r, y + h - r], 0.0), ([x + r, y + h - r], 90.0), ([x + r, y + r], 180.0)];
    for (c, start) in corners {
        for step in 0..=3 {
            let a = (start + step as f32 * 30.0).to_radians();
            pts.push([c[0] + r * a.cos(), c[1] + r * a.sin()]);
        }
    }
    pts
}

fn circle(c: [f32; 2], r: f32) -> Vec<[f32; 2]> {
    (0..24).map(|i| {
        let a = i as f32 * std::f32::consts::TAU / 24.0;
        [c[0] + r * a.cos(), c[1] + r * a.sin()]
    }).collect()
}

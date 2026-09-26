//! Capture: what each mutation batch removes, changes and mounts, read before the mirror forgets it.

use super::scan::*;
use super::*;
use crate::time::Timestamp;
use crate::{
    recording::rrweb::{Added, AttrValue, Mutation, NodeId},
    replay::{
        Mirror,
        grid::{row_of, row_snapshot, rows_in},
        is_non_visual,
    },
};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

impl GestureWindow {
    /// Before a mutation batch applies: what it removes, and the prior value of what it changes.
    pub fn before_batch(&mut self, mirror: &Mirror, mutation: &Mutation, at: Timestamp) {
        self.track_touched_rows(mirror, mutation, at);
        self.capture_removals(mirror, mutation, at);
        self.capture_text_changes(mirror, mutation, at);
        self.capture_attribute_changes(mirror, mutation, at);
    }

    /// Rows the batch touches, with their cells before it (on first touch). A cell replaced
    /// whole has the row as its parent, so rows are tracked, not cells.
    fn track_touched_rows(&mut self, mirror: &Mirror, mutation: &Mutation, at: Timestamp) {
        self.touched_rows.clear();
        let touched = mutation
            .texts
            .iter()
            .map(|text| text.id)
            .chain(mutation.removes.iter().map(|remove| remove.parent))
            .chain(mutation.adds.iter().map(|add| add.parent));
        for id in touched {
            let Some(row) = row_of(mirror, id) else {
                continue;
            };
            if is_offscreen(mirror, row) {
                continue;
            }
            if self.touched_rows.contains(&row) {
                continue;
            }
            self.touched_rows.push(row);
            if !self.rows.contains_key(&row) && !self.born.contains(&row) && mirror.is_attached(row)
            {
                let start = row_snapshot(mirror, &self.grid, row);
                self.rows.insert(
                    row,
                    RowTrack {
                        current: start.texts_by_slot(),
                        start,
                        first_change: HashMap::default(),
                        last_change: HashMap::default(),
                        seen: at,
                    },
                );
            }
        }
    }

    fn capture_removals(&mut self, mirror: &Mirror, mutation: &Mutation, at: Timestamp) {
        let removing: HashSet<NodeId> = mutation.removes.iter().map(|remove| remove.id).collect();
        for remove in &mutation.removes {
            // Detached content (its parent never arrived) was never rendered.
            if !mirror.is_attached(remove.id) || is_offscreen(mirror, remove.id) {
                continue;
            }
            // Leaves with a removed ancestor, or was never on screen.
            let covered = mirror
                .ancestors(remove.id)
                .any(|a| removing.contains(&a.id) || is_non_visual(&a.tag));
            if covered {
                continue;
            }
            // Subtrees mounted in this window leave with it, by identity. Most windows have no
            // mount still on the page, and walking every removed subtree for them is wasted work.
            if self.mounts.values().any(|mount| mount.departed.is_none()) {
                for id in mirror.subtree(remove.id) {
                    if let Some(mount) = self.mounts.get_mut(&id)
                        && mount.departed.is_none()
                    {
                        mount.departed = Some((at, scan_subtree(mirror, id)));
                    }
                }
            }
            self.changed(at);
            if self.born.contains(&remove.id) {
                continue;
            }
            let departed = self.summarize_departure(mirror, remove.id, remove.parent);
            self.departures.insert(
                remove.id,
                Departure {
                    at,
                    parent: remove.parent,
                    departed,
                },
            );
        }
    }

    fn summarize_departure(&self, mirror: &Mirror, id: NodeId, parent: NodeId) -> Departed {
        let scan = scan_subtree(mirror, id);
        if scan.overlay.is_none() {
            let rows: Vec<_> = rows_in(mirror, id)
                .into_iter()
                .map(|row| match self.rows.get(&row) {
                    Some(track) => track.start.clone(),
                    None => row_snapshot(mirror, &self.grid, row),
                })
                .collect();
            if !rows.is_empty() {
                return Departed::Rows(rows);
            }
            if row_of(mirror, parent).is_some() {
                return Departed::InRow;
            }
        }
        Departed::Piece(scan, Content::of(mirror, id))
    }

    fn capture_text_changes(&mut self, mirror: &Mirror, mutation: &Mutation, at: Timestamp) {
        for change in &mutation.texts {
            let Some(node) = mirror.get(change.id) else {
                continue;
            };
            let is_noop = node.text == change.value;
            if !mirror.is_attached(change.id) || is_offscreen(mirror, change.id) || is_noop {
                continue;
            }
            // A reaction even on a node mounted this window (a label that appeared, then changed).
            self.changed(at);
            if self.born.contains(&change.id) {
                continue;
            }
            self.texts
                .entry(change.id)
                .and_modify(|track| track.end = at)
                .or_insert_with(|| TextTrack {
                    before: node.text.clone(),
                    at,
                    end: at,
                });
        }
    }

    fn capture_attribute_changes(&mut self, mirror: &Mirror, mutation: &Mutation, at: Timestamp) {
        for change in &mutation.attributes {
            let Some(node) = mirror.get(change.id) else {
                continue;
            };
            if !mirror.is_attached(change.id) || is_offscreen(mirror, change.id) {
                continue;
            }
            for (name, value) in &change.attributes.0 {
                let is_restyle = name == "class" && self.restyle_targets.contains(&change.id);
                if !is_state_attribute(name) && !is_restyle {
                    continue;
                }
                let before = node.attribute(name).and_then(AttrValue::compared_text);
                if before == value.compared_text() {
                    continue; // no-op write
                }
                self.changed(at);
                if self.born.contains(&change.id) {
                    continue;
                }
                self.attributes
                    .entry((change.id, name.clone()))
                    .and_modify(|track| track.end = at)
                    .or_insert_with(|| AttributeTrack {
                        before,
                        at,
                        end: at,
                    });
            }
        }
    }

    /// After the batch applies: what it mounted, and how the rows it touched now read.
    pub fn after_batch(&mut self, mirror: &Mirror, added: &[Added], at: Timestamp) {
        self.capture_mounts(mirror, added, at);
        self.capture_row_changes(mirror, at);
    }

    /// rrweb lists every added node flat, each with its parent: only a node whose parent
    /// existed before the window roots a mount; the rest are read as part of their root.
    fn capture_mounts(&mut self, mirror: &Mirror, adds: &[Added], at: Timestamp) {
        let added: HashSet<NodeId> = adds.iter().map(|add| add.id).collect();
        for add in adds {
            let inside_mount = added.contains(&add.parent) || self.born.contains(&add.parent);
            let id = add.id;
            let Some(node) = mirror.get(id) else {
                continue;
            };
            self.born.extend(mirror.subtree(id));
            if !mirror.is_attached(id) || is_non_visual(&node.tag) || is_offscreen(mirror, id) {
                continue;
            }
            self.changed(at);
            if inside_mount {
                continue;
            }
            match self.mounts.get_mut(&id) {
                Some(mount) => {
                    mount.parent = add.parent;
                    mount.departed = None;
                }
                None => {
                    self.mounts.insert(
                        id,
                        Mount {
                            at,
                            parent: add.parent,
                            departed: None,
                        },
                    );
                }
            }
        }
    }

    fn capture_row_changes(&mut self, mirror: &Mirror, at: Timestamp) {
        let mut any_changed = false;
        for id in &self.touched_rows {
            let Some(track) = self.rows.get_mut(id) else {
                continue;
            };
            if !mirror.is_attached(*id) {
                continue;
            }
            track.seen = at;
            let now = row_snapshot(mirror, &self.grid, *id).texts_by_slot();
            let slots: HashSet<String> = track.current.keys().chain(now.keys()).cloned().collect();
            for slot in slots {
                let text = now.get(&slot).cloned().unwrap_or_default();
                if track.current.get(&slot).map_or("", String::as_str) == text {
                    continue;
                }
                let differs_from_start = track.start.cell(&slot).map_or("", |c| &c.text) != text;
                if differs_from_start {
                    track.first_change.entry(slot.clone()).or_insert(at);
                }
                track.last_change.insert(slot.clone(), at);
                track.current.insert(slot, text);
                any_changed = true;
            }
        }
        if any_changed {
            self.changed(at);
        }
    }
}

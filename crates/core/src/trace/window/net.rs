//! Netting: what the captured changes amount to when the window closes.

use super::scan::*;
use super::*;
use crate::time::Timestamp;
use crate::trace::effect::{Change, Effect, OverlayOp, Presence, TextOp};
use crate::{
    recording::rrweb::{AttrValue, NodeId},
    replay::{
        Mirror,
        grid::{RowKey, RowSnapshot, row_of, row_snapshot, rows_in},
    },
    text::collapse_whitespace,
};
use indexmap::{IndexMap, IndexSet};
use rustc_hash::{FxBuildHasher, FxHashSet as HashSet};

/// A subtree leaving or arriving, for netting re-renders by location.
struct Piece {
    op: Presence,
    /// The attach point: where the subtree was (or is) in the page.
    group: NodeId,
    at: Timestamp,
    id: NodeId,
    scan: Scan,
    content: Content,
}

/// A row that left or arrived.
#[derive(Clone)]
struct MovedRow {
    snapshot: RowSnapshot,
    at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RowIdentity {
    Key(String),
    Label(String),
}

fn identity(row: &RowKey) -> (String, RowIdentity) {
    let identity = match &row.key {
        Some(key) => RowIdentity::Key(key.clone()),
        None => RowIdentity::Label(row.label.clone()),
    };
    (row.table.clone(), identity)
}

impl GestureWindow {
    /// Describe what the tracked nodes net to, and start tracking afresh. Called when the window
    /// closes, and before a full snapshot replaces the mirror mid-window.
    pub fn flush(&mut self, mirror: &Mirror) {
        let mut out = Vec::new();
        let mut left_rows = Vec::new();
        let mut arrived_rows = Vec::new();
        let mut pieces = Vec::new();

        for (&id, departure) in &self.departures {
            match &departure.departed {
                Departed::Rows(rows) => left_rows.extend(rows.iter().map(|snapshot| MovedRow {
                    snapshot: snapshot.clone(),
                    at: departure.at,
                })),
                Departed::InRow => {}
                Departed::Piece(scan, content) => pieces.push(Piece {
                    op: Presence::Remove,
                    group: self.attach_point(departure.parent),
                    at: departure.at,
                    id,
                    scan: scan.clone(),
                    content: content.clone(),
                }),
            }
        }
        self.collect_mounts(mirror, &mut arrived_rows, &mut pieces, &mut out);
        self.net_pieces(pieces, &mut out);
        let left_ids: HashSet<NodeId> = left_rows.iter().map(|row| row.snapshot.id).collect();
        self.match_rows(left_rows, arrived_rows, &mut out);
        self.diff_rows_in_place(mirror, &left_ids, &mut out);
        self.diff_texts(mirror, &mut out);
        self.diff_attributes(mirror, &mut out);

        self.effects.extend(out);
        self.born.clear();
        self.mounts.clear();
        self.departures.clear();
        self.texts.clear();
        self.attributes.clear();
        self.rows.clear();
    }

    pub fn close(&mut self, mirror: &Mirror) {
        if !self.closed {
            self.flush(mirror);
            self.closed = true;
        }
    }

    /// An attach point that itself left in the window stands for where it was attached.
    fn attach_point(&self, mut parent: NodeId) -> NodeId {
        for _ in 0..MAX_ATTACH_POINT_HOPS {
            match self.departures.get(&parent) {
                Some(departure) => parent = departure.parent,
                None => break,
            }
        }
        parent
    }

    fn collect_mounts(
        &self,
        mirror: &Mirror,
        arrived_rows: &mut Vec<MovedRow>,
        pieces: &mut Vec<Piece>,
        out: &mut Vec<Effect>,
    ) {
        for (&id, mount) in &self.mounts {
            if mirror.is_attached(id) {
                let scan = scan_subtree(mirror, id);
                if scan.overlay.is_none() {
                    let rows = rows_in(mirror, id);
                    if !rows.is_empty() {
                        arrived_rows.extend(rows.into_iter().map(|row| MovedRow {
                            snapshot: row_snapshot(mirror, &self.grid, row),
                            at: mount.at,
                        }));
                        continue;
                    }
                }
                // In a row: its cell diff says what the text did; controls and overlays still count.
                let content = if row_of(mirror, id).is_some() {
                    Content::empty()
                } else {
                    Content::of(mirror, id)
                };
                pieces.push(Piece {
                    op: Presence::Add,
                    group: self.attach_point(mount.parent),
                    at: mount.at,
                    id,
                    scan,
                    content,
                });
                continue;
            }
            // Mounted and gone inside the window: a toast that came and went is a confirmation.
            let Some((gone, scan)) = &mount.departed else {
                continue;
            };
            let Some(overlay) = &scan.overlay else {
                continue;
            };
            if is_toast_role(&overlay.role)
                && !overlay.screen_reader_only
                && !overlay.title.is_empty()
            {
                out.push(Effect::spanning(
                    self.relative(mount.at),
                    self.relative(*gone),
                    Change::Overlay {
                        op: OverlayOp::Open,
                        role: overlay.role.clone(),
                        title: overlay.title.clone(),
                        node: overlay.node,
                        lived_ms: Some(*gone - mount.at),
                    },
                ));
            }
        }
    }

    /// Re-renders: at one attach point, the same text leaving and arriving is no change. The
    /// same text at different places is (two labels trading text, an item moving between lists).
    fn net_pieces(&self, pieces: Vec<Piece>, out: &mut Vec<Effect>) {
        let mut groups: IndexMap<NodeId, Vec<Piece>, FxBuildHasher> = IndexMap::default();
        for piece in pieces {
            groups.entry(piece.group).or_default().push(piece);
        }
        for group in groups.values() {
            let sorted_bag = |op: Presence| {
                let mut bag: Vec<&str> = group
                    .iter()
                    .filter(|p| p.op == op)
                    .flat_map(|p| p.content.bag.iter().map(String::as_str))
                    .collect();
                bag.sort_unstable();
                bag
            };
            let same_text = group.iter().any(|p| !p.content.bag.is_empty())
                && sorted_bag(Presence::Remove) == sorted_bag(Presence::Add);
            // With the same text, an overlay replaced by one with the same role is a re-render.
            // (Not widgets: two different icon toolbars have no identity to compare.)
            let roles = |op: Presence| -> HashSet<&str> {
                group
                    .iter()
                    .filter(|p| p.op == op)
                    .filter_map(|p| p.scan.overlay.as_ref().map(|o| o.role.as_str()))
                    .collect()
            };
            let replaced: HashSet<&str> = if same_text {
                roles(Presence::Add)
                    .intersection(&roles(Presence::Remove))
                    .copied()
                    .collect()
            } else {
                HashSet::default()
            };

            for piece in group {
                match &piece.scan.overlay {
                    Some(overlay) => {
                        if !overlay.screen_reader_only && !replaced.contains(overlay.role.as_str())
                        {
                            out.push(Effect::seen(
                                self.relative(piece.at),
                                Change::Overlay {
                                    op: match piece.op {
                                        Presence::Add => OverlayOp::Open,
                                        Presence::Remove => OverlayOp::Close,
                                    },
                                    role: overlay.role.clone(),
                                    title: overlay.title.clone(),
                                    node: overlay.node,
                                    lived_ms: None,
                                },
                            ));
                        }
                    }
                    // The same node leaving and coming back to the same place (React re-keying
                    // a list) is on screen before and after: no change.
                    None if came_back(group, piece.id) => {}
                    None if !piece.content.text.is_empty() => {
                        if !same_text {
                            out.push(Effect::seen(
                                self.relative(piece.at),
                                Change::Text {
                                    op: piece.op.into(),
                                    text: piece.content.text.clone(),
                                    before: None,
                                    node: piece.id,
                                },
                            ));
                        }
                    }
                    None if piece.scan.has_state => out.push(Effect::seen(
                        self.relative(piece.at),
                        Change::Widget {
                            op: piece.op,
                            node: piece.id,
                        },
                    )),
                    None => {}
                }
            }
        }
    }

    /// Rows that left and came back, matched by identity as a multiset (one item can have two
    /// rows). Same cells: a re-render. Different cells: cell edits, never a re-render.
    fn match_rows(&self, left: Vec<MovedRow>, arrived: Vec<MovedRow>, out: &mut Vec<Effect>) {
        let mut pool: IndexMap<_, Vec<MovedRow>> = IndexMap::default();
        for row in left {
            pool.entry(identity(&row.snapshot.row))
                .or_default()
                .push(row);
        }
        let mut unmatched = Vec::new();
        for arrival in arrived {
            let candidates = pool.get_mut(&identity(&arrival.snapshot.row));
            let same = candidates.and_then(|candidates| {
                let index = candidates
                    .iter()
                    .position(|left| left.snapshot.same_content(&arrival.snapshot))?;
                Some(candidates.remove(index))
            });
            match same {
                Some(left) => out.push(Effect::spanning(
                    self.relative(left.at.min(arrival.at)),
                    self.relative(left.at.max(arrival.at)),
                    Change::Rerender {
                        row: arrival.snapshot.row.clone(),
                        node: arrival.snapshot.id,
                        was: left.snapshot.id,
                    },
                )),
                None => unmatched.push(arrival),
            }
        }
        for arrival in unmatched {
            // A keyless row with no label has no identity: only an identical row stands for it.
            let anonymous =
                arrival.snapshot.row.key.is_none() && arrival.snapshot.row.label.is_empty();
            let candidates = pool
                .get_mut(&identity(&arrival.snapshot.row))
                .filter(|candidates| !anonymous && !candidates.is_empty());
            let Some(candidates) = candidates else {
                out.push(self.row_effect(&arrival, Presence::Add));
                continue;
            };
            // Of the rows that left under this identity, the one sharing the most cells was
            // replaced (the first, on ties).
            let shared = |left: &MovedRow| {
                left.snapshot
                    .cells
                    .iter()
                    .filter(|c| {
                        arrival
                            .snapshot
                            .cells
                            .iter()
                            .any(|d| d.slot == c.slot && d.text == c.text)
                    })
                    .count()
            };
            let mut best = 0;
            for index in 1..candidates.len() {
                if shared(&candidates[index]) > shared(&candidates[best]) {
                    best = index;
                }
            }
            let replaced = candidates.remove(best);
            let (at, end) = (replaced.at.min(arrival.at), replaced.at.max(arrival.at));
            out.extend(self.cell_diff(&replaced.snapshot, &arrival.snapshot, |_| (at, end)));
        }
        for left in pool.values().flatten() {
            out.push(self.row_effect(left, Presence::Remove));
        }
    }

    fn row_effect(&self, row: &MovedRow, op: Presence) -> Effect {
        Effect::seen(
            self.relative(row.at),
            Change::Row {
                op,
                row: row.snapshot.row.clone(),
                cells: row
                    .snapshot
                    .cells
                    .iter()
                    .map(|cell| (cell.column.clone(), cell.text.clone()))
                    .collect(),
                node: row.snapshot.id,
            },
        )
    }

    /// Rows edited in place, cell by cell (a replaced `<td>` included).
    fn diff_rows_in_place(&self, mirror: &Mirror, left: &HashSet<NodeId>, out: &mut Vec<Effect>) {
        for (&id, track) in &self.rows {
            if left.contains(&id) || !mirror.is_attached(id) {
                continue;
            }
            let now = row_snapshot(mirror, &self.grid, id);
            out.extend(self.cell_diff(&track.start, &now, |slot| {
                (
                    track.first_change.get(slot).copied().unwrap_or(track.seen),
                    track.last_change.get(slot).copied().unwrap_or(track.seen),
                )
            }));
        }
    }

    /// Cell effects between two readings of one row, column by column; `time` gives each
    /// column's absolute first and last change.
    fn cell_diff(
        &self,
        before: &RowSnapshot,
        after: &RowSnapshot,
        time: impl Fn(&str) -> (Timestamp, Timestamp),
    ) -> Vec<Effect> {
        let slots: IndexSet<&str> = before
            .cells
            .iter()
            .chain(&after.cells)
            .map(|cell| cell.slot.as_str())
            .collect();
        let mut effects = Vec::new();
        for slot in slots {
            let old = before.cell(slot);
            let new = after.cell(slot);
            let old_text = old.map_or("", |c| c.text.as_str());
            let new_text = new.map_or("", |c| c.text.as_str());
            if old_text == new_text {
                continue;
            }
            let Some(current) = new.or(old) else { continue };
            let (at, end) = time(slot);
            effects.push(Effect::spanning(
                self.relative(at),
                self.relative(end),
                Change::Cell {
                    row: before.row.clone(),
                    col: current.column.clone(),
                    before: old_text.to_owned(),
                    after: new_text.to_owned(),
                    node: current.node,
                    was: old.map(|c| c.node).filter(|node| *node != current.node),
                },
            ));
        }
        effects
    }

    fn diff_texts(&self, mirror: &Mirror, out: &mut Vec<Effect>) {
        for (&id, track) in &self.texts {
            let Some(node) = mirror.get(id) else { continue };
            let in_row = row_of(mirror, id).is_some();
            if self.born.contains(&id) || !mirror.is_attached(id) || in_row {
                continue;
            }
            let before = collapse_whitespace(&track.before);
            let after = collapse_whitespace(&node.text);
            if before != after {
                out.push(Effect::spanning(
                    self.relative(track.at),
                    self.relative(track.end),
                    Change::Text {
                        op: TextOp::Change,
                        text: after,
                        before: Some(before),
                        node: id,
                    },
                ));
            }
        }
    }

    fn diff_attributes(&self, mirror: &Mirror, out: &mut Vec<Effect>) {
        for ((id, name), track) in &self.attributes {
            let Some(node) = mirror.get(*id) else {
                continue;
            };
            if !mirror.is_attached(*id) {
                continue;
            }
            let after = node.attribute(name).and_then(AttrValue::compared_text);
            if after != track.before {
                out.push(Effect::spanning(
                    self.relative(track.at),
                    self.relative(track.end),
                    Change::State {
                        attr: name.clone(),
                        before: track.before.clone(),
                        after,
                        node: *id,
                    },
                ));
            }
        }
    }
}

/// The node left and came back to the same group with the same text.
fn came_back(group: &[Piece], id: NodeId) -> bool {
    let text = |op: Presence| {
        group
            .iter()
            .find(|p| p.op == op && p.id == id)
            .map(|p| &p.content.text)
    };
    matches!((text(Presence::Remove), text(Presence::Add)), (Some(a), Some(b)) if a == b)
}

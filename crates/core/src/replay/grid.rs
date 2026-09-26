//! Grid context. Data grids are real tables: `<th>` headers, `<tr role=row>`, and
//! `<td role=gridcell data-col=N>` cells. A click or edit inside one is described as a column of
//! a row ("Status" of "Two Sigma"), which is how the people using it think about it.

use super::{Mirror, Node};
use crate::{
    recording::rrweb::NodeId,
    text::{JS_SPACE_CLASS, clip, number_to_string, parse_number},
    vocab::GridRules,
};
use regex::Regex;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Bound on ancestor walks looking for a cell or row: grids are shallow.
const MAX_GRID_DEPTH: usize = 40;
/// Bound on nodes visited looking for rows under a node.
const MAX_ROW_SCAN: usize = 2000;

/// A row's identity. `key` is the row's own id when the product renders one (see
/// [`GridRules::row_keys`]); otherwise rows are told apart by label. Positional attributes such
/// as `data-row` are not identities: after a re-sort they name a different row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowKey {
    pub table: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RowCell {
    /// Header name, made unique within the row ("Notes #2").
    pub column: String,
    /// Column position (`data-col`, else index), made unique: stable while a header's text changes.
    pub slot: String,
    pub text: String,
    pub node: NodeId,
}

/// A data row as it read at one moment: identity plus each cell's text, in column order.
#[derive(Clone, Debug)]
pub struct RowSnapshot {
    pub id: NodeId,
    pub row: RowKey,
    pub cells: Vec<RowCell>,
}

impl RowSnapshot {
    pub fn cell(&self, slot: &str) -> Option<&RowCell> {
        self.cells.iter().find(|cell| cell.slot == slot)
    }

    pub fn texts_by_slot(&self) -> HashMap<String, String> {
        self.cells
            .iter()
            .map(|cell| (cell.slot.clone(), cell.text.clone()))
            .collect()
    }

    /// Same cells in the same columns with the same text.
    pub fn same_content(&self, other: &RowSnapshot) -> bool {
        self.cells.len() == other.cells.len()
            && self
                .cells
                .iter()
                .zip(&other.cells)
                .all(|(a, b)| a.slot == b.slot && a.text == b.text)
    }
}

/// A grid cell. A row's own header (`<th scope=row>`, `role=rowheader`) counts as a cell.
pub fn is_cell(node: &Node) -> bool {
    node.is_element()
        && node.extension.is_none()
        && (node.tag == "td"
            || matches!(node.attr("role"), Some("gridcell" | "rowheader"))
            || (node.tag == "th" && node.attr("scope") == Some("row")))
}

fn is_data_row(mirror: &Mirror, node: &Node) -> bool {
    node.is_element()
        && node.extension.is_none()
        && (node.tag == "tr" || node.attr("role") == Some("row"))
        && node
            .children
            .iter()
            .any(|child| mirror.get(*child).is_some_and(is_cell))
}

fn is_boundary(node: &Node) -> bool {
    matches!(node.tag.as_str(), "table" | "body") || node.attr("role") == Some("dialog")
}

fn nearest(mirror: &Mirror, id: NodeId, matches: impl Fn(&Node) -> bool) -> Option<NodeId> {
    for node in mirror.lineage(id).take(MAX_GRID_DEPTH) {
        if node.extension.is_some() {
            return None;
        }
        if matches(node) {
            return Some(node.id);
        }
        if is_boundary(node) {
            return None;
        }
    }
    None
}

/// The cell a node sits in (the cell itself included); `None` outside grids.
pub fn cell_of(mirror: &Mirror, id: NodeId) -> Option<NodeId> {
    nearest(mirror, id, is_cell)
}

/// The data row a node sits in (the row itself included); `None` outside grids.
pub fn row_of(mirror: &Mirror, id: NodeId) -> Option<NodeId> {
    nearest(mirror, id, |node| is_data_row(mirror, node))
}

/// Data rows at or under a node, bounded: a re-sort re-adds whole bodies, not single rows.
pub fn rows_in(mirror: &Mirror, id: NodeId) -> Vec<NodeId> {
    let mut rows = Vec::new();
    let mut stack = vec![id];
    for _ in 0..MAX_ROW_SCAN {
        let Some(id) = stack.pop() else { break };
        let Some(node) = mirror.get(id) else { continue };
        if !node.is_element() || node.extension.is_some() || is_cell(node) {
            continue;
        }
        if is_data_row(mirror, node) {
            rows.push(id);
        } else {
            stack.extend(node.children.iter().rev());
        }
    }
    rows
}

/// Sort indicators after a header's text: "Status ↑ 2".
static SORT_INDICATOR: LazyLock<Regex> = LazyLock::new(|| {
    let space = JS_SPACE_CLASS;
    Regex::new(&format!("{space}*[↑↓▲▼⇅]({space}*[0-9]+)?{space}*$")).expect("valid regex")
});

fn headers(mirror: &Mirror, table: NodeId) -> Vec<String> {
    let mut headers = Vec::new();
    let mut stack = vec![table];
    while let Some(id) = stack.pop() {
        let Some(node) = mirror.get(id) else { continue };
        if !node.is_element() || node.extension.is_some() || node.tag == "tbody" {
            continue;
        }
        if node.tag == "th" || node.attr("role") == Some("columnheader") {
            let text = mirror.visible_text(id);
            headers.push(SORT_INDICATOR.replace(&text, "").into_owned());
        } else {
            stack.extend(node.children.iter().rev());
        }
    }
    headers
}

struct Grid {
    name: String,
    headers: Vec<String>,
    /// Which column labels a row (see [`GridRules::label_columns`]), else the first.
    label_column: f64,
}

fn grid_of(mirror: &Mirror, rules: &GridRules, row: NodeId) -> Grid {
    let table = mirror
        .ancestors(row)
        .find(|node| node.tag == "table" || node.attr("role") == Some("grid"));
    let headers = table.map(|t| headers(mirror, t.id)).unwrap_or_default();
    let lowercase: Vec<String> = headers.iter().map(|h| h.to_lowercase()).collect();
    let label_column = rules
        .label_columns
        .iter()
        .find_map(|words| {
            lowercase.iter().position(|header| {
                words
                    .iter()
                    .any(|word| header.contains(&word.to_lowercase()))
            })
        })
        .unwrap_or(0);
    let name = table
        .and_then(|t| t.attr("aria-label").or_else(|| t.attr("data-testid")))
        .map_or_else(|| headers.join(" · "), str::to_owned);
    Grid {
        name,
        headers,
        label_column: label_column as f64,
    }
}

fn cells(mirror: &Mirror, row: NodeId) -> Vec<&Node> {
    mirror.get(row).map_or_else(Vec::new, |row| {
        row.children
            .iter()
            .filter_map(|id| mirror.get(*id))
            .filter(|node| is_cell(node))
            .collect()
    })
}

/// A cell's column: its `data-col` as JavaScript's `Number` reads it, else its position.
fn column_index(cell: &Node, position: usize) -> f64 {
    match cell.attribute("data-col").and_then(|v| v.as_str()) {
        Some(data_col) if !data_col.is_empty() => parse_number(data_col),
        _ => position as f64,
    }
}

/// Avatars render the initials first ("T Two Sigma", "8 8VC"): drop them.
fn strip_avatar_initials(text: String) -> String {
    if let Some((initials, rest)) = text.split_once(' ')
        && (1..=2).contains(&initials.len())
        && initials
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && rest.starts_with(initials)
    {
        return rest.to_owned();
    }
    text
}

fn read_label(mirror: &Mirror, row: NodeId, grid: &Grid) -> String {
    let cells = cells(mirror, row);
    let has_data_col = cells.iter().any(|cell| {
        cell.attribute("data-col")
            .is_some_and(|v| v.as_str().is_some())
    });
    let label_cell = cells
        .iter()
        .enumerate()
        .find(|(position, cell)| column_index(cell, *position) == grid.label_column)
        .map(|(_, cell)| *cell)
        .or_else(|| {
            if has_data_col {
                None
            } else {
                cells.first().copied()
            }
        });
    label_cell.map_or_else(String::new, |cell| {
        strip_avatar_initials(mirror.visible_text(cell.id))
    })
}

/// The row's label. A label cell may rowspan a group, so a later row of the group takes the
/// label of the nearest row above it that has one.
fn row_label(mirror: &Mirror, row: NodeId, grid: &Grid) -> String {
    let own = read_label(mirror, row, grid);
    if !own.is_empty() {
        return own;
    }
    let Some(parent) = mirror.parent(row) else {
        return own;
    };
    let Some(position) = parent.children.iter().position(|child| *child == row) else {
        return own;
    };
    parent.children[..position]
        .iter()
        .rev()
        .filter(|id| {
            mirror
                .get(**id)
                .is_some_and(|node| is_data_row(mirror, node))
        })
        .map(|id| read_label(mirror, *id, grid))
        .find(|label| !label.is_empty())
        .unwrap_or_default()
}

fn row_identity(rules: &GridRules, row: &Node) -> Option<String> {
    rules.row_keys.iter().find_map(|rule| {
        row.attr(&rule.attribute)
            .map(|value| format!("{}{value}", rule.prefix))
    })
}

/// A data row's identity and cells, read from the mirror. The row must be attached for its
/// label and headers to be found.
pub fn row_snapshot(mirror: &Mirror, rules: &GridRules, row: NodeId) -> RowSnapshot {
    let grid = grid_of(mirror, rules, row);
    // Two cells in one column (colspans, alternates) or under one header stay distinct.
    let mut per_name = HashMap::<String, usize>::default();
    let mut per_slot = HashMap::<String, usize>::default();
    let cells = cells(mirror, row)
        .into_iter()
        .enumerate()
        .map(|(position, cell)| {
            let index = column_index(cell, position);
            let index_text = number_to_string(index);
            let header = (index.fract() == 0.0 && index >= 0.0)
                .then(|| grid.headers.get(index as usize))
                .flatten()
                .filter(|header| !header.is_empty());
            let base = header
                .cloned()
                .unwrap_or_else(|| format!("col {index_text}"));
            let name_count = per_name.entry(base.clone()).or_default();
            *name_count += 1;
            let column = if *name_count == 1 {
                base
            } else {
                format!("{base} #{name_count}")
            };
            let slot_count = per_slot.entry(index_text.clone()).or_default();
            *slot_count += 1;
            let slot = if *slot_count == 1 {
                index_text
            } else {
                format!("{index_text}#{slot_count}")
            };
            RowCell {
                column,
                slot,
                text: mirror.visible_text(cell.id),
                node: cell.id,
            }
        })
        .collect();
    RowSnapshot {
        id: row,
        row: RowKey {
            table: grid.name.clone(),
            label: row_label(mirror, row, &grid),
            key: mirror.get(row).and_then(|row| row_identity(rules, row)),
        },
        cells,
    }
}

/// Column header and row label ("Status", "Two Sigma Ventures") for a cell, as targets name them.
pub fn cell_context(mirror: &Mirror, rules: &GridRules, cell: NodeId) -> (String, String) {
    let Some(row) = mirror.parent(cell) else {
        return ("col ?".into(), String::new());
    };
    let snapshot = row_snapshot(mirror, rules, row.id);
    let column = snapshot
        .cells
        .iter()
        .find(|c| c.node == cell)
        .map_or_else(|| "col ?".into(), |c| c.column.clone());
    (column, clip(&snapshot.row.label, 50))
}

use crate::{recording::rrweb::NodeId, replay::grid::RowKey, time::Millis};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// One observed change a gesture caused.
///
/// `node` fields are mirror (rrweb) ids the change was read from; `was` is the id the prior value
/// was read from, when a re-render replaced the node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// ms from the recording's first event when the change first showed.
    pub at: Millis,
    /// When it last moved, if it did not land at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Millis>,
    /// Whether a user could see it. Requests and console output are not a reaction anyone saw.
    pub visible: bool,
    #[serde(flatten)]
    pub change: Change,
}

impl Effect {
    pub fn seen(at: Millis, change: Change) -> Self {
        Self {
            at,
            end: None,
            visible: true,
            change,
        }
    }

    pub fn unseen(at: Millis, change: Change) -> Self {
        Self {
            visible: false,
            ..Self::seen(at, change)
        }
    }

    /// A visible change that first showed at `at` and last moved at `end`.
    pub fn spanning(at: Millis, end: Millis, change: Change) -> Self {
        Self {
            end: (end > at).then_some(end),
            ..Self::seen(at, change)
        }
    }

    /// The latest time this effect was still changing.
    pub fn last_moved(&self) -> Millis {
        self.end.unwrap_or(self.at)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Add,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayOp {
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextOp {
    Add,
    Remove,
    Change,
}

impl From<Presence> for TextOp {
    fn from(presence: Presence) -> Self {
        match presence {
            Presence::Add => Self::Add,
            Presence::Remove => Self::Remove,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Nav {
        to: String,
    },
    /// A grid cell's value, before and after.
    Cell {
        row: RowKey,
        col: String,
        before: String,
        after: String,
        node: NodeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        was: Option<NodeId>,
    },
    /// A grid row appeared or left, with its cells by column name.
    Row {
        op: Presence,
        row: RowKey,
        cells: IndexMap<String, String>,
        node: NodeId,
    },
    /// A row left and came back unchanged (re-sort, refetch).
    Rerender {
        row: RowKey,
        node: NodeId,
        was: NodeId,
    },
    Overlay {
        op: OverlayOp,
        role: String,
        title: String,
        node: NodeId,
        /// Set when the overlay opened and closed within the gesture (a toast).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lived_ms: Option<Millis>,
    },
    Text {
        op: TextOp,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        node: NodeId,
    },
    /// A widget-state attribute (`aria-expanded`, `data-state`, …) or a restyle (`class`).
    State {
        attr: String,
        before: Option<String>,
        after: Option<String>,
        node: NodeId,
    },
    /// A control or state-carrying element without text appeared or left.
    Widget {
        op: Presence,
        node: NodeId,
    },
    Request {
        path: String,
        /// HTTP status, when the recorder saw a response.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<Millis>,
    },
    Selection {
        text: String,
    },
    Console {
        level: String,
        message: String,
    },
    Visibility {
        hidden: bool,
    },
}

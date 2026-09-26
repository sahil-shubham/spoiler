//! Effect windows: what one gesture changed.
//!
//! While open, a window records every node the mutation stream touches. When it closes it
//! describes the *net* difference — what differs at the end of the window, not everything that
//! flickered — plus the moments anything visible moved (for latency), which do include changes
//! that later netted away (a "Sending…" label, a spinner): the product reacted then.
//!
//! Content that leaves the page is summarized at the moment it leaves (the mirror forgets it).

mod capture;
mod net;
mod scan;

use crate::time::{Millis, Timestamp};
use scan::{Content, Scan};

use super::effect::Effect;
use crate::{recording::rrweb::NodeId, replay::grid::RowSnapshot, vocab::GridRules};
use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;

/// What a departed subtree amounted to.
#[derive(Clone, Debug)]
enum Departed {
    /// Grid rows, compared with rows that arrive by identity.
    Rows(Vec<RowSnapshot>),
    /// Content inside a grid row: its row's cell diff describes it.
    InRow,
    /// Anything else, netted against arrivals at the same attach point.
    Piece(Scan, Content),
}

/// A subtree present when the window opened that left during it, summarized as it left.
#[derive(Clone, Debug)]
struct Departure {
    at: Timestamp,
    parent: NodeId,
    departed: Departed,
}

/// A subtree mounted during the window under a node that existed before it.
#[derive(Clone, Debug)]
struct Mount {
    at: Timestamp,
    parent: NodeId,
    /// If it left again: when, and what it was (a toast that came and went).
    departed: Option<(Timestamp, Scan)>,
}

#[derive(Clone, Debug)]
struct TextTrack {
    before: String,
    at: Timestamp,
    end: Timestamp,
}

#[derive(Clone, Debug)]
struct AttributeTrack {
    before: Option<String>,
    at: Timestamp,
    end: Timestamp,
}

/// A row present when the window opened whose content the window touched.
#[derive(Clone, Debug)]
struct RowTrack {
    /// The row at first touch.
    start: RowSnapshot,
    /// Cell texts by slot, as of the latest batch.
    current: HashMap<String, String>,
    first_change: HashMap<String, Timestamp>,
    last_change: HashMap<String, Timestamp>,
    /// The latest batch that touched the row.
    seen: Timestamp,
}

pub(crate) struct GestureWindow {
    /// Absolute timestamps bounding the window.
    pub start: Timestamp,
    pub until: Timestamp,
    pub closed: bool,
    /// Nodes whose class changes count as a restyle: the gesture's target chains.
    pub restyle_targets: HashSet<NodeId>,
    /// Effects observed so far, relative to the recording start.
    pub effects: Vec<Effect>,
    /// Every moment (relative) anything visible changed, including changes that netted away.
    pub changes: Vec<Millis>,
    t0: Timestamp,
    grid: Arc<GridRules>,
    /// Nodes mounted in this window: their churn is the mount, not a change to the page.
    born: HashSet<NodeId>,
    mounts: IndexMap<NodeId, Mount, FxBuildHasher>,
    departures: IndexMap<NodeId, Departure, FxBuildHasher>,
    texts: IndexMap<NodeId, TextTrack, FxBuildHasher>,
    attributes: IndexMap<(NodeId, String), AttributeTrack, FxBuildHasher>,
    rows: IndexMap<NodeId, RowTrack, FxBuildHasher>,
    /// Rows the current batch touches, as found before it applied.
    touched_rows: Vec<NodeId>,
}

impl GestureWindow {
    pub fn new(
        start: Timestamp,
        until: Timestamp,
        restyle_targets: HashSet<NodeId>,
        t0: Timestamp,
        grid: Arc<GridRules>,
    ) -> Self {
        Self {
            start,
            until,
            closed: false,
            restyle_targets,
            effects: Vec::new(),
            changes: Vec::new(),
            t0,
            grid,
            born: HashSet::default(),
            mounts: IndexMap::default(),
            departures: IndexMap::default(),
            texts: IndexMap::default(),
            attributes: IndexMap::default(),
            rows: IndexMap::default(),
            touched_rows: Vec::new(),
        }
    }

    /// `at` falls in the window. Requests are reported when they finish, so they may be
    /// attributed to a window that already closed.
    pub fn covers(&self, at: Timestamp) -> bool {
        at >= self.start && at <= self.until
    }

    /// The window is still collecting DOM changes at `at`.
    pub fn is_collecting(&self, at: Timestamp) -> bool {
        !self.closed && self.covers(at)
    }

    fn relative(&self, at: Timestamp) -> Millis {
        at - self.t0
    }

    fn changed(&mut self, at: Timestamp) {
        let at = self.relative(at);
        if self.changes.last() != Some(&at) {
            self.changes.push(at);
        }
    }
}

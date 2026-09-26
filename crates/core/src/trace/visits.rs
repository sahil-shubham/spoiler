//! Visits: a recording split where the user was away. A tab left open for days is many visits,
//! and narrating it as one session buries each in the others (and in a prompt too large to use).

use super::{Action, ActionKind, Ref};
use crate::{time::Millis, vocab::Thresholds};
use serde::{Deserialize, Serialize};

/// A run of actions without a long absence. Refs are the trace's own, so analyses of a visit
/// cite the same refs as the whole trace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Visit {
    pub first_ref: Ref,
    pub last_ref: Ref,
    pub start_ms: Millis,
    pub end_ms: Millis,
    /// Index range into the trace's actions (`start..end`).
    pub start: usize,
    pub end: usize,
}

impl Visit {
    /// A visit always spans a non-idle action, even when idle markers flank its range.
    pub(crate) fn matches(&self, actions: &[Action]) -> bool {
        if self.start >= self.end {
            return false;
        }
        let Some(span) = actions.get(self.start..self.end) else {
            return false;
        };
        let Some(first) = span.iter().find(|a| a.kind() != ActionKind::Idle) else {
            return false;
        };
        let last = span
            .iter()
            .rev()
            .find(|a| a.kind() != ActionKind::Idle)
            .unwrap_or(first);
        self.first_ref == first.reference
            && self.last_ref == last.reference
            && self.start_ms == first.t_ms
            && self.end_ms == last.t_ms
    }
}

/// Split actions (in time order) into visits. Synthetic idle markers neither start nor extend a
/// visit; they belong to whichever visit surrounds them.
/// A visit ends after `visit_gap_ms` with no action in any tab.
pub fn visits(actions: &[Action], thresholds: &Thresholds) -> Vec<Visit> {
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut last_real: Option<Millis> = None;
    for (index, action) in actions.iter().enumerate() {
        if action.kind() == ActionKind::Idle {
            continue;
        }
        match (last_real, bounds.last_mut()) {
            (Some(previous), Some(current)) if action.t_ms - previous < thresholds.visit_gap_ms => {
                current.1 = index + 1;
            }
            _ => bounds.push((index, index + 1)),
        }
        last_real = Some(action.t_ms);
    }
    // Idle markers between visits trail the earlier one; leading ones join the first.
    if let Some(first) = bounds.first_mut() {
        first.0 = 0;
    }
    for i in 1..bounds.len() {
        bounds[i - 1].1 = bounds[i].0;
    }
    if let Some(last) = bounds.last_mut() {
        last.1 = actions.len();
    }
    bounds
        .into_iter()
        .map(|(start, end)| {
            let span = &actions[start..end];
            let first = span
                .iter()
                .find(|a| a.kind() != ActionKind::Idle)
                .unwrap_or(&span[0]);
            let last = span
                .iter()
                .rev()
                .find(|a| a.kind() != ActionKind::Idle)
                .unwrap_or(&span[span.len() - 1]);
            Visit {
                first_ref: first.reference,
                last_ref: last.reference,
                start_ms: first.t_ms,
                end_ms: last.t_ms,
                start,
                end,
            }
        })
        .collect()
}

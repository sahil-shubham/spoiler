//! The compiler: rrweb events → actions, each carrying the effects of its gesture window.
//!
//! Effect window rules (thresholds from [`Thresholds`], overridable per vocabulary):
//!
//! - A gesture opens at mouse-down / touch-start — menus and tabs act on pointer-down, before the
//!   click — and a click within `gesture_ms` adopts it.
//! - Its effects run until the next gesture or keystroke in the same tab, capped at
//!   `effect_window_ms` after the click. Tabs have independent windows: a click in another tab
//!   does not close this one.
//! - Typing opens its own window, kept open `effect_window_ms` past each keystroke.

mod handlers;
mod tab;

use super::{Action, Coverage, signals::finalize};
use crate::time::{Millis, Timestamp};
use crate::{
    recording::{
        DecodeError, EventRef, Reading, Recording,
        rrweb::{SerializedNode, Signal},
    },
    vocab::{GridRules, Matcher, Thresholds},
};
use handlers::*;
use indexmap::IndexMap;
use std::sync::Arc;
use tab::Tab;

/// A compiled recording: its actions, and what of the recording they could not account for.
#[derive(Clone, Debug)]
pub struct Compilation {
    pub actions: Vec<Action>,
    pub coverage: Coverage,
}

/// Distinct custom-event and plugin names reported before the rest are pooled: names are
/// chosen by the recorded app, so their number is not bounded by rrweb.
const MAX_NAMED_KINDS: usize = 50;

/// Compile a recording (sorted by timestamp) into a trace for one app of the vocabulary.
///
/// Only an exhausted input budget is fatal; malformed events and snapshot lines are counted.
pub fn compile(
    recording: &Recording,
    matcher: &Matcher<'_>,
    app: &str,
) -> Result<Compilation, DecodeError> {
    let mut coverage = Coverage::default();
    let skipped = recording.malformed_snapshot_lines();
    if skipped != 0 {
        coverage.events += skipped;
        coverage.malformed.insert("snapshot_line".into(), skipped);
    }
    let Some(first) = recording.events().next() else {
        return Ok(Compilation {
            actions: Vec::new(),
            coverage,
        });
    };
    let mut compiler = Compiler {
        context: Context {
            matcher,
            grid: Arc::new(matcher.vocabulary().grid.clone()),
            thresholds: &matcher.vocabulary().thresholds,
            app,
            t0: first.timestamp,
        },
        tabs: IndexMap::new(),
        actions: Vec::new(),
        folds: Vec::new(),
        coverage,
    };
    // Events PostHog stored twice: identical to another at the same timestamp in the same tab.
    let mut same_time: Vec<EventRef<'_>> = Vec::new();
    for event in recording.events() {
        compiler.coverage.events += 1;
        if same_time
            .first()
            .is_some_and(|kept| kept.timestamp != event.timestamp)
        {
            same_time.clear();
        }
        if same_time.iter().any(|kept| kept.same_as(&event)) {
            compiler.coverage.duplicates += 1;
            continue;
        }
        same_time.push(event);
        let reading = recording.read(&event)?;
        compiler.handle(&event, reading);
    }
    for tab in compiler.tabs.values_mut() {
        tab.close_gesture(&mut compiler.actions, compiler.context.t0);
    }
    Ok(Compilation {
        actions: finalize(
            compiler.actions,
            &matcher.vocabulary().thresholds,
            &compiler.folds,
            &|text| matcher.is_error_text(text),
        ),
        coverage: compiler.coverage,
    })
}

impl Coverage {
    fn count(map: &mut std::collections::BTreeMap<String, usize>, name: &str) {
        if let Some(count) = map.get_mut(name) {
            *count += 1;
            return;
        }
        let key = if map.len() < MAX_NAMED_KINDS {
            name
        } else {
            "other"
        };
        *map.entry(key.to_owned()).or_default() += 1;
    }

    fn uninterpreted(&mut self, name: &str) {
        Self::count(&mut self.uninterpreted, name);
    }

    /// Survey mounted content the trace cannot see into.
    fn survey(&mut self, root: &SerializedNode) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let opaque = match node.tag.as_str() {
                "iframe" | "frame" => Some("iframe"),
                "canvas" => Some("canvas"),
                "embed" | "object" => Some("embed"),
                _ => None,
            };
            if let Some(kind) = opaque {
                *self.opaque_mounts.entry(kind.to_owned()).or_default() += 1;
            }
            if node.is_shadow_host {
                *self
                    .opaque_mounts
                    .entry("shadow_root".to_owned())
                    .or_default() += 1;
            }
            stack.extend(&node.children);
        }
    }
}

struct Context<'a> {
    matcher: &'a Matcher<'a>,
    thresholds: &'a Thresholds,
    grid: Arc<GridRules>,
    app: &'a str,
    /// Timestamp of the recording's first event: action and effect times are relative to it.
    t0: Timestamp,
}

impl Context<'_> {
    fn relative(&self, at: Timestamp) -> Millis {
        at - self.t0
    }
}

struct Compiler<'a> {
    context: Context<'a>,
    tabs: IndexMap<String, Tab>,
    actions: Vec<Action>,
    /// (earlier click, double-click) pairs to merge when finalizing.
    folds: Vec<(usize, usize)>,
    coverage: Coverage,
}

impl Compiler<'_> {
    fn handle(&mut self, event: &EventRef<'_>, reading: Reading) {
        let Self {
            context,
            tabs,
            actions,
            folds,
            coverage,
        } = self;
        // Tab numbers follow each tab's first event, whatever the event is.
        if !tabs.contains_key(event.win) {
            let number = tabs.len() + 1;
            tabs.insert(event.win.to_owned(), Tab::new(number));
        }
        let Some(tab) = tabs.get_mut(event.win) else {
            return;
        };
        let at = event.timestamp;
        // A window's end state is the mirror before the first event past it.
        if tab.open_gesture().is_some_and(|g| at > g.window.until) {
            tab.close_gesture(actions, context.t0);
        }

        let signal = match reading {
            Reading::Signal(signal) => signal,
            Reading::Uninterpreted(name) => return coverage.uninterpreted(&name),
            Reading::Malformed(name) => return Coverage::count(&mut coverage.malformed, &name),
        };
        match signal {
            Signal::FullSnapshot(root) => on_full_snapshot(tab, coverage, root, at),
            Signal::Meta { href } => tab.navigate(context, &href, at, actions),
            Signal::Mutation(mutation) => on_mutation(tab, coverage, mutation, at),
            Signal::Mouse(mouse) => on_mouse(context, tab, actions, folds, mouse, at),
            Signal::Input(input) => on_input(context, tab, actions, input, at),
            Signal::Selection(ranges) => on_selection(context, tab, actions, &ranges, at),
            Signal::Custom { tag, payload } => {
                if !on_custom(context, tab, actions, &tag, &payload, at) {
                    coverage.uninterpreted(&format!("custom:{tag}"));
                }
            }
            Signal::Plugin { name, payload } => {
                if !on_plugin(context, tab, actions, &name, &payload, at) {
                    coverage.uninterpreted(&format!("plugin:{name}"));
                }
            }
        }
    }
}

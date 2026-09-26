//! Per-tab state: the DOM mirror, location, and the open gesture window.

use super::Context;
use crate::time::{Millis, Timestamp};
use crate::trace::{
    Action, Control, Detail, Flag, Reaction, TargetClass,
    effect::{Change, Effect},
    signals::latency,
    target,
    window::GestureWindow,
};
use crate::{
    recording::rrweb::{NodeId, SerializedNode},
    replay::{Mirror, serialized_ids},
    vocab::{path_and_query, pathname},
};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::VecDeque;

/// An effect window and the action it describes, if a gesture completed.
pub(super) struct Gesture {
    pub(super) window: GestureWindow,
    /// Index into the compiler's actions. A mouse-down no click completed has none.
    pub(super) action: Option<usize>,
    /// Flags raised while the window was open, handed to the action when it closes.
    pub(super) flags: Vec<Flag>,
    /// Inputs: the last keystroke. An input's latency is measured from here, not the first key.
    pub(super) last_key_at: Option<Timestamp>,
}

/// The input being typed into, so keystrokes extend one action rather than each making one.
pub(super) struct Typing {
    pub(super) node: NodeId,
    pub(super) action: usize,
    /// The gesture the latest keystroke opened or extended (see [`Tab::gesture_serial`]).
    pub(super) gesture_serial: u64,
    pub(super) last_key_at: Timestamp,
}

/// Completed windows retained for requests whose start times precede their delayed reports.
struct ClosedGesture {
    start: Timestamp,
    until: Timestamp,
    action: usize,
}

const MAX_CLOSED_GESTURES: usize = 32;

/// The control and coordinates at touch-down. Native events usually name a pointer id rather
/// than a DOM node; keep the hit before a touch-down response changes the mirror.
pub(super) struct PendingTouch {
    pub(super) pointer: NodeId,
    pub(super) at: Timestamp,
    pub(super) point: super::super::Point,
    pub(super) control: Control,
    pub(super) class: TargetClass,
    pub(super) path: Option<String>,
    pub(super) surface: Option<String>,
}

/// Per-tab state. Tabs have separate DOMs, locations and effect windows.
pub(super) struct Tab {
    pub(super) number: usize,
    pub(super) mirror: Mirror,
    pub(super) mounted_at: HashMap<NodeId, Timestamp>,
    pub(super) root: Option<NodeId>,
    pub(super) mobile_snapshot: bool,
    /// Whether the last full snapshot came from native wireframes (not web rrweb DOM).
    pub(super) native_full_snapshot: bool,
    pub(super) mobile_semantics_seen: bool,
    pub(super) location: Option<String>,
    /// The vocabulary surface `location` is on.
    pub(super) surface: Option<String>,
    pub(super) navigations: Vec<(String, Timestamp)>,
    pub(super) gesture: Option<Gesture>,
    pub(super) typing: Option<Typing>,
    pub(super) hidden: bool,
    pub(super) pending_double: Option<(NodeId, Timestamp)>,
    pub(super) pending_touch: Option<PendingTouch>,
    closed_gestures: VecDeque<ClosedGesture>,
    pub(super) selection: Option<String>,
    /// Incremented per gesture opened, to tell whether a gesture is still the one typing opened.
    pub(super) gesture_serial: u64,
}

enum Landing<'a> {
    Window(&'a mut Gesture),
    Action(&'a mut Action),
}

/// How an effect may be attributed to a gesture window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Attribution {
    /// Only while the window is still collecting.
    WhileOpen,
    /// Also after it closed, as long as the time falls inside it (requests report on completion).
    WithinBounds,
}

impl Tab {
    pub(super) fn new(number: usize) -> Self {
        Self {
            number,
            mirror: Mirror::default(),
            mounted_at: HashMap::default(),
            root: None,
            mobile_snapshot: false,
            mobile_semantics_seen: false,
            native_full_snapshot: false,
            location: None,
            surface: None,
            navigations: Vec::new(),
            gesture: None,
            selection: None,
            typing: None,
            hidden: false,
            pending_double: None,
            pending_touch: None,
            closed_gestures: VecDeque::new(),
            gesture_serial: 0,
        }
    }

    /// An action at `at` on this tab's current page.
    pub(super) fn action(&self, context: &Context<'_>, detail: Detail, at: Timestamp) -> Action {
        let mut action = Action::new(detail, context.relative(at), self.number);
        action.path = self.location.clone();
        action.surface = self.surface.clone();
        action
    }

    pub(super) fn open_gesture(&self) -> Option<&Gesture> {
        self.gesture.as_ref().filter(|g| !g.window.closed)
    }

    /// A mouse-down that no click has claimed yet, recent enough to be `at`'s gesture.
    pub(super) fn unclaimed_press(&self, at: Timestamp, gesture_ms: Millis) -> bool {
        self.open_gesture()
            .is_some_and(|g| g.action.is_none() && at - g.window.start <= gesture_ms)
    }

    /// Close the current gesture: net its effects into its action and measure latency.
    pub(super) fn close_gesture(&mut self, actions: &mut [Action], t0: Timestamp) {
        let Some(gesture) = self.gesture.as_mut() else {
            return;
        };
        if gesture.window.closed {
            return;
        }
        let Some(index) = gesture.action else {
            gesture.window.closed = true; // a mouse-down no click completed: nothing to describe
            return;
        };
        gesture.window.close(&self.mirror);
        if self.closed_gestures.len() == MAX_CLOSED_GESTURES {
            self.closed_gestures.pop_front();
        }
        self.closed_gestures.push_back(ClosedGesture {
            start: gesture.window.start,
            until: gesture.window.until,
            action: index,
        });
        let action = &mut actions[index];
        action.effects.append(&mut gesture.window.effects);
        for flag in gesture.flags.drain(..) {
            action.flag(flag);
        }
        let (from, since) = match gesture.last_key_at {
            Some(key_at) => (key_at - t0, key_at - t0),
            None => (action.t_ms, Millis::NEG_INFINITY),
        };
        let reaction = latency(&action.effects, &gesture.window.changes, from, since);
        if let Some(control) = action.control_mut() {
            control.reaction = reaction.map(|(react_ms, settle_ms)| Reaction {
                react_ms,
                settle_ms,
            });
        }
    }

    /// Open a gesture window, closing the current one first (its window ends just before `at`).
    pub(super) fn start_gesture(
        &mut self,
        context: &Context<'_>,
        at: Timestamp,
        until: Timestamp,
        restyle_targets: HashSet<NodeId>,
        actions: &mut [Action],
    ) -> &mut Gesture {
        if let Some(previous) = self.gesture.as_mut()
            && !previous.window.closed
        {
            previous.window.until = previous.window.until.min(at - Millis(1.0));
        }
        self.close_gesture(actions, context.t0);
        self.gesture_serial += 1;
        self.gesture.insert(Gesture {
            window: GestureWindow::new(
                at,
                until,
                restyle_targets,
                context.t0,
                context.grid.clone(),
            ),
            action: None,
            flags: Vec::new(),
            last_key_at: None,
        })
    }

    /// Where something that happened at `at` belongs: the gesture window covering it while that
    /// is open, else (for [`Attribution::WithinBounds`]) the action of the window it fell in.
    fn landing<'a>(
        &'a mut self,
        at: Timestamp,
        attribution: Attribution,
        actions: &'a mut [Action],
    ) -> Option<Landing<'a>> {
        if let Some(gesture) = self.gesture.as_mut().filter(|g| g.window.covers(at)) {
            if !gesture.window.closed {
                return Some(Landing::Window(gesture));
            }
            if attribution == Attribution::WithinBounds
                && let Some(index) = gesture.action
            {
                return Some(Landing::Action(&mut actions[index]));
            }
        }
        if attribution == Attribution::WithinBounds {
            self.closed_gestures
                .iter()
                .rev()
                .find(|closed| at >= closed.start && at <= closed.until)
                .map(|closed| Landing::Action(&mut actions[closed.action]))
        } else {
            None
        }
    }

    /// Record an effect in the gesture window covering `at`. A window that already closed hands
    /// late effects straight to its action.
    pub(super) fn record(
        &mut self,
        effect: Effect,
        at: Timestamp,
        attribution: Attribution,
        actions: &mut [Action],
    ) {
        match self.landing(at, attribution, actions) {
            Some(Landing::Window(gesture)) => gesture.window.effects.push(effect),
            Some(Landing::Action(action)) => action.effects.push(effect),
            None => {}
        }
    }

    /// Flag the gesture covering `at` as followed by an error.
    pub(super) fn flag_error(
        &mut self,
        at: Timestamp,
        attribution: Attribution,
        actions: &mut [Action],
    ) {
        match self.landing(at, attribution, actions) {
            Some(Landing::Window(gesture)) if !gesture.flags.contains(&Flag::ErrorAfter) => {
                gesture.flags.push(Flag::ErrorAfter);
            }
            Some(Landing::Action(action)) => action.flag(Flag::ErrorAfter),
            _ => {}
        }
    }

    pub(super) fn navigate(
        &mut self,
        context: &Context<'_>,
        href: &str,
        at: Timestamp,
        actions: &mut Vec<Action>,
    ) {
        let path = path_and_query(href);
        if self.location.as_deref() == Some(path.as_str()) {
            return;
        }
        let previous = self.location.replace(path.clone());
        self.surface = context
            .matcher
            .surface(context.app, pathname(&path))
            .map(|surface| surface.id.clone());
        let effect = Effect::seen(context.relative(at), Change::Nav { to: path.clone() });
        self.record(effect, at, Attribution::WhileOpen, actions);
        let mut action = self.action(context, Detail::Nav, at);
        let back = self
            .navigations
            .len()
            .checked_sub(2)
            .map(|i| &self.navigations[i]);
        if previous.is_some()
            && let Some((back_path, back_at)) = back
            && *back_path == path
            && at - *back_at <= context.thresholds.thrash_ms
        {
            action.flag(Flag::Thrash);
        }
        self.navigations.push((path, at));
        actions.push(action);
    }

    pub(super) fn mark_mounted(&mut self, root: &SerializedNode, at: Timestamp) {
        for id in serialized_ids(root) {
            self.mounted_at.insert(id, at);
        }
    }

    /// Nodes from `id` up: their class changes count as a restyle of the gesture.
    pub(super) fn lineage_ids(&self, id: NodeId) -> HashSet<NodeId> {
        self.mirror.lineage(id).map(|node| node.id).collect()
    }

    /// The control on node `id` of this tab's page, and how its element was classified; the
    /// class is `None`, and the target empty, when the node is not in the page.
    pub(super) fn control(
        &self,
        context: &Context<'_>,
        id: NodeId,
    ) -> (Control, Option<TargetClass>) {
        self.control_with_text(context, id, false)
    }

    pub(super) fn mobile_control(
        &self,
        context: &Context<'_>,
        id: NodeId,
    ) -> (Control, Option<TargetClass>) {
        self.control_with_text(context, id, true)
    }

    fn control_with_text(
        &self,
        context: &Context<'_>,
        id: NodeId,
        mobile: bool,
    ) -> (Control, Option<TargetClass>) {
        let resolve = if mobile {
            target::resolve_mobile
        } else {
            target::resolve
        };
        let resolution = resolve(
            &self.mirror,
            context.matcher,
            &context.grid,
            self.surface.as_deref(),
            id,
        );
        let class = resolution.as_ref().map(|r| r.class);
        let control = match resolution {
            Some(resolution) => Control {
                node: id,
                target: resolution.label,
                feature: resolution.feature,
                element: Some(resolution.desc),
                reaction: None,
            },
            None => Control {
                node: id,
                target: String::new(),
                feature: None,
                element: None,
                reaction: None,
            },
        };
        (control, class)
    }
}

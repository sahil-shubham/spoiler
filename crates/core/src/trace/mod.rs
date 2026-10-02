//! Traces: what a user did, one action per click, input, navigation, visibility change and
//! error, each with what it changed on the page and the signals code detected.

mod compiler;
mod effect;
mod render;
mod signals;
mod target;
mod timeline;
mod visits;
mod window;

use crate::{
    recording::rrweb::NodeId,
    time::Millis,
    vocab::{TargetDesc, pathname},
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, fmt, str::FromStr};

pub use compiler::{Compilation, compile};
pub use effect::{Change, Effect, OverlayOp, Presence, TextOp};
pub use render::{effect_cell, render_effect, render_effects, to_tsv};
pub use timeline::{
    Capture, Fidelity, FidelitySpan, FocusSpan, PRESENCE_GAP_MS, Page, SessionLink, Span,
    TabTimeline, Timeline,
};
pub use visits::{Visit, visits};

/// What of a recording the trace could not account for. A trace is only as complete as this
/// says: content inside iframes, canvases or shadow roots, and events the compiler reads past
/// leave no actions or effects even when the user saw them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// Events in the recording, including duplicates.
    pub events: usize,
    /// Events PostHog stored twice, dropped.
    pub duplicates: usize,
    /// Events read past, by kind: mouse moves, scrolls, canvas drawing, unknown plugins…
    pub uninterpreted: BTreeMap<String, usize>,
    /// Events whose data lacked the shape rrweb gives them, skipped, by kind.
    pub malformed: BTreeMap<String, usize>,
    /// Elements whose inside the trace cannot see (iframe documents, canvas pixels, embeds,
    /// shadow roots), counted each time one is mounted.
    pub opaque_mounts: BTreeMap<String, usize>,
    /// Suppressed extension roots, gestures, console errors and requests by public id or family.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, usize>,
    /// Full snapshots on tabs without any location or pageview URL to borrow.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unlocated_snapshots: usize,
    /// Screenshot content was recorded, but no labelled native wireframe content was captured.
    #[serde(default, skip_serializing_if = "is_false")]
    pub screenshot_only: bool,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

fn is_false(flag: &bool) -> bool {
    !flag
}

/// Version of the compilation rules. Traces from other versions are not comparable.
pub const COMPILER_VERSION: u32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Click,
    Dblclick,
    Contextmenu,
    Input,
    Nav,
    Hidden,
    Visible,
    ConsoleError,
    NetError,
    /// A gap of at least `idle_ms` on a visible page.
    Idle,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Dblclick => "dblclick",
            Self::Contextmenu => "contextmenu",
            Self::Input => "input",
            Self::Nav => "nav",
            Self::Hidden => "hidden",
            Self::Visible => "visible",
            Self::ConsoleError => "console_error",
            Self::NetError => "net_error",
            Self::Idle => "idle",
        }
    }

    pub fn is_click(self) -> bool {
        matches!(self, Self::Click | Self::Dblclick)
    }

    /// Something the user did with a control, as opposed to something that happened.
    pub fn is_gesture(self) -> bool {
        matches!(
            self,
            Self::Click | Self::Dblclick | Self::Input | Self::Contextmenu
        )
    }
}

/// A code-detected signal. The model explains these; it never invents them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// An inert element was clicked and nothing visible happened.
    Dead,
    /// A control was clicked and nothing visible happened.
    Unresponsive,
    /// At least `rage_clicks` clicks close together within `rage_window_ms`.
    Rage,
    /// The first visible reaction took at least `slow_ms`.
    Slow,
    /// A console error or failed request during the gesture.
    ErrorAfter,
    /// An error message appeared on screen in reply to the gesture (apps often report failures
    /// inside successful responses, where no request or console error shows them).
    ErrorShown,
    /// A → B → A navigation within `thrash_ms`.
    Thrash,
}

impl Flag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dead => "dead",
            Self::Unresponsive => "unresponsive",
            Self::Rage => "rage",
            Self::Slow => "slow",
            Self::ErrorAfter => "error_after",
            Self::ErrorShown => "error_shown",
            Self::Thrash => "thrash",
        }
    }
}

/// How a click target was classified when it was resolved: decides dead vs unresponsive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetClass {
    Interactive,
    /// Form fields: a click focuses them, which shows nothing.
    Focus,
    Inert,
    Unresolved,
}

/// The element an action resolved to, and how many ancestors were climbed from the event's node
/// to reach it (clicks on an icon resolve to its button).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionTarget {
    #[serde(flatten)]
    pub element: TargetDesc,
    pub depth: usize,
}

/// An action's id within its trace: `e1`, `e2`, … in time order. Analyses cite these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref(pub u32);

impl Ref {
    /// Held by actions until the trace is finalized and refs are assigned in time order.
    pub const UNASSIGNED: Self = Self(0);
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}", self.0)
    }
}

impl FromStr for Ref {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.strip_prefix('e')
            .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .filter(|&n| n > 0)
            .map(Self)
            .ok_or_else(|| format!("not a trace ref: {s:?}"))
    }
}

impl Serialize for Ref {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ref {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Where a pointer event happened, in page coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// How fast the product visibly reacted, in ms from the click (or the last keystroke).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    /// The first visible change; negative when the page reacted on mouse-down.
    pub react_ms: Millis,
    /// The last visible change of the gesture.
    pub settle_ms: Millis,
}

/// The control a gesture was aimed at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Control {
    /// The node the event named.
    pub node: NodeId,
    /// Rendered: `button[testid] "Label"`, a grid cell, or the bare node id when the node was
    /// not in the page.
    pub target: String,
    /// Vocabulary feature id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// The element resolved; absent when the node was not in the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<ActionTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<Reaction>,
}

/// A click, double-click or right-click.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Press {
    #[serde(flatten)]
    pub control: Control,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<Point>,
    /// Decides whether pressing without reaction is dead or unresponsive.
    pub class: TargetClass,
}

/// Typing into or toggling a field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub control: Control,
    /// What was entered, as recorded (masked inputs give only a length).
    pub typed: String,
}

/// What kind of action it was, with only the fields that kind can have. Kind-specific JSON
/// avoids ambiguous optional fields: presses carry a control, point and class; inputs carry
/// a control and typed value; errors carry their message or request; idle gaps carry `idle_ms`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detail {
    Click(Press),
    Dblclick(Press),
    Contextmenu(Press),
    Input(Entry),
    Nav,
    Hidden,
    Visible,
    ConsoleError {
        message: String,
    },
    NetError {
        status: u16,
        request: String,
    },
    /// A gap of at least `idle_ms` on a visible page.
    Idle {
        idle_ms: Millis,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    #[serde(rename = "ref")]
    pub reference: Ref,
    /// Time since the recording's first event.
    pub t_ms: Millis,
    /// Tab number, 1-based in order of first appearance.
    pub win: usize,
    /// Pathname (and search) the tab was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Vocabulary surface id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(flatten)]
    pub detail: Detail,
    /// What the gesture changed, in time order.
    pub effects: Vec<Effect>,
    pub flags: Vec<Flag>,
}

impl Action {
    pub fn new(detail: Detail, t_ms: Millis, win: usize) -> Self {
        Self {
            reference: Ref::UNASSIGNED,
            t_ms,
            win,
            path: None,
            surface: None,
            detail,
            effects: Vec::new(),
            flags: Vec::new(),
        }
    }

    pub fn kind(&self) -> ActionKind {
        match self.detail {
            Detail::Click(_) => ActionKind::Click,
            Detail::Dblclick(_) => ActionKind::Dblclick,
            Detail::Contextmenu(_) => ActionKind::Contextmenu,
            Detail::Input(_) => ActionKind::Input,
            Detail::Nav => ActionKind::Nav,
            Detail::Hidden => ActionKind::Hidden,
            Detail::Visible => ActionKind::Visible,
            Detail::ConsoleError { .. } => ActionKind::ConsoleError,
            Detail::NetError { .. } => ActionKind::NetError,
            Detail::Idle { .. } => ActionKind::Idle,
        }
    }

    pub fn press(&self) -> Option<&Press> {
        match &self.detail {
            Detail::Click(press) | Detail::Dblclick(press) | Detail::Contextmenu(press) => {
                Some(press)
            }
            _ => None,
        }
    }

    /// The control of a gesture (press or input).
    pub fn control(&self) -> Option<&Control> {
        match &self.detail {
            Detail::Click(p) | Detail::Dblclick(p) | Detail::Contextmenu(p) => Some(&p.control),
            Detail::Input(entry) => Some(&entry.control),
            _ => None,
        }
    }

    pub fn control_mut(&mut self) -> Option<&mut Control> {
        match &mut self.detail {
            Detail::Click(p) | Detail::Dblclick(p) | Detail::Contextmenu(p) => Some(&mut p.control),
            Detail::Input(entry) => Some(&mut entry.control),
            _ => None,
        }
    }

    /// What the action was aimed at, or said: the control, or the error's message.
    pub fn target(&self) -> Option<Cow<'_, str>> {
        match &self.detail {
            Detail::ConsoleError { message } => Some(Cow::Borrowed(message)),
            Detail::NetError { status, request } => Some(Cow::Owned(format!("{status} {request}"))),
            _ => self.control().map(|c| Cow::Borrowed(c.target.as_str())),
        }
    }

    pub fn feature(&self) -> Option<&str> {
        self.control()?.feature.as_deref()
    }

    pub fn reaction(&self) -> Option<Reaction> {
        self.control()?.reaction
    }

    /// Where the action happened: its surface, else (outside the vocabulary) its pathname.
    pub fn place(&self) -> Option<&str> {
        self.surface
            .as_deref()
            .or_else(|| self.path.as_deref().map(pathname))
    }

    pub fn has_flag(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    /// Add a flag once.
    pub fn flag(&mut self, flag: Flag) {
        if !self.has_flag(flag) {
            self.flags.push(flag);
        }
    }
}

//! The session's timeline: when the user was there, which tab was in front, and how much of each
//! tab the recording can show. Built in the compiler's one pass over the events; it reads them
//! but never changes an action, so traces are the same with or without it.
//!
//! Presence is posthog-js's own notion of activity (its `ACTIVE_SOURCES`), so the timeline agrees
//! with PostHog's active time. Fidelity follows what the recorder actually captured: nothing before
//! a tab's first full snapshot, nothing while it was idle or paused, and a stale page after it
//! resumed without a fresh snapshot (see `docs/PLAN-timeline.md`).

use super::{Action, ActionKind};
use crate::recording::{
    Reading,
    rrweb::{Signal, source},
};
use crate::time::{Millis, Timestamp};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Inputs closer than this are one run of presence: rrweb batches mouse moves every 500 ms.
pub const PRESENCE_GAP_MS: f64 = 1_000.0;
/// A full snapshot this soon after the recorder resumes means it resumed on a fresh page.
const STALE_GRACE_MS: f64 = 1_000.0;

/// When the user was there, which tab was in front, and how much of each tab the recording can
/// show. Times are spans from `t0`, like every action time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    /// The recording's first event, ms since the Unix epoch: the origin of every `*_ms` here
    /// and in the trace's actions.
    pub t0: Timestamp,
    /// The recording's last event.
    pub end_ms: Millis,
    pub tabs: Vec<TabTimeline>,
    /// Runs of user input in any tab, split where inputs are [`PRESENCE_GAP_MS`] apart.
    pub presence: Vec<Span>,
    /// The tab in front. No span covers a moment when every tab was hidden.
    pub focus: Vec<FocusSpan>,
    /// Gaps in `presence` that no gesture's reaction fills: nobody acting, nothing they caused
    /// still happening. Every gap, however short; consumers choose their own minimum.
    pub quiet: Vec<Span>,
    /// The browser said it was offline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offline: Vec<Span>,
    /// How the recorder was configured, from the configuration events it writes at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Capture>,
    /// The session this one continues, as PostHog linked them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session: Option<SessionLink>,
    /// The session this one rotated into. Events after `at_ms` on its window belong to it
    /// (PostHog filed them here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_session: Option<SessionLink>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start_ms: Millis,
    pub end_ms: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusSpan {
    pub win: usize,
    #[serde(flatten)]
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabTimeline {
    /// The `win` the trace's actions carry.
    pub win: usize,
    /// PostHog's window id: what a player keys this tab's events by.
    pub window_id: String,
    pub first_ms: Millis,
    pub last_ms: Millis,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<Span>,
    /// Each page the tab showed, from its navigations.
    pub pages: Vec<Page>,
    /// How far the reconstructed page can be trusted, covering `[first_ms, last_ms]` in order.
    pub fidelity: Vec<FidelitySpan>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub at_ms: Millis,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Built from a full snapshot and every event since.
    Exact,
    /// No full snapshot yet: the page is whatever mutations happened to add.
    Blind,
    /// The recorder was discarding events (idle, or paused on a blocked URL).
    Dropped,
    /// Recording resumed after dropping, before the next full snapshot.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FidelitySpan {
    pub fidelity: Fidelity,
    #[serde(flatten)]
    pub span: Span,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// Whether typed values were masked, as the recorder applied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_all_inputs: Option<bool>,
    /// The recorder's plugins (console, network…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<String>,
    /// The names of the options the recorder was started with. Recorder builds differ in
    /// which they know, so this identifies the build where no version is recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recorder_options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionLink {
    pub session_id: String,
    /// The window on the other side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    /// Where in this recording PostHog wrote the link.
    pub at_ms: Millis,
}

/// What the timeline keeps per tab while events stream past.
struct Track {
    window_id: String,
    first: Timestamp,
    last: Timestamp,
    hidden_since: Option<Timestamp>,
    hidden: Vec<Span>,
    /// Whether a full snapshot has been seen, and the spans that are not exact.
    snapshot_seen: bool,
    /// The first page activity (a mutation, gesture, request…) before any full snapshot:
    /// from here the tab is blind. A Meta or a custom event changes no page.
    blind_since: Option<Timestamp>,
    dropping_since: Option<Timestamp>,
    stale_since: Option<Timestamp>,
    flawed: Vec<FidelitySpan>,
    /// After `$session_id_change`: the rest of the tab is the next session's.
    rotated: bool,
}

/// Collects the timeline as the compiler streams events past it.
pub(super) struct TimelineBuilder {
    t0: Timestamp,
    end: Timestamp,
    tracks: IndexMap<String, Track>,
    inputs: Vec<(Timestamp, usize)>,
    focus: Vec<FocusSpan>,
    front: Option<(usize, Timestamp)>,
    offline_since: Option<Timestamp>,
    offline: Vec<Span>,
    capture: Option<Capture>,
    previous_session: Option<SessionLink>,
    next_session: Option<SessionLink>,
}

/// posthog-js `ACTIVE_SOURCES`: the incremental sources that mean a person is there.
fn is_presence(reading: &Reading) -> bool {
    match reading {
        Reading::Signal(Signal::Mouse(_) | Signal::Input(_)) => true,
        Reading::Uninterpreted(name) => [
            source::MOUSE_MOVE,
            source::SCROLL,
            source::VIEWPORT_RESIZE,
            source::TOUCH_MOVE,
            source::MEDIA_INTERACTION,
            source::DRAG,
        ]
        .into_iter()
        .any(|kind| name == source::name(kind)),
        _ => false,
    }
}

/// Custom tags the timeline accounts for, so coverage no longer counts them as read past.
pub(super) const LIFECYCLE_TAGS: &[&str] = &[
    "sessionIdle",
    "sessionNoLongerIdle",
    "recording paused",
    "recording resumed",
    "$session_id_change",
    "$session_ending",
    "$session_starting",
    "browser offline",
    "browser online",
    "$session_options",
];

impl TimelineBuilder {
    pub(super) fn new(t0: Timestamp) -> Self {
        Self {
            t0,
            end: t0,
            tracks: IndexMap::new(),
            inputs: Vec::new(),
            focus: Vec::new(),
            front: None,
            offline_since: None,
            offline: Vec::new(),
            capture: None,
            previous_session: None,
            next_session: None,
        }
    }

    fn span(&self, start: Timestamp, end: Timestamp) -> Span {
        Span {
            start_ms: start - self.t0,
            end_ms: end - self.t0,
        }
    }

    /// The tab in front changes to `win` (or to none) at `at`.
    fn front(&mut self, win: Option<usize>, at: Timestamp) {
        if self.front.map(|(front, _)| front) == win {
            return;
        }
        if let Some((front, since)) = self.front.take()
            && at > since
        {
            let span = self.span(since, at);
            match self.focus.last_mut() {
                Some(last) if last.win == front && last.span.end_ms == span.start_ms => {
                    last.span.end_ms = span.end_ms;
                }
                _ => self.focus.push(FocusSpan { win: front, span }),
            }
        }
        self.front = win.map(|win| (win, at));
    }

    /// One event, in recording order, after duplicates are dropped.
    pub(super) fn observe(&mut self, win: &str, at: Timestamp, reading: &Reading) {
        self.end = self.end.max(at);
        if !self.tracks.contains_key(win) {
            self.tracks.insert(
                win.to_owned(),
                Track {
                    window_id: win.to_owned(),
                    first: at,
                    last: at,
                    hidden_since: None,
                    hidden: Vec::new(),
                    snapshot_seen: false,
                    blind_since: None,
                    dropping_since: None,
                    stale_since: None,
                    flawed: Vec::new(),
                    rotated: false,
                },
            );
            if self.front.is_none() && self.focus.is_empty() {
                // Before any input, the first tab to record anything is the one on screen.
                self.front(Some(self.tracks.len()), at);
            }
        }
        let number = self.tracks.get_index_of(win).map_or(0, |index| index + 1);
        let t0 = self.t0;
        let span = |start: Timestamp, end: Timestamp| Span {
            start_ms: start - t0,
            end_ms: end - t0,
        };
        let track = &mut self.tracks[win];
        track.last = track.last.max(at);

        // The recorder resumed: what it missed while dropping is gone, and until a full
        // snapshot the page it shows may be out of date.
        if let Some(since) = track.dropping_since
            && at > since
        {
            track.dropping_since = None;
            track.flawed.push(FidelitySpan {
                fidelity: Fidelity::Dropped,
                span: span(since, at),
            });
            track.stale_since = Some(at);
        }
        let full = matches!(
            reading,
            Reading::Signal(Signal::FullSnapshot(_) | Signal::NativeFullSnapshot(_))
        );
        let pageless = matches!(
            reading,
            Reading::Signal(Signal::Meta { .. } | Signal::Custom { .. })
        );
        if !track.snapshot_seen && !full && !pageless {
            track.blind_since.get_or_insert(at);
        }
        if full {
            if !track.snapshot_seen
                && let Some(since) = track.blind_since
            {
                track.flawed.push(FidelitySpan {
                    fidelity: Fidelity::Blind,
                    span: span(since, at),
                });
            }
            track.snapshot_seen = true;
            if let Some(since) = track.stale_since.take()
                && at - since > Millis(STALE_GRACE_MS)
            {
                track.flawed.push(FidelitySpan {
                    fidelity: Fidelity::Stale,
                    span: span(since, at),
                });
            }
        }

        if is_presence(reading) {
            self.inputs.push((at, number));
            self.front(Some(number), at);
            return;
        }
        let Reading::Signal(Signal::Custom { tag, payload }) = reading else {
            return;
        };
        let track = &mut self.tracks[win];
        match tag.as_str() {
            "window hidden" => {
                track.hidden_since.get_or_insert(at);
                if self.front.is_some_and(|(front, _)| front == number) {
                    self.front(None, at);
                }
            }
            "window visible" => {
                if let Some(since) = track.hidden_since.take() {
                    track.hidden.push(span(since, at));
                }
                self.front(Some(number), at);
            }
            "sessionIdle" => {
                // The marker is backdated to the last activity; dropping began when the recorder
                // noticed, at the event it was handling.
                let noticed = payload["eventTimestamp"].as_f64().map_or(at, Timestamp);
                track.dropping_since.get_or_insert(noticed.max(at));
            }
            "recording paused" => {
                track.dropping_since.get_or_insert(at);
            }
            "$session_id_change" if !track.rotated => {
                if let Some(id) = payload["sessionId"].as_str() {
                    track.rotated = true;
                    self.next_session.get_or_insert(SessionLink {
                        session_id: id.to_owned(),
                        window_id: payload["windowId"].as_str().map(str::to_owned),
                        at_ms: at - t0,
                    });
                }
            }
            "$session_ending" => {
                if let Some(id) = payload["nextSessionId"].as_str() {
                    self.next_session.get_or_insert(SessionLink {
                        session_id: id.to_owned(),
                        window_id: payload["nextWindowId"].as_str().map(str::to_owned),
                        at_ms: at - t0,
                    });
                }
            }
            // In a rotated tab this is the next session's, filed here: it names this one.
            "$session_starting" if !track.rotated => {
                if let Some(id) = payload["previousSessionId"].as_str() {
                    self.previous_session.get_or_insert(SessionLink {
                        session_id: id.to_owned(),
                        window_id: payload["previousWindowId"].as_str().map(str::to_owned),
                        at_ms: at - t0,
                    });
                }
            }
            "browser offline" => {
                self.offline_since.get_or_insert(at);
            }
            "browser online" => {
                if let Some(since) = self.offline_since.take() {
                    self.offline.push(span(since, at));
                }
            }
            "$session_options" if !track.rotated => {
                let capture = self.capture.get_or_insert_with(Capture::default);
                let options = &payload["sessionRecordingOptions"];
                if let Some(mask) = options["maskAllInputs"].as_bool() {
                    capture.mask_all_inputs = Some(mask);
                }
                if let Some(options) = options.as_object() {
                    capture.recorder_options = options.keys().cloned().collect();
                }
                if let Some(plugins) = payload["activePlugins"].as_array() {
                    capture.plugins = plugins
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
            }
            _ => {}
        }
    }

    /// The timeline, given the compiled actions (for pages and gesture reactions).
    pub(super) fn finish(mut self, actions: &[Action]) -> Timeline {
        let end = self.end;
        self.front(None, end);
        if let Some(since) = self.offline_since.take() {
            self.offline.push(self.span(since, end));
        }
        let t0 = self.t0;
        let span = |start: Timestamp, end: Timestamp| Span {
            start_ms: start - t0,
            end_ms: end - t0,
        };

        let tabs = self
            .tracks
            .into_iter()
            .enumerate()
            .map(|(index, (_, mut track))| {
                let win = index + 1;
                if let Some(since) = track.hidden_since.take() {
                    track.hidden.push(span(since, track.last));
                }
                if !track.snapshot_seen {
                    // Never a picture: blind throughout, whether or not anything happened.
                    track.flawed.push(FidelitySpan {
                        fidelity: Fidelity::Blind,
                        span: span(track.first, track.last),
                    });
                }
                if let Some(since) = track.dropping_since.take() {
                    track.flawed.push(FidelitySpan {
                        fidelity: Fidelity::Dropped,
                        span: span(since, track.last.max(since)),
                    });
                }
                if let Some(since) = track.stale_since.take()
                    && track.last > since
                {
                    track.flawed.push(FidelitySpan {
                        fidelity: Fidelity::Stale,
                        span: span(since, track.last),
                    });
                }
                let pages = actions
                    .iter()
                    .filter(|action| action.win == win && action.kind() == ActionKind::Nav)
                    .filter_map(|action| {
                        Some(Page {
                            at_ms: action.t_ms,
                            path: action.path.clone()?,
                        })
                    })
                    .collect();
                TabTimeline {
                    win,
                    window_id: track.window_id,
                    first_ms: track.first - t0,
                    last_ms: track.last - t0,
                    hidden: track.hidden,
                    pages,
                    fidelity: fill_exact(track.flawed, span(track.first, track.last)),
                }
            })
            .collect();

        let presence = runs(self.inputs.iter().map(|(at, _)| *at - t0));
        let reactions: Vec<Span> = actions
            .iter()
            .filter(|action| action.kind().is_gesture())
            .filter_map(|action| {
                let reaction = action.reaction()?;
                Some(Span {
                    start_ms: action.t_ms + reaction.react_ms.min(Millis::ZERO),
                    end_ms: action.t_ms + reaction.settle_ms,
                })
            })
            .collect();
        let quiet = presence
            .windows(2)
            .map(|pair| Span {
                start_ms: pair[0].end_ms,
                end_ms: pair[1].start_ms,
            })
            .flat_map(|gap| subtract(gap, &reactions))
            .collect();

        Timeline {
            t0,
            end_ms: end - t0,
            tabs,
            presence,
            focus: self.focus,
            quiet,
            offline: self.offline,
            capture: self.capture,
            previous_session: self.previous_session,
            next_session: self.next_session,
        }
    }
}

/// Merge input times (in order) into runs no closer than [`PRESENCE_GAP_MS`].
fn runs(times: impl Iterator<Item = Millis>) -> Vec<Span> {
    let mut runs: Vec<Span> = Vec::new();
    for at in times {
        match runs.last_mut() {
            Some(run) if at - run.end_ms < Millis(PRESENCE_GAP_MS) => {
                run.end_ms = run.end_ms.max(at)
            }
            _ => runs.push(Span {
                start_ms: at,
                end_ms: at,
            }),
        }
    }
    runs
}

/// `gap` minus every span in `cut`.
fn subtract(gap: Span, cut: &[Span]) -> Vec<Span> {
    let mut pieces = vec![gap];
    for cut in cut {
        pieces = pieces
            .into_iter()
            .flat_map(|piece| {
                if cut.end_ms <= piece.start_ms || cut.start_ms >= piece.end_ms {
                    return vec![piece];
                }
                [
                    Span {
                        start_ms: piece.start_ms,
                        end_ms: cut.start_ms,
                    },
                    Span {
                        start_ms: cut.end_ms,
                        end_ms: piece.end_ms,
                    },
                ]
                .into_iter()
                .filter(|part| part.end_ms > part.start_ms)
                .collect()
            })
            .collect();
    }
    pieces
}

/// The flawed spans in time order, with `Exact` filling what they leave of `whole`.
fn fill_exact(mut flawed: Vec<FidelitySpan>, whole: Span) -> Vec<FidelitySpan> {
    flawed.sort_by(|a, b| a.span.start_ms.0.total_cmp(&b.span.start_ms.0));
    let mut filled = Vec::new();
    let mut cursor = whole.start_ms;
    for span in flawed {
        let start = span.span.start_ms.max(cursor);
        if start > cursor {
            filled.push(FidelitySpan {
                fidelity: Fidelity::Exact,
                span: Span {
                    start_ms: cursor,
                    end_ms: start,
                },
            });
        }
        if span.span.end_ms > start {
            filled.push(FidelitySpan {
                fidelity: span.fidelity,
                span: Span {
                    start_ms: start,
                    end_ms: span.span.end_ms,
                },
            });
            cursor = span.span.end_ms;
        }
    }
    if whole.end_ms > cursor || filled.is_empty() {
        filled.push(FidelitySpan {
            fidelity: Fidelity::Exact,
            span: Span {
                start_ms: cursor,
                end_ms: whole.end_ms.max(cursor),
            },
        });
    }
    filled
}

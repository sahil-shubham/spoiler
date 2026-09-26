//! Signals: latency, and the flags a session is scanned for. Code decides these, never the model.

use super::{Action, ActionKind, Change, Detail, Flag, Reaction, Ref, TargetClass, effect::Effect};
use crate::{
    time::{Millis, chronological},
    vocab::Thresholds,
};
use std::collections::HashSet;

/// ms from `from` to the first and last visible change at or after `since`.
///
/// Visible changes are the net visible effects plus `changes`: every visible DOM change the
/// window saw, including ones that later netted away. A request with nothing on screen is not a
/// reaction. Clicks count from the click (negative = reacted on press, since `since` is
/// unbounded); inputs from their last keystroke. All times are relative to the recording start.
pub(crate) fn latency(
    effects: &[Effect],
    changes: &[Millis],
    from: Millis,
    since: Millis,
) -> Option<(Millis, Millis)> {
    let visible_effects = effects
        .iter()
        // A tab going hidden is a response (a link may open a new tab) but not a render to time.
        .filter(|effect| effect.visible && effect.at >= since)
        .filter(|effect| !matches!(effect.change, Change::Visibility { .. }))
        .map(|effect| (effect.at, effect.last_moved()));
    let dom_changes = changes.iter().filter(|t| **t >= since).map(|t| (*t, *t));
    let (first, last) = visible_effects.chain(dom_changes).fold(
        (Millis(f64::INFINITY), Millis::NEG_INFINITY),
        |(first, last), (at, end)| (first.min(at), last.max(end)),
    );
    first.is_finite().then_some((first - from, last - from))
}

/// Order actions and their effects, derive flags, insert idle gaps, and assign refs.
///
/// `folds` are (earlier click, double-click) pairs: the double-click absorbs the click rrweb
/// reported before it, which is part of the same gesture rather than a dead click of its own.
pub(crate) fn finalize(
    mut actions: Vec<Action>,
    thresholds: &Thresholds,
    folds: &[(usize, usize)],
    is_error_text: &dyn Fn(&str) -> bool,
) -> Vec<Action> {
    fold_double_clicks(&mut actions, folds);
    actions.sort_by(|a, b| chronological(&a.t_ms, &b.t_ms));
    for action in &mut actions {
        action.effects.sort_by(|a, b| chronological(&a.at, &b.at));
    }
    flag_rage(&mut actions, thresholds);
    for action in &mut actions {
        flag_error_shown(action, is_error_text);
    }

    let mut out = Vec::with_capacity(actions.len());
    let mut previous: Option<(Millis, usize)> = None;
    // Per tab: a tab in the background says nothing about the one the user is looking at.
    let mut hidden_tabs = HashSet::new();
    for mut action in actions {
        flag_no_reaction(&mut action);
        if action
            .reaction()
            .is_some_and(|r| r.react_ms >= thresholds.slow_ms)
        {
            action.flag(Flag::Slow);
        }
        // A gap on a visible page is idle; a gap after its tab went hidden is time away.
        if let Some((t_ms, win)) = previous
            && !hidden_tabs.contains(&win)
            && action.t_ms - t_ms >= thresholds.idle_ms
            && action.kind() != ActionKind::Visible
        {
            let idle_ms = action.t_ms - t_ms;
            out.push(Action::new(
                Detail::Idle { idle_ms },
                t_ms + Millis(1.0),
                win,
            ));
        }
        match action.kind() {
            ActionKind::Hidden => {
                hidden_tabs.insert(action.win);
            }
            ActionKind::Visible => {
                hidden_tabs.remove(&action.win);
            }
            _ => {}
        }
        previous = Some((action.t_ms, action.win));
        out.push(action);
    }
    for (number, action) in (1..).zip(out.iter_mut()) {
        action.reference = Ref(number);
    }
    out
}

/// Rage: bursts of clicks in one tab, close together. Close means within `rage_px` when both
/// clicks have coordinates, else the same node (a missing position is not position 0,0).
fn flag_rage(actions: &mut [Action], thresholds: &Thresholds) {
    let clicks: Vec<usize> = (0..actions.len())
        .filter(|&i| actions[i].kind().is_click())
        .collect();
    for (position, &first_index) in clicks.iter().enumerate() {
        let first = &actions[first_index];
        let mut burst = vec![first_index];
        for &index in &clicks[position + 1..] {
            let click = &actions[index];
            if click.t_ms - first.t_ms > thresholds.rage_window_ms {
                break;
            }
            // Double/triple-clicking to select text is not rage.
            let selected_text = click
                .effects
                .iter()
                .any(|e| matches!(e.change, Change::Selection { .. }));
            if selected_text {
                continue;
            }
            let (Some(press), Some(first_press)) = (click.press(), first.press()) else {
                continue;
            };
            let near = match (press.point, first_press.point) {
                (Some(p), Some(p0)) => (p.x - p0.x).hypot(p.y - p0.y) <= thresholds.rage_px,
                _ => press.control.node == first_press.control.node,
            };
            if click.win == first.win && near {
                burst.push(index);
            }
        }
        // A double-click is two presses: folding rrweb's click pair must not hide hammering.
        let presses: usize = burst
            .iter()
            .map(|&i| {
                if actions[i].kind() == ActionKind::Dblclick {
                    2
                } else {
                    1
                }
            })
            .sum();
        if presses >= thresholds.rage_clicks {
            for index in burst {
                actions[index].flag(Flag::Rage);
            }
        }
    }
}

/// A click with no visible effect and nothing visible even flickering did nothing a user could
/// see: unresponsive on a control, dead on an inert element.
fn flag_no_reaction(action: &mut Action) {
    let Some(press) = action.press().filter(|_| action.kind().is_click()) else {
        return;
    };
    let reacted = press.control.reaction.is_some() || action.effects.iter().any(|e| e.visible);
    let flag = match press.class {
        _ if reacted => return,
        TargetClass::Interactive => Flag::Unresponsive,
        TargetClass::Inert => Flag::Dead,
        TargetClass::Focus | TargetClass::Unresolved => return,
    };
    action.flag(flag);
}

fn fold_double_clicks(actions: &mut Vec<Action>, folds: &[(usize, usize)]) {
    if folds.is_empty() {
        return;
    }
    let mut folded = vec![false; actions.len()];
    for &(click, double) in folds {
        let earlier = actions[click].reaction().map(|reaction| {
            let offset = actions[click].t_ms - actions[double].t_ms;
            Reaction {
                react_ms: reaction.react_ms + offset,
                settle_ms: reaction.settle_ms + offset,
            }
        });
        let effects = std::mem::take(&mut actions[click].effects);
        let flags = std::mem::take(&mut actions[click].flags);
        let target = &mut actions[double];
        target.effects.extend(effects);
        if let Some(earlier) = earlier
            && let Some(control) = target.control_mut()
        {
            control.reaction = Some(match control.reaction {
                Some(later) => Reaction {
                    react_ms: earlier.react_ms.min(later.react_ms),
                    settle_ms: earlier.settle_ms.max(later.settle_ms),
                },
                None => earlier,
            });
        }
        for flag in flags {
            target.flag(flag);
        }
        folded[click] = true;
    }
    let mut index = 0;
    actions.retain(|_| {
        index += 1;
        !folded[index - 1]
    });
}

/// Visible messages longer than this are page content, not a reply to one gesture.
const MAX_MESSAGE_UNITS: usize = 200;

/// A gesture that brought up a short error message on screen. Navigations bring up whole
/// pages, whose text says nothing about the gesture, so they are not read.
fn flag_error_shown(action: &mut Action, is_error_text: &dyn Fn(&str) -> bool) {
    if !action.kind().is_gesture()
        || action
            .effects
            .iter()
            .any(|e| matches!(e.change, Change::Nav { .. }))
    {
        return;
    }
    let shown = action
        .effects
        .iter()
        .filter(|e| e.visible)
        .any(|effect| match &effect.change {
            Change::Text { op, text, .. } => {
                *op != super::TextOp::Remove
                    && crate::text::utf16_len(text) <= MAX_MESSAGE_UNITS
                    && is_error_text(text)
            }
            Change::Cell { after, .. } => {
                crate::text::utf16_len(after) <= MAX_MESSAGE_UNITS && is_error_text(after)
            }
            Change::Overlay { op, title, .. } => {
                *op == super::OverlayOp::Open
                    && crate::text::utf16_len(title) <= MAX_MESSAGE_UNITS
                    && is_error_text(title)
            }
            _ => false,
        });
    if shown {
        action.flag(Flag::ErrorShown);
    }
}

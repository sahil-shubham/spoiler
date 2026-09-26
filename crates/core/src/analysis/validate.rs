use super::{
    Check, FlaggedAction, Friction, FrictionKind, ModelSummary, SessionSummary, Step, Task,
    ValidatedStep, ValidatedTask,
};
use crate::{
    text::{JS_SPACE_CLASS, round_half_up},
    time::Millis,
    trace::{Action, ActionKind, Change, Detail, Effect, Flag, Ref, render_effect},
    vocab::Vocabulary,
};
use indexmap::IndexMap;
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

/// Idle gaps count toward a task's active time up to this long.
const MAX_COUNTED_GAP: Millis = Millis(30_000.0);

/// A duration quoted in prose matches one a cited action carries within this share…
const DURATION_TOLERANCE: f64 = 0.05;
/// …or this many ms, whichever is larger.
const DURATION_TOLERANCE_MS: Millis = Millis(50.0);

/// Enforce the output contract: take every fact the trace knows from the trace, and report
/// what the model got wrong.
pub fn validate(
    summary: ModelSummary,
    actions: &[Action],
    vocabulary: &Vocabulary,
) -> (SessionSummary, Check) {
    let evidence = Evidence::new(actions);
    let features: HashSet<&str> = vocabulary.features.iter().map(|f| f.id.as_str()).collect();
    let mut check = Check::default();

    let cited: Vec<&String> = summary
        .steps
        .iter()
        .flat_map(|s| &s.refs)
        .chain(summary.friction.iter().flat_map(|f| &f.refs))
        .chain(summary.tasks.iter().flat_map(|t| &t.refs))
        .collect();
    check.bad_refs = cited
        .iter()
        .filter(|r| evidence.get(r).is_none())
        .map(|r| (*r).clone())
        .collect();
    check.bad_ref_ratio = if cited.is_empty() {
        0.0
    } else {
        check.bad_refs.len() as f64 / cited.len() as f64
    };

    let friction = summary
        .friction
        .into_iter()
        .filter_map(|mut friction| {
            friction
                .refs
                .retain(|reference| evidence.get(reference).is_some());
            let supported = !friction.refs.is_empty()
                && match signal_for(friction.kind) {
                    Some(signal) => evidence.cited(&friction.refs).iter().any(|a| signal(a)),
                    None => true,
                };
            if !supported {
                check.dropped_friction.push(format!(
                    "{}: {}",
                    friction.kind.as_str(),
                    friction.what
                ));
            }
            supported.then_some(friction)
        })
        .collect::<Vec<_>>();
    let signals = flagged_actions(actions, &friction);
    check.unexplained_signals = signals
        .iter()
        .filter(|signal| signal.explained_by.is_empty())
        .map(|signal| signal.reference)
        .collect();
    let narrated: HashSet<Ref> = summary
        .steps
        .iter()
        .flat_map(|s| &s.refs)
        .filter_map(|r| r.parse().ok())
        .collect();
    check.uncited_gestures = actions
        .iter()
        .filter(|a| a.kind().is_gesture() && !narrated.contains(&a.reference))
        .map(|a| a.reference)
        .collect();

    let mut steps = Vec::new();
    for (index, step) in summary.steps.into_iter().enumerate() {
        match validate_step(step, index, &evidence, &features, &mut check) {
            Some(step) => steps.push(step),
            None => check
                .corrected
                .push(format!("step {}: no valid refs, dropped", index + 1)),
        }
    }

    let mut tasks = Vec::new();
    for (index, task) in summary.tasks.into_iter().enumerate() {
        match measure_task(task, &evidence) {
            Some(task) => tasks.push(task),
            None => check
                .corrected
                .push(format!("task {}: no valid refs, dropped", index + 1)),
        }
    }

    let summary = SessionSummary {
        reasoning: summary.reasoning,
        who: summary.who,
        intent: summary.intent,
        tasks,
        steps,
        friction,
        signals,
        outcome: summary.outcome,
        summary: summary.summary,
    };
    (summary, check)
}

struct Evidence<'a> {
    actions: &'a [Action],
    by_ref: HashMap<Ref, &'a Action>,
}

impl<'a> Evidence<'a> {
    fn new(actions: &'a [Action]) -> Self {
        Self {
            actions,
            by_ref: actions.iter().map(|a| (a.reference, a)).collect(),
        }
    }

    /// The action a cited ref names, if it is a ref of these actions.
    fn get(&self, cited: &str) -> Option<&'a Action> {
        self.by_ref.get(&cited.parse().ok()?).copied()
    }

    /// The actions a list of refs names, in time order; unknown refs are skipped.
    fn cited(&self, refs: &[String]) -> Vec<&'a Action> {
        let mut cited: Vec<_> = refs.iter().filter_map(|r| self.get(r)).collect();
        cited.sort_by(|a, b| crate::time::chronological(&a.t_ms, &b.t_ms));
        cited
    }
}

/// Every flagged action, with the (kept) friction items citing it.
fn flagged_actions(actions: &[Action], friction: &[Friction]) -> Vec<FlaggedAction> {
    actions
        .iter()
        .filter(|action| !action.flags.is_empty())
        .map(|action| FlaggedAction {
            reference: action.reference,
            at_s: seconds(action.t_ms),
            surface: action.place().map(str::to_owned),
            target: action.target().map(|t| t.into_owned()),
            flags: action.flags.clone(),
            explained_by: friction
                .iter()
                .enumerate()
                .filter(|(_, f)| f.refs.iter().any(|r| r.parse() == Ok(action.reference)))
                .map(|(index, _)| index)
                .collect(),
        })
        .collect()
}

/// The code-detected signal a friction kind claims. The model explains signals; it does not
/// invent them. Kinds without one (confusion, abandonment) are judgment and need no signal.
pub(super) fn signal_for(kind: FrictionKind) -> Option<fn(&Action) -> bool> {
    match kind {
        FrictionKind::DeadClick => {
            Some(|a| a.has_flag(Flag::Dead) || a.has_flag(Flag::Unresponsive))
        }
        FrictionKind::RageClick => Some(|a| a.has_flag(Flag::Rage)),
        FrictionKind::Error => Some(|a| {
            a.has_flag(Flag::ErrorAfter)
                || a.has_flag(Flag::ErrorShown)
                || matches!(a.kind(), ActionKind::ConsoleError | ActionKind::NetError)
        }),
        FrictionKind::Slow => Some(|a| a.has_flag(Flag::Slow)),
        FrictionKind::ConfusionLoop | FrictionKind::Abandonment | FrictionKind::Other => None,
    }
}

/// Persisted data changes an action made: cell edits, rows added/removed, a box ticked. Changes
/// first seen after navigation belong to the new page, not to the user's edit. Navigation effects
/// may precede earlier DOM effects in the vector because the window nets DOM changes on close.
fn persisted(action: &Action) -> Vec<&Effect> {
    let first_nav = action
        .effects
        .iter()
        .filter(|effect| matches!(effect.change, Change::Nav { .. }))
        .map(|effect| effect.at)
        .reduce(Millis::min);
    action
        .effects
        .iter()
        .filter(|effect| first_nav.is_none_or(|at| effect.at < at))
        .filter(|e| match &e.change {
            Change::Cell { .. } | Change::Row { .. } => true,
            Change::State { attr, .. } => attr == "checked",
            _ => false,
        })
        .collect()
}

/// Durations an action carries: reaction times, idle gap, request and toast durations.
fn durations(action: &Action) -> Vec<Millis> {
    let reaction = action.reaction();
    let idle = match action.detail {
        Detail::Idle { idle_ms } => Some(idle_ms),
        _ => None,
    };
    let own = [
        reaction.map(|r| r.react_ms),
        reaction.map(|r| r.settle_ms),
        idle,
    ]
    .into_iter()
    .flatten();
    let from_effects = action.effects.iter().filter_map(|e| match &e.change {
        Change::Request { ms, .. } => *ms,
        Change::Overlay { lived_ms, .. } => *lived_ms,
        _ => None,
    });
    own.chain(from_effects).collect()
}

/// "1331 ms", "1.3 s", "4 min" in prose.
static QUOTED_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"([0-9]+(?:\.[0-9]+)?){JS_SPACE_CLASS}?(ms|s|sec|seconds?|min|minutes?)(?-u:\b)"
    ))
    .expect("valid regex")
});

/// Durations quoted in `text` that none of `known` supports.
fn unverified_durations(text: &str, known: &[Millis]) -> Vec<String> {
    QUOTED_DURATION
        .captures_iter(text)
        .filter_map(|capture| {
            let amount: f64 = capture[1].parse().ok()?;
            let unit = &capture[2];
            let ms = Millis(if unit == "ms" {
                amount
            } else if unit.starts_with("min") {
                amount * 60_000.0
            } else {
                amount * 1000.0
            });
            let tolerance = DURATION_TOLERANCE_MS.max(Millis(ms.0 * DURATION_TOLERANCE));
            let supported = known.iter().any(|k| (k.0 - ms.0).abs() <= tolerance.0);
            (!supported).then(|| capture[0].to_owned())
        })
        .collect()
}

/// Seconds to one decimal, halves rounding up.
fn seconds(time: Millis) -> f64 {
    round_half_up(time.0 / 100.0) / 10.0
}

fn validate_step(
    mut step: Step,
    index: usize,
    evidence: &Evidence<'_>,
    features: &HashSet<&str>,
    check: &mut Check,
) -> Option<ValidatedStep> {
    step.refs
        .retain(|reference| evidence.get(reference).is_some());
    let cited = evidence.cited(&step.refs);
    let first = cited.first()?;
    if let Some(feature) = step.feature.clone().filter(|f| !f.is_empty()) {
        if !features.contains(feature.as_str()) {
            check.unknown_features.push(feature);
            step.feature = None;
        } else if !cited.iter().any(|a| a.feature() == Some(feature.as_str())) {
            check.corrected.push(format!(
                "step {}: feature {feature} not on cited actions",
                index + 1
            ));
            step.feature = None;
        }
    }
    let known: Vec<Millis> = cited.iter().flat_map(|a| durations(a)).collect();
    let unverified = unverified_durations(&step.response, &known);
    check.unverified_numbers += unverified.len();

    // The most common place among the cited actions (the first, on ties).
    let mut counts: IndexMap<&str, usize> = IndexMap::new();
    for action in &cited {
        if let Some(place) = action.place() {
            *counts.entry(place).or_default() += 1;
        }
    }
    let mut surface: Option<(&str, usize)> = None;
    for (place, count) in counts {
        if surface.is_none_or(|(_, best)| count > best) {
            surface = Some((place, count));
        }
    }

    Some(ValidatedStep {
        at_s: seconds(first.t_ms),
        surface: surface.map(|(place, _)| place.to_owned()),
        changes: cited
            .iter()
            .flat_map(|a| persisted(a))
            .map(render_effect)
            .collect(),
        unverified,
        step,
    })
}

fn measure_task(mut task: Task, evidence: &Evidence<'_>) -> Option<ValidatedTask> {
    task.refs
        .retain(|reference| evidence.get(reference).is_some());
    let cited = evidence.cited(&task.refs);
    let (first, last) = (cited.first()?, cited.last()?);
    // The task is everything between its first and last cited action, in every tab. Actions
    // are in time order.
    let start = evidence.actions.partition_point(|a| a.t_ms < first.t_ms);
    let end = evidence.actions.partition_point(|a| a.t_ms <= last.t_ms);
    let span = &evidence.actions[start..end];

    let mut path: Vec<String> = Vec::new();
    for action in span {
        if let Some(place) = action.place().filter(|p| !p.is_empty())
            && path.last().map(String::as_str) != Some(place)
            && (action.kind() == ActionKind::Nav || path.is_empty())
        {
            path.push(place.to_owned());
        }
    }
    // A hidden tab stays away until it becomes visible, but another tab can stay active.
    let mut visible_tabs = HashSet::new();
    let mut hidden_tabs = HashSet::new();
    let mut visible_after = |action: &Action| {
        match action.kind() {
            ActionKind::Hidden => {
                hidden_tabs.insert(action.win);
                visible_tabs.remove(&action.win);
            }
            ActionKind::Visible => {
                hidden_tabs.remove(&action.win);
                visible_tabs.insert(action.win);
            }
            _ if !hidden_tabs.contains(&action.win) => {
                visible_tabs.insert(action.win);
            }
            _ => {}
        }
        !visible_tabs.is_empty()
    };
    for action in &evidence.actions[..start] {
        visible_after(action);
    }
    let active: Millis = span
        .windows(2)
        .filter(|pair| visible_after(&pair[0]))
        .map(|pair| (pair[1].t_ms - pair[0].t_ms).min(MAX_COUNTED_GAP))
        .sum();

    Some(ValidatedTask {
        start_s: seconds(first.t_ms),
        end_s: seconds(last.t_ms),
        active_s: seconds(active),
        path,
        actions: span.iter().filter(|a| a.kind().is_gesture()).count(),
        changes: span.iter().map(|a| persisted(a).len()).sum(),
        task,
    })
}

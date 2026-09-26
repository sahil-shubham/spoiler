//! What each kind of event does to a tab and its gestures.

use super::{
    Context,
    tab::{Attribution, Tab, Typing},
};
use crate::replay::{extension_request_url, extension_url};
use crate::time::{Millis, Timestamp};
use crate::trace::{
    Action, Coverage, Detail, Entry, Point, Press, TargetClass,
    effect::{Change, Effect},
};
use crate::{
    recording::rrweb::{
        Input, Interaction, Mouse, Mutation, NodeId, SelectionRange, SerializedNode,
    },
    text::{
        collapse_runs, collapse_whitespace, js_string, number_to_string, round_half_up, trim,
        utf16_len, utf16_slice,
    },
    vocab::path_and_query,
};
use rustc_hash::FxHashSet as HashSet;
use serde_json::Value;

/// Requests to these are the recorder's and common monitoring's own traffic, not the
/// product's. Vocabularies add their own (`telemetry`).
const TELEMETRY: [&str; 6] = [
    "posthog",
    "sentry",
    "/otel/",
    "/envelope/",
    "cfExtPri",
    "cdn-cgi",
];

/// The page was re-serialized (often under new ids): what changed so far is read off the old
/// mirror before it is replaced.
pub(super) fn on_full_snapshot(
    tab: &mut Tab,
    coverage: &mut Coverage,
    root: SerializedNode,
    at: Timestamp,
) {
    if !tab.hidden
        && let Some(gesture) = tab.gesture.as_mut()
        && gesture.window.is_collecting(at)
    {
        gesture.window.flush(&tab.mirror);
    }
    tab.mark_mounted(&root, at);
    coverage.survey(&root, false);
    tab.mirror.reset(root);
}

pub(super) fn on_mutation(
    tab: &mut Tab,
    coverage: &mut Coverage,
    mutation: Mutation,
    at: Timestamp,
) {
    for add in &mutation.adds {
        tab.mark_mounted(&add.node, at);
        coverage.survey(&add.node, tab.mirror.extension(add.parent).is_some());
    }
    // Attributes and style text can turn an already-mounted node into extension content.
    let mut newly_marked = Vec::new();
    let mut consider = |id| {
        if tab.mirror.extension(id).is_none() && !newly_marked.contains(&id) {
            newly_marked.push(id);
        }
    };
    for change in &mutation.attributes {
        if change.attributes.0.iter().any(|(name, _)| {
            matches!(
                name.as_str(),
                "src" | "href" | "rr_src" | "id" | "class" | "textContent" | "_cssText"
            )
        }) {
            consider(change.id);
        }
    }
    for change in &mutation.texts {
        if let Some(style) = tab
            .mirror
            .lineage(change.id)
            .take(2)
            .find(|node| node.tag == "style")
        {
            consider(style.id);
        }
    }
    let added = mutation.added();
    let collecting = (!tab.hidden)
        .then_some(())
        .and_then(|()| tab.gesture.as_mut().filter(|g| g.window.is_collecting(at)));
    if let Some(gesture) = collecting {
        gesture.window.before_batch(&tab.mirror, &mutation, at);
        tab.mirror.apply(mutation);
        gesture.window.after_batch(&tab.mirror, &added, at);
    } else {
        tab.mirror.apply(mutation);
    }
    for id in newly_marked {
        if let Some(extension) = tab.mirror.extension(id) {
            coverage.extension(extension);
        }
    }
}

/// rrweb may stamp a double-click before the second click even when it lists it afterward.
fn mark_double_click(
    actions: &mut [Action],
    folds: &mut Vec<(usize, usize)>,
    node: NodeId,
    win: usize,
    double: usize,
    gesture_ms: Millis,
) {
    let is_click_here = |a: &Action| {
        matches!(&a.detail, Detail::Click(press) if press.control.node == node) && a.win == win
    };
    let started = actions[double].t_ms;
    let earlier = actions[..double]
        .iter()
        .rposition(|a| is_click_here(a) && started - a.t_ms <= gesture_ms);
    let detail = &mut actions[double].detail;
    if let Detail::Click(press) = std::mem::replace(detail, Detail::Nav) {
        *detail = Detail::Dblclick(press);
    }
    if let Some(click) = earlier {
        folds.push((click, double));
    }
}

pub(super) fn on_mouse(
    context: &Context<'_>,
    tab: &mut Tab,
    coverage: &mut Coverage,
    actions: &mut Vec<Action>,
    folds: &mut Vec<(usize, usize)>,
    mouse: Mouse,
    at: Timestamp,
) {
    if let Some(extension) = tab.mirror.extension(mouse.id) {
        if matches!(
            mouse.interaction,
            Interaction::Click | Interaction::ContextMenu
        ) {
            coverage.extension(extension);
        }
        return;
    }
    match mouse.interaction {
        Interaction::MouseDown | Interaction::TouchStart => {
            tab.pending_double = None;
            let targets = tab.lineage_ids(mouse.id);
            tab.start_gesture(
                context,
                at,
                at + context.thresholds.gesture_ms,
                targets,
                actions,
            );
        }
        // rrweb emits Click, Click, DblClick: the last click becomes the double-click, and the
        // click before it (same node and tab, just before) folds into it.
        Interaction::DblClick => {
            if tab.unclaimed_press(at, context.thresholds.gesture_ms)
                && tab
                    .gesture
                    .as_ref()
                    .is_some_and(|g| g.window.restyle_targets.contains(&mouse.id))
            {
                tab.pending_double = Some((mouse.id, at));
            } else if let Some(double) = actions.iter().rposition(|a| {
                matches!(&a.detail, Detail::Click(press) if press.control.node == mouse.id)
                    && a.win == tab.number
            }) {
                mark_double_click(
                    actions,
                    folds,
                    mouse.id,
                    tab.number,
                    double,
                    context.thresholds.gesture_ms,
                );
            }
        }
        Interaction::Click | Interaction::ContextMenu => {
            tab.typing = None;
            let (mut control, class) = tab.control(context, mouse.id);
            if class.is_none() {
                control.target = format!("UNRESOLVED node#{}", mouse.id);
            }
            let press = Press {
                control,
                point: mouse.x.zip(mouse.y).map(|(x, y)| Point { x, y }),
                class: class.unwrap_or(TargetClass::Unresolved),
            };
            let detail = if mouse.interaction == Interaction::ContextMenu {
                Detail::Contextmenu(press)
            } else {
                Detail::Click(press)
            };
            let action = tab.action(context, detail, at);
            if !tab.unclaimed_press(at, context.thresholds.gesture_ms) {
                // Covers nothing until the click below sets how long it collects.
                let until = Timestamp::NEG_INFINITY;
                tab.start_gesture(context, at, until, HashSet::default(), actions);
            }
            let targets = tab.lineage_ids(mouse.id);
            if let Some(gesture) = tab.gesture.as_mut() {
                gesture.window.until = at + context.thresholds.effect_window_ms;
                // Restyle is watched from the node actually clicked (often an icon) up.
                gesture.window.restyle_targets.extend(targets);
                gesture.action = Some(actions.len());
            }
            actions.push(action);
            if mouse.interaction == Interaction::Click
                && tab.pending_double.take().is_some_and(|(id, reported)| {
                    id == mouse.id && at - reported <= context.thresholds.gesture_ms
                })
            {
                let double = actions.len() - 1;
                mark_double_click(
                    actions,
                    folds,
                    mouse.id,
                    tab.number,
                    double,
                    context.thresholds.gesture_ms,
                );
            }
        }
        Interaction::Other => {}
    }
}

/// What an input shows in the trace. posthog-js masks inputs as `*` runs.
fn describe_input(input: &Input, checkable: bool) -> String {
    let text = input.text.as_deref().unwrap_or_default();
    if checkable {
        return if input.checked == Some(true) {
            "checked"
        } else {
            "unchecked"
        }
        .into();
    }
    if !text.is_empty() && text.chars().all(|c| c == '*') {
        return format!("{} chars (masked)", utf16_len(text));
    }
    if text.is_empty() {
        return "cleared".into();
    }
    format!("\"{}\"", utf16_slice(text, 0, Some(80)))
}

pub(super) fn on_input(
    context: &Context<'_>,
    tab: &mut Tab,
    coverage: &mut Coverage,
    actions: &mut Vec<Action>,
    input: Input,
    at: Timestamp,
) {
    let mounted_at = tab
        .mounted_at
        .get(&input.id)
        .copied()
        .unwrap_or(Timestamp::NEG_INFINITY);
    if input.id < 0 || at - mounted_at <= context.thresholds.programmatic_input_ms {
        return;
    }
    if let Some(extension) = tab.mirror.extension(input.id) {
        coverage.extension(extension);
        return;
    }
    let node = tab.mirror.get(input.id);
    let checkable = node.is_some_and(|node| {
        matches!(node.attr("type"), Some("checkbox" | "radio"))
            || matches!(node.attr("role"), Some("checkbox" | "radio" | "switch"))
    });
    let shown = describe_input(&input, checkable);

    // Resuming after a visit-length absence is a new action, even in the same field.
    if let Some(mut current) = tab.typing.take()
        && current.node == input.id
        && tab.gesture_serial == current.gesture_serial
        && at - current.last_key_at < context.thresholds.visit_gap_ms
    {
        if let Detail::Input(entry) = &mut actions[current.action].detail {
            entry.typed = shown;
        }
        // A shorter pause can close the window while keeping the same input action.
        if tab.gesture.as_ref().is_some_and(|g| g.window.closed) {
            let targets = tab.lineage_ids(input.id);
            let reopened = tab.start_gesture(context, at, at, targets, actions);
            reopened.action = Some(current.action);
        }
        if let Some(gesture) = tab.gesture.as_mut() {
            gesture.window.until = gesture
                .window
                .until
                .max(at + context.thresholds.effect_window_ms);
            gesture.last_key_at = Some(at);
        }
        current.gesture_serial = tab.gesture_serial;
        current.last_key_at = at;
        tab.typing = Some(current);
        return;
    }

    let (mut control, class) = tab.control(context, input.id);
    if class.is_none() {
        control.target = format!("input#{}", input.id);
    }
    let entry = Entry {
        control,
        typed: shown,
    };
    let action = tab.action(context, Detail::Input(entry), at);

    // A checkbox toggled between a mouse-down and its click is that click's doing (the browser
    // fires input before click): recorded, but it does not split the gesture's window.
    if checkable && tab.unclaimed_press(at, context.thresholds.gesture_ms) {
        if let Some(gesture) = tab.gesture.as_mut() {
            let checked = input.checked == Some(true);
            gesture.window.effects.push(Effect::seen(
                context.relative(at),
                Change::State {
                    attr: "checked".into(),
                    before: Some((!checked).to_string()),
                    after: Some(checked.to_string()),
                    node: input.id,
                },
            ));
        }
        actions.push(action);
        return;
    }

    // Typing closes the click's window and opens its own.
    let targets = tab.lineage_ids(input.id);
    let gesture = tab.start_gesture(
        context,
        at,
        at + context.thresholds.effect_window_ms,
        targets,
        actions,
    );
    gesture.action = Some(actions.len());
    tab.typing = Some(Typing {
        node: input.id,
        action: actions.len(),
        gesture_serial: tab.gesture_serial,
        last_key_at: at,
    });
    actions.push(action);
}

/// Double/triple-clicking text selects it: an effect, not a dead click.
pub(super) fn on_selection(
    context: &Context<'_>,
    tab: &mut Tab,
    actions: &mut [Action],
    ranges: &[SelectionRange],
    at: Timestamp,
) {
    let key = ranges
        .iter()
        .map(|r| {
            let n = number_to_string;
            format!(
                "{}:{}-{}:{}",
                n(r.start),
                n(r.start_offset),
                n(r.end),
                n(r.end_offset)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    if tab.selection.as_deref() == Some(key.as_str()) {
        return; // re-reporting the current selection
    }
    tab.selection = Some(key);
    for range in ranges.iter().filter(|range| !range.is_caret()) {
        if tab.mirror.extension(range.start as NodeId).is_some()
            || tab.mirror.extension(range.end as NodeId).is_some()
        {
            continue;
        }
        let node_id = range.start as NodeId;
        let node_text = match tab.mirror.get(node_id) {
            Some(node) if !node.text.is_empty() => node.text.clone(),
            Some(_) => tab.mirror.text_preview(node_id, 200),
            None => String::new(),
        };
        let end = range.within_one_node().then_some(range.end_offset as usize);
        let text = collapse_whitespace(&utf16_slice(&node_text, range.start_offset as usize, end));
        if text.is_empty() {
            continue;
        }
        // A range past its start node continues into text not read here: "…" says so.
        let text = if range.within_one_node() {
            text
        } else {
            format!("{text}…")
        };
        let effect = Effect::seen(context.relative(at), Change::Selection { text });
        tab.record(effect, at, Attribution::WhileOpen, actions);
    }
}

/// Handle a custom event; `false` when its tag means nothing to the compiler.
pub(super) fn on_custom(
    context: &Context<'_>,
    tab: &mut Tab,
    actions: &mut Vec<Action>,
    tag: &str,
    payload: &Value,
    at: Timestamp,
) -> bool {
    match tag {
        "$pageview" | "$url_changed" => {
            let href = payload["href"].as_str().map(trim).filter(|h| !h.is_empty());
            if let Some(href) = href {
                tab.navigate(context, href, at, actions);
            }
        }
        "window hidden" | "window visible" => {
            let hidden = tag == "window hidden";
            let effect = Effect::seen(context.relative(at), Change::Visibility { hidden });
            if hidden
                && !tab.hidden
                && let Some(gesture) = tab.gesture.as_mut()
                && gesture.window.is_collecting(at)
            {
                gesture.window.flush(&tab.mirror);
            }
            tab.hidden = hidden;
            tab.record(effect, at, Attribution::WhileOpen, actions);
            let detail = if hidden {
                Detail::Hidden
            } else {
                Detail::Visible
            };
            actions.push(Action::new(detail, context.relative(at), tab.number));
        }
        _ => return false,
    }
    true
}

/// Handle a plugin event; `false` for plugins the compiler does not read.
pub(super) fn on_plugin(
    context: &Context<'_>,
    tab: &mut Tab,
    coverage: &mut Coverage,
    actions: &mut Vec<Action>,
    name: &str,
    payload: &Value,
    at: Timestamp,
) -> bool {
    match name {
        "rrweb/console@1" => {
            if payload["level"] == "error"
                && let Some(extension) = ["payload", "trace", "stack"]
                    .into_iter()
                    .find_map(|field| extension_in_value(&payload[field]))
            {
                coverage.extension(extension);
                return true;
            }
            if let Some(message) = console_error(payload) {
                let effect = Effect::unseen(
                    context.relative(at),
                    Change::Console {
                        level: "error".into(),
                        message: message.clone(),
                    },
                );
                tab.record(effect, at, Attribution::WhileOpen, actions);
                tab.flag_error(at, Attribution::WhileOpen, actions);
                actions.push(tab.action(context, Detail::ConsoleError { message }, at));
            }
        }
        "rrweb/network@1" => {
            for request in product_requests(
                payload,
                at,
                &context.matcher.vocabulary().telemetry,
                coverage,
            ) {
                let effect = Effect::unseen(
                    context.relative(request.at),
                    Change::Request {
                        path: request.path.clone(),
                        status: request.status,
                        ms: request.duration_ms,
                    },
                );
                tab.record(effect, request.at, Attribution::WithinBounds, actions);
                if let Some(status) = request.status.filter(|s| *s >= 400) {
                    tab.flag_error(request.at, Attribution::WithinBounds, actions);
                    let detail = Detail::NetError {
                        status,
                        request: request.path,
                    };
                    actions.push(tab.action(context, detail, request.at));
                }
            }
        }
        _ => return false,
    }
    true
}

fn extension_in_value(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => extension_url(text),
        Value::Array(parts) => parts.iter().find_map(extension_in_value),
        Value::Object(fields) => fields.values().find_map(extension_in_value),
        _ => None,
    }
}

/// An error-level console message, whitespace runs collapsed, cut to 160 UTF-16 units.
fn console_error(payload: &Value) -> Option<String> {
    if payload["level"] != "error" {
        return None;
    }
    let message = match &payload["payload"] {
        Value::Array(parts) => parts.iter().map(js_string).collect::<Vec<_>>().join(" "),
        Value::Null => String::new(),
        other => js_string(other),
    };
    let message = utf16_slice(&collapse_runs(&message), 0, Some(160));
    (!message.is_empty()).then_some(message)
}

struct Request {
    /// Absolute start time.
    at: Timestamp,
    path: String,
    status: Option<u16>,
    duration_ms: Option<Millis>,
}

/// The product's own fetch/XHR requests in a network plugin payload.
fn product_requests(
    payload: &Value,
    fallback_at: Timestamp,
    telemetry: &[String],
    coverage: &mut Coverage,
) -> Vec<Request> {
    let Some(requests) = payload["requests"].as_array() else {
        return Vec::new();
    };
    requests
        .iter()
        .filter_map(|request| {
            let name = request["name"].as_str()?;
            if let Some(extension) = extension_request_url(name) {
                coverage.extension(extension);
                return None;
            }
            let is_product = matches!(
                request["initiatorType"].as_str(),
                Some("fetch" | "xmlhttprequest")
            ) && !TELEMETRY.iter().any(|t| name.contains(t))
                && !telemetry.iter().any(|t| name.contains(t.as_str()));
            if !is_product {
                return None;
            }
            let number = |field: &str| request[field].as_f64();
            let at = match (number("timeOrigin"), number("startTime")) {
                (Some(origin), Some(start)) => Timestamp(origin + start),
                _ => fallback_at,
            };
            let duration = match (
                number("duration"),
                number("startTime"),
                number("responseEnd"),
            ) {
                (Some(duration), _, _) if duration > 0.0 => Some(duration),
                (_, Some(start), Some(end)) if end > start => Some(end - start),
                _ => None,
            };
            Some(Request {
                at,
                path: path_and_query(name),
                // Absent or 0 when the recorder saw no response (blocked, cross-origin).
                status: number("responseStatus")
                    .filter(|s| (1.0..=999.0).contains(s))
                    .map(|s| s as u16),
                duration_ms: duration.map(|ms| Millis(round_half_up(ms))),
            })
        })
        .collect()
}

//! Text for people and the narrator. The only place effects become strings.

use super::{
    Action, ActionKind, Detail,
    effect::{Change, Effect, OverlayOp, Presence, TextOp},
};
use crate::{
    text::{clip, number_to_string, round_half_up, to_fixed_1},
    vocab::pathname,
};
use std::collections::HashSet;

fn quoted(text: &str, max: usize) -> String {
    format!("\"{}\"", clip(text, max))
}

fn sign(op: Presence) -> &'static str {
    match op {
        Presence::Add => "+",
        Presence::Remove => "-",
    }
}

pub fn render_effect(effect: &Effect) -> String {
    match &effect.change {
        Change::Nav { to } => format!("→ {to}"),
        Change::Cell {
            row,
            col,
            before,
            after,
            ..
        } => format!(
            "cell {col} {}: {} → {}",
            quoted(&row.label, 50),
            quoted(before, 80),
            quoted(after, 80)
        ),
        Change::Row { op, row, .. } => format!("{}row {}", sign(*op), quoted(&row.label, 50)),
        Change::Rerender { row, .. } => format!("re-rendered row {}", quoted(&row.label, 50)),
        Change::Overlay {
            op,
            role,
            title,
            lived_ms,
            ..
        } => {
            let title = if title.is_empty() {
                String::new()
            } else {
                format!(" {}", quoted(title, 40))
            };
            match (lived_ms, op) {
                (Some(lived), _) => {
                    format!(
                        "+{role}{title} (gone after {}ms)",
                        number_to_string(lived.0)
                    )
                }
                (None, OverlayOp::Open) => format!("+{role}{title}"),
                (None, OverlayOp::Close) => format!("-{role}{title}"),
            }
        }
        Change::Text {
            op, text, before, ..
        } => match (op, before.as_deref()) {
            (TextOp::Add, _) => format!("+text {}", quoted(text, 100)),
            (TextOp::Remove, _) => format!("-text {}", quoted(text, 100)),
            (TextOp::Change, Some(before)) if !before.is_empty() => {
                format!("text {} → {}", quoted(before, 60), quoted(text, 60))
            }
            (TextOp::Change, _) => format!("text {}", quoted(text, 60)),
        },
        Change::State { attr, .. } if attr == "class" => "restyle".into(),
        Change::State {
            attr,
            before,
            after,
            ..
        } => match (before.as_deref(), after.as_deref()) {
            (Some(before), Some(after)) => {
                format!("{attr}:{}→{}", clip(before, 40), clip(after, 40))
            }
            (None, Some(after)) => format!("{attr}={}", clip(after, 40)),
            (Some(before), None) => format!("{attr}:{}→∅", clip(before, 40)),
            (None, None) => attr.clone(),
        },
        Change::Widget { op, .. } => format!("{}widget", sign(*op)),
        Change::Request { path, status, ms } => {
            let duration =
                ms.map_or_else(String::new, |ms| format!(" {}ms", number_to_string(ms.0)));
            match status {
                Some(status) if *status >= 400 => format!("net {status} {path}{duration}"),
                Some(status) => format!("req {} {status}{duration}", pathname(path)),
                None => format!("req {}{duration}", pathname(path)),
            }
        }
        Change::Selection { text } => format!("selected {}", quoted(text, 41)),
        Change::Console { level, .. } => format!("console.{level}"),
        Change::Visibility { hidden } => {
            format!("window {}", if *hidden { "hidden" } else { "visible" })
        }
    }
}

/// Text additions/removals shown before the rest is counted.
const MAX_TEXT_LINES: usize = 4;

/// One line per action's effects. A filter or search swaps dozens of rows; listing each costs
/// the narrator tokens and says nothing more than "results changed". Structural effects are
/// kept whole; text additions and removals keep the first few each, then a count; re-rendered
/// rows are one count. Cell diffs are never cut. Identical lines are said once.
pub fn render_effects(effects: &[Effect]) -> String {
    let mut lines = Vec::new();
    let mut said = HashSet::new();
    let (mut added, mut removed) = (0, 0);
    let mut rerendered = 0;
    let mut rerender_line = 0;
    for effect in effects {
        if matches!(effect.change, Change::Rerender { .. }) {
            if rerendered == 0 {
                rerender_line = lines.len();
                lines.push(String::new());
            }
            rerendered += 1;
            continue;
        }
        let line = render_effect(effect);
        if !said.insert(line.clone()) {
            continue;
        }
        if let Change::Text { op, .. } = effect.change
            && op != TextOp::Change
        {
            let count = if op == TextOp::Add {
                &mut added
            } else {
                &mut removed
            };
            *count += 1;
            if *count > MAX_TEXT_LINES {
                continue;
            }
        }
        lines.push(line);
    }
    if rerendered > 0 {
        lines[rerender_line] = format!("re-rendered {rerendered} rows");
    }
    if added > MAX_TEXT_LINES {
        lines.push(format!("+{} more text", added - MAX_TEXT_LINES));
    }
    if removed > MAX_TEXT_LINES {
        lines.push(format!("-{} more text", removed - MAX_TEXT_LINES));
    }
    lines.join("; ")
}

/// The trace's effect column: what the input typed, then what the gesture changed.
pub fn effect_cell(action: &Action) -> String {
    let effects = render_effects(&action.effects);
    match &action.detail {
        Detail::Idle { idle_ms } => {
            format!("{}s", number_to_string(round_half_up(idle_ms.0 / 1000.0)))
        }
        Detail::Input(entry) if effects.is_empty() => format!("typed {}", entry.typed),
        Detail::Input(entry) => format!("typed {}; {effects}", entry.typed),
        Detail::Click(_) if effects.is_empty() => "none".into(),
        _ => effects,
    }
}

/// How fast the product reacted: `120ms`, `120→480ms`, `on press`, `on press→300ms`.
fn reaction(action: &Action) -> String {
    let Some(reaction) = action.reaction() else {
        return String::new();
    };
    let (react, settle) = (reaction.react_ms.0, reaction.settle_ms.0);
    // A reaction that kept moving for 50 ms or more shows when it settled.
    let settle = Some(settle).filter(|settle| settle - react >= 50.0);
    let n = number_to_string;
    match (react < 0.0, settle) {
        (true, Some(settle)) if settle > 0.0 => format!("on press→{}ms", n(settle)),
        (true, _) => "on press".into(),
        (false, Some(settle)) => format!("{}→{}ms", n(react), n(settle)),
        (false, None) => format!("{}ms", n(react)),
    }
}

pub const TSV_HEADER: &str = "ref\tt_s\twin\tkind\tsurface\ttarget\tfeature\treact\teffect\tflags";

pub fn to_tsv(actions: &[Action]) -> String {
    let mut out = String::from(TSV_HEADER);
    for action in actions {
        let target = if action.kind() == ActionKind::Nav {
            action.path.as_deref().map(std::borrow::Cow::Borrowed)
        } else {
            action.target()
        };
        let reference = action.reference.to_string();
        let flags: Vec<&str> = action.flags.iter().map(|flag| flag.as_str()).collect();
        let cells = [
            &reference,
            &to_fixed_1(action.t_ms.0 / 1000.0),
            &format!("w{}", action.win),
            action.kind().as_str(),
            action.place().unwrap_or_default(),
            target.as_deref().unwrap_or_default(),
            action.feature().unwrap_or_default(),
            &reaction(action),
            &effect_cell(action),
            &flags.join(","),
        ];
        out.push('\n');
        for (index, cell) in cells.into_iter().enumerate() {
            if index > 0 {
                out.push('\t');
            }
            // Tabs and line breaks inside a cell would break the row.
            if cell.contains(['\t', '\n', '\r']) {
                out.push_str(&cell.replace(['\t', '\n', '\r'], " "));
            } else {
                out.push_str(cell);
            }
        }
    }
    out
}

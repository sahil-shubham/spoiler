//! Target resolution: from the node an event names to the element a user meant, how it is
//! described, and which vocabulary feature it is.

use super::{ActionTarget, TargetClass};
use crate::{
    recording::rrweb::NodeId,
    replay::{
        Mirror, Node,
        grid::{cell_context, cell_of},
        is_interactive,
    },
    text::{clip, collapse_whitespace, truncate, utf16_len},
    vocab::{GridRules, Matcher, TargetDesc, path_and_query},
};

/// How many element ancestors are considered when looking for the control a click hit.
const MAX_CHAIN: usize = 8;

const MOBILE_X: &str = "data-spoiler-mobile-x";
const MOBILE_Y: &str = "data-spoiler-mobile-y";
const MOBILE_WIDTH: &str = "data-spoiler-mobile-width";
const MOBILE_HEIGHT: &str = "data-spoiler-mobile-height";

/// Recorded mobile rectangles are absolute viewport coordinates, including parent offsets.
/// Prefer interactive controls, then depth and later siblings for overlapping wireframes.
pub(crate) fn mobile_hit(mirror: &Mirror, root: NodeId, x: f64, y: f64) -> Option<NodeId> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let mut best: Option<(bool, usize, NodeId, bool)> = None;
    let mut screenshot_seen = false;
    let mut stack = vec![(root, 0)];
    while let Some((id, depth)) = stack.pop() {
        let Some(node) = mirror.get(id) else {
            continue;
        };
        screenshot_seen |= node.has_attribute("data-posthog-screenshot");
        if node.extension.is_some() || mobile_excluded(node) {
            continue;
        }
        if node.is_element()
            && let Some((left, top, width, height)) = mobile_rect(node)
            && x >= left
            && y >= top
            && x < left + width
            && y < top + height
        {
            let interactive = is_interactive(node);
            if best.is_none_or(|(was_interactive, old_depth, _, _)| {
                (interactive, depth) >= (was_interactive, old_depth)
            }) {
                best = Some((interactive, depth, id, mobile_label(mirror, node)));
            }
        }
        // Reverse push visits siblings in document order, so the last sibling wins an overlap.
        stack.extend(node.children.iter().rev().map(|&child| (child, depth + 1)));
    }
    best.and_then(|(interactive, _, id, labelled)| {
        // A screenshot-only screen can have an unlabelled outer wireframe container; that
        // container does not reveal which control a user tapped in the image.
        (!screenshot_seen || interactive || labelled).then_some(id)
    })
}

/// User-readable wireframe content, as opposed to a screenshot or app chrome.
pub(crate) fn mobile_has_labels(mirror: &Mirror, root: NodeId) -> bool {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some(node) = mirror.get(id) else {
            continue;
        };
        if node.extension.is_some() || mobile_excluded(node) {
            continue;
        }
        if node.is_element() && node.has_attribute(MOBILE_X) && mobile_label(mirror, node) {
            return true;
        }
        stack.extend(node.children.iter().copied());
    }
    false
}

fn mobile_rect(node: &Node) -> Option<(f64, f64, f64, f64)> {
    let number = |key| {
        node.attr(key)?
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
    };
    let (x, y, width, height) = (
        number(MOBILE_X)?,
        number(MOBILE_Y)?,
        number(MOBILE_WIDTH)?,
        number(MOBILE_HEIGHT)?,
    );
    (width > 0.0 && height > 0.0).then_some((x, y, width, height))
}

fn mobile_excluded(node: &Node) -> bool {
    node.has_attribute("data-spoiler-mobile-chrome")
        || node.has_attribute("data-posthog-screenshot")
        || node.has_attribute("hidden")
        || node.attr("aria-hidden") == Some("true")
        || crate::replay::is_non_visual(&node.tag)
        || node.attr("style").is_some_and(|style| {
            style.split(';').any(|declaration| {
                let Some((property, value)) = declaration.split_once(':') else {
                    return false;
                };
                (property.trim().eq_ignore_ascii_case("display")
                    && value.trim().eq_ignore_ascii_case("none"))
                    || (property.trim().eq_ignore_ascii_case("visibility")
                        && value.trim().eq_ignore_ascii_case("hidden"))
            })
        })
}

fn mobile_label(mirror: &Mirror, node: &Node) -> bool {
    node.children
        .iter()
        .filter_map(|&id| mirror.get(id))
        .any(|child| {
            child.kind == crate::replay::NodeKind::Text
                && !crate::text::trim(&child.text).is_empty()
        })
        || ["aria-label", "title", "placeholder", "alt"]
            .iter()
            .any(|attr| node.attr(attr).is_some())
        || matches!(node.tag.as_str(), "input" | "select")
            && node.attr("type") != Some("password")
            && node.attr("value").is_some()
}

/// Mobile-only text: a hidden subtree or system chrome must not become its parent's label.
fn mobile_text_preview(mirror: &Mirror, id: NodeId, limit: usize) -> String {
    let mut parts = Vec::new();
    let mut length = 0;
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        let Some(node) = mirror.get(id) else {
            continue;
        };
        if node.extension.is_some() || mobile_excluded(node) {
            continue;
        }
        if node.kind == crate::replay::NodeKind::Text {
            let text = collapse_whitespace(&node.text);
            if text.is_empty() || text == crate::replay::SCRIPT_PLACEHOLDER {
                continue;
            }
            length += utf16_len(&text) + 1;
            parts.push(text);
            if length >= limit {
                break;
            }
        }
        stack.extend(node.children.iter().rev().copied());
    }
    truncate(parts.join(" "), limit)
}

pub(crate) struct Resolution {
    /// Rendered for the trace: `button[testid] "Save"`, or a grid cell description.
    pub label: String,
    pub feature: Option<String>,
    pub desc: ActionTarget,
    pub class: TargetClass,
}

/// Resolve the target of an event on node `id`: the nearest interactive element among the node
/// and its first [`MAX_CHAIN`] element ancestors, else the element itself.
pub(crate) fn resolve(
    mirror: &Mirror,
    matcher: &Matcher<'_>,
    rules: &GridRules,
    surface: Option<&str>,
    id: NodeId,
) -> Option<Resolution> {
    resolve_with_text(mirror, matcher, rules, surface, id, false)
}

/// Native wireframe labels must omit hidden children and system chrome.
pub(crate) fn resolve_mobile(
    mirror: &Mirror,
    matcher: &Matcher<'_>,
    rules: &GridRules,
    surface: Option<&str>,
    id: NodeId,
) -> Option<Resolution> {
    resolve_with_text(mirror, matcher, rules, surface, id, true)
}

fn resolve_with_text(
    mirror: &Mirror,
    matcher: &Matcher<'_>,
    rules: &GridRules,
    surface: Option<&str>,
    id: NodeId,
    mobile: bool,
) -> Option<Resolution> {
    let node = mirror.get(id)?;
    if node.extension.is_some() {
        return None;
    }
    let start = if node.is_element() {
        node
    } else {
        mirror.parent(id)?
    };
    let chain: Vec<&Node> = mirror
        .lineage(start.id)
        .filter(|node| node.is_element())
        .take(MAX_CHAIN)
        .collect();
    let hit = chain.iter().position(|node| is_interactive(node));
    let depth = hit.unwrap_or(0);
    let target = *chain.get(depth)?;
    let descs: Vec<TargetDesc> = chain[depth..]
        .iter()
        .map(|node| describe_with_text(mirror, node, mobile))
        .collect();

    let mut label = render(&descs[0]);
    let mut feature = matcher.feature(surface, &descs).map(|f| f.id.clone());
    if let Some(cell) = cell_of(mirror, target.id) {
        let (column, row) = cell_context(mirror, rules, cell);
        let place = format!("cell[{column}] \"{row}\"");
        label = if cell == target.id {
            format!("{place}: \"{}\"", clip(&mirror.visible_text(cell), 60))
        } else {
            format!("{label} in {place}")
        };
        // A control inside a cell keeps its own feature; a bare cell takes its column's.
        let generic = feature
            .as_deref()
            .is_none_or(|f| rules.generic_cell_features.includes(f));
        if generic {
            let header = TargetDesc {
                tag: "th".into(),
                text: column,
                ..TargetDesc::default()
            };
            feature = matcher.feature(surface, &[header]).map(|f| f.id.clone());
        }
    }

    let class = if matches!(target.tag.as_str(), "input" | "textarea" | "select") {
        TargetClass::Focus
    } else if hit.is_some() {
        TargetClass::Interactive
    } else {
        TargetClass::Inert
    };
    let element = descs.into_iter().next()?;
    Some(Resolution {
        label,
        feature,
        desc: ActionTarget { element, depth },
        class,
    })
}

fn describe_with_text(mirror: &Mirror, node: &Node, mobile: bool) -> TargetDesc {
    let attr = |name: &str| node.attr(name).map(str::to_owned);
    TargetDesc {
        tag: if node.tag.is_empty() {
            "?".into()
        } else {
            node.tag.clone()
        },
        // A click on the page background resolves to <html>/<body>; its "text" is the page.
        text: if matches!(node.tag.as_str(), "html" | "body") {
            String::new()
        } else if mobile {
            mobile_text_preview(mirror, node.id, 60)
        } else {
            mirror.text_preview(node.id, 60)
        },
        aria: attr("aria-label"),
        testid: attr("data-testid").or_else(|| attr("data-attr")),
        role: attr("role"),
        href: attr("href"),
        classes: attr("class"),
        placeholder: attr("placeholder"),
        title: attr("title").or_else(|| attr("alt")),
        data: node
            .attributes
            .iter()
            .filter_map(|(name, value)| {
                if name.starts_with("data-spoiler-mobile-") {
                    return None;
                }
                Some((
                    name.strip_prefix("data-")?.to_owned(),
                    value.as_str()?.to_owned(),
                ))
            })
            .collect(),
    }
}

/// `tag[testid] "label"`, `tag[role=r] "label"`, or just `tag`.
fn render(desc: &TargetDesc) -> String {
    let label = [
        Some(desc.text.as_str()),
        desc.aria.as_deref(),
        desc.title.as_deref(),
        desc.placeholder.as_deref(),
        desc.testid.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|label| !label.is_empty())
    .map(str::to_owned)
    .or_else(|| desc.href.as_deref().map(path_and_query))
    .unwrap_or_default();
    let id = match (&desc.testid, &desc.role) {
        (Some(testid), _) => format!("[{testid}]"),
        (None, Some(role)) => format!("[role={role}]"),
        (None, None) => String::new(),
    };
    if label.is_empty() {
        format!("{}{id}", desc.tag)
    } else {
        format!("{}{id} \"{label}\"", desc.tag)
    }
}

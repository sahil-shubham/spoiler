//! What a subtree is: overlays, widget state, and its text.

use crate::{
    recording::rrweb::{AttrValue, NodeId},
    replay::{Mirror, SCRIPT_PLACEHOLDER, is_interactive, is_non_visual},
    text::{JS_SPACE_CLASS, strip_whitespace, trim},
};
use regex::Regex;
use std::sync::LazyLock;

/// Attributes whose changes are widget state a user can see.
pub(super) fn is_state_attribute(name: &str) -> bool {
    matches!(
        name,
        "aria-expanded"
            | "aria-selected"
            | "aria-checked"
            | "aria-pressed"
            | "data-state"
            | "open"
            | "hidden"
            | "disabled"
    )
}

pub(super) fn is_overlay_role(role: &str) -> bool {
    matches!(
        role,
        "dialog" | "alertdialog" | "alert" | "status" | "menu" | "listbox" | "tooltip"
    )
}

/// Overlays that announce something and leave by themselves: a toast that comes and goes
/// inside the window is kept, as a confirmation the user saw.
pub(super) fn is_toast_role(role: &str) -> bool {
    matches!(role, "status" | "alert")
}

pub(super) const MAX_OVERLAY_SCAN: usize = 400;
pub(super) const MAX_ATTACH_POINT_HOPS: usize = 1000;

pub(super) static SR_ONLY_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)sr-only(?-u:\b)").expect("valid regex"));

pub(super) static CLIPPED_STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"clip:{JS_SPACE_CLASS}*rect\(0")).expect("valid regex"));

fn is_screen_reader_only(node: &crate::replay::Node) -> bool {
    let matches = |name: &str, pattern: &Regex| {
        node.attribute(name).is_some_and(|value| match value {
            AttrValue::Text(text) => pattern.is_match(text),
            _ => pattern.is_match(&value.js_string()),
        })
    };
    matches("class", &SR_ONLY_CLASS) || matches("style", &CLIPPED_STYLE)
}

#[derive(Clone, Debug)]
pub(super) struct Overlay {
    pub(super) role: String,
    pub(super) title: String,
    pub(super) node: NodeId,
    /// Screen-reader live regions (clipped to nothing): announced, never seen.
    pub(super) screen_reader_only: bool,
}

/// What a subtree is: an overlay (its first overlay-role element), or whether it carries widget
/// state (a control, or state attributes; an icon-only toolbar has no text but is a change).
#[derive(Clone, Debug, Default)]
pub(super) struct Scan {
    pub(super) overlay: Option<Overlay>,
    pub(super) has_state: bool,
}

pub(super) fn scan_subtree(mirror: &Mirror, root: NodeId) -> Scan {
    let mut has_state = false;
    let mut stack = vec![root];
    for _ in 0..MAX_OVERLAY_SCAN {
        let Some(id) = stack.pop() else { break };
        let Some(node) = mirror.get(id) else { continue };
        if !node.is_element() || node.extension.is_some() || is_non_visual(&node.tag) {
            continue;
        }
        if let Some(role) = node.attr("role").filter(|role| is_overlay_role(role)) {
            let overlay = Overlay {
                role: role.to_owned(),
                title: node
                    .attr("aria-label")
                    .map_or_else(|| mirror.text_preview(id, 120), str::to_owned),
                node: id,
                screen_reader_only: is_screen_reader_only(node),
            };
            return Scan {
                overlay: Some(overlay),
                has_state: true,
            };
        }
        has_state = has_state
            || is_interactive(node)
            || node
                .attributes
                .iter()
                .any(|(name, _)| is_state_attribute(name));
        stack.extend(&node.children);
    }
    Scan {
        overlay: None,
        has_state,
    }
}

/// A subtree's text values with whitespace removed: a re-render may split the same text
/// differently across elements, so text is compared as a multiset of these.
pub(super) fn text_bag(mirror: &Mirror, root: NodeId) -> Vec<String> {
    mirror
        .text_nodes(root, is_non_visual)
        .filter(|node| !trim(&node.text).is_empty() && node.text != SCRIPT_PLACEHOLDER)
        .map(|node| strip_whitespace(&node.text))
        .collect()
}

/// Under `<head>`, `<style>`, `<script>`…: never on screen (a `<title>` change included).
pub(super) fn is_offscreen(mirror: &Mirror, id: NodeId) -> bool {
    mirror.lineage(id).any(|node| {
        node.extension.is_some() || is_non_visual(&node.tag) || is_screen_reader_only(node)
    })
}

/// The text content of a subtree, for netting against other subtrees at the same place.
#[derive(Clone, Debug)]
pub(super) struct Content {
    pub(super) text: String,
    pub(super) bag: Vec<String>,
}

impl Content {
    pub(super) fn of(mirror: &Mirror, id: NodeId) -> Self {
        Self {
            text: mirror.visible_text(id),
            bag: text_bag(mirror, id),
        }
    }

    pub(super) fn empty() -> Self {
        Self {
            text: String::new(),
            bag: Vec::new(),
        }
    }
}

//! Replaying a tab's DOM from rrweb snapshots and mutations, and reading it the way users do.

pub mod grid;
mod mirror;

pub(crate) use mirror::serialized_ids;
pub use mirror::{Mirror, Node, NodeKind, SCRIPT_PLACEHOLDER, TextNodes};

/// Elements whose content is never on screen.
pub fn is_non_visual(tag: &str) -> bool {
    matches!(
        tag,
        "style" | "script" | "head" | "noscript" | "template" | "link" | "meta"
    )
}

/// Whether an element is a control a user would expect to respond to a click.
pub fn is_interactive(node: &Node) -> bool {
    let tag_is_control = matches!(
        node.tag.as_str(),
        "a" | "button" | "input" | "select" | "textarea" | "label" | "summary" | "option" | "th"
    );
    let role_is_control = matches!(
        node.attr("role"),
        Some(
            "button"
                | "link"
                | "tab"
                | "menuitem"
                | "menuitemcheckbox"
                | "menuitemradio"
                | "option"
                | "checkbox"
                | "radio"
                | "switch"
                | "combobox"
                | "gridcell"
                | "treeitem"
                | "columnheader"
        )
    );
    let focusable = node
        .attribute("tabindex")
        .and_then(|v| v.as_str())
        .is_some_and(|tabindex| crate::text::parse_number(tabindex) >= 0.0);
    tag_is_control
        || role_is_control
        || node.has_attribute("onclick")
        || focusable
        || node.attribute("contenteditable").and_then(|v| v.as_str()) == Some("true")
}

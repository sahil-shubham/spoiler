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
    text::clip,
    vocab::{GridRules, Matcher, TargetDesc, path_and_query},
};

/// How many element ancestors are considered when looking for the control a click hit.
const MAX_CHAIN: usize = 8;

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
    let node = mirror.get(id)?;
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
        .map(|node| describe(mirror, node))
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

pub(crate) fn describe(mirror: &Mirror, node: &Node) -> TargetDesc {
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

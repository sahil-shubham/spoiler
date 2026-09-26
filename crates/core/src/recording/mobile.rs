//! Native PostHog replay wireframes → the rrweb DOM subset used by the mirror.
//!
//! Based on PostHog's MIT-licensed `common/replay-shared/src/mobile/transformer/transformers.ts`
//! and `screen-chrome.ts` (https://github.com/PostHog/posthog). We keep only semantic content,
//! controls and rectangles: CSS, screenshot bytes, decorative icons and fonts are not traces.
//! PostHog reserves rrweb ids 1–12 for document/chrome and 100–9,999,999 for synthetic nodes,
//! but native SDK hashes can occupy that synthetic range too. Reserve native ids across the
//! recording before generating nodes. Native iOS uses `toAbsoluteRect(window)` and Android
//! `getLocationOnScreen`, so child x/y already include their parents' offsets.

use super::{Data, Entry, parse_borrowed, parse_deep, rrweb};
use rrweb::{
    Add, AttrValue, AttributeChange, Attributes, Mutation, NodeId, Remove, SerializedNode,
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::collections::HashSet;

pub(super) const BODY_ID: NodeId = 5;
const KEYBOARD_PARENT_ID: NodeId = 9;
const KEYBOARD_ID: NodeId = 10;
const NAV_PARENT_ID: NodeId = 7;
const STATUS_PARENT_ID: NodeId = 11;
const SYNTHETIC_FIRST: NodeId = 100;
const SYNTHETIC_LAST: NodeId = 9_999_999;
// Keep recursive conversion and destruction of owned wireframes well within the thread stack.
const MAX_WIREFRAME_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Context {
    pub mobile: bool,
    pub viewport_width: Option<f64>,
    pub offset_left: f64,
    pub offset_top: f64,
}

#[derive(Deserialize)]
struct Wireframe {
    id: NodeId,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    width: Value,
    height: f64,
    #[serde(default, rename = "childWireframes")]
    children: Vec<Wireframe>,
    text: Option<String>,
    value: Option<Value>,
    label: Option<String>,
    url: Option<String>,
    #[serde(rename = "inputType")]
    input_type: Option<String>,
    checked: Option<bool>,
    disabled: Option<bool>,
    options: Option<Vec<String>>,
    max: Option<Value>,
}

#[derive(Deserialize, Default)]
struct Offset {
    #[serde(default)]
    top: f64,
    #[serde(default)]
    left: f64,
}

/// Tiny borrowed probes: index metadata without decoding any screenshot bytes.
#[derive(Deserialize)]
struct FullProbe<'a> {
    #[serde(borrow)]
    wireframes: Option<&'a RawValue>,
    #[serde(default, rename = "initialOffset")]
    offset: Offset,
}

#[derive(Deserialize)]
struct WidthProbe {
    width: Option<f64>,
}

#[derive(Deserialize)]
struct AddProbe<'a> {
    #[serde(borrow)]
    wireframe: Option<&'a RawValue>,
    #[serde(borrow)]
    node: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct AddsProbe<'a> {
    #[serde(borrow)]
    adds: Option<Vec<AddProbe<'a>>>,
    #[serde(borrow)]
    updates: Option<Vec<AddProbe<'a>>>,
}
#[derive(Deserialize)]
struct WireframeIdProbe<'a> {
    id: NodeId,
    #[serde(borrow, rename = "childWireframes")]
    children: Option<&'a RawValue>,
}

// Borrow each JSON subtree instead of deserializing screenshots or allocating owned wireframes.
// The explicit worklist also handles malicious nesting without recursive parsing or dropping.
fn scan_wireframes<'a>(
    roots: impl IntoIterator<Item = &'a RawValue>,
    max_depth: usize,
    mut on_id: impl FnMut(NodeId),
) -> bool {
    let mut pending: Vec<_> = roots.into_iter().map(|root| (root, 1)).collect();
    while let Some((raw, depth)) = pending.pop() {
        if depth > max_depth {
            return false;
        }
        let Ok(probe) = parse_borrowed::<WireframeIdProbe<'_>>(raw.get()) else {
            return false;
        };
        on_id(probe.id);
        if let Some(children) = probe.children {
            let Ok(children) = parse_borrowed::<Vec<&RawValue>>(children.get()) else {
                return false;
            };
            pending.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }
    true
}

fn full_depth_ok(data: &str) -> bool {
    let Ok(probe) = parse_borrowed::<FullProbe<'_>>(data) else {
        return false;
    };
    let Some(raw) = probe.wireframes else {
        return false;
    };
    parse_borrowed::<Vec<&RawValue>>(raw.get())
        .is_ok_and(|roots| scan_wireframes(roots, MAX_WIREFRAME_DEPTH, |_| {}))
}

fn mutation_depth_ok(data: &str) -> bool {
    let Ok(probe) = parse_borrowed::<AddsProbe<'_>>(data) else {
        return false;
    };
    scan_wireframes(
        probe
            .adds
            .into_iter()
            .flatten()
            .chain(probe.updates.into_iter().flatten())
            .filter_map(|add| add.wireframe),
        MAX_WIREFRAME_DEPTH,
        |_| {},
    )
}

pub(super) fn has_wireframe_mutation(json: &str) -> bool {
    json.contains("\"wireframe\"")
        && parse_borrowed::<AddsProbe<'_>>(json).is_ok_and(|probe| {
            probe
                .adds
                .into_iter()
                .flatten()
                .chain(probe.updates.into_iter().flatten())
                .any(|add| add.wireframe.is_some())
        })
}

#[derive(Deserialize)]
struct ScreenshotNode<'a> {
    #[serde(rename = "tagName")]
    tag: Option<&'a str>,
    #[serde(borrow)]
    attributes: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ScreenshotAttrs {
    #[serde(rename = "data-posthog-screenshot")]
    marker: Option<Value>,
}

#[derive(Deserialize)]
struct ScreenshotKind<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
}

/// Match a JSON *key*, not a text node whose content happens to name the marker.
pub(super) fn screenshot_attribute_key(json: &str) -> bool {
    const KEY: &str = "\"data-posthog-screenshot\"";
    json.match_indices(KEY)
        .any(|(at, _)| json[at + KEY.len()..].trim_start().starts_with(':'))
}

/// Bootstrap the first screenshot mutation when a window has no full snapshot yet.
/// The player detects screenshot images among the first three adds.
fn screenshot_add(json: &str) -> bool {
    let Ok(probe) = parse_borrowed::<AddsProbe<'_>>(json) else {
        return false;
    };
    probe
        .adds
        .unwrap_or_default()
        .into_iter()
        .take(3)
        .any(|add| {
            if let Some(wireframe) = add.wireframe
                && let Ok(kind) = parse_borrowed::<ScreenshotKind<'_>>(wireframe.get())
                && kind.kind == Some("screenshot")
            {
                return true;
            }
            let Some(node) = add
                .node
                .and_then(|node| parse_borrowed::<ScreenshotNode<'_>>(node.get()).ok())
            else {
                return false;
            };
            node.tag == Some("img")
                && node
                    .attributes
                    .and_then(|attrs| parse_borrowed::<ScreenshotAttrs>(attrs.get()).ok())
                    .and_then(|attrs| attrs.marker)
                    .is_some_and(|value| {
                        value == true || value.as_str().is_some_and(|value| !value.is_empty())
                    })
        })
}

pub(super) fn index_contexts(
    entries: &mut Vec<Entry>,
    text: &str,
    tabs: usize,
) -> Option<(Vec<Context>, Vec<HashSet<NodeId>>)> {
    let data = |entry: &Entry| match entry.data {
        Data::Span { start, end } => &text[start..end],
        _ => "null",
    };
    if !entries.iter().any(|entry| {
        let json = data(entry);
        matches!(
            entry.kind,
            rrweb::FULL_SNAPSHOT | rrweb::INCREMENTAL_SNAPSHOT
        ) && (json.contains("\"wireframes\"")
            || json.contains("\"wireframe\"")
            || json.contains("\"data-posthog-screenshot\""))
    }) {
        return None;
    }
    let mut by_tab = vec![Context::default(); tabs];
    let mut reserved = vec![HashSet::new(); tabs];
    let mut has_full = vec![false; tabs];
    let mut last_meta = vec![None; tabs];
    let mut indexed = Vec::with_capacity(entries.len());
    let mut contexts = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        let tab = entry.tab as usize;
        let json = data(&entry);
        if entry.kind == rrweb::META {
            last_meta[tab] = Some(indexed.len());
            if let Ok(meta) = parse_borrowed::<WidthProbe>(json) {
                by_tab[tab].viewport_width = meta.width;
            }
        }
        if entry.kind == rrweb::FULL_SNAPSHOT {
            has_full[tab] = true;
            by_tab[tab].mobile = false;
            if let Ok(probe) = parse_borrowed::<FullProbe<'_>>(json)
                && let Some(raw) = probe.wireframes
                && raw.get().starts_with('[')
            {
                if let Ok(roots) = parse_borrowed::<Vec<&RawValue>>(raw.get()) {
                    scan_wireframes(roots, usize::MAX, |id| {
                        reserved[tab].insert(id);
                    });
                }
                by_tab[tab].mobile = true;
                by_tab[tab].offset_left = probe.offset.left;
                by_tab[tab].offset_top = probe.offset.top;
            } else if screenshot_attribute_key(json) {
                // Already-transformed screenshot-mode DOM snapshots have no `wireframes`.
                by_tab[tab].mobile = true;
            }
        }
        if entry.kind == rrweb::INCREMENTAL_SNAPSHOT
            && (json.contains("\"screenshot\"") || json.contains("\"data-posthog-screenshot\""))
            && screenshot_add(json)
        {
            by_tab[tab].mobile = true;
            if !has_full[tab] {
                let mut boot = entry;
                boot.kind = rrweb::FULL_SNAPSHOT;
                boot.timestamp = crate::time::Timestamp((entry.timestamp.0 - 1.0).max(0.0));
                boot.data = Data::MinimalScreenshot;
                indexed.push(boot);
                contexts.push(by_tab[tab]);
                has_full[tab] = true;
            }
        }
        if entry.kind == rrweb::INCREMENTAL_SNAPSHOT
            && json.contains("\"wireframe\"")
            && let Ok(probe) = parse_borrowed::<AddsProbe<'_>>(json)
        {
            let mut wireframes = probe
                .adds
                .into_iter()
                .flatten()
                .chain(probe.updates.into_iter().flatten())
                .filter_map(|add| add.wireframe)
                .peekable();
            if wireframes.peek().is_some() {
                by_tab[tab].mobile = true;
                scan_wireframes(wireframes, usize::MAX, |id| {
                    reserved[tab].insert(id);
                });
            }
        }
        if by_tab[tab].mobile
            && entry.kind != rrweb::META
            && let Some(index) = last_meta[tab]
        {
            contexts[index].mobile = true;
        }
        indexed.push(entry);
        contexts.push(by_tab[tab]);
    }
    *entries = indexed;
    Some((contexts, reserved))
}

pub(super) fn minimal_full_json() -> String {
    serde_json::json!({"node":{"id":1,"type":0,"childNodes":[
        {"id":3,"type":2,"tagName":"html","childNodes":[
            {"id":4,"type":2,"tagName":"head","childNodes":[]},
            {"id":5,"type":2,"tagName":"body","childNodes":[
                {"id":9,"type":2,"tagName":"div","attributes":{"data-spoiler-mobile-chrome":"true"},"childNodes":[]},
                {"id":7,"type":2,"tagName":"div","attributes":{"data-spoiler-mobile-chrome":"true"},"childNodes":[]},
                {"id":11,"type":2,"tagName":"div","attributes":{"data-spoiler-mobile-chrome":"true"},"childNodes":[]}
            ]}
        ]}
    ]},"initialOffset":{"top":0,"left":0}}).to_string()
}

#[derive(Deserialize)]
struct FullData {
    wireframes: Vec<Wireframe>,
    #[serde(default, rename = "initialOffset")]
    initial_offset: Offset,
}

#[derive(Deserialize)]
struct Changed {
    // Upstream screenshot transformer fixtures omit parentId; screenshots attach to body
    // regardless. Other wireframe mutations still require an SDK parent.
    #[serde(rename = "parentId")]
    parent: Option<NodeId>,
    wireframe: Wireframe,
}

#[derive(Deserialize)]
struct Incremental {
    #[serde(default)]
    adds: Vec<Changed>,
    #[serde(default)]
    updates: Vec<Changed>,
    #[serde(default)]
    removes: Vec<Remove>,
}

fn element(
    id: NodeId,
    tag: &str,
    attrs: Attributes,
    children: Vec<SerializedNode>,
) -> SerializedNode {
    let mut node = SerializedNode::default();
    node.id = id;
    node.kind = 2;
    node.tag = tag.to_owned();
    node.attributes = attrs;
    node.children = children;
    node
}

fn text(id: NodeId, content: String) -> SerializedNode {
    let mut node = SerializedNode::default();
    node.id = id;
    node.kind = 3;
    node.text = content;
    node
}

fn attrs(pairs: impl IntoIterator<Item = (&'static str, AttrValue)>) -> Attributes {
    Attributes(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn string(value: impl Into<String>) -> AttrValue {
    AttrValue::Text(value.into())
}

fn fresh(next: &mut NodeId, reserved: &HashSet<NodeId>) -> Option<NodeId> {
    while reserved.contains(next) {
        *next += 1;
    }
    (*next <= SYNTHETIC_LAST).then(|| {
        let id = *next;
        *next += 1;
        id
    })
}

fn numeric_width(width: &Value, viewport: Option<f64>) -> Option<f64> {
    width.as_f64().or_else(|| {
        (width.as_str() == Some("100vw"))
            .then_some(viewport)
            .flatten()
    })
}

fn rect_attrs(w: &Wireframe, context: Context) -> Attributes {
    let mut pairs = vec![
        (
            "data-spoiler-mobile-x".into(),
            string((w.x - context.offset_left).to_string()),
        ),
        (
            "data-spoiler-mobile-y".into(),
            string((w.y - context.offset_top).to_string()),
        ),
        (
            "data-spoiler-mobile-height".into(),
            string(w.height.to_string()),
        ),
    ];
    if let Some(width) = numeric_width(&w.width, context.viewport_width) {
        pairs.push((
            "data-spoiler-mobile-width".into(),
            string(width.to_string()),
        ));
    }
    Attributes(pairs)
}

fn convert(
    w: Wireframe,
    context: Context,
    next: &mut NodeId,
    reserved: &HashSet<NodeId>,
    chrome: &mut (Vec<SerializedNode>, Vec<SerializedNode>),
) -> Option<SerializedNode> {
    let mut attributes = rect_attrs(&w, context);
    let mut children = Vec::with_capacity(w.children.len() + 1);
    for child in w.children {
        if let Some(node) = convert(child, context, next, reserved, chrome) {
            children.push(node);
        }
    }
    let tag = match w.kind.as_str() {
        "text" => {
            if let Some(content) = w.text {
                children.insert(0, text(fresh(next, reserved)?, content));
            }
            "div"
        }
        "image" | "screenshot" => {
            // No src: base64 pixels are neither a target label nor an observable trace effect.
            if w.kind == "screenshot" {
                attributes
                    .0
                    .push(("data-posthog-screenshot".into(), string("true")));
            }
            "img"
        }
        "input" => {
            let input_type = w.input_type.as_deref().unwrap_or("text");
            if w.disabled == Some(true) {
                attributes
                    .0
                    .push(("disabled".into(), AttrValue::Json(Value::Bool(true))));
            }
            if w.checked == Some(true) {
                attributes
                    .0
                    .push(("checked".into(), AttrValue::Json(Value::Bool(true))));
            }
            if matches!(input_type, "checkbox" | "radio" | "toggle") {
                attributes.0.push((
                    "aria-checked".into(),
                    string(if w.checked.unwrap_or(false) {
                        "true"
                    } else {
                        "false"
                    }),
                ));
            }
            if let Some(label) = w.label {
                attributes.0.push(("aria-label".into(), string(label)));
            }
            match input_type {
                "button" => {
                    if let Some(label) = w.value.as_ref().and_then(Value::as_str) {
                        children.insert(0, text(fresh(next, reserved)?, label.to_owned()));
                    }
                    attributes.0.push(("type".into(), string("button")));
                    "button"
                }
                "select" => {
                    let selected = w.value.as_ref().and_then(Value::as_str);
                    let mut options = Vec::with_capacity(w.options.as_ref().map_or(0, Vec::len));
                    for option in w.options.unwrap_or_default() {
                        let mut option_attrs = Attributes::default();
                        if selected == Some(option.as_str()) {
                            option_attrs
                                .0
                                .push(("selected".into(), AttrValue::Json(Value::Bool(true))));
                        }
                        let option_id = fresh(next, reserved)?;
                        let option_text = text(fresh(next, reserved)?, option);
                        options.push(element(
                            option_id,
                            "option",
                            option_attrs,
                            vec![option_text],
                        ));
                    }
                    options.append(&mut children);
                    children = options;
                    if let Some(value) = w.value {
                        attributes.0.push((
                            "value".into(),
                            string(
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string()),
                            ),
                        ));
                    }
                    "select"
                }
                "progress" => {
                    if let Some(value) = w.value {
                        attributes.0.push((
                            "value".into(),
                            string(
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string()),
                            ),
                        ));
                    }
                    if let Some(max) = w.max {
                        attributes.0.push((
                            "max".into(),
                            string(
                                max.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| max.to_string()),
                            ),
                        ));
                    }
                    "progress"
                }
                other => {
                    attributes.0.push((
                        "type".into(),
                        string(if other == "toggle" { "checkbox" } else { other }),
                    ));
                    if other == "toggle" {
                        attributes.0.push(("role".into(), string("switch")));
                    }
                    if matches!(
                        other,
                        "text"
                            | "password"
                            | "email"
                            | "number"
                            | "search"
                            | "tel"
                            | "url"
                            | "text_area"
                    ) {
                        let value = w.value.as_ref().and_then(Value::as_str).unwrap_or("");
                        attributes.0.push(("value".into(), string(value)));
                    }
                    "input"
                }
            }
        }
        "web_view" => {
            children.insert(
                0,
                text(
                    fresh(next, reserved)?,
                    w.url.unwrap_or_else(|| "web_view".into()),
                ),
            );
            "div"
        }
        "placeholder" => {
            children.insert(
                0,
                text(
                    fresh(next, reserved)?,
                    w.label.unwrap_or_else(|| "placeholder".into()),
                ),
            );
            "div"
        }
        "radio_group" => {
            let name = format!("radio_group_{}", w.id);
            for child in &mut children {
                if child.tag == "input"
                    && child
                        .attributes
                        .0
                        .iter()
                        .any(|(k, v)| k == "type" && v.as_str() == Some("radio"))
                {
                    child
                        .attributes
                        .0
                        .push(("name".into(), string(name.clone())));
                }
            }
            "div"
        }
        "status_bar" | "navigation_bar" => {
            attributes
                .0
                .push(("data-spoiler-mobile-chrome".into(), string("true")));
            let node = element(w.id, "div", attributes, children);
            if w.kind == "status_bar" {
                chrome.0.push(node);
            } else {
                chrome.1.push(node);
            }
            return None;
        }
        "div" | "rectangle" | "" => "div",
        _ => return None,
    };
    Some(element(w.id, tag, attributes, children))
}

fn document(
    app: Vec<SerializedNode>,
    status: Vec<SerializedNode>,
    nav: Vec<SerializedNode>,
) -> SerializedNode {
    let keyboard = element(
        KEYBOARD_PARENT_ID,
        "div",
        attrs([("data-spoiler-mobile-chrome", string("true"))]),
        vec![],
    );
    let status = element(
        STATUS_PARENT_ID,
        "div",
        attrs([("data-spoiler-mobile-chrome", string("true"))]),
        status,
    );
    let nav = element(
        NAV_PARENT_ID,
        "div",
        attrs([("data-spoiler-mobile-chrome", string("true"))]),
        nav,
    );
    let body = element(
        BODY_ID,
        "body",
        Attributes::default(),
        app.into_iter().chain([keyboard, nav, status]).collect(),
    );
    let head = element(4, "head", Attributes::default(), vec![]);
    let html = element(3, "html", Attributes::default(), vec![head, body]);
    let mut doctype = SerializedNode::default();
    doctype.id = 2;
    doctype.kind = 1;
    let mut document = SerializedNode::default();
    document.id = 1;
    document.kind = 0;
    document.children = vec![doctype, html];
    document
}

pub(super) fn empty_document() -> SerializedNode {
    document(vec![], vec![], vec![])
}

/// PostHog's minimal screenshot-only full snapshot has a body but no chrome parents.
/// Keyboard mutations still need their reserved parent to be attached to the mirror.
pub(super) fn ensure_chrome_parents(root: &mut SerializedNode) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id == BODY_ID && node.tag == "body" {
            for id in [KEYBOARD_PARENT_ID, NAV_PARENT_ID, STATUS_PARENT_ID] {
                if !node.children.iter().any(|child| child.id == id) {
                    node.children.push(element(
                        id,
                        "div",
                        attrs([("data-spoiler-mobile-chrome", string("true"))]),
                        vec![],
                    ));
                }
            }
            return;
        }
        stack.extend(node.children.iter_mut());
    }
}

pub(super) fn full(
    data: &str,
    context: Context,
    next: &mut NodeId,
    reserved: &HashSet<NodeId>,
) -> Option<SerializedNode> {
    if !full_depth_ok(data) {
        return None;
    }
    let full: FullData = parse_deep(data.as_bytes()).ok()?;
    *next = SYNTHETIC_FIRST;
    let context = Context {
        offset_left: full.initial_offset.left,
        offset_top: full.initial_offset.top,
        ..context
    };
    let mut chrome = (Vec::new(), Vec::new());
    let mut app = Vec::with_capacity(full.wireframes.len());
    for wireframe in full.wireframes {
        if let Some(converted) = convert(wireframe, context, next, reserved, &mut chrome) {
            app.push(converted);
        }
    }
    Some(document(app, chrome.0, chrome.1))
}

fn parent_for(change: &Changed) -> Option<NodeId> {
    if change.wireframe.kind == "screenshot" {
        Some(BODY_ID)
    } else {
        change.parent
    }
}

/// Compare native controls' state before removing and after adding an updated subtree.
fn state_changes(root: &SerializedNode, mutation: &mut Mutation) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.tag == "input" {
            let mut changed = Vec::new();
            for key in ["aria-checked", "disabled"] {
                changed.push((
                    key.to_owned(),
                    node.attributes
                        .0
                        .iter()
                        .find(|(name, _)| name == key)
                        .map_or(AttrValue::Json(Value::Null), |(_, value)| value.clone()),
                ));
            }
            mutation.attributes.push(AttributeChange {
                id: node.id,
                attributes: Attributes(changed),
            });
        }
        stack.extend(&node.children);
    }
}

pub(super) fn mutation(
    data: &str,
    context: Context,
    next: &mut NodeId,
    reserved: &HashSet<NodeId>,
) -> Option<Mutation> {
    if !mutation_depth_ok(data) {
        return None;
    }
    let incremental: Incremental = parse_deep(data.as_bytes()).ok()?;
    let mut mutation = Mutation {
        removes: incremental.removes,
        ..Mutation::default()
    };
    for update in &incremental.updates {
        mutation.replacements.push(update.wireframe.id);
        mutation.removes.push(Remove {
            parent: parent_for(update)?,
            id: update.wireframe.id,
        });
    }
    for (changed, updating) in incremental
        .adds
        .into_iter()
        .map(|add| (add, false))
        .chain(incremental.updates.into_iter().map(|update| (update, true)))
    {
        let parent = parent_for(&changed)?;
        let mut chrome = (Vec::new(), Vec::new());
        if let Some(node) = convert(changed.wireframe, context, next, reserved, &mut chrome) {
            if updating {
                state_changes(&node, &mut mutation);
            }
            mutation.adds.push(Add {
                parent,
                next: None,
                node,
            });
        }
        for node in chrome.0 {
            mutation.adds.push(Add {
                parent: STATUS_PARENT_ID,
                next: None,
                node,
            });
        }
        for node in chrome.1 {
            mutation.adds.push(Add {
                parent: NAV_PARENT_ID,
                next: None,
                node,
            });
        }
    }
    Some(mutation)
}

pub(super) fn keyboard(
    data: &Value,
    context: Context,
    next: &mut NodeId,
    reserved: &HashSet<NodeId>,
) -> Option<Mutation> {
    let payload = data.get("payload")?;
    let mut mutation = Mutation::default();
    if payload.get("open")?.as_bool()? {
        let height = payload.get("height")?.as_f64()?;
        let width = payload
            .get("width")
            .and_then(Value::as_f64)
            .or(context.viewport_width);
        let y = payload.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let x = payload.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let mut attributes = attrs([("data-spoiler-mobile-chrome", string("true"))]);
        attributes.0.extend([
            ("data-spoiler-mobile-x".into(), string(x.to_string())),
            ("data-spoiler-mobile-y".into(), string(y.to_string())),
            (
                "data-spoiler-mobile-height".into(),
                string(height.to_string()),
            ),
        ]);
        if let Some(width) = width {
            attributes.0.push((
                "data-spoiler-mobile-width".into(),
                string(width.to_string()),
            ));
        }
        mutation.adds.push(Add {
            parent: KEYBOARD_PARENT_ID,
            next: None,
            node: element(
                KEYBOARD_ID,
                "div",
                attributes,
                vec![text(fresh(next, reserved)?, "keyboard".into())],
            ),
        });
    } else {
        mutation.removes.push(Remove {
            parent: KEYBOARD_PARENT_ID,
            id: KEYBOARD_ID,
        });
    }
    Some(mutation)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::replay::Mirror;

    #[test]
    fn converts_all_wireframe_kinds_to_semantic_nodes() {
        let kinds = [
            "text",
            "image",
            "screenshot",
            "rectangle",
            "div",
            "placeholder",
            "radio_group",
            "web_view",
            "status_bar",
            "navigation_bar",
        ];
        for (index, kind) in kinds.iter().enumerate() {
            let data = serde_json::json!({"wireframes":[{"id":10000000+index,"type":kind,"width":100,"height":20,"text":"Notice","base64":"aGVsbG8=","label":"Loading","url":"https://example.com"}],"initialOffset":{"top":0,"left":0}});
            let mut next = 100;
            let mut mirror = Mirror::default();
            mirror.reset(
                full(
                    &data.to_string(),
                    Context::default(),
                    &mut next,
                    &HashSet::new(),
                )
                .unwrap(),
            );
            let node = mirror.get(10000000 + index as i64).unwrap();
            assert_eq!(
                node.tag,
                if matches!(*kind, "image" | "screenshot") {
                    "img"
                } else {
                    "div"
                },
                "{kind}"
            );
            if *kind == "screenshot" {
                assert!(node.has_attribute("data-posthog-screenshot"));
            }
            if *kind == "text" {
                assert_eq!(mirror.text_preview(node.id, 60), "Notice");
            }
            if *kind == "web_view" {
                assert_eq!(mirror.text_preview(node.id, 60), "https://example.com");
            }
            if *kind == "placeholder" {
                assert_eq!(mirror.text_preview(node.id, 60), "Loading");
            }
            if *kind == "image" {
                assert!(!node.has_attribute("data-posthog-screenshot"));
            }
            if *kind == "status_bar" {
                assert!(node.has_attribute("data-spoiler-mobile-chrome"));
                assert_eq!(node.parent, Some(STATUS_PARENT_ID));
            }
            if *kind == "navigation_bar" {
                assert!(node.has_attribute("data-spoiler-mobile-chrome"));
                assert_eq!(node.parent, Some(NAV_PARENT_ID));
            }
        }
    }

    #[test]
    fn radio_group_names_its_radio_inputs() {
        let snapshot = serde_json::json!({"initialOffset":{"top":0,"left":0},"wireframes":[
            {"id":10000000,"type":"radio_group","width":200,"height":70,"childWireframes":[
                {"id":10000001,"type":"input","inputType":"radio","checked":true,
                 "disabled":false,"label":"First","width":80,"height":30}
            ]}
        ]});
        let mut next = 100;
        let mut mirror = Mirror::default();
        mirror.reset(
            full(
                &snapshot.to_string(),
                Context::default(),
                &mut next,
                &HashSet::new(),
            )
            .unwrap(),
        );
        assert_eq!(
            mirror.get(10000001).unwrap().attr("name"),
            Some("radio_group_10000000")
        );
    }

    #[test]
    fn converts_inputs_and_options_without_clobbering_wireframe_ids() {
        let types = [
            "text",
            "password",
            "email",
            "number",
            "search",
            "checkbox",
            "radio",
            "toggle",
            "button",
            "select",
            "text_area",
            "progress",
        ];
        for (index, input_type) in types.iter().enumerate() {
            let data = serde_json::json!({"wireframes":[{"id":100,"type":"input","inputType":input_type,"width":120,"height":30,"value":if *input_type=="progress" { serde_json::json!(0.5) } else { serde_json::json!("Save") },"checked":true,"disabled":false,"label":"Choice","max":1,"options":["Save","Cancel"]},{"id":101,"type":"text","width":80,"height":20,"text":"Visible"}],"initialOffset":{"top":0,"left":0}});
            let mut next = 100;
            let mut mirror = Mirror::default();
            mirror.reset(
                full(
                    &data.to_string(),
                    Context::default(),
                    &mut next,
                    &HashSet::from([100, 101]),
                )
                .unwrap(),
            );
            let node = mirror.get(100).unwrap();
            assert_eq!(
                node.tag,
                match *input_type {
                    "button" => "button",
                    "select" => "select",
                    "progress" => "progress",
                    _ => "input",
                },
                "{input_type} at {index}"
            );
            assert_eq!(node.attr("aria-label"), Some("Choice"));
            assert!(!node.has_attribute("disabled"));
            assert_eq!(mirror.text_preview(101, 60), "Visible");
            match *input_type {
                "button" => {
                    assert_eq!(node.attr("type"), Some("button"));
                    assert_eq!(mirror.text_preview(100, 60), "Save");
                }
                "select" => {
                    assert_eq!(node.attr("value"), Some("Save"));
                    assert_eq!(node.children.len(), 2);
                    assert!(
                        mirror
                            .get(node.children[0])
                            .unwrap()
                            .has_attribute("selected")
                    );
                    assert!(
                        !mirror
                            .get(node.children[1])
                            .unwrap()
                            .has_attribute("selected")
                    );
                }
                "progress" => {
                    assert_eq!(node.attr("value"), Some("0.5"));
                    assert_eq!(node.attr("max"), Some("1"));
                    assert!(!node.has_attribute("type"));
                }
                "toggle" => {
                    assert_eq!(node.attr("type"), Some("checkbox"));
                    assert_eq!(node.attr("role"), Some("switch"));
                    assert!(node.has_attribute("checked"));
                }
                "checkbox" | "radio" => {
                    assert_eq!(node.attr("type"), Some(*input_type));
                    assert!(node.has_attribute("checked"));
                }
                _ => {
                    assert_eq!(node.attr("type"), Some(*input_type));
                    assert_eq!(node.attr("value"), Some("Save"));
                }
            }
        }
    }

    #[test]
    fn future_native_add_cannot_reuse_snapshot_text_id() {
        use crate::recording::{Reading, decode};

        let snapshot = serde_json::json!({"win":"native","type":2,"timestamp":1,
            "data":{"initialOffset":{"top":0,"left":0},"wireframes":[{"id":10000000,"type":"text","text":"First","width":80,"height":20}]}});
        let add = serde_json::json!({"win":"native","type":3,"timestamp":2,
        "data":{"source":0,"adds":[{"parentId":5,"wireframe":{
            "id":100,"type":"input","inputType":"button","value":"Tap","disabled":false,"width":80,"height":20
        }}]}});
        let recording = decode(format!("{snapshot}\n{add}").as_bytes()).unwrap();
        let events: Vec<_> = recording.events().collect();
        let mut mirror = Mirror::default();
        let Reading::Signal(rrweb::Signal::NativeFullSnapshot(root)) =
            recording.read(&events[0]).unwrap()
        else {
            panic!("expected native full snapshot")
        };
        mirror.reset(root);
        let text_id = mirror.get(10000000).unwrap().children[0];
        assert_ne!(text_id, 100);
        let Reading::Signal(rrweb::Signal::Mutation(batch)) = recording.read(&events[1]).unwrap()
        else {
            panic!("expected native add")
        };
        mirror.apply(batch);
        assert_eq!(mirror.text_preview(10000000, 60), "First");
        assert_eq!(mirror.get(text_id).unwrap().parent, Some(10000000));
        assert_eq!(mirror.get(100).unwrap().tag, "button");
        assert_eq!(mirror.text_preview(100, 60), "Tap");
    }

    #[test]
    fn overly_deep_wireframes_are_malformed_on_a_small_stack() {
        use crate::recording::{Reading, decode};
        use std::fmt::Write as _;

        let mut wireframe = String::new();
        for id in 0..MAX_WIREFRAME_DEPTH + 256 {
            write!(
                wireframe,
                "{{\"id\":{},\"type\":\"div\",\"width\":1,\"height\":1,\"childWireframes\":[",
                10_000_000 + id
            )
            .unwrap();
        }
        wireframe.push_str(r#"{"id":20000000,"type":"text","width":1,"height":1,"text":"leaf"}"#);
        for _ in 0..MAX_WIREFRAME_DEPTH + 256 {
            wireframe.push_str("]}");
        }
        let snapshot = format!(
            r#"{{"win":"native","type":2,"timestamp":1,"data":{{"wireframes":[{wireframe}]}}}}"#
        );
        let mutation = format!(
            r#"{{"win":"native","type":3,"timestamp":2,"data":{{"source":0,"adds":[{{"parentId":5,"wireframe":{wireframe}}}]}}}}"#
        );
        let input = format!("{snapshot}\n{mutation}");
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let recording = decode(input.as_bytes()).unwrap();
                let events: Vec<_> = recording.events().collect();
                assert_eq!(events.len(), 2);
                assert!(matches!(
                    recording.read(&events[0]).unwrap(),
                    Reading::Malformed(_)
                ));
                assert!(matches!(
                    recording.read(&events[1]).unwrap(),
                    Reading::Malformed(_)
                ));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn geometry_uses_absolute_native_coordinates_and_snapshot_offset() {
        let snapshot = serde_json::json!({"initialOffset":{"top":7,"left":5},"wireframes":[
            {"id":10000000,"type":"div","x":40,"y":20,"width":320,"height":300,"childWireframes":[
                {"id":10000001,"type":"input","inputType":"button","x":90,"y":60,
                 "width":"100vw","height":40,"value":"Save"}
            ]}
        ]});
        let mut next = 100;
        let mut mirror = Mirror::default();
        mirror.reset(
            full(
                &snapshot.to_string(),
                Context {
                    viewport_width: Some(320.0),
                    ..Context::default()
                },
                &mut next,
                &HashSet::new(),
            )
            .unwrap(),
        );
        let button = mirror.get(10000001).unwrap();
        assert_eq!(button.attr("data-spoiler-mobile-x"), Some("85"));
        assert_eq!(button.attr("data-spoiler-mobile-y"), Some("53"));
        assert_eq!(button.attr("data-spoiler-mobile-width"), Some("320"));
        assert_eq!(button.attr("data-spoiler-mobile-height"), Some("40"));
    }

    #[test]
    fn screenshot_incremental_bootstraps_the_mirror_without_a_full_snapshot() {
        use crate::recording::{Reading, decode};
        let screenshot = serde_json::json!({"win":"native","type":3,"timestamp":20,
        "data":{"source":0,"adds":[{"parentId":0,"wireframe":{
            "id":10000001,"type":"screenshot","x":0,"y":0,"width":320,"height":600,"base64":"aGVsbG8="
        }}]}});
        let update = serde_json::json!({"win":"native","type":3,"timestamp":30,
        "data":{"source":0,"updates":[{"wireframe":{
            "id":10000001,"type":"screenshot","x":0,"y":0,"width":320,"height":600,"base64":"dXBkYXRl"
        }}]}});
        let input = format!("{screenshot}\n{update}");
        let recording = decode(input.as_bytes()).unwrap();
        let events: Vec<_> = recording.events().collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].timestamp.0, 19.0);
        let mut mirror = Mirror::default();
        let Reading::Signal(rrweb::Signal::FullSnapshot(root)) =
            recording.read(&events[0]).unwrap()
        else {
            panic!("expected bootstrap")
        };
        mirror.reset(root);
        let Reading::Signal(rrweb::Signal::Mutation(batch)) = recording.read(&events[1]).unwrap()
        else {
            panic!("expected converted add")
        };
        mirror.apply(batch);
        assert_eq!(mirror.get(10000001).unwrap().tag, "img");
        assert!(
            mirror
                .get(10000001)
                .unwrap()
                .has_attribute("data-posthog-screenshot")
        );
        assert_eq!(mirror.get(10000001).unwrap().parent, Some(BODY_ID));
        let Reading::Signal(rrweb::Signal::Mutation(batch)) = recording.read(&events[2]).unwrap()
        else {
            panic!("expected screenshot update")
        };
        mirror.apply(batch);
        assert_eq!(mirror.get(10000001).unwrap().parent, Some(BODY_ID));
    }

    #[test]
    fn transformed_screenshot_image_bootstraps_and_attaches_to_body() {
        use crate::recording::{Reading, decode};
        let screenshot = serde_json::json!({"win":"native","type":3,"timestamp":20,
        "data":{"source":0,"adds":[{"parentId":0,"node":{
            "id":10000001,"type":2,"tagName":"img",
            "attributes":{"data-posthog-screenshot":"true","width":320,"height":600},
            "childNodes":[]
        }}]}});
        let recording = decode(screenshot.to_string().as_bytes()).unwrap();
        let events: Vec<_> = recording.events().collect();
        assert_eq!(events.len(), 2);
        let mut mirror = Mirror::default();
        let Reading::Signal(rrweb::Signal::FullSnapshot(root)) =
            recording.read(&events[0]).unwrap()
        else {
            panic!("expected bootstrap")
        };
        mirror.reset(root);
        let Reading::Signal(rrweb::Signal::Mutation(batch)) = recording.read(&events[1]).unwrap()
        else {
            panic!("expected image add")
        };
        mirror.apply(batch);
        assert_eq!(mirror.get(10000001).unwrap().parent, Some(BODY_ID));
    }

    #[test]
    fn transformed_minimal_screenshot_full_can_receive_keyboard_events() {
        use crate::recording::{Reading, decode};
        let full = serde_json::json!({"win":"native","type":2,"timestamp":0,
        "data":{"node":{"id":1,"type":0,"childNodes":[
            {"id":3,"type":2,"tagName":"html","childNodes":[
                {"id":5,"type":2,"tagName":"body","childNodes":[
                    {"id":10000001,"type":2,"tagName":"img",
                     "attributes":{"data-posthog-screenshot":"true"},"childNodes":[]}
                ]}
            ]}
        ]}}});
        let keyboard = serde_json::json!({"win":"native","type":5,"timestamp":100,
            "data":{"tag":"keyboard","payload":{"open":true,"height":200}}});
        let input = format!("{full}\n{keyboard}");
        let recording = decode(input.as_bytes()).unwrap();
        let events: Vec<_> = recording.events().collect();
        assert_eq!(events.len(), 2);
        let mut mirror = Mirror::default();
        let Reading::Signal(rrweb::Signal::FullSnapshot(root)) =
            recording.read(&events[0]).unwrap()
        else {
            panic!("expected full screenshot")
        };
        mirror.reset(root);
        assert!(mirror.get(KEYBOARD_PARENT_ID).is_some());
        let Reading::Signal(rrweb::Signal::Mutation(batch)) = recording.read(&events[1]).unwrap()
        else {
            panic!("expected keyboard add")
        };
        mirror.apply(batch);
        assert_eq!(
            mirror.get(KEYBOARD_ID).unwrap().parent,
            Some(KEYBOARD_PARENT_ID)
        );
    }

    #[test]
    fn optional_mobile_meta_href_does_not_invent_a_screen() {
        use crate::recording::{Reading, decode};
        let meta = serde_json::json!({"win":"native","type":4,"timestamp":0,
            "data":{"width":320,"height":600}});
        let snapshot = serde_json::json!({"win":"native","type":2,"timestamp":1,
        "data":{"initialOffset":{"top":0,"left":0},"wireframes":[
            {"id":10000000,"type":"text","text":"Hello","width":120,"height":40}
        ]}});
        let empty = serde_json::json!({"win":"native","type":4,"timestamp":2,
            "data":{"href":"","width":320}});
        let web_empty = serde_json::json!({"win":"web","type":4,"timestamp":3,
            "data":{"href":""}});
        let input = format!("{meta}\n{snapshot}\n{empty}\n{web_empty}");
        let recording = decode(input.as_bytes()).unwrap();
        let events: Vec<_> = recording.events().collect();
        for event in [&events[0], &events[2]] {
            assert!(matches!(
                recording.read(event).unwrap(),
                Reading::Uninterpreted(name) if name == "mobile_meta_without_screen"
            ));
        }
        assert!(matches!(
            recording.read(&events[3]).unwrap(),
            Reading::Signal(rrweb::Signal::Meta { href }) if href.is_empty()
        ));
    }

    #[test]
    fn update_replaces_descendants_and_keyboard_opens_closes() {
        let mut next = 100;
        let initial = serde_json::json!({"wireframes":[{"id":10000000,"type":"text","text":"Before","width":100,"height":30}],"initialOffset":{"top":0,"left":0}});
        let mut mirror = Mirror::default();
        mirror.reset(
            full(
                &initial.to_string(),
                Context::default(),
                &mut next,
                &HashSet::new(),
            )
            .unwrap(),
        );
        let batch = serde_json::json!({"updates":[{"parentId":5,"wireframe":{"id":10000000,"type":"text","text":"After","width":100,"height":30}}]});
        mirror.apply(
            mutation(
                &batch.to_string(),
                Context::default(),
                &mut next,
                &HashSet::new(),
            )
            .unwrap(),
        );
        assert_eq!(mirror.text_preview(10000000, 60), "After");
        let open = serde_json::json!({"payload":{"open":true,"height":150}});
        mirror.apply(keyboard(&open, Context::default(), &mut next, &HashSet::new()).unwrap());
        assert_eq!(mirror.text_preview(KEYBOARD_ID, 60), "keyboard");
        let close = serde_json::json!({"payload":{"open":false}});
        mirror.apply(keyboard(&close, Context::default(), &mut next, &HashSet::new()).unwrap());
        assert!(mirror.get(KEYBOARD_ID).is_none());
    }
}

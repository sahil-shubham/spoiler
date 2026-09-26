use super::is_non_visual;
use crate::{
    recording::rrweb::{Add, AttrValue, Attributes, Mutation, NodeId, SerializedNode},
    text::{collapse_whitespace, truncate, utf16_len},
};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

/// Bound on parent-chain walks: a malformed recording must not loop forever.
const MAX_DEPTH: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Element,
    Text,
    /// Doctype, CDATA, comments.
    Other,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// Element tag name; empty for other kinds.
    pub tag: String,
    /// Text node content.
    pub text: String,
    /// In document order. Elements carry a handful, so a list beats a hash map.
    pub attributes: Vec<(String, AttrValue)>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

impl Node {
    pub fn is_element(&self) -> bool {
        self.kind == NodeKind::Element
    }

    pub fn attribute(&self, name: &str) -> Option<&AttrValue> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn has_attribute(&self, name: &str) -> bool {
        self.attribute(name).is_some()
    }

    /// A string attribute, trimmed; empty values count as absent.
    pub fn attr(&self, name: &str) -> Option<&str> {
        let value = crate::text::trim(self.attribute(name)?.as_str()?);
        (!value.is_empty()).then_some(value)
    }

    /// Apply one recorded attribute write: `null` removes, anything else sets (in place when the
    /// attribute exists, as a JavaScript object keeps key order).
    fn write_attribute(&mut self, name: &str, value: &AttrValue) {
        let existing = self.attributes.iter().position(|(key, _)| key == name);
        match (existing, value.is_null()) {
            (Some(index), true) => {
                self.attributes.remove(index);
            }
            (Some(index), false) => self.attributes[index].1 = value.clone(),
            (None, true) => {}
            (None, false) => self.attributes.push((name.to_owned(), value.clone())),
        }
    }
}

/// Every node id in a serialized subtree.
pub(crate) fn serialized_ids(root: &SerializedNode) -> Vec<NodeId> {
    let mut ids = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        ids.push(node.id);
        stack.extend(&node.children);
    }
    ids
}

/// The serialized DOM of one tab. Tabs have separate node-id spaces: sharing a mirror across
/// them mis-resolves clicks.
///
/// Mutations apply in rrweb `Replayer.applyMutation` order. Where rrweb drops a subtree whose
/// parent never arrives, the mirror keeps it addressable but detached.
#[derive(Clone, Debug, Default)]
pub struct Mirror {
    nodes: HashMap<NodeId, Node>,
}

impl Mirror {
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    pub fn parent(&self, id: NodeId) -> Option<&Node> {
        self.get(self.get(id)?.parent?)
    }

    /// The node and its ancestors, nearest first.
    pub fn lineage(&self, id: NodeId) -> impl Iterator<Item = &Node> {
        std::iter::successors(self.get(id), |node| self.get(node.parent?)).take(MAX_DEPTH)
    }

    /// Strict ancestors, nearest first.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = &Node> {
        self.lineage(id).skip(1)
    }

    /// Part of the rendered document (reaches the document node).
    pub fn is_attached(&self, id: NodeId) -> bool {
        self.lineage(id).any(|node| node.kind == NodeKind::Document)
    }

    /// Ids in a subtree, the root first, depth-first.
    pub fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let mut ids = Vec::new();
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.get(id) {
                ids.push(id);
                stack.extend(&node.children);
            }
        }
        ids
    }

    /// Replace the whole document.
    pub fn reset(&mut self, root: SerializedNode) {
        self.nodes.clear();
        self.register(root, None);
    }

    pub fn apply(&mut self, mutation: Mutation) {
        let Mutation {
            adds,
            removes,
            texts,
            attributes,
        } = mutation;
        if !removes.is_empty() {
            // An add with an existing id is a move. Keep its subtree even when the old parent
            // (or the moved node itself) is removed earlier in this batch.
            let moved: HashSet<_> = adds.iter().map(|add| add.node.id).collect();
            for id in &moved {
                self.detach(*id);
            }
            for remove in removes {
                if !moved.contains(&remove.id) {
                    self.remove(remove.id);
                }
            }
        }
        self.apply_adds(adds);
        for change in texts {
            if let Some(node) = self.nodes.get_mut(&change.id) {
                node.text = change.value;
            }
        }
        for change in attributes {
            let Some(node) = self.nodes.get_mut(&change.id) else {
                continue;
            };
            let Attributes(pairs) = change.attributes;
            for (name, value) in &pairs {
                node.write_attribute(name, value);
            }
        }
    }

    /// rrweb applies an add once its parent and its `next` sibling exist, retrying the rest until
    /// no progress is made; it then goes before `next`, else last. Getting this wrong reorders
    /// re-sorted rows. (A legacy `previousId` exists but posthog-js never emits it.)
    fn apply_adds(&mut self, mut pending: Vec<Add>) {
        while !pending.is_empty() {
            let attempted = pending.len();
            let mut waiting = Vec::new();
            // Within a pass, adds apply in list order: an earlier add can ready a later one.
            for add in pending {
                if self.contains(add.parent) && add.next.is_none_or(|next| self.contains(next)) {
                    let parent = add.parent;
                    self.insert(add, Some(parent));
                } else {
                    waiting.push(add);
                }
            }
            if waiting.len() == attempted {
                for add in waiting {
                    let parent = self.contains(add.parent).then_some(add.parent);
                    self.insert(add, parent);
                }
                return;
            }
            pending = waiting;
        }
    }

    fn insert(&mut self, add: Add, parent: Option<NodeId>) {
        let id = add.node.id;
        self.detach(id);
        if let Some(parent_id) = parent
            && let Some(parent_node) = self.nodes.get_mut(&parent_id)
        {
            let position = add
                .next
                .and_then(|next| parent_node.children.iter().position(|child| *child == next));
            match position {
                Some(at) => parent_node.children.insert(at, id),
                None => parent_node.children.push(id),
            }
        }
        if let Some(existing) = self.nodes.get_mut(&id) {
            existing.parent = parent;
        } else {
            self.register(add.node, parent);
        }
    }

    /// Move a serialized subtree into the mirror. Children are visited last-first (a stack), and
    /// a later registration of a duplicate id replaces the earlier one.
    fn register(&mut self, root: SerializedNode, parent: Option<NodeId>) {
        let mut stack = vec![(root, parent)];
        while let Some((mut serialized, parent)) = stack.pop() {
            let children = std::mem::take(&mut serialized.children);
            let id = serialized.id;
            let node = Node {
                id,
                kind: match serialized.kind {
                    0 => NodeKind::Document,
                    2 => NodeKind::Element,
                    3 => NodeKind::Text,
                    _ => NodeKind::Other,
                },
                tag: std::mem::take(&mut serialized.tag),
                text: std::mem::take(&mut serialized.text),
                attributes: std::mem::take(&mut serialized.attributes.0),
                children: children.iter().map(|child| child.id).collect(),
                parent,
            };
            self.nodes.insert(id, node);
            stack.extend(children.into_iter().map(|child| (child, Some(id))));
        }
    }

    fn detach(&mut self, id: NodeId) {
        if let Some(parent) = self.nodes.get_mut(&id).and_then(|node| node.parent.take())
            && let Some(parent) = self.nodes.get_mut(&parent)
        {
            parent.children.retain(|child| *child != id);
        }
    }

    fn remove(&mut self, id: NodeId) {
        self.detach(id);
        for id in self.subtree(id) {
            self.nodes.remove(&id);
        }
    }

    /// Text a user sees in a subtree: every text node, whitespace-collapsed, in document order.
    pub fn visible_text(&self, id: NodeId) -> String {
        let parts: Vec<_> = self
            .text_nodes(id, is_non_visual)
            .filter(|node| node.text != SCRIPT_PLACEHOLDER)
            .map(|node| collapse_whitespace(&node.text))
            .filter(|text| !text.is_empty())
            .collect();
        parts.join(" ")
    }

    /// A short label for a subtree: text nodes in document order, read until about `limit`
    /// UTF-16 units, then cut to exactly `limit`.
    ///
    /// Only `<script>`, `<style>`, `<noscript>` and `<template>` are skipped, so `<head>` text
    /// such as a title can appear.
    pub fn text_preview(&self, id: NodeId, limit: usize) -> String {
        let skip = |tag: &str| matches!(tag, "script" | "style" | "noscript" | "template");
        let mut parts = Vec::new();
        let mut length = 0;
        for node in self.text_nodes(id, skip) {
            let text = collapse_whitespace(&node.text);
            if text.is_empty() || text == SCRIPT_PLACEHOLDER {
                continue;
            }
            length += utf16_len(&text) + 1;
            parts.push(text);
            if length >= limit {
                break;
            }
        }
        truncate(parts.join(" "), limit)
    }

    /// Text nodes of a subtree in document order, not descending into elements matching `skip`.
    pub fn text_nodes<F: Fn(&str) -> bool>(&self, id: NodeId, skip: F) -> TextNodes<'_, F> {
        TextNodes {
            mirror: self,
            stack: vec![id],
            skip,
        }
    }
}

/// Placeholder text rrweb records for inline scripts.
pub const SCRIPT_PLACEHOLDER: &str = "SCRIPT_PLACEHOLDER";

pub struct TextNodes<'m, F> {
    mirror: &'m Mirror,
    stack: Vec<NodeId>,
    skip: F,
}

impl<'m, F: Fn(&str) -> bool> Iterator for TextNodes<'m, F> {
    type Item = &'m Node;

    fn next(&mut self) -> Option<&'m Node> {
        while let Some(id) = self.stack.pop() {
            let Some(node) = self.mirror.get(id) else {
                continue;
            };
            if node.is_element() && (self.skip)(&node.tag) {
                continue;
            }
            self.stack.extend(node.children.iter().rev());
            if node.kind == NodeKind::Text {
                return Some(node);
            }
        }
        None
    }
}

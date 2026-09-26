//! The subset of rrweb's wire format Spoiler interprets, as owned typed values.
//!
//! Constants mirror `@rrweb/types` (`EventType`, `IncrementalSource`, `MouseInteractions`).
//! Only what the compiler reads is typed: full snapshots and mutations (the bulk of every
//! recording) get dedicated structs; the small remaining events are read from JSON values.
//!
//! Deserialization is lenient: a field of the wrong type reads as absent instead of failing the
//! event, so an odd recording loses that detail rather than the event.

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::{fmt, marker::PhantomData};

/// rrweb node id. Ids are per tab; `-1` never names a node.
pub type NodeId = i64;

pub const DOM_CONTENT_LOADED: u32 = 0;
pub const LOAD: u32 = 1;
pub const FULL_SNAPSHOT: u32 = 2;
pub const INCREMENTAL_SNAPSHOT: u32 = 3;
pub const META: u32 = 4;
pub const CUSTOM: u32 = 5;
pub const PLUGIN: u32 = 6;

pub mod source {
    pub const MUTATION: u64 = 0;
    pub const MOUSE_MOVE: u64 = 1;
    pub const MOUSE_INTERACTION: u64 = 2;
    pub const SCROLL: u64 = 3;
    pub const VIEWPORT_RESIZE: u64 = 4;
    pub const INPUT: u64 = 5;
    pub const TOUCH_MOVE: u64 = 6;
    pub const MEDIA_INTERACTION: u64 = 7;
    pub const STYLE_SHEET_RULE: u64 = 8;
    pub const CANVAS_MUTATION: u64 = 9;
    pub const FONT: u64 = 10;
    pub const LOG: u64 = 11;
    pub const DRAG: u64 = 12;
    pub const STYLE_DECLARATION: u64 = 13;
    pub const SELECTION: u64 = 14;
    pub const ADOPTED_STYLE_SHEET: u64 = 15;
    pub const CUSTOM_ELEMENT: u64 = 16;

    /// A stable name for coverage reports.
    pub fn name(source: u64) -> &'static str {
        match source {
            MUTATION => "mutation",
            MOUSE_MOVE => "mouse_move",
            MOUSE_INTERACTION => "mouse_interaction",
            SCROLL => "scroll",
            VIEWPORT_RESIZE => "viewport_resize",
            INPUT => "input",
            TOUCH_MOVE => "touch_move",
            MEDIA_INTERACTION => "media_interaction",
            STYLE_SHEET_RULE => "style_sheet_rule",
            CANVAS_MUTATION => "canvas_mutation",
            FONT => "font",
            LOG => "log",
            DRAG => "drag",
            STYLE_DECLARATION => "style_declaration",
            SELECTION => "selection",
            ADOPTED_STYLE_SHEET => "adopted_style_sheet",
            CUSTOM_ELEMENT => "custom_element",
            _ => "unknown_source",
        }
    }
}

/// What one event means to the compiler.
#[derive(Clone, Debug)]
pub enum Signal {
    /// The page, re-serialized from scratch (often under new node ids).
    FullSnapshot(SerializedNode),
    /// A native iOS/Android wireframe frame, converted to synthetic DOM.
    /// Unlike web full snapshots, successive native frames can be a gesture's reaction.
    NativeFullSnapshot(SerializedNode),
    /// The tab's URL.
    Meta {
        href: String,
    },
    Mutation(Mutation),
    Mouse(Mouse),
    Input(Input),
    Selection(Vec<SelectionRange>),
    Custom {
        tag: String,
        payload: Value,
    },
    Plugin {
        name: String,
        payload: Value,
    },
}

// ── lenient leaves ──────────────────────────────────────────────────────────

/// Accepts any JSON value, keeping it only if `accept` maps it.
struct Loose<T>(Option<T>);

trait LooseLeaf: Sized {
    fn from_i64(_: i64) -> Option<Self> {
        None
    }
    fn from_u64(v: u64) -> Option<Self> {
        i64::try_from(v).ok().and_then(Self::from_i64)
    }
    fn from_f64(_: f64) -> Option<Self> {
        None
    }
    fn from_str(_: &str) -> Option<Self> {
        None
    }
    fn from_string(s: String) -> Option<Self> {
        Self::from_str(&s)
    }
}

/// A node id: integers are ids; other numbers are numbers that name no node.
#[derive(Clone, Copy)]
struct Id(NodeId);
impl LooseLeaf for Id {
    fn from_i64(v: i64) -> Option<Self> {
        Some(Id(v))
    }
    fn from_u64(v: u64) -> Option<Self> {
        Some(Id(i64::try_from(v).unwrap_or(-1)))
    }
    fn from_f64(_: f64) -> Option<Self> {
        Some(Id(-1))
    }
}

struct Text(String);
impl LooseLeaf for Text {
    fn from_str(s: &str) -> Option<Self> {
        Some(Text(s.to_owned()))
    }
    fn from_string(s: String) -> Option<Self> {
        Some(Text(s))
    }
}

struct Kind(u64);
impl LooseLeaf for Kind {
    fn from_u64(v: u64) -> Option<Self> {
        Some(Kind(v))
    }
}

impl<'de, T: LooseLeaf> Deserialize<'de> for Loose<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: LooseLeaf> Visitor<'de> for V<T> {
            type Value = Loose<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(Loose(T::from_i64(v)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(Loose(T::from_u64(v)))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(Loose(T::from_f64(v)))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Loose(T::from_str(v)))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(Loose(T::from_string(v)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Loose(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Loose(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Loose(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Loose(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Loose(None))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// An id field, `-1` when absent or not a number.
fn id_field<'de, D: Deserializer<'de>>(d: D) -> Result<NodeId, D::Error> {
    Ok(Loose::<Id>::deserialize(d)?.0.map_or(-1, |id| id.0))
}

/// A string field, empty when absent, `null`, or not a string.
fn text_field<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Loose::<Text>::deserialize(d)?
        .0
        .map_or_else(String::new, |t| t.0))
}

fn minus_one() -> NodeId {
    -1
}

// ── attributes ──────────────────────────────────────────────────────────────

/// An attribute value. rrweb records strings, but also `true`, numbers, `null`, and objects
/// (style mutations are `{"style": {"color": "red"}}`).
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    Text(String),
    Json(Value),
}

impl AttrValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Json(_) => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Json(Value::Null))
    }

    /// The value as attribute states are compared: `null` is absent, strings as-is, anything else
    /// as JSON.
    pub fn compared_text(&self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text.clone()),
            Self::Json(Value::Null) => None,
            Self::Json(other) => Some(other.to_string()),
        }
    }

    /// JavaScript `String(value)`.
    pub fn js_string(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Json(value) => crate::text::js_string(value),
        }
    }
}

impl<'de> Deserialize<'de> for AttrValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(text) => Self::Text(text),
            other => Self::Json(other),
        })
    }
}

/// Attributes in document order. A later duplicate key replaces the value in place, as
/// `JSON.parse` does. Anything but an object reads as no attributes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attributes(pub Vec<(String, AttrValue)>);

impl<'de> Deserialize<'de> for Attributes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Attributes;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("attributes")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Attributes, A::Error> {
                let mut pairs: Vec<(String, AttrValue)> =
                    Vec::with_capacity(map.size_hint().unwrap_or(4));
                while let Some((name, value)) = map.next_entry::<String, AttrValue>()? {
                    match pairs.iter_mut().find(|(existing, _)| *existing == name) {
                        Some(pair) => pair.1 = value,
                        None => pairs.push((name, value)),
                    }
                }
                Ok(Attributes(pairs))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Attributes, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Attributes::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
            fn visit_unit<E>(self) -> Result<Attributes, E> {
                Ok(Attributes::default())
            }
        }
        d.deserialize_any(V)
    }
}

// ── serialized DOM ──────────────────────────────────────────────────────────

/// An rrweb-snapshot serialized node.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SerializedNode {
    #[serde(default = "minus_one", deserialize_with = "id_field")]
    pub id: NodeId,
    /// rrweb `NodeType`; `u64::MAX` when unreadable.
    #[serde(rename = "type", default, deserialize_with = "kind_field")]
    pub kind: u64,
    #[serde(rename = "tagName", default, deserialize_with = "text_field")]
    pub tag: String,
    #[serde(rename = "textContent", default, deserialize_with = "text_field")]
    pub text: String,
    #[serde(default)]
    pub attributes: Attributes,
    #[serde(rename = "childNodes", default, deserialize_with = "children_field")]
    pub children: Vec<SerializedNode>,
    /// A shadow host: its shadow tree's content is recorded under it.
    #[serde(rename = "isShadowHost", default, deserialize_with = "flag_field")]
    pub is_shadow_host: bool,
    /// Set on nodes in an iframe's document (their root is not the page's).
    #[serde(rename = "rootId", default, deserialize_with = "present_field")]
    pub in_nested_document: bool,
}

impl Drop for SerializedNode {
    fn drop(&mut self) {
        // A snapshot can be arbitrarily deep; recursive Vec drop would exhaust the stack.
        let mut stack = std::mem::take(&mut self.children);
        while let Some(mut child) = stack.pop() {
            stack.append(&mut child.children);
        }
    }
}

fn kind_field<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Loose::<Kind>::deserialize(d)?.0.map_or(u64::MAX, |k| k.0))
}

fn flag_field<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)? == Value::Bool(true))
}

pub(crate) fn present_field<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(d)?;
    Ok(true)
}

/// Child nodes; entries that are not nodes are skipped, a non-array reads as none.
fn children_field<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SerializedNode>, D::Error> {
    Ok(ObjectsOnly::<SerializedNode>::deserialize(d)?.0)
}

/// A list of objects of `T`, skipping entries that are not objects; any non-array is empty.
struct ObjectsOnly<T>(Vec<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectsOnly<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match List::<T>::deserialize(d)? {
            List::Items(items) => ObjectsOnly(items),
            List::Absent | List::Packed(_) | List::Invalid => ObjectsOnly(Vec::new()),
        })
    }
}

/// A mutation list field: absent, an array, a posthog-js packed string, or something else
/// (including `null`, which is not a list).
#[derive(Default)]
enum List<T> {
    #[default]
    Absent,
    Items(Vec<T>),
    Packed(String),
    Invalid,
}

/// One array element: an object of `T`, or anything else (skipped).
struct Element<T>(Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Element<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Element<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an element")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(|t| Element(Some(t)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Element(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Element(None))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = List<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(Element(item)) = seq.next_element::<Element<T>>()? {
                    items.extend(item);
                }
                Ok(List::Items(items))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(List::Packed(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(List::Packed(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(List::Invalid)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(List::Invalid)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(List::Invalid)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(List::Invalid)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(List::Invalid)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(List::Invalid)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

// ── mutations ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
pub struct Add {
    #[serde(
        rename = "parentId",
        default = "minus_one",
        deserialize_with = "id_field"
    )]
    pub parent: NodeId,
    /// Insert before this sibling; `None` appends.
    #[serde(rename = "nextId", default, deserialize_with = "next_field")]
    pub next: Option<NodeId>,
    pub node: SerializedNode,
}

fn next_field<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NodeId>, D::Error> {
    Ok(Loose::<Id>::deserialize(d)?
        .0
        .map(|id| id.0)
        .filter(|id| *id != -1))
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Remove {
    #[serde(
        rename = "parentId",
        default = "minus_one",
        deserialize_with = "id_field"
    )]
    pub parent: NodeId,
    #[serde(default = "minus_one", deserialize_with = "id_field")]
    pub id: NodeId,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TextChange {
    #[serde(default = "minus_one", deserialize_with = "id_field")]
    pub id: NodeId,
    /// `null` clears the text.
    #[serde(default, deserialize_with = "text_field")]
    pub value: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AttributeChange {
    #[serde(default = "minus_one", deserialize_with = "id_field")]
    pub id: NodeId,
    /// `null` values remove the attribute.
    #[serde(default)]
    pub attributes: Attributes,
}

/// One DOM mutation batch, applied in rrweb's order: removes, adds, texts, attributes.
#[derive(Clone, Debug, Default)]
pub struct Mutation {
    pub adds: Vec<Add>,
    pub removes: Vec<Remove>,
    pub texts: Vec<TextChange>,
    pub attributes: Vec<AttributeChange>,
    /// Native update ids replace the old subtree; rrweb remove+add with the same id is a move.
    pub replacements: Vec<NodeId>,
}

/// A mutation's data as recorded: lists, or strings packed by posthog-js.
#[derive(Deserialize)]
pub(crate) struct MutationData {
    #[serde(default)]
    adds: List<Add>,
    #[serde(default)]
    removes: List<Remove>,
    #[serde(default)]
    texts: List<TextChange>,
    #[serde(default)]
    attributes: List<AttributeChange>,
}

impl MutationData {
    /// The batch, unpacking compressed fields when the event is posthog-js compressed. `None`
    /// when a field is present but not a list: the batch cannot be applied faithfully.
    pub(crate) fn resolve(
        self,
        compressed: bool,
        mut unpack: impl FnMut(&str) -> super::Result<Vec<u8>>,
    ) -> super::Result<Option<Mutation>> {
        fn field<T: for<'de> Deserialize<'de>>(
            list: List<T>,
            compressed: bool,
            unpack: &mut impl FnMut(&str) -> super::Result<Vec<u8>>,
        ) -> super::Result<Option<Vec<T>>> {
            match list {
                List::Absent => Ok(Some(Vec::new())),
                List::Items(items) => Ok(Some(items)),
                List::Packed(packed) if compressed => {
                    let json = unpack(&packed)?;
                    let objects = super::parse_deep::<ObjectsOnly<T>>(&json)
                        .map_err(super::DecodeError::json("compressed field"))?;
                    Ok(Some(objects.0))
                }
                List::Packed(_) | List::Invalid => Ok(None),
            }
        }
        let (Some(adds), Some(removes), Some(texts), Some(attributes)) = (
            field(self.adds, compressed, &mut unpack)?,
            field(self.removes, compressed, &mut unpack)?,
            field(self.texts, compressed, &mut unpack)?,
            field(self.attributes, compressed, &mut unpack)?,
        ) else {
            return Ok(None);
        };
        Ok(Some(Mutation {
            adds,
            removes,
            texts,
            attributes,
            replacements: Vec::new(),
        }))
    }
}

/// An added node's placement, kept after the node itself moves into the mirror.
#[derive(Clone, Copy, Debug)]
pub struct Added {
    pub parent: NodeId,
    pub id: NodeId,
}

impl Mutation {
    pub fn added(&self) -> Vec<Added> {
        self.adds
            .iter()
            .map(|add| Added {
                parent: add.parent,
                id: add.node.id,
            })
            .collect()
    }
}

// ── small events ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    MouseDown,
    Click,
    ContextMenu,
    DblClick,
    TouchStart,
    TouchEnd,
    /// Mouse-up, focus, blur and the rest: not gestures the compiler acts on.
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Mouse {
    pub interaction: Interaction,
    pub id: NodeId,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

/// A numeric id field. Fractional ids are kept as numbers that name no node, as in JavaScript.
fn number_id(value: &Value) -> Option<NodeId> {
    value.is_number().then(|| value.as_i64().unwrap_or(-1))
}

impl Mouse {
    pub(crate) fn from_data(data: &Value) -> Option<Self> {
        data["type"].as_f64()?;
        let interaction = match data["type"].as_i64() {
            Some(1) => Interaction::MouseDown,
            Some(2) => Interaction::Click,
            Some(3) => Interaction::ContextMenu,
            Some(4) => Interaction::DblClick,
            Some(7) => Interaction::TouchStart,
            Some(9) => Interaction::TouchEnd,
            _ => Interaction::Other,
        };
        Some(Self {
            interaction,
            id: number_id(&data["id"])?,
            x: data["x"].as_f64(),
            y: data["y"].as_f64(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Input {
    pub id: NodeId,
    pub text: Option<String>,
    /// posthog-js sends this on every input; it only means something on checkables.
    pub checked: Option<bool>,
}

impl Input {
    pub(crate) fn from_data(data: &Value) -> Option<Self> {
        Some(Self {
            id: number_id(&data["id"])?,
            text: data["text"].as_str().map(str::to_owned),
            checked: data["isChecked"].as_bool(),
        })
    }
}

/// A text selection range. Offsets count UTF-16 units within the start/end nodes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionRange {
    pub start: f64,
    pub start_offset: f64,
    pub end: f64,
    pub end_offset: f64,
}

impl SelectionRange {
    /// The well-formed ranges of a selection event; malformed ones are ignored.
    pub fn parse_all(ranges: &[Value]) -> Vec<Self> {
        ranges
            .iter()
            .filter_map(|range| {
                Some(Self {
                    start: range["start"].as_f64()?,
                    start_offset: range["startOffset"].as_f64()?,
                    end: range["end"].as_f64()?,
                    end_offset: range["endOffset"].as_f64()?,
                })
            })
            .collect()
    }

    pub fn is_caret(&self) -> bool {
        self.start == self.end && self.start_offset == self.end_offset
    }

    pub fn within_one_node(&self) -> bool {
        self.start == self.end
    }
}

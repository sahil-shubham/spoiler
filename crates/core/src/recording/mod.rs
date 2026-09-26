//! Recordings: rrweb events, each tagged with the browser tab (PostHog "window") it came from.
//!
//! A [`Recording`] keeps the recording's JSON text once, plus a small index entry per event (its
//! kind, time, tab, and where its data sits in the text). Events are parsed when the compiler
//! reaches them and dropped after, and posthog-js compressed fields stay compressed until then,
//! so memory follows the input's size rather than a many-fold expansion of it.
//!
//! [`decode`] accepts every encoding Spoiler produces or consumes: recording artifacts, decoded
//! event arrays or JSONL (optionally gzip/zstd-compressed, as fixture stores keep them),
//! and raw PostHog snapshot lines.

mod mobile;
mod posthog;
pub mod rrweb;

use crate::time::Timestamp;
use rrweb::{Input, Mouse, MutationData, SelectionRange, Signal};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use serde_json::{Value, value::RawValue};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashSet,
    io::Read,
    path::Path,
};

/// One rrweb event as an owned value: for building recordings in code. Decoded recordings keep
/// events as text instead (see [`Recording`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: u32,
    pub timestamp: Timestamp,
    pub data: Value,
    /// The PostHog window id: one per browser tab, each with its own DOM node-id space.
    pub win: String,
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

fn is_compressed(bytes: &[u8]) -> bool {
    bytes.starts_with(&ZSTD_MAGIC) || bytes.starts_with(&GZIP_MAGIC)
}

/// Bounds on untrusted input. A recording's content is shaped by whoever used the page (and by
/// anyone able to send posthog-js payloads), so decoding must not trust its sizes: a few KiB of
/// compressed field can expand to gigabytes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most JSON bytes a recording may decode to, counting its text and every decompressed
    /// stream and field.
    pub max_bytes: u64,
}

impl Default for Limits {
    /// 512 MiB: eight times the largest recording seen so far (63 MiB).
    fn default() -> Self {
        Self {
            max_bytes: 512 << 20,
        }
    }
}

/// Why a recording could not be decoded or read. Oddly shaped events are not errors (they are
/// [`Reading::Malformed`]); these are recordings that cannot be read at all.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Over [`Limits::max_bytes`]: the recording, or a compressed field in it, is too large.
    #[error("recording decodes to more than {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("compressed content is neither gzip nor zstd")]
    UnknownCompression,
    #[error("corrupt compressed content")]
    Corrupt(#[source] std::io::Error),
    #[error("{0} is not UTF-8")]
    NotUtf8(&'static str),
    #[error("invalid {what}")]
    Json {
        what: Cow<'static, str>,
        #[source]
        source: serde_json::Error,
    },
    #[error("reading {path}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl DecodeError {
    pub(crate) fn json(
        what: impl Into<Cow<'static, str>>,
    ) -> impl FnOnce(serde_json::Error) -> Self {
        let what = what.into();
        move |source| Self::Json { what, source }
    }
}

pub type Result<T, E = DecodeError> = std::result::Result<T, E>;

/// What is left of a recording's [`Limits`].
struct Budget {
    remaining: Cell<u64>,
    max_bytes: u64,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self {
            remaining: Cell::new(limits.max_bytes),
            max_bytes: limits.max_bytes,
        }
    }

    fn charge(&self, bytes: usize) -> Result<()> {
        let bytes = bytes as u64;
        let remaining = self.remaining.get();
        if bytes > remaining {
            return Err(DecodeError::TooLarge {
                max_bytes: self.max_bytes,
            });
        }
        self.remaining.set(remaining - bytes);
        Ok(())
    }

    /// Decompress a gzip or zstd stream within the budget. Reading stops one byte past it, so an
    /// oversized stream is never fully materialized.
    fn decompress(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let reader: Box<dyn Read + '_> = if bytes.starts_with(&ZSTD_MAGIC) {
            Box::new(zstd::stream::read::Decoder::new(bytes).map_err(DecodeError::Corrupt)?)
        } else if bytes.starts_with(&GZIP_MAGIC) {
            // Every gzip member: concatenated `.gz` chunks are one stream.
            Box::new(flate2::read::MultiGzDecoder::new(bytes))
        } else {
            return Err(DecodeError::UnknownCompression);
        };
        let mut out = Vec::new();
        reader
            .take(self.remaining.get().saturating_add(1))
            .read_to_end(&mut out)
            .map_err(DecodeError::Corrupt)?;
        self.charge(out.len())?;
        Ok(out)
    }
}

/// Replace each unpaired UTF-16 surrogate escape in JSON text (`\ud83d` with no low half) with
/// `\ufffd`, in place.
///
/// Browsers record such strings (text cut through an emoji is still a valid JavaScript string),
/// but JSON parsers reject them, which would fail the whole recording. The replacement is the
/// same six bytes and one UTF-16 unit, so offsets and JavaScript lengths are unchanged.
fn replace_lone_surrogates(json: &mut [u8]) {
    fn escape(json: &[u8], at: usize) -> Option<u16> {
        let digits = json.get(at..at + 6)?;
        if digits[0] != b'\\' || digits[1] != b'u' {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(&digits[2..]).ok()?, 16).ok()
    }
    const HIGH: std::ops::RangeInclusive<u16> = 0xd800..=0xdbff;
    const LOW: std::ops::RangeInclusive<u16> = 0xdc00..=0xdfff;
    let mut at = 0;
    while let Some(offset) = json
        .get(at..)
        .and_then(|rest| rest.iter().position(|&byte| byte == b'\\'))
    {
        at += offset;
        match escape(json, at) {
            Some(unit) if HIGH.contains(&unit) => {
                if escape(json, at + 6).is_some_and(|next| LOW.contains(&next)) {
                    at += 12;
                } else {
                    json[at..at + 6].copy_from_slice(br"\ufffd");
                    at += 6;
                }
            }
            Some(unit) if LOW.contains(&unit) => {
                json[at..at + 6].copy_from_slice(br"\ufffd");
                at += 6;
            }
            Some(_) => at += 6,
            // Any other escape is two bytes, including `\\`, whose second backslash starts nothing.
            None => at += 2,
        }
    }
}

/// Where an event's data sits in the recording text.
#[derive(Clone, Copy, Debug)]
enum Data {
    Null,
    Span {
        start: usize,
        end: usize,
    },
    /// Minimal document inserted before a first screenshot incremental without a full snapshot.
    MinimalScreenshot,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    kind: u32,
    timestamp: Timestamp,
    tab: u32,
    data: Data,
    /// posthog-js `cv` compression: some fields are packed strings.
    compressed: bool,
}

/// A decoded recording, parsed lazily. See the module docs.
pub struct Recording {
    text: String,
    entries: Vec<Entry>,
    tabs: Vec<String>,
    malformed_snapshot_lines: usize,
    mobile_contexts: Option<Vec<mobile::Context>>,
    mobile_reserved: Vec<HashSet<rrweb::NodeId>>,
    mobile_ids: RefCell<Vec<rrweb::NodeId>>,
    budget: Budget,
}

/// One event of a [`Recording`].
#[derive(Clone, Copy, Debug)]
pub struct EventRef<'r> {
    pub kind: u32,
    pub timestamp: Timestamp,
    /// The PostHog window id.
    pub win: &'r str,
    tab: u32,
    data: &'r str,
    compressed: bool,
    mobile_context: mobile::Context,
    synthetic_full: bool,
}

impl EventRef<'_> {
    /// Same timestamp/tab/kind/compression and exact data text, across all sources. Unlike
    /// PostHog's source-local whole-event hash, this also removes cross-source ingestion copies.
    /// Extra event metadata cannot change a trace action if these fields match.
    pub fn same_as(&self, other: &EventRef<'_>) -> bool {
        self.kind == other.kind
            && self.tab == other.tab
            && self.compressed == other.compressed
            && self.data == other.data
    }
}

/// What an event means, or why it has no meaning to the compiler.
#[derive(Debug)]
pub enum Reading {
    Signal(Signal),
    /// Recorded content the compiler deliberately reads past, named for coverage reports.
    Uninterpreted(Cow<'static, str>),
    /// Data without the shape rrweb gives this kind of event.
    Malformed(Cow<'static, str>),
}

/// Parse JSON of any nesting depth: real DOMs nest deeper than serde_json's default limit of
/// 128 allows (each DOM level is two JSON levels), and the stack grows on demand.
fn parse_deep<T: for<'de> Deserialize<'de>>(json: &[u8]) -> serde_json::Result<T> {
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    deserializer.disable_recursion_limit();
    let value = T::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

fn is_object(json: &str) -> bool {
    json.trim_start().starts_with('{')
}

/// Whether a JSON object has a `win` key: a decoded event rather than a PostHog line.
fn has_win(json: &str) -> bool {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "rrweb::present_field")]
        win: bool,
    }
    is_object(json) && parse_borrowed::<Probe>(json).is_ok_and(|probe| probe.win)
}

/// Parse borrowing from `text`, without a depth limit (raw values are skipped iteratively).
fn parse_borrowed<'a, T: Deserialize<'a>>(text: &'a str) -> serde_json::Result<T> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Builds a recording's index over text it will own.
struct Indexer<'t> {
    text: &'t str,
    entries: Vec<Entry>,
    tabs: Vec<String>,
    tab_numbers: FxHashMap<String, u32>,
    malformed_snapshot_lines: usize,
}

impl<'t> Indexer<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            entries: Vec::new(),
            tabs: Vec::new(),
            tab_numbers: FxHashMap::default(),
            malformed_snapshot_lines: 0,
        }
    }

    fn tab(&mut self, win: &str) -> u32 {
        if let Some(number) = self.tab_numbers.get(win) {
            return *number;
        }
        let number = self.tabs.len() as u32;
        self.tabs.push(win.to_owned());
        self.tab_numbers.insert(win.to_owned(), number);
        number
    }

    /// `data` must be a slice of the indexer's text (borrowed by the parser, not copied).
    fn push(
        &mut self,
        kind: u32,
        timestamp: Timestamp,
        win: &str,
        data: Option<&RawValue>,
        compressed: bool,
    ) {
        let data = match data {
            Some(raw) => {
                let json = raw.get();
                let start = json.as_ptr() as usize - self.text.as_ptr() as usize;
                debug_assert!(start + json.len() <= self.text.len());
                Data::Span {
                    start,
                    end: start + json.len(),
                }
            }
            None => Data::Null,
        };
        let tab = self.tab(win);
        self.entries.push(Entry {
            kind,
            timestamp,
            tab,
            data,
            compressed,
        });
    }
}

/// A decoded event, as Spoiler writes them.
#[derive(Deserialize)]
struct DecodedEvent<'a> {
    #[serde(rename = "type")]
    kind: u32,
    timestamp: Timestamp,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    #[serde(borrow)]
    win: Cow<'a, str>,
}

#[derive(Deserialize)]
struct EventsDocument<'a> {
    #[serde(borrow)]
    events: Vec<DecodedEvent<'a>>,
}

impl Recording {
    fn build(
        text: String,
        budget: Budget,
        index: impl FnOnce(&mut Indexer<'_>) -> Result<()>,
    ) -> Result<Self> {
        let mut bytes = text.into_bytes();
        replace_lone_surrogates(&mut bytes);
        // Only ASCII escapes changed, so the text is still UTF-8.
        let text = String::from_utf8(bytes).map_err(|_| DecodeError::NotUtf8("recording"))?;
        let (mut entries, tabs, malformed_snapshot_lines) = {
            let mut indexer = Indexer::new(&text);
            index(&mut indexer)?;
            (
                indexer.entries,
                indexer.tabs,
                indexer.malformed_snapshot_lines,
            )
        };
        let (mobile_contexts, mobile_reserved) =
            match mobile::index_contexts(&mut entries, &text, tabs.len()) {
                Some((contexts, reserved)) => (Some(contexts), reserved),
                None => (None, Vec::new()),
            };
        let mobile_ids = RefCell::new(vec![100; tabs.len()]);
        Ok(Self {
            text,
            entries,
            tabs,
            budget,
            malformed_snapshot_lines,
            mobile_contexts,
            mobile_reserved,
            mobile_ids,
        })
    }

    /// Recordings built in code (tests, tools).
    pub fn from_events(events: &[Event]) -> Result<Self> {
        let mut text = String::new();
        for event in events {
            text.push_str(&serde_json::to_string(event).map_err(DecodeError::json("event"))?);
            text.push('\n');
        }
        decode(text.as_bytes())
    }

    /// Parse PostHog snapshot response bodies, sorted by timestamp.
    ///
    /// Bodies are kept separate by a newline: PostHog strips the trailing newline, so
    /// byte-concatenating two responses would fuse their boundary lines.
    pub fn from_snapshot_bodies(bodies: &[String], limits: Limits) -> Result<Self> {
        let budget = Budget::new(limits);
        let mut length = 0_usize;
        for (index, body) in bodies.iter().enumerate() {
            let bytes =
                body.len()
                    .checked_add(usize::from(index != 0))
                    .ok_or(DecodeError::TooLarge {
                        max_bytes: limits.max_bytes,
                    })?;
            budget.charge(bytes)?;
            length = length.checked_add(bytes).ok_or(DecodeError::TooLarge {
                max_bytes: limits.max_bytes,
            })?;
        }
        let mut text = String::with_capacity(length);
        for (index, body) in bodies.iter().enumerate() {
            if index != 0 {
                text.push('\n');
            }
            text.push_str(body);
        }
        Self::build(text, budget, |indexer| {
            posthog::index_lines(indexer);
            Ok(())
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of recording text held.
    pub fn text_len(&self) -> usize {
        self.text.len()
    }

    /// Unparseable PostHog snapshot lines omitted from the event index.
    pub fn malformed_snapshot_lines(&self) -> usize {
        self.malformed_snapshot_lines
    }

    pub fn events(&self) -> impl Iterator<Item = EventRef<'_>> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| EventRef {
                kind: entry.kind,
                timestamp: entry.timestamp,
                win: &self.tabs[entry.tab as usize],
                tab: entry.tab,
                data: match entry.data {
                    Data::Null | Data::MinimalScreenshot => "null",
                    Data::Span { start, end } => &self.text[start..end],
                },
                compressed: entry.compressed,
                mobile_context: self
                    .mobile_contexts
                    .as_ref()
                    .map_or(mobile::Context::default(), |contexts| contexts[index]),
                synthetic_full: matches!(entry.data, Data::MinimalScreenshot),
            })
    }

    /// Unpack a posthog-js packed string: compressed bytes, one per UTF-16 unit.
    fn unpack(&self, packed: &str) -> Result<Vec<u8>> {
        let bytes: Vec<u8> = packed.encode_utf16().map(|unit| unit as u8).collect();
        let mut unpacked = self.budget.decompress(&bytes)?;
        replace_lone_surrogates(&mut unpacked);
        Ok(unpacked)
    }

    /// What an event means. Only an exhausted input budget is fatal; corrupt compressed
    /// content and oddly shaped data are skipped and counted as [`Reading::Malformed`].
    pub fn read(&self, event: &EventRef<'_>) -> Result<Reading> {
        use rrweb::{
            CUSTOM, DOM_CONTENT_LOADED, FULL_SNAPSHOT, INCREMENTAL_SNAPSHOT, LOAD, META, PLUGIN,
            source,
        };
        let data = event.data;
        let small = || serde_json::from_str::<Value>(data).unwrap_or(Value::Null);
        Ok(match event.kind {
            FULL_SNAPSHOT => self.read_full_snapshot(event)?,
            INCREMENTAL_SNAPSHOT => {
                #[derive(Deserialize)]
                struct Head {
                    #[serde(default)]
                    source: Value,
                }
                let head = is_object(data)
                    .then(|| parse_borrowed::<Head>(data).ok())
                    .flatten();
                let Some(head) = head else {
                    return Ok(Reading::Malformed("incremental_snapshot".into()));
                };
                let Some(kind) = head.source.as_u64() else {
                    return Ok(Reading::Malformed("incremental_snapshot".into()));
                };
                match kind {
                    source::MUTATION => self.read_mutation(event)?,
                    source::STYLE_SHEET_RULE => self.read_style_sheet_rule(event)?,
                    source::MOUSE_INTERACTION => match Mouse::from_data(&small()) {
                        Some(mouse) => Reading::Signal(Signal::Mouse(mouse)),
                        None => Reading::Malformed("mouse_interaction".into()),
                    },
                    source::INPUT => match Input::from_data(&small()) {
                        Some(input) => Reading::Signal(Signal::Input(input)),
                        None => Reading::Malformed("input".into()),
                    },
                    source::SELECTION => match small()["ranges"].as_array() {
                        Some(ranges) => {
                            Reading::Signal(Signal::Selection(SelectionRange::parse_all(ranges)))
                        }
                        None => Reading::Malformed("selection".into()),
                    },
                    other => Reading::Uninterpreted(source::name(other).into()),
                }
            }
            META => match small()["href"].as_str() {
                Some(href) if !href.is_empty() || !event.mobile_context.mobile => {
                    Reading::Signal(Signal::Meta {
                        href: href.to_owned(),
                    })
                }
                _ if event.mobile_context.mobile => {
                    Reading::Uninterpreted("mobile_meta_without_screen".into())
                }
                _ => Reading::Malformed("meta".into()),
            },
            CUSTOM => {
                let mut value = small();
                match value["tag"].as_str().map(str::to_owned) {
                    Some(tag) if tag == "keyboard" && event.mobile_context.mobile => {
                        let mut ids = self.mobile_ids.borrow_mut();
                        match mobile::keyboard(
                            &value,
                            event.mobile_context,
                            &mut ids[event.tab as usize],
                            &self.mobile_reserved[event.tab as usize],
                        ) {
                            Some(mutation) => Reading::Signal(Signal::Mutation(mutation)),
                            None => Reading::Malformed("keyboard".into()),
                        }
                    }
                    Some(tag) => Reading::Signal(Signal::Custom {
                        tag,
                        payload: value["payload"].take(),
                    }),
                    None => Reading::Malformed("custom".into()),
                }
            }
            PLUGIN => {
                let mut value = small();
                match value["plugin"].as_str().map(str::to_owned) {
                    Some(name) => Reading::Signal(Signal::Plugin {
                        name,
                        payload: value["payload"].take(),
                    }),
                    None => Reading::Malformed("plugin".into()),
                }
            }
            DOM_CONTENT_LOADED => Reading::Uninterpreted("dom_content_loaded".into()),
            LOAD => Reading::Uninterpreted("load".into()),
            other => Reading::Uninterpreted(format!("event_type_{other}").into()),
        })
    }

    fn read_full_snapshot(&self, event: &EventRef<'_>) -> Result<Reading> {
        if event.synthetic_full {
            self.mobile_ids.borrow_mut()[event.tab as usize] = 100;
            return Ok(Reading::Signal(Signal::FullSnapshot(
                mobile::empty_document(),
            )));
        }
        #[derive(Deserialize)]
        struct Shape<'a> {
            #[serde(borrow, default)]
            node: Option<&'a RawValue>,
        }
        #[derive(Deserialize)]
        struct FullSnapshot {
            node: rrweb::SerializedNode,
        }
        let unpacked;
        let json: &str = match serde_json::from_str::<String>(event.data) {
            Ok(packed) if event.compressed => {
                let bytes = match self.unpack(&packed) {
                    Ok(bytes) => bytes,
                    Err(error @ DecodeError::TooLarge { .. }) => return Err(error),
                    Err(_) => return Ok(Reading::Malformed("full_snapshot".into())),
                };
                unpacked = match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => return Ok(Reading::Malformed("full_snapshot".into())),
                };
                &unpacked
            }
            _ => event.data,
        };
        if event.mobile_context.mobile && json.contains("\"wireframes\"") {
            let mut ids = self.mobile_ids.borrow_mut();
            return Ok(
                match mobile::full(
                    json,
                    event.mobile_context,
                    &mut ids[event.tab as usize],
                    &self.mobile_reserved[event.tab as usize],
                ) {
                    Some(node) => Reading::Signal(Signal::NativeFullSnapshot(node)),
                    None => Reading::Malformed("full_snapshot".into()),
                },
            );
        }
        // A snapshot without a node object has no page to mirror.
        let node_is_object = is_object(json)
            && parse_borrowed::<Shape<'_>>(json)
                .ok()
                .and_then(|shape| shape.node)
                .is_some_and(|node| node.get().starts_with('{'));
        if !node_is_object {
            return Ok(Reading::Malformed("full_snapshot".into()));
        }
        Ok(match parse_deep::<FullSnapshot>(json.as_bytes()) {
            Ok(mut snapshot) => {
                if event.mobile_context.mobile && mobile::screenshot_attribute_key(json) {
                    mobile::ensure_chrome_parents(&mut snapshot.node);
                }
                Reading::Signal(Signal::FullSnapshot(snapshot.node))
            }
            Err(_) => Reading::Malformed("full_snapshot".into()),
        })
    }

    fn read_mutation(&self, event: &EventRef<'_>) -> Result<Reading> {
        if !is_object(event.data) {
            return Ok(Reading::Malformed("mutation".into()));
        }
        if event.mobile_context.mobile && mobile::has_wireframe_mutation(event.data) {
            let mut ids = self.mobile_ids.borrow_mut();
            return Ok(
                match mobile::mutation(
                    event.data,
                    event.mobile_context,
                    &mut ids[event.tab as usize],
                    &self.mobile_reserved[event.tab as usize],
                ) {
                    Some(mutation) => Reading::Signal(Signal::Mutation(mutation)),
                    None => Reading::Malformed("mutation".into()),
                },
            );
        }
        let Ok(data) = parse_deep::<MutationData>(event.data.as_bytes()) else {
            return Ok(Reading::Malformed("mutation".into()));
        };
        Ok(
            match data.resolve(event.compressed, |packed| self.unpack(packed)) {
                Ok(Some(mut mutation)) => {
                    if event.mobile_context.mobile {
                        for add in &mut mutation.adds {
                            if add.parent == 0
                                && add.node.tag == "img"
                                && add.node.attributes.0.iter().any(|(name, value)| {
                                    name == "data-posthog-screenshot" && !value.is_null()
                                })
                            {
                                add.parent = mobile::BODY_ID;
                            }
                        }
                    }
                    Reading::Signal(Signal::Mutation(mutation))
                }
                Err(error @ DecodeError::TooLarge { .. }) => return Err(error),
                Ok(None) | Err(_) => Reading::Malformed("mutation".into()),
            },
        )
    }

    fn read_style_sheet_rule(&self, event: &EventRef<'_>) -> Result<Reading> {
        #[derive(Deserialize)]
        struct Fields<'a> {
            #[serde(borrow, default)]
            adds: Option<&'a RawValue>,
            #[serde(borrow, default)]
            removes: Option<&'a RawValue>,
        }
        let malformed = || Reading::Malformed("style_sheet_rule".into());
        let Ok(fields) = parse_borrowed::<Fields<'_>>(event.data) else {
            return Ok(malformed());
        };
        for field in [fields.adds, fields.removes].into_iter().flatten() {
            if field.get().starts_with('[') {
                continue;
            }
            if !event.compressed {
                return Ok(malformed());
            }
            let Ok(packed) = serde_json::from_str::<String>(field.get()) else {
                return Ok(malformed());
            };
            match self.unpack(&packed) {
                Ok(json) if parse_deep::<Vec<serde::de::IgnoredAny>>(&json).is_ok() => {}
                Err(error @ DecodeError::TooLarge { .. }) => return Err(error),
                _ => return Ok(malformed()),
            }
        }
        Ok(Reading::Uninterpreted("style_sheet_rule".into()))
    }

    /// An event's data as JSON with posthog-js compression undone, for recording artifacts.
    /// Malformed packed events retain their raw data, which remains malformed after a roundtrip.
    pub fn expanded_data(&self, event: &EventRef<'_>) -> Result<String> {
        match self.expand_data(event) {
            Ok(data) => Ok(data),
            Err(error @ DecodeError::TooLarge { .. }) => Err(error),
            Err(_) => Ok(event.data.to_owned()),
        }
    }

    fn expand_data(&self, event: &EventRef<'_>) -> Result<String> {
        if event.synthetic_full {
            return Ok(mobile::minimal_full_json());
        }
        if !event.compressed {
            return Ok(event.data.to_owned());
        }
        if event.kind == rrweb::FULL_SNAPSHOT
            && let Ok(packed) = serde_json::from_str::<String>(event.data)
        {
            let json = String::from_utf8(self.unpack(&packed)?)
                .map_err(|_| DecodeError::NotUtf8("compressed snapshot"))?;
            parse_borrowed::<&RawValue>(&json).map_err(DecodeError::json("compressed snapshot"))?;
            return Ok(json);
        }
        let mut data: Value =
            serde_json::from_str(event.data).map_err(DecodeError::json("event data"))?;
        if event.kind == rrweb::INCREMENTAL_SNAPSHOT && data.is_object() {
            let fields: &[&str] = match data["source"].as_u64() {
                Some(rrweb::source::MUTATION) => &["adds", "removes", "texts", "attributes"],
                Some(rrweb::source::STYLE_SHEET_RULE) => &["adds", "removes"],
                _ => &[],
            };
            for field in fields {
                if let Some(packed) = data[*field].as_str() {
                    let json = self.unpack(packed)?;
                    data[*field] = serde_json::from_slice(&json)
                        .map_err(DecodeError::json("compressed field"))?;
                }
            }
        }
        Ok(data.to_string())
    }

    /// Serialize as decoded events (`{type, timestamp, data, win}`), compression undone.
    pub fn serialize_events<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            #[serde(rename = "type")]
            kind: u32,
            timestamp: Timestamp,
            data: &'a RawValue,
            win: &'a str,
        }
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for event in self.events() {
            let json = self
                .expanded_data(&event)
                .map_err(serde::ser::Error::custom)?;
            let data = RawValue::from_string(json).map_err(serde::ser::Error::custom)?;
            seq.serialize_element(&Out {
                kind: event.kind,
                timestamp: event.timestamp,
                data: &data,
                win: event.win,
            })?;
        }
        seq.end()
    }
}

/// Decode a recording in any supported encoding, within the default [`Limits`].
pub fn decode(raw: &[u8]) -> Result<Recording> {
    decode_with(raw, Limits::default())
}

pub fn decode_with(raw: &[u8], limits: Limits) -> Result<Recording> {
    let budget = Budget::new(limits);
    let bytes = if is_compressed(raw) {
        budget.decompress(raw)?
    } else {
        budget.charge(raw.len())?;
        raw.to_vec()
    };
    let text = String::from_utf8(bytes).map_err(|_| DecodeError::NotUtf8("recording"))?;
    Recording::build(text, budget, |indexer| {
        index_decoded(indexer)?;
        // Decoded inputs keep the order they were written in; replay needs recording order.
        // The sort is stable, so events sharing a timestamp keep their written order.
        indexer
            .entries
            .sort_by(|a, b| crate::time::chronological(&a.timestamp, &b.timestamp));
        Ok(())
    })
}

/// Index a decoded recording: a recording artifact, a bare events document or array, decoded
/// JSONL, or PostHog snapshot lines.
fn index_decoded(indexer: &mut Indexer<'_>) -> Result<()> {
    let text = indexer.text.trim();
    if text.is_empty() {
        return Ok(());
    }
    // One JSON document: an object with `events`, or a bare array of decoded events.
    if parse_borrowed::<serde::de::IgnoredAny>(text).is_ok() {
        if is_object(text) {
            #[derive(Deserialize)]
            struct Probe<'a> {
                #[serde(borrow)]
                events: Option<&'a RawValue>,
                #[serde(borrow)]
                kind: Option<&'a RawValue>,
                #[serde(borrow)]
                schema_version: Option<&'a RawValue>,
            }
            if let Ok(probe) = parse_borrowed::<Probe<'_>>(text)
                && probe.events.is_some()
            {
                // A bare `{events}` document is accepted; anything claiming to be an artifact
                // must be a recording artifact of a supported version.
                if probe.kind.is_some() || probe.schema_version.is_some() {
                    crate::artifact::Header::read_json(
                        text.as_bytes(),
                        crate::artifact::Kind::Recording,
                    )
                    .map_err(|error| DecodeError::Json {
                        what: "recording artifact".into(),
                        source: serde::de::Error::custom(error),
                    })?;
                }
                let document: EventsDocument<'_> =
                    parse_borrowed(text).map_err(DecodeError::json("recording artifact"))?;
                for event in document.events {
                    indexer.push(event.kind, event.timestamp, &event.win, event.data, false);
                }
                return Ok(());
            }
        } else if let Ok(items) = parse_borrowed::<Vec<&RawValue>>(text)
            && items.first().is_none_or(|first| has_win(first.get()))
        {
            for item in items {
                let event: DecodedEvent<'_> =
                    parse_borrowed(item.get()).map_err(DecodeError::json("recording event"))?;
                indexer.push(event.kind, event.timestamp, &event.win, event.data, false);
            }
            return Ok(());
        }
    }
    // Line-delimited: decoded events carry `win`; anything else is PostHog snapshot lines.
    // A bad first snapshot line must not abort inspection of the lines that follow it.
    let first_valid = text
        .lines()
        .find(|line| parse_borrowed::<serde::de::IgnoredAny>(line).is_ok());
    if first_valid.is_none_or(|line| !has_win(line)) {
        posthog::index_lines(indexer);
        return Ok(());
    }
    for (number, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let event: DecodedEvent<'_> = parse_borrowed(line)
            .map_err(DecodeError::json(format!("event on line {}", number + 1)))?;
        indexer.push(event.kind, event.timestamp, &event.win, event.data, false);
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<Recording> {
    let raw = std::fs::read(path).map_err(|source| DecodeError::Io {
        path: path.to_owned(),
        source,
    })?;
    decode(&raw)
}

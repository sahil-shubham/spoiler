//! PostHog snapshot lines (`blob_v2` bodies), indexed without parsing event data.
//!
//! Line shapes, as the PostHog player accepts them: `[windowId, event]`,
//! `{window_id | windowId, data: [events]}`, and a bare event carrying `windowId`.

use super::{DecodeError, Indexer, Result, is_object, parse_borrowed, rrweb};
use crate::text::js_string;
use crate::time::Timestamp;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

/// A number field as JavaScript's `typeof x === "number"` sees it.
fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(Value::deserialize(d)?.as_f64())
}

fn unsigned<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Value::deserialize(d)?.as_u64())
}

/// An event object on a snapshot line, and (for bare events) the line's window fields.
#[derive(Deserialize)]
struct SnapshotObject<'a> {
    #[serde(rename = "type", default, deserialize_with = "unsigned")]
    kind: Option<u64>,
    #[serde(default, deserialize_with = "number")]
    timestamp: Option<f64>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    /// posthog-js field compression (`cv: "2024-10"`), by presence.
    #[serde(default, deserialize_with = "rrweb::present_field")]
    cv: bool,
    #[serde(borrow, default)]
    window_id: Option<&'a RawValue>,
    #[serde(borrow, rename = "windowId", default)]
    window_id_camel: Option<&'a RawValue>,
}

/// JS `String(value)` of a raw JSON value; a missing value is `undefined`.
fn window_name(raw: Option<&RawValue>) -> String {
    match raw {
        Some(raw) => js_string(&serde_json::from_str(raw.get()).unwrap_or(Value::Null)),
        None => "undefined".into(),
    }
}

/// Index one event object; entries that are not events (no numeric type/timestamp) are skipped.
fn index_event(indexer: &mut Indexer<'_>, raw: &RawValue, win: &str) -> Result<()> {
    if !is_object(raw.get()) {
        return Ok(());
    }
    let event: SnapshotObject<'_> =
        parse_borrowed(raw.get()).map_err(DecodeError::json("snapshot event"))?;
    let (Some(kind), Some(timestamp)) = (event.kind, event.timestamp) else {
        return Ok(());
    };
    let kind = u32::try_from(kind).map_err(|_| DecodeError::EventType(kind))?;
    indexer.push(kind, Timestamp(timestamp), win, event.data, event.cv);
    Ok(())
}

pub(super) fn index_lines(indexer: &mut Indexer<'_>) -> Result<()> {
    let text = indexer.text;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let tuple: Vec<&RawValue> =
                parse_borrowed(line).map_err(DecodeError::json("snapshot line"))?;
            let win = window_name(tuple.first().copied());
            if let Some(event) = tuple.get(1) {
                index_event(indexer, event, &win)?;
            }
        } else if trimmed.starts_with('{') {
            let object: SnapshotObject<'_> =
                parse_borrowed(line).map_err(DecodeError::json("snapshot line"))?;
            let batch = object.data.filter(|data| data.get().starts_with('['));
            match batch {
                Some(batch) => {
                    // `window_id ?? windowId`: a null window_id falls through.
                    let window = object
                        .window_id
                        .filter(|id| id.get() != "null")
                        .or(object.window_id_camel);
                    let win = window_name(window);
                    let events: Vec<&RawValue> =
                        parse_borrowed(batch.get()).map_err(DecodeError::json("snapshot batch"))?;
                    for event in events {
                        index_event(indexer, event, &win)?;
                    }
                }
                None => {
                    let win = window_name(object.window_id_camel);
                    let raw: &RawValue =
                        parse_borrowed(line).map_err(DecodeError::json("snapshot line"))?;
                    index_event(indexer, raw, &win)?;
                }
            }
        } else {
            // Validate it is JSON at all, then reject its shape.
            parse_borrowed::<serde::de::IgnoredAny>(line)
                .map_err(DecodeError::json("snapshot line"))?;
            return Err(DecodeError::UnrecognizedLine);
        }
    }
    indexer
        .entries
        .sort_by(|a, b| crate::time::chronological(&a.timestamp, &b.timestamp));
    Ok(())
}

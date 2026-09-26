//! PostHog snapshot lines (`blob_v2` bodies), indexed without parsing event data.
//!
//! Line shapes, as the PostHog player accepts them: `[windowId, event]`,
//! `{window_id | windowId, data: [events]}`, and a bare event carrying `windowId`.

use super::{Indexer, is_object, parse_borrowed};
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
    /// Only posthog-js `cv: "2024-10"` uses packed fields.
    #[serde(borrow, default)]
    cv: Option<&'a RawValue>,
    #[serde(borrow, default)]
    window_id: Option<&'a RawValue>,
    #[serde(borrow, rename = "windowId", default)]
    window_id_camel: Option<&'a RawValue>,
}

/// PostHog coalesces missing, null and empty window IDs into its default tab.
fn window_name(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let json = raw.get();
    if json == "null" || json == "\"\"" {
        return String::new();
    }
    let value = serde_json::from_str(json).unwrap_or(Value::Null);
    match value {
        Value::Null | Value::Bool(false) => String::new(),
        Value::String(win) => win,
        other => js_string(&other),
    }
}

fn compressed_version(raw: Option<&RawValue>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let json = raw.get();
    match serde_json::from_str::<&str>(json) {
        Ok(version) => version == "2024-10",
        // Escaped JSON strings cannot borrow from the input.
        Err(_) => serde_json::from_str::<String>(json).is_ok_and(|version| version == "2024-10"),
    }
}

/// PostHog ignores events that cannot be interpreted, without dropping their neighbors.
fn index_event(indexer: &mut Indexer<'_>, raw: &RawValue, win: &str) {
    if !is_object(raw.get()) {
        return;
    }
    let Ok(event) = parse_borrowed::<SnapshotObject<'_>>(raw.get()) else {
        indexer.malformed_snapshot_lines += 1;
        return;
    };
    let (Some(kind), Some(timestamp)) = (event.kind, event.timestamp) else {
        return;
    };
    let Ok(kind) = u32::try_from(kind) else {
        indexer.malformed_snapshot_lines += 1;
        return;
    };
    indexer.push(
        kind,
        Timestamp(timestamp),
        win,
        event.data,
        compressed_version(event.cv),
    );
}

pub(super) fn index_lines(indexer: &mut Indexer<'_>) {
    let text = indexer.text;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let Ok(tuple) = parse_borrowed::<Vec<&RawValue>>(line) else {
                indexer.malformed_snapshot_lines += 1;
                continue;
            };
            if let Some(event) = tuple.get(1) {
                index_event(indexer, event, &window_name(tuple.first().copied()));
            } else {
                indexer.malformed_snapshot_lines += 1;
            }
        } else if trimmed.starts_with('{') {
            let Ok(object) = parse_borrowed::<SnapshotObject<'_>>(line) else {
                indexer.malformed_snapshot_lines += 1;
                continue;
            };
            let batch = object.data.filter(|data| data.get().starts_with('['));
            match batch {
                Some(batch) => {
                    // `window_id ?? windowId`: a null window_id falls through.
                    let window = object
                        .window_id
                        .filter(|id| id.get() != "null")
                        .or(object.window_id_camel);
                    let win = window_name(window);
                    let Ok(events) = parse_borrowed::<Vec<&RawValue>>(batch.get()) else {
                        indexer.malformed_snapshot_lines += 1;
                        continue;
                    };
                    for event in events {
                        index_event(indexer, event, &win);
                    }
                }
                None => {
                    let win = window_name(object.window_id_camel);
                    if let Ok(raw) = parse_borrowed::<&RawValue>(line) {
                        index_event(indexer, raw, &win);
                    } else {
                        indexer.malformed_snapshot_lines += 1;
                    }
                }
            }
        } else {
            indexer.malformed_snapshot_lines += 1;
        }
    }
    indexer
        .entries
        .sort_by(|a, b| crate::time::chronological(&a.timestamp, &b.timestamp));
}

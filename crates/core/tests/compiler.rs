//! Decoded recordings, regardless of their container, replay by event time.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use spoiler_core::recording::decode;

#[test]
fn decoded_document_array_and_jsonl_sort_stably_by_timestamp() {
    let events = [
        json!({"type": 4, "timestamp": 2000, "data": {"href": "https://demo.test/later"}, "win": "later"}),
        json!({"type": 4, "timestamp": 1000, "data": {"href": "https://demo.test/first"}, "win": "first"}),
        json!({"type": 4, "timestamp": 1000, "data": {"href": "https://demo.test/second"}, "win": "second"}),
    ];
    let document = json!({"events": events}).to_string();
    let array = json!(events).to_string();
    let jsonl = events
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    for input in [&document, &array, &jsonl] {
        let recording = decode(input.as_bytes()).unwrap();
        let order: Vec<_> = recording
            .events()
            .map(|event| (event.timestamp.0, event.win.to_owned()))
            .collect();
        assert_eq!(
            order,
            [
                (1000.0, "first".into()),
                (1000.0, "second".into()),
                (2000.0, "later".into())
            ]
        );
    }
}

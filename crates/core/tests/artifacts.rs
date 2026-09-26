#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use spoiler_core::{
    artifact::{ArtifactError, Header, Kind, TraceArtifact},
    recording,
    time::Millis,
    trace::{self, Action, Detail, Ref},
    vocab::{Thresholds, Vocabulary, VocabularyError},
};

fn trace_document() -> Value {
    let mut first = Action::new(Detail::Nav, Millis(100.0), 1);
    first.reference = Ref(1);
    let mut last = Action::new(Detail::Nav, Millis(200.0), 1);
    last.reference = Ref(2);
    let actions = vec![first, last];
    serde_json::to_value(TraceArtifact {
        header: Header::new(Kind::Trace),
        compiler_version: trace::COMPILER_VERSION,
        app: "demo".into(),
        recording_digest: "recording".into(),
        vocab_digest: "vocabulary".into(),
        coverage: Default::default(),
        visits: trace::visits(&actions, &Thresholds::default()),
        tsv: trace::to_tsv(&actions),
        actions,
    })
    .unwrap()
}

fn load(document: &Value) -> Result<TraceArtifact, ArtifactError> {
    TraceArtifact::from_json(&serde_json::to_vec(document).unwrap())
}

#[test]
fn trace_checks_kind_and_schema_before_body() {
    let mut wrong_kind = trace_document();
    wrong_kind["kind"] = json!("recording");
    wrong_kind["actions"] = json!("not a trace");
    assert!(matches!(
        load(&wrong_kind),
        Err(ArtifactError::WrongKind {
            expected: Kind::Trace,
            found: Kind::Recording
        })
    ));

    let mut old_schema = trace_document();
    old_schema["schema_version"] = json!(1);
    old_schema["actions"] = json!("not a trace");
    assert!(matches!(
        load(&old_schema),
        Err(ArtifactError::SchemaVersion {
            kind: Kind::Trace,
            found: 1,
            ..
        })
    ));
    assert_eq!(Kind::Trace.schema_version(), 2);
}

#[test]
fn trace_rejects_invalid_visit_bounds_and_refs() {
    let valid = trace_document();
    assert!(load(&valid).is_ok());
    for (start, end) in [(3, 3), (1, 0), (1, 1)] {
        let mut document = valid.clone();
        document["visits"][0]["start"] = json!(start);
        document["visits"][0]["end"] = json!(end);
        assert!(
            load(&document).is_err(),
            "visit {start}..{end} was accepted"
        );
    }
    for key in ["first_ref", "last_ref"] {
        let mut document = valid.clone();
        document["visits"][0][key] = json!("e99");
        assert!(load(&document).is_err(), "invented {key} was accepted");
    }
    let mut wrong_time = valid.clone();
    wrong_time["visits"][0]["start_ms"] = json!(9999.0);
    assert!(
        load(&wrong_time).is_err(),
        "visit timestamps must match its refs"
    );
}

#[test]
fn trace_rejects_stale_rendered_actions() {
    let mut document = trace_document();
    document["tsv"] = json!("the actions say something else");
    assert!(load(&document).is_err());
}

#[test]
fn snapshot_checks_header_before_vocabulary() {
    let mut document = json!({
        "kind": "trace", "schema_version": 1, "content_digest": "wrong",
        "provenance": { "config_digest": "", "source_revision": null, "sources": [], "generator": {} },
        "vocabulary": { "version": "not a vocabulary" }
    });
    assert!(matches!(
        Vocabulary::from_document(document.clone()),
        Err(VocabularyError::Snapshot(ArtifactError::WrongKind {
            expected: Kind::VocabularySnapshot,
            found: Kind::Trace
        }))
    ));
    document["kind"] = json!("vocabulary_snapshot");
    document["schema_version"] = json!(9);
    assert!(matches!(
        Vocabulary::from_document(document),
        Err(VocabularyError::Snapshot(ArtifactError::SchemaVersion {
            kind: Kind::VocabularySnapshot,
            found: 9,
            ..
        }))
    ));
}

#[test]
fn recording_objects_require_recording_kind_and_version() {
    let event = json!({ "type": 4, "timestamp": 100, "data": { "href": "https://demo.test/page" }, "win": "w1" });
    let events = json!([event]);
    for (kind, version) in [("trace", 1), ("recording", 99)] {
        let bytes = serde_json::to_vec(&json!({
            "kind": kind, "schema_version": version, "events": events
        }))
        .unwrap();
        assert!(
            recording::decode(&bytes).is_err(),
            "{kind} version {version} was accepted"
        );
    }
    assert_eq!(
        recording::decode(
            &serde_json::to_vec(&json!({
                "kind": "recording", "schema_version": Kind::Recording.schema_version(),
                "events": events
            }))
            .unwrap()
        )
        .unwrap()
        .len(),
        1
    );
    assert!(
        recording::decode(
            &serde_json::to_vec(&json!({
                "kind": "recording", "events": events
            }))
            .unwrap()
        )
        .is_err()
    );
    // Without a header, an object with `events` is a plain document (corpus cases are these).
    assert_eq!(
        recording::decode(&serde_json::to_vec(&json!({ "events": events })).unwrap())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        recording::decode(&serde_json::to_vec(&events).unwrap())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        recording::decode(format!("{event}\n").as_bytes())
            .unwrap()
            .len(),
        1
    );
}

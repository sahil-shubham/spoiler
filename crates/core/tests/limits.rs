#![allow(clippy::unwrap_used)]

use spoiler_core::recording::{DecodeError, Limits, Recording};

#[test]
fn snapshot_bodies_charge_separators_against_aggregate_limit() {
    let bodies = vec!["{}".to_owned(), "{}".to_owned()];
    assert!(matches!(
        Recording::from_snapshot_bodies(&bodies, Limits { max_bytes: 4 }),
        Err(DecodeError::TooLarge { max_bytes: 4 })
    ));
    let recording = Recording::from_snapshot_bodies(&bodies, Limits { max_bytes: 5 }).unwrap();
    assert_eq!(recording.text_len(), 5);
}

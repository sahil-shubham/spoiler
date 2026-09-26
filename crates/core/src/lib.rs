//! Deterministic core of Spoiler.
//!
//! The pipeline is a chain of explicit artifacts:
//!
//! ```text
//! recording (rrweb events) ──compile──▶ trace (actions + effects) ──analysis──▶ validated summary
//!                              ▲                                        ▲
//!                   vocabulary snapshot (pinned) ──────────────────────┘
//! ```
//!
//! Nothing in this crate performs network, database, or clock-dependent work: the same inputs
//! always produce the same outputs. Fetching recordings, calling models, and persisting results
//! belong to callers (the `spoiler` CLI, or a host scheduler invoking it).
//!
//! Behavior is defined by the synthetic corpus (`corpus/`, checked by `tests/corpus.rs`).
//! Recorded text comes from browsers, so [`text`] measures and prints it with JavaScript
//! semantics (UTF-16 lengths, ECMAScript whitespace, `String(n)` numbers).

pub mod analysis;
pub mod artifact;
pub mod model;
pub mod recording;
pub mod replay;
pub mod text;
pub mod time;
pub mod trace;
pub mod vocab;

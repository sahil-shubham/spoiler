//! The synthetic corpus: every `corpus/*.json` case compiles to its committed golden trace.
//!
//! A behavior change shows up as a golden diff to review. After an intended change, rewrite
//! goldens with `SPOILER_BLESS=1 cargo test --test corpus` and commit them with the change.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use spoiler_core::{
    recording,
    trace::{compile, to_tsv, visits},
    vocab::{Matcher, Vocabulary},
};
use std::path::{Path, PathBuf};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

#[test]
fn corpus_cases_match_their_goldens() {
    let directory = corpus();
    let vocabulary =
        Vocabulary::parse(&std::fs::read(directory.join("vocabulary.yaml")).unwrap()).unwrap();
    let matcher = Matcher::new(&vocabulary);
    // Only an explicit `1` rewrites goldens: `SPOILER_BLESS=0` or an empty value must still compare.
    let bless = std::env::var("SPOILER_BLESS").is_ok_and(|value| value == "1");

    let mut cases: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.ends_with(".json") && !name.contains(".expected.")
        })
        .collect();
    cases.sort();
    assert!(
        !cases.is_empty(),
        "no corpus cases in {}",
        directory.display()
    );

    let mut failures = Vec::new();
    for case in &cases {
        let raw = std::fs::read(case).unwrap();
        let app = serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["app"]
            .as_str()
            .unwrap()
            .to_owned();
        let recording = recording::decode(&raw).unwrap();
        let compiled = compile(&recording, &matcher, &app).unwrap();
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        // Goldens get blessed; the timeline's own promise must hold whatever they say: each
        // tab's fidelity covers `[first_ms, last_ms]` in order, with no gaps or overlaps.
        for tab in compiled.timeline.iter().flat_map(|timeline| &timeline.tabs) {
            let spans = &tab.fidelity;
            let covers = spans
                .first()
                .is_some_and(|s| s.span.start_ms == tab.first_ms)
                && spans.last().is_some_and(|s| s.span.end_ms == tab.last_ms)
                && spans
                    .windows(2)
                    .all(|pair| pair[0].span.end_ms == pair[1].span.start_ms)
                && spans.iter().all(|s| s.span.end_ms >= s.span.start_ms);
            if !covers {
                failures.push(format!(
                    "{name}: tab {} fidelity does not cover [{:?}, {:?}] in order: {spans:?}",
                    tab.win, tab.first_ms, tab.last_ms
                ));
            }
        }
        let tsv = to_tsv(&compiled.actions) + "\n";
        let report = serde_json::to_string_pretty(&json!({
            "coverage": compiled.coverage,
            "visits": visits(&compiled.actions, &vocabulary.thresholds),
            "timeline": compiled.timeline,
        }))
        .unwrap()
            + "\n";

        let stem = case.with_extension("");
        for (path, actual) in [
            (stem.with_extension("expected.tsv"), &tsv),
            (stem.with_extension("expected.json"), &report),
        ] {
            if bless {
                std::fs::write(&path, actual).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path).unwrap_or_default();
            if &expected != actual {
                failures.push(format!(
                    "{}\n--- expected\n{expected}--- actual\n{actual}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden(s) differ (SPOILER_BLESS=1 to accept):\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

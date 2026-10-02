#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use spoiler_core::{
    analysis::{Assessment, ModelSummary, assess, validate},
    artifact::{VocabularyProvenance, VocabularySnapshot},
    time::Millis,
    trace::{Action, Change, Control, Detail, Effect, Flag, Press, Reaction, Ref, TargetClass},
    vocab::{Matcher, TargetDesc, Vocabulary, VocabularyError},
};

fn vocabulary() -> Vocabulary {
    Vocabulary::from_document(json!({
        "version": 1,
        "apps": { "demo": { "project": 1, "host": "example.test", "audience": "users" } },
        "surfaces": [
            { "id": "demo.dynamic", "app": "demo", "route": "/item/:id", "name": "Item" },
            { "id": "demo.create", "app": "demo", "route": "/item/new", "name": "Create" },
        ],
        "features": [
            { "id": "chrome", "surface": "*", "app": "demo", "name": "Search", "matchers": { "text": ["Search"] } },
            { "id": "page", "surface": "demo.create", "name": "Search", "matchers": { "text": ["Search"] } },
            { "id": "structural", "surface": "demo.create", "name": "Save", "matchers": { "testid": ["save"] } },
            { "id": "templated", "surface": "demo.dynamic", "name": "Open", "matchers": { "aria_template": ["Open {document}"] } },
        ],
    }))
    .unwrap()
}

fn button(text: &str) -> TargetDesc {
    TargetDesc {
        tag: "button".into(),
        text: text.into(),
        ..TargetDesc::default()
    }
}

#[test]
fn an_extract_check_reports_only_what_the_source_does_not_say() {
    use spoiler_core::vocab::extract::{Rule, VocabularyExtract, check};
    let vocabulary = Vocabulary::from_document(json!({
        "version": 1,
        "apps": {
            "demo": { "project": 1, "host": "example.test", "audience": "users" },
            "other": { "project": 2, "host": "other.test", "audience": "users" },
        },
        "surfaces": [
            { "id": "demo.item", "app": "demo", "route": "/item/:id", "name": "Item", "source": "app/routes/item.$id.jsx:3" },
            { "id": "demo.gone", "app": "demo", "route": "/gone", "name": "Gone", "source": "app/routes/gone.jsx" },
            { "id": "other.home", "app": "other", "route": "/elsewhere", "name": "Other app" },
        ],
        "excluded_surfaces": [{ "app": "demo", "route": "/admin", "reason": "staff only" }],
        "features": [
            { "id": "chrome", "surface": "*", "app": "demo", "name": "Menu", "matchers": { "aria": ["Account menu"] } },
            { "id": "open", "surface": "demo.item", "name": "Open", "matchers": { "aria_template": ["Open {document}"] } },
            { "id": "renamed", "surface": "demo.item", "name": "Save", "matchers": { "text": ["Save"], "testid": ["save"] } },
            { "id": "elsewhere", "surface": "other.home", "name": "Not ours", "matchers": { "text": ["Nowhere"] } },
        ],
        "events": [{ "name": "item.saved", "app": "demo" }, { "name": "item.opened", "app": "demo" }],
    }))
    .unwrap();
    let extract: VocabularyExtract = serde_json::from_value(json!({
        "schema_version": 1, "kind": "vocabulary_extract", "app": "demo",
        "routes_from": "react-router-flat-routes",
        "files": [{ "name": "app/routes/item.$id.jsx", "sha256": "x" }],
        "routes": [
            { "route": "/item/:id", "params": ["id"], "file": "app/routes/item.$id.jsx", "renders": true, "page": true },
            { "route": "/admin", "file": "app/routes/admin.jsx", "renders": true, "page": true },
            { "route": "/new", "file": "app/routes/new.jsx", "renders": true, "page": true },
            { "route": "/", "file": "app/routes/_app.jsx", "renders": true, "page": false },
            { "route": "/api/ping", "file": "app/routes/api.ping.ts", "renders": false, "page": false },
        ],
        "literals": [
            { "id": "1", "kind": "aria", "value": "Account menu", "at": "app/root.jsx:9", "routes": ["/item/:id"] },
            { "id": "2", "kind": "aria", "value": "Open {name}", "template": true, "at": "app/routes/item.$id.jsx:5", "routes": ["/item/:id"] },
            { "id": "3", "kind": "prop", "value": "save", "at": "app/routes/item.$id.jsx:6", "routes": ["/item/:id"] },
        ],
        "events": [{ "at": "app/routes/item.$id.jsx:7", "call": "capture", "name": "item.opened" }],
    }))
    .unwrap();
    let found: Vec<(Rule, String)> = check(&vocabulary, &extract)
        .into_iter()
        .map(|finding| (finding.rule, finding.subject))
        .collect();
    assert_eq!(
        found,
        [
            // An excluded route, a layout and a resource route need no surface.
            (Rule::RouteWithoutSurface, "/new".into()),
            (Rule::SurfaceWithoutRoute, "demo.gone".into()),
            // A template matches whatever its placeholders are named; a prop can supply a testid.
            (Rule::MatcherNotInSource, "renamed".into()),
            // `file:line` is the file; another app's surfaces and features are not checked.
            (Rule::CitationNotInSource, "demo.gone".into()),
            (Rule::EventNotInSource, "item.saved".into()),
        ]
    );
}

#[test]
fn routes_and_features_respect_specificity_and_scope() {
    let vocabulary = vocabulary();
    let matcher = Matcher::new(&vocabulary);
    fn id(feature: Option<&spoiler_core::vocab::Feature>) -> Option<&str> {
        feature.map(|f| f.id.as_str())
    }
    assert_eq!(
        matcher.surface("demo", "/item/new").map(|s| s.id.as_str()),
        Some("demo.create")
    );
    assert_eq!(matcher.surface("other", "/item/new"), None);
    // The page's own control wins over chrome with the same label.
    assert_eq!(
        id(matcher.feature(Some("demo.create"), &[button("Search")])),
        Some("page")
    );
    // Chrome applies on its app's other pages; nothing applies on unknown pages.
    assert_eq!(
        id(matcher.feature(Some("demo.dynamic"), &[button("Search")])),
        Some("chrome")
    );
    assert_eq!(id(matcher.feature(None, &[button("Search")])), None);
    // Structural keys match up the ancestor chain and beat text on the target.
    let ancestor = TargetDesc {
        tag: "div".into(),
        testid: Some("save".into()),
        ..TargetDesc::default()
    };
    assert_eq!(
        id(matcher.feature(Some("demo.create"), &[button("Search"), ancestor])),
        Some("structural")
    );
    // Template placeholders do not span line breaks, as JavaScript's `.` does not.
    let aria = |label: &str| TargetDesc {
        aria: Some(label.into()),
        ..button("")
    };
    assert_eq!(
        id(matcher.feature(Some("demo.dynamic"), &[aria("Open Notebook")])),
        Some("templated")
    );
    assert_eq!(
        id(matcher.feature(Some("demo.dynamic"), &[aria("Open Notebook\u{2028}Draft")])),
        None
    );
}

#[test]
fn malformed_vocabularies_are_rejected_and_inert_chrome_is_reported() {
    let mut document = json!({ "version": 1, "apps": {}, "surfaces": [], "features": [
        { "id": "bad", "name": "bad", "surface": "*", "matchers": { "text": [123] } }
    ]});
    assert!(Vocabulary::from_document(document.clone()).is_err());
    document["features"][0]["matchers"]["text"] = json!(["Save"]);
    let vocabulary = Vocabulary::from_document(document.clone()).unwrap();
    assert_eq!(
        vocabulary.warnings().len(),
        1,
        "chrome without an app never matches"
    );
    document["terms"] = json!([{ "term": "Save", "means": false }]);
    assert!(Vocabulary::from_document(document).is_err());
    let snapshot = json!({ "schema_version": 999, "kind": "vocabulary_snapshot", "content_digest": "",
        "provenance": { "config_digest": "", "source_revision": null, "sources": [], "generator": {} },
        "vocabulary": { "version": 1, "apps": {}, "surfaces": [] } });
    assert!(Vocabulary::from_document(snapshot).is_err());
}

fn response(steps: Value, friction: Value) -> Value {
    json!({
        "reasoning": "evidence", "who": "user", "intent": { "text": "save", "confidence": 0.5 },
        "tasks": [], "steps": steps, "friction": friction,
        "outcome": { "success": "unknown", "text": "unclear" }, "summary": "Observed",
    })
}

fn click(number: u32, t_ms: f64) -> Action {
    let control = Control {
        node: 1,
        target: "button \"Save\"".into(),
        feature: None,
        element: None,
        reaction: None,
    };
    let press = Press {
        control,
        point: None,
        class: TargetClass::Interactive,
    };
    let mut action = Action::new(Detail::Click(press), Millis(t_ms), 1);
    action.reference = Ref(number);
    action
}

fn clicked() -> Vec<Action> {
    let mut action = click(1, 500.0);
    if let Detail::Click(press) = &mut action.detail {
        press.control.reaction = Some(Reaction {
            react_ms: Millis(50.0),
            settle_ms: Millis(50.0),
        });
    }
    vec![action]
}

#[test]
fn validation_holds_claims_to_the_trace() {
    let steps = json!([{ "refs": ["e1", "missing"], "action": "Save", "response": "50 ms, not 9 seconds or ١٢ ms", "feature": "invented", "outcome": "progressed" }]);
    let friction = json!([
        { "refs": ["e1"], "kind": "rage_click", "what": "Rage", "why": null, "severity": "blocking" },
        { "refs": ["missing"], "kind": "confusion_loop", "what": "Looped", "why": null, "severity": "degrading" },
    ]);
    let model = ModelSummary::from_value(response(steps, friction)).unwrap();
    let (summary, check) = validate(model, &clicked(), &vocabulary());
    // Unsupported signals and judgment items with no real evidence are both dropped.
    assert!(summary.friction.is_empty());
    assert_eq!(
        check.dropped_friction,
        ["rage_click: Rage", "confusion_loop: Looped"]
    );
    // Only ASCII digits are durations (and non-ASCII ones must not crash parsing).
    assert_eq!(summary.steps[0].unverified, ["9 seconds"]);
    assert_eq!(summary.steps[0].step.refs, ["e1"]);
    assert_eq!(summary.steps[0].step.feature, None);
    assert_eq!(check.unknown_features, ["invented"]);
    assert_eq!(check.bad_refs, ["missing", "missing"]);
    assert_eq!(
        check.bad_ref_ratio, 0.5,
        "two of four cited refs do not exist"
    );
}

/// Every flagged action is listed with the friction that explains it, counting only friction
/// that survived validation, so a signal the model passed over is still reported.
#[test]
fn every_signal_is_listed_with_the_friction_explaining_it() {
    let mut actions = Vec::new();
    for (index, flag) in [Flag::Dead, Flag::Slow].into_iter().enumerate() {
        let mut action = click(index as u32 + 1, 1000.0 * index as f64);
        action.flag(flag);
        actions.push(action);
    }
    let steps = json!([{ "refs": ["e1"], "action": "Clicked", "response": "", "feature": null, "outcome": "no_effect" }]);
    let friction = json!([
        { "refs": ["e1"], "kind": "rage_click", "what": "Rage", "why": null, "severity": "blocking" },
        { "refs": ["e1"], "kind": "dead_click", "what": "Dead", "why": null, "severity": "degrading" },
    ]);
    let model = ModelSummary::from_value(response(steps, friction)).unwrap();
    let (summary, check) = validate(model, &actions, &vocabulary());
    let explained: Vec<(String, &[usize])> = summary
        .signals
        .iter()
        .map(|s| (s.reference.to_string(), s.explained_by.as_slice()))
        .collect();
    // The rage item is dropped, so the dead-click item is friction 0.
    assert_eq!(
        explained,
        [("e1".to_owned(), &[0][..]), ("e2".to_owned(), &[][..])]
    );
    assert_eq!(check.unexplained_signals, [Ref(2)]);
    assert_eq!(check.uncited_gestures, [Ref(2)]);
}

#[test]
fn required_nullable_fields_must_be_present() {
    let steps =
        json!([{ "refs": ["e1"], "action": "Save", "response": "", "outcome": "progressed" }]);
    assert!(ModelSummary::from_value(response(steps, json!([]))).is_err());
}

#[test]
fn assessment_asks_again_when_refs_are_invented() {
    let steps = json!([{ "refs": ["e9"], "action": "Save", "response": "", "feature": null, "outcome": "progressed" }]);
    let content = response(steps, json!([])).to_string();
    match assess(&content, &clicked(), &vocabulary()) {
        Assessment::Rejected { reason } => assert!(reason.contains("e9"), "{reason}"),
        Assessment::Accepted { .. } => panic!("invented refs were accepted"),
    }
    match assess("not json", &clicked(), &vocabulary()) {
        Assessment::Rejected { reason } => assert!(reason.starts_with("That does not match")),
        Assessment::Accepted { .. } => panic!("malformed output was accepted"),
    }
}

#[test]
fn thresholds_default_and_override_per_field() {
    let mut document =
        json!({ "version": 1, "apps": {}, "surfaces": [], "thresholds": { "slow_ms": 5000 } });
    let vocabulary = Vocabulary::from_document(document.clone()).unwrap();
    assert_eq!(vocabulary.thresholds.slow_ms, Millis(5000.0));
    assert_eq!(
        vocabulary.thresholds.effect_window_ms,
        Millis(2000.0),
        "unspecified fields keep defaults"
    );
    // A misspelled threshold must fail loudly rather than silently keep the default.
    document["thresholds"] = json!({ "slow": 5000 });
    assert!(Vocabulary::from_document(document).is_err());
}

#[test]
fn a_data_attribute_value_may_contain_equals_signs() {
    let vocabulary = Vocabulary::from_document(json!({
        "version": 1,
        "apps": { "demo": { "project": 1, "host": "example.test", "audience": "users" } },
        "surfaces": [{ "id": "demo.page", "app": "demo", "route": "/page", "name": "Page" }],
        "features": [
            { "id": "org", "surface": "demo.page", "name": "Org", "matchers": { "data_attr": ["action=org=123"] } },
        ],
    }))
    .unwrap();
    let matcher = Matcher::new(&vocabulary);
    let with_action = |value: &str| TargetDesc {
        data: [("action".to_owned(), value.to_owned())]
            .into_iter()
            .collect(),
        ..button("")
    };
    let matched = |value: &str| {
        matcher
            .feature(Some("demo.page"), &[with_action(value)])
            .map(|f| f.id.clone())
    };
    assert_eq!(matched("org=123").as_deref(), Some("org"));
    assert_eq!(matched("org"), None);
}

#[test]
fn unsupported_vocab_versions_and_ambiguous_surfaces_are_rejected() {
    let mut document = json!({ "version": 2, "apps": {}, "surfaces": [] });
    assert!(matches!(
        Vocabulary::from_document(document.clone()),
        Err(VocabularyError::Version { found: 2, .. })
    ));
    document["version"] = json!(1);
    document["surfaces"] = json!([
        { "id": "duplicate", "app": "demo", "route": "/one", "name": "One" },
        { "id": "duplicate", "app": "demo", "route": "/two", "name": "Two" }
    ]);
    assert!(matches!(
        Vocabulary::from_document(document),
        Err(VocabularyError::DuplicateSurface(id)) if id == "duplicate"
    ));
}

#[test]
fn unknown_matcher_keys_cannot_be_silently_ignored() {
    let mut document = json!({ "version": 1, "apps": {}, "surfaces": [],
        "features": [{ "id": "save", "surface": "*", "name": "Save",
            "matchers": { "txt": ["Save"] } }]
    });
    assert!(matches!(
        Vocabulary::from_document(document.clone()),
        Err(VocabularyError::Invalid(_))
    ));
    document["features"][0]["matchers"] = json!({ "text_template": ["Save {name}"] });
    assert!(
        Vocabulary::from_document(document).is_ok(),
        "supported templates still load"
    );
}

#[test]
fn validation_keeps_only_existing_refs_in_accepted_items() {
    let steps = json!([
        { "refs": ["e1", "e99"], "action": "Save", "response": "", "feature": null, "outcome": "progressed" },
        { "refs": ["e99"], "action": "Invented", "response": "", "feature": null, "outcome": "progressed" }
    ]);
    let friction = json!([
        { "refs": ["e1", "e99"], "kind": "confusion_loop", "what": "Hesitation", "why": null, "severity": "degrading" },
        { "refs": ["e99"], "kind": "other", "what": "Imagined", "why": null, "severity": "degrading" }
    ]);
    let mut answer = response(steps, friction);
    answer["tasks"] = json!([
        { "goal": "Save", "refs": ["e1", "e99"], "outcome": "unclear", "obstacle": null },
        { "goal": "Imagined", "refs": ["e99"], "outcome": "unclear", "obstacle": null }
    ]);
    let (summary, check) = validate(
        ModelSummary::from_value(answer).unwrap(),
        &clicked(),
        &vocabulary(),
    );
    assert_eq!(check.bad_ref_ratio, 6.0 / 9.0);
    assert_eq!(summary.steps.len(), 1);
    assert_eq!(summary.steps[0].step.refs, ["e1"]);
    assert_eq!(summary.tasks.len(), 1);
    assert_eq!(summary.tasks[0].task.refs, ["e1"]);
    assert_eq!(summary.friction.len(), 1);
    assert_eq!(summary.friction[0].refs, ["e1"]);
}

#[test]
fn changes_before_navigation_still_count_as_persisted() {
    let mut action = click(1, 1000.0);
    let checked = |at| {
        Effect::seen(
            Millis(at),
            Change::State {
                attr: "checked".into(),
                before: Some("false".into()),
                after: Some("true".into()),
                node: 1,
            },
        )
    };
    // Tab::navigate appends Nav immediately; the gesture flush can append earlier DOM
    // changes afterward, so effect order is not necessarily timestamp order.
    action.effects = vec![
        Effect::seen(
            Millis(1200.0),
            Change::Nav {
                to: "/other".into(),
            },
        ),
        checked(1100.0),
        checked(1300.0),
    ];
    let mut answer = response(
        json!([{ "refs": ["e1"], "action": "Save", "response": "", "feature": null, "outcome": "progressed" }]),
        json!([]),
    );
    answer["tasks"] =
        json!([{ "goal": "Save", "refs": ["e1"], "outcome": "done", "obstacle": null }]);
    let (summary, _) = validate(
        ModelSummary::from_value(answer).unwrap(),
        &[action],
        &vocabulary(),
    );
    assert_eq!(summary.steps[0].changes, ["checked:false→true"]);
    assert_eq!(summary.tasks[0].changes, 1);
}

#[test]
fn task_activity_resumes_only_when_a_tab_is_visible() {
    let mut actions = vec![
        click(1, 0.0),
        Action::new(Detail::Hidden, Millis(1000.0), 1),
        Action::new(
            Detail::ConsoleError {
                message: "background error".into(),
            },
            Millis(2000.0),
            1,
        ),
        Action::new(Detail::Visible, Millis(7000.0), 1),
        click(5, 8000.0),
    ];
    for (index, action) in actions.iter_mut().enumerate() {
        action.reference = Ref(index as u32 + 1);
    }
    let mut answer = response(json!([]), json!([]));
    answer["tasks"] =
        json!([{ "goal": "Finish", "refs": ["e1", "e5"], "outcome": "done", "obstacle": null }]);
    let (summary, _) = validate(
        ModelSummary::from_value(answer).unwrap(),
        &actions,
        &vocabulary(),
    );
    assert_eq!(summary.tasks[0].active_s, 2.0);
}

#[test]
fn task_activity_tracks_visibility_independently_per_tab() {
    let mut actions = vec![
        click(1, 0.0),
        Action::new(Detail::Hidden, Millis(1000.0), 1),
        Action::new(Detail::Visible, Millis(2000.0), 2),
        Action::new(Detail::Hidden, Millis(3000.0), 2),
        Action::new(
            Detail::ConsoleError {
                message: "background error".into(),
            },
            Millis(6000.0),
            1,
        ),
        Action::new(Detail::Visible, Millis(7000.0), 1),
        click(7, 8000.0),
    ];
    for (index, action) in actions.iter_mut().enumerate() {
        action.reference = Ref(index as u32 + 1);
    }
    let mut answer = response(json!([]), json!([]));
    answer["tasks"] =
        json!([{ "goal": "Finish", "refs": ["e1", "e7"], "outcome": "done", "obstacle": null }]);
    let (summary, _) = validate(
        ModelSummary::from_value(answer).unwrap(),
        &actions,
        &vocabulary(),
    );
    assert_eq!(summary.tasks[0].active_s, 3.0);
}

#[test]
fn snapshot_digest_does_not_override_inner_vocabulary_version() {
    let mut vocabulary = vocabulary();
    vocabulary.version = 2;
    let snapshot = VocabularySnapshot::new(
        vocabulary,
        VocabularyProvenance {
            config_digest: String::new(),
            source_revision: None,
            sources: vec![],
            generator: json!({}),
        },
    );
    assert!(matches!(
        Vocabulary::from_document(serde_json::to_value(snapshot).unwrap()),
        Err(VocabularyError::Version { found: 2, .. })
    ));
}

#[test]
fn task_visibility_before_first_citation_still_applies() {
    let mut actions = vec![
        Action::new(Detail::Hidden, Millis(0.0), 1),
        Action::new(
            Detail::ConsoleError {
                message: "background error".into(),
            },
            Millis(1000.0),
            1,
        ),
        Action::new(Detail::Visible, Millis(3000.0), 1),
        click(4, 4000.0),
    ];
    for (index, action) in actions.iter_mut().enumerate() {
        action.reference = Ref(index as u32 + 1);
    }
    let mut answer = response(json!([]), json!([]));
    answer["tasks"] =
        json!([{ "goal": "Resume", "refs": ["e2", "e4"], "outcome": "done", "obstacle": null }]);
    let (summary, _) = validate(
        ModelSummary::from_value(answer).unwrap(),
        &actions,
        &vocabulary(),
    );
    assert_eq!(summary.tasks[0].active_s, 1.0);
}

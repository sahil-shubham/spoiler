#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use spoiler_core::{
    artifact::{RecordingArtifact, RecordingSource},
    recording::{DecodeError, Event, Limits, Reading, Recording, decode_with, rrweb::Signal},
    replay::Mirror,
    time::{Millis, Timestamp},
    trace::{Action, ActionKind, Change, Compilation, Detail, Flag, Ref, compile, to_tsv},
    vocab::{Matcher, Vocabulary},
};
use std::io::Write;

fn vocabulary() -> Vocabulary {
    Vocabulary::from_document(json!({
        "version": 1,
        "apps": { "demo": { "project": 1, "host": "example.test", "audience": "users" } },
        "surfaces": [{ "id": "demo.page", "app": "demo", "route": "/page", "name": "Page" }],
    }))
    .unwrap()
}

fn event(win: &str, at: f64, kind: u32, data: Value) -> Event {
    Event {
        kind,
        timestamp: Timestamp(at),
        data,
        win: win.into(),
    }
}

/// A document with one button (id 3) labelled by text node 4.
fn page(label: &str) -> Value {
    json!({ "node": { "id": 1, "type": 0, "childNodes": [
        { "id": 2, "type": 2, "tagName": "body", "childNodes": [
            { "id": 3, "type": 2, "tagName": "button", "attributes": {}, "childNodes": [
                { "id": 4, "type": 3, "textContent": label }
            ]}
        ]}
    ]}})
}

fn click(id: i64) -> Value {
    json!({ "source": 2, "type": 2, "id": id })
}

fn compile_events(events: &[Event]) -> Compilation {
    let vocabulary = vocabulary();
    let recording = Recording::from_events(events).unwrap();
    compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap()
}

fn run(events: &[Event]) -> Vec<Action> {
    compile_events(events).actions
}

#[test]
fn trace_times_round_like_javascript_to_fixed() {
    let mut action = Action::new(Detail::Visible, Millis(64_250.0), 1);
    action.reference = Ref(1);
    let row = to_tsv(&[action]).lines().nth(1).unwrap().to_owned();
    assert_eq!(row.split('\t').nth(1), Some("64.3"));
}

#[test]
fn visible_text_uses_ecmascript_whitespace() {
    let events = [
        event("a", 0.0, 2, page("\u{feff}Save\u{a0}now")),
        event("a", 200.0, 3, click(3)),
        event(
            "a",
            230.0,
            3,
            json!({ "source": 0, "texts": [{ "id": 4, "value": "\u{feff}Saved\u{85}value" }] }),
        ),
    ];
    let actions = run(&events);
    assert_eq!(actions[0].target().as_deref(), Some("button \"Save now\""));
    let Change::Text { before, text, .. } = &actions[0].effects[0].change else {
        panic!("expected a text change, got {:?}", actions[0].effects);
    };
    assert_eq!(before.as_deref(), Some("Save now"));
    assert_eq!(text, "Saved\u{85}value"); // NEL is not whitespace in JavaScript
    assert_eq!(
        actions[0].reaction().map(|r| r.react_ms),
        Some(Millis(30.0))
    );
}

#[test]
fn duplicate_events_are_dropped_per_tab_only() {
    let events = [
        event("a", 0.0, 2, page("First")),
        event("b", 0.0, 2, page("Second")),
        event("a", 200.0, 3, click(3)),
        event("a", 200.0, 3, click(3)),
        event("b", 200.0, 3, click(3)),
        event(
            "a",
            250.0,
            3,
            json!({ "source": 0, "texts": [{ "id": 4, "value": "Saved" }] }),
        ),
    ];
    let actions = run(&events);
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].target().as_deref(), Some("button \"First\""));
    assert_eq!(actions[1].target().as_deref(), Some("button \"Second\""));
    assert_eq!((actions[0].win, actions[1].win), (1, 2));
    assert!(matches!(&actions[0].effects[0].change, Change::Text { text, .. } if text == "Saved"));
    assert_eq!(actions[1].flags, [Flag::Unresponsive]);
}

#[test]
fn console_errors_keep_leading_whitespace() {
    let console = json!({ "plugin": "rrweb/console@1", "payload": { "level": "error", "payload": ["  boom", 1.0, null] } });
    let actions = run(&[event("a", 0.0, 2, page("x")), event("a", 10.0, 6, console)]);
    assert_eq!(actions[0].kind(), ActionKind::ConsoleError);
    // String(1.0) is "1" and String(null) is "null"; runs collapse but ends are kept.
    assert_eq!(actions[0].target().as_deref(), Some(" boom 1 null"));
}

#[test]
fn a_bare_query_string_is_not_part_of_the_path() {
    let actions = run(&[event(
        "a",
        0.0,
        4,
        json!({ "href": "https://example.test/page?" }),
    )]);
    assert_eq!(actions[0].path.as_deref(), Some("/page"));
    assert_eq!(actions[0].surface.as_deref(), Some("demo.page"));
}

fn mobile_element(id: i64, tag: &str, rect: [i64; 4], label: &str) -> Value {
    let [x, y, width, height] = rect;
    let children = if label.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "id": id + 1000, "type": 3, "textContent": label })]
    };
    json!({
        "id": id, "type": 2, "tagName": tag,
        "attributes": {
            "data-spoiler-mobile-x": x.to_string(),
            "data-spoiler-mobile-y": y.to_string(),
            "data-spoiler-mobile-width": width.to_string(),
            "data-spoiler-mobile-height": height.to_string(),
        },
        "childNodes": children,
    })
}

fn mobile_page(children: Vec<Value>) -> Value {
    json!({ "node": { "id": 1, "type": 0, "childNodes": [
        { "id": 2, "type": 2, "tagName": "body", "childNodes": children }
    ]}})
}

fn touch(at: f64, kind: i32, id: i64, x: i64, y: i64) -> Event {
    event(
        "a",
        at,
        3,
        json!({ "source": 2, "type": kind, "id": id, "x": x, "y": y }),
    )
}

fn mobile_compile(events: &[Event]) -> Compilation {
    let vocabulary = Vocabulary::from_document(json!({
        "version": 1,
        "apps": { "demo": { "project": 1, "host": "example.test", "audience": "users" } },
        "surfaces": [
            { "id": "demo.home", "app": "demo", "route": "Home", "name": "Home" },
            { "id": "demo.settings", "app": "demo", "route": "Settings", "name": "Settings" }
        ],
        "features": [
            { "id": "save", "surface": "demo.home", "name": "Save",
              "matchers": { "text": ["Save"] } }
        ]
    }))
    .unwrap();
    let recording = Recording::from_events(events).unwrap();
    compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap()
}
fn ios_full_button(label: &str) -> Value {
    json!({ "initialOffset":{"top":0,"left":0}, "wireframes": [
        { "id": 10000000, "type": "input", "inputType": "button", "disabled": false,
          "x": 100, "y": 100, "width": 120, "height": 45, "value": label }
    ] })
}

#[test]
fn ios_successive_full_frames_report_tap_text_change_but_not_later_idle_frames() {
    let compiled = mobile_compile(&[
        event("a", 0.0, 4, json!({ "href": "Home" })),
        event("a", 1.0, 2, ios_full_button("Save")),
        touch(100.0, 7, 0, 130, 120),
        touch(120.0, 9, 0, 130, 120),
        event("a", 145.0, 2, ios_full_button("Saved")),
        event("a", 3300.0, 2, ios_full_button("Done")),
    ]);
    let click = compiled
        .actions
        .iter()
        .find(|a| a.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(click.target().as_deref(), Some("button \"Save\""));
    assert_eq!(click.path.as_deref(), Some("Home"));
    assert_eq!(click.surface.as_deref(), Some("demo.home"));
    assert!(
        matches!(
            click.effects.as_slice(),
            [spoiler_core::trace::Effect {
                change: Change::Text { before: Some(before), text, .. },
                ..
            }] if before == "Save" && text == "Saved"
        ),
        "{:?}",
        click.effects
    );
    assert_eq!(click.reaction().map(|r| r.react_ms), Some(Millis(25.0)));
    assert!(click.flags.is_empty(), "{:?}", click.flags);
}

#[test]
fn ios_screenshot_only_full_frames_do_not_manufacture_a_pixel_effect() {
    let screenshot = |id, image| {
        json!({ "initialOffset":{"top":0,"left":0}, "wireframes": [
        { "id": id, "type": "screenshot", "x": 0, "y": 0,
          "width": 390, "height": 850, "base64": image }
    ] })
    };
    let compiled = mobile_compile(&[
        event("a", 0.0, 4, json!({ "href": "Home" })),
        event("a", 1.0, 2, screenshot(10000000, "before")),
        touch(100.0, 7, 0, 120, 340),
        touch(120.0, 9, 0, 120, 340),
        event("a", 150.0, 2, screenshot(10000001, "after")),
    ]);
    let click = compiled
        .actions
        .iter()
        .find(|a| a.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(click.target().as_deref(), Some("screen (120,340)"));
    assert!(click.effects.is_empty(), "{:?}", click.effects);
    assert!(click.reaction().is_none());
    assert!(click.flags.is_empty());
    assert!(compiled.coverage.screenshot_only);
}

#[test]
fn ios_navigation_on_touch_down_keeps_tap_on_source_surface() {
    let compiled = mobile_compile(&[
        event("a", 0.0, 4, json!({ "href": "Home" })),
        event("a", 1.0, 2, ios_full_button("Save")),
        touch(100.0, 7, 0, 130, 120),
        event("a", 110.0, 4, json!({ "href": "Settings" })),
        event("a", 115.0, 2, ios_full_button("Continue")),
        touch(125.0, 9, 0, 130, 120),
    ]);
    let click = compiled
        .actions
        .iter()
        .find(|a| a.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(click.target().as_deref(), Some("button \"Save\""));
    assert_eq!(click.path.as_deref(), Some("Home"));
    assert_eq!(click.surface.as_deref(), Some("demo.home"));
    assert!(click.effects.iter().any(|effect| matches!(
        &effect.change,
        Change::Nav { to } if to == "Settings"
    )));
    assert!(
        compiled
            .actions
            .iter()
            .any(|action| action.kind() == ActionKind::Nav
                && action.path.as_deref() == Some("Settings")
                && action.surface.as_deref() == Some("demo.settings"))
    );
}

#[test]
fn native_tap_hits_absolute_nested_button_over_screenshot_chrome_and_inert_overlay() {
    let mut screenshot = mobile_element(3, "img", [0, 0, 390, 850], "");
    screenshot["attributes"]["data-posthog-screenshot"] = json!("true");
    let mut chrome = mobile_element(4, "div", [0, 0, 390, 200], "System overlay");
    chrome["attributes"]["data-spoiler-mobile-chrome"] = json!("true");
    let mut card = mobile_element(5, "div", [80, 90, 220, 200], "");
    let mut button = mobile_element(6, "button", [105, 125, 110, 48], "Save");
    button["attributes"]["data-testid"] = json!("save");
    card["childNodes"].as_array_mut().unwrap().push(button);
    let overlay = mobile_element(7, "div", [95, 110, 140, 75], "Decorative overlay");
    let events = [
        event("a", 0.0, 4, json!({ "href": "Home" })),
        event(
            "a",
            1.0,
            2,
            mobile_page(vec![screenshot, chrome, card, overlay]),
        ),
        touch(100.0, 7, 0, 112, 145),
        // Native controls may respond on touch-down, before the touch-end completes the tap.
        event(
            "a",
            110.0,
            3,
            json!({ "source": 0, "texts": [{ "id": 1006, "value": "Saved" }] }),
        ),
        touch(120.0, 9, 0, 112, 145),
        event("a", 170.0, 4, json!({ "href": "Settings" })),
    ];
    let compiled = mobile_compile(&events);
    let clicks: Vec<_> = compiled
        .actions
        .iter()
        .filter(|action| action.kind() == ActionKind::Click)
        .collect();
    assert_eq!(clicks.len(), 1, "touch start and end are one click");
    let click = clicks[0];
    let Detail::Click(press) = &click.detail else {
        unreachable!();
    };
    assert_eq!(press.control.node, 6);
    assert_eq!(press.control.target, "button[save] \"Save\"");
    assert_eq!(press.control.feature.as_deref(), Some("save"));
    assert!(
        press
            .control
            .element
            .as_ref()
            .unwrap()
            .element
            .data
            .keys()
            .all(|name| !name.starts_with("spoiler-mobile-"))
    );
    assert_eq!(click.path.as_deref(), Some("Home"));
    assert_eq!(click.surface.as_deref(), Some("demo.home"));
    assert_eq!(click.reaction().map(|r| r.react_ms), Some(Millis(-10.0)));
    assert!(click.flags.is_empty(), "{:?}", click.flags);
    assert!(
        click
            .effects
            .iter()
            .any(|effect| matches!(&effect.change, Change::Text { text, .. } if text == "Saved"))
    );
    assert!(
        click
            .effects
            .iter()
            .any(|effect| matches!(&effect.change, Change::Nav { to } if to == "Settings"))
    );
    assert!(compiled.actions.iter().any(|action| {
        action.kind() == ActionKind::Nav
            && action.path.as_deref() == Some("Settings")
            && action.surface.as_deref() == Some("demo.settings")
    }));
    assert!(
        !compiled.coverage.screenshot_only,
        "a labelled wireframe is available"
    );
}

#[test]
fn screenshot_only_touch_uses_coordinates_without_false_dead_or_unresponsive_flag() {
    let mut image = mobile_element(3, "img", [0, 0, 390, 850], "");
    image["attributes"]["data-posthog-screenshot"] = json!("true");
    let mut chrome = mobile_element(4, "div", [0, 0, 390, 75], "12:30");
    chrome["attributes"]["data-spoiler-mobile-chrome"] = json!("true");
    let compiled = mobile_compile(&[
        event("a", 0.0, 4, json!({ "href": "Home" })),
        event("a", 1.0, 2, mobile_page(vec![image, chrome])),
        touch(100.0, 7, 5, 120, 340),
        touch(125.0, 9, 5, 121, 340),
    ]);
    assert!(compiled.coverage.screenshot_only);
    assert_eq!(compiled.coverage.opaque_mounts["mobile_screenshot"], 1);
    let clicks: Vec<_> = compiled
        .actions
        .iter()
        .filter(|action| action.kind() == ActionKind::Click)
        .collect();
    assert_eq!(clicks.len(), 1);
    let Detail::Click(press) = &clicks[0].detail else {
        unreachable!();
    };
    assert_eq!(press.control.target, "screen (120,340)");
    assert!(press.control.element.is_none());
    assert!(press.control.feature.is_none());
    assert_eq!(press.class, spoiler_core::trace::TargetClass::Unresolved);
    assert!(clicks[0].flags.is_empty(), "{:?}", clicks[0].flags);
}

#[test]
fn native_hit_chooses_later_interactive_overlap_and_ignores_hidden_descendants() {
    let first = mobile_element(3, "button", [100, 100, 100, 70], "First");
    let mut second = mobile_element(4, "button", [100, 100, 100, 70], "Second");
    let mut hidden = mobile_element(5, "button", [105, 105, 60, 50], "Hidden");
    hidden["attributes"]["hidden"] = json!(true);
    second["childNodes"].as_array_mut().unwrap().push(hidden);
    let note = mobile_element(6, "div", [250, 100, 100, 70], "Note");
    let compiled = mobile_compile(&[
        event("a", 0.0, 2, mobile_page(vec![first, second, note])),
        touch(100.0, 7, 0, 120, 120),
        touch(125.0, 9, 0, 120, 120),
        touch(500.0, 7, 0, 280, 120),
        touch(525.0, 9, 0, 280, 120),
    ]);
    let clicks: Vec<_> = compiled
        .actions
        .iter()
        .filter(|action| action.kind() == ActionKind::Click)
        .collect();
    assert_eq!(clicks.len(), 2);
    assert_eq!(clicks[0].target().as_deref(), Some("button \"Second\""));
    assert_eq!(clicks[0].flags, [Flag::Unresponsive]);
    assert_eq!(clicks[1].target().as_deref(), Some("div \"Note\""));
    assert_eq!(clicks[1].flags, [Flag::Dead]);
}

#[test]
fn blank_wireframe_is_a_dead_tap_but_screenshot_wrapper_is_not_a_named_target() {
    let blank = mobile_compile(&[
        event(
            "a",
            0.0,
            2,
            mobile_page(vec![mobile_element(3, "div", [0, 0, 390, 850], "")]),
        ),
        touch(100.0, 7, 0, 100, 200),
        touch(125.0, 9, 0, 100, 200),
    ]);
    let blank_click = blank
        .actions
        .iter()
        .find(|action| action.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(blank_click.target().as_deref(), Some("div"));
    assert_eq!(blank_click.flags, [Flag::Dead]);

    let mut wrapper = mobile_element(3, "div", [0, 0, 390, 850], "");
    let mut image = mobile_element(4, "img", [0, 0, 390, 850], "");
    image["attributes"]["data-posthog-screenshot"] = json!("true");
    wrapper["childNodes"].as_array_mut().unwrap().push(image);
    let screenshot = mobile_compile(&[
        event("a", 0.0, 2, mobile_page(vec![wrapper])),
        touch(100.0, 7, 0, 100, 200),
        touch(125.0, 9, 0, 100, 200),
    ]);
    let screenshot_click = screenshot
        .actions
        .iter()
        .find(|action| action.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(
        screenshot_click.target().as_deref(),
        Some("screen (100,200)")
    );
    assert!(screenshot.coverage.screenshot_only);
    assert!(screenshot_click.flags.is_empty());
}

#[test]
fn screenshot_added_by_mutation_enables_coordinate_taps_and_screenshot_only_coverage() {
    let mut image = mobile_element(3, "img", [0, 0, 390, 850], "");
    image["attributes"]["data-posthog-screenshot"] = json!("true");
    let compiled = mobile_compile(&[
        event("a", 0.0, 2, mobile_page(vec![])),
        event(
            "a",
            20.0,
            3,
            json!({ "source": 0, "adds": [
                { "parentId": 2, "nextId": null, "node": image }
            ] }),
        ),
        touch(100.0, 7, 0, 80, 200),
        touch(120.0, 9, 0, 80, 200),
    ]);
    assert_eq!(compiled.coverage.opaque_mounts["mobile_screenshot"], 1);
    assert!(compiled.coverage.screenshot_only);
    let click = compiled
        .actions
        .iter()
        .find(|action| action.kind() == ActionKind::Click)
        .unwrap();
    assert_eq!(click.target().as_deref(), Some("screen (80,200)"));
}

#[test]
fn native_swipes_do_not_create_clicks_and_web_touch_ends_remain_unchanged() {
    let mobile = mobile_compile(&[
        event(
            "a",
            0.0,
            2,
            mobile_page(vec![mobile_element(3, "button", [10, 20, 100, 40], "Save")]),
        ),
        touch(100.0, 7, 0, 20, 30),
        touch(140.0, 9, 0, 180, 290),
    ]);
    assert!(!mobile.actions.iter().any(|action| action.kind().is_click()));
    let web = run(&[
        event("a", 0.0, 2, page("Save")),
        event(
            "a",
            100.0,
            3,
            json!({
                "source": 2, "type": 7, "id": 3, "x": 20, "y": 30, "pointerType": 2
            }),
        ),
        event(
            "a",
            140.0,
            3,
            json!({
                "source": 2, "type": 9, "id": 3, "x": 20, "y": 30, "pointerType": 2
            }),
        ),
        event("a", 150.0, 3, click(3)),
    ]);
    let clicks: Vec<_> = web
        .iter()
        .filter(|action| action.kind() == ActionKind::Click)
        .collect();
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0].target().as_deref(), Some("button \"Save\""));
}

/// The typed signal of a single event.
fn signal(event: Event) -> Signal {
    let recording = Recording::from_events(&[event]).unwrap();
    let event = recording.events().next().unwrap();
    match recording.read(&event).unwrap() {
        Reading::Signal(signal) => signal,
        other => panic!("not a signal: {other:?}"),
    }
}

#[test]
fn additions_wait_for_their_next_sibling() {
    let mut mirror = Mirror::default();
    let Signal::FullSnapshot(root) = signal(event("a", 0.0, 2, page("Start"))) else {
        panic!("not a snapshot");
    };
    mirror.reset(root);
    let adds = json!({ "source": 0, "adds": [
        { "parentId": 2, "nextId": 8, "node": { "id": 7, "type": 3, "textContent": "first" } },
        { "parentId": 2, "nextId": null, "node": { "id": 8, "type": 3, "textContent": "second" } },
    ]});
    let Signal::Mutation(mutation) = signal(event("a", 1.0, 3, adds)) else {
        panic!("not a mutation");
    };
    mirror.apply(mutation);
    assert_eq!(mirror.get(2).unwrap().children, [3, 7, 8]);
    assert_eq!(mirror.visible_text(2), "Start first second");
}

#[test]
fn a_null_mutation_field_makes_the_event_not_a_mutation() {
    let recording =
        Recording::from_events(&[event("a", 0.0, 3, json!({ "source": 0, "adds": null }))])
            .unwrap();
    let event = recording.events().next().unwrap();
    assert!(matches!(
        recording.read(&event).unwrap(),
        Reading::Malformed(_)
    ));
}

#[test]
fn dom_depth_is_not_limited_by_the_json_parser() {
    // 300 nested elements is 600 JSON levels, well past serde_json's default of 128.
    let mut node = json!({ "id": 1000, "type": 3, "textContent": "deep" });
    for id in (10..310).rev() {
        node = json!({ "id": id, "type": 2, "tagName": "div", "attributes": {}, "childNodes": [node] });
    }
    let snapshot = json!({ "node": { "id": 1, "type": 0, "childNodes": [
        { "id": 2, "type": 2, "tagName": "body", "childNodes": [node] }
    ]}});
    let actions = run(&[
        event("a", 0.0, 2, snapshot),
        event("a", 100.0, 3, click(1000)),
    ]);
    assert_eq!(actions[0].kind(), ActionKind::Click);
    assert!(
        actions[0].target().as_deref().unwrap().contains("deep"),
        "{:?}",
        actions[0].target()
    );
}

#[test]
fn coverage_reports_what_the_trace_cannot_see() {
    let mut snapshot = page("x");
    snapshot["node"]["childNodes"][0]["childNodes"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({ "id": 20, "type": 2, "tagName": "iframe", "attributes": {}, "childNodes": [] }),
            json!({ "id": 21, "type": 2, "tagName": "div", "isShadowHost": true, "attributes": {}, "childNodes": [] }),
        ]);
    let compilation = compile_events(&[
        event("a", 0.0, 2, snapshot),
        event("a", 5.0, 3, json!({ "source": 1, "positions": [] })),
        event("a", 5.0, 3, json!({ "source": 1, "positions": [] })),
        event("a", 6.0, 3, json!({ "source": 9, "id": 3, "commands": [] })),
        event("a", 7.0, 5, json!({ "tag": "app-specific", "payload": {} })),
    ]);
    let coverage = compilation.coverage;
    assert_eq!((coverage.events, coverage.duplicates), (5, 1));
    assert_eq!(coverage.uninterpreted["mouse_move"], 1);
    assert_eq!(coverage.uninterpreted["canvas_mutation"], 1);
    assert_eq!(coverage.uninterpreted["custom:app-specific"], 1);
    assert_eq!(coverage.opaque_mounts["iframe"], 1);
    assert_eq!(coverage.opaque_mounts["shadow_root"], 1);
}

#[test]
fn bare_snapshot_events_take_their_tab_from_window_id_only() {
    // The player reads `window_id ?? windowId` for batches but `windowId` for bare events.
    let bare = json!({ "type": 4, "timestamp": 1, "data": { "href": "https://example.test/page" }, "window_id": "ignored", "windowId": "tab" });
    let batch = json!({ "window_id": null, "windowId": "other", "data": [{ "type": 4, "timestamp": 2, "data": { "href": "https://example.test/page" } }] });
    let recording =
        Recording::from_snapshot_bodies(&[bare.to_string(), batch.to_string()], Limits::default())
            .unwrap();
    let tabs: Vec<&str> = recording.events().map(|e| e.win).collect();
    assert_eq!(tabs, ["tab", "other"]);
}

#[test]
fn invalid_snapshot_line_does_not_discard_neighboring_actions() {
    let body = [
        r#"["tab", {"type": 3, "timestamp": "#.to_owned(),
        json!(["tab", { "type": 2, "timestamp": 1, "data": page("Save") }]).to_string(),
        r#"["tab", {"type": 3, "timestamp": "#.to_owned(),
        json!(["tab", { "type": 3, "timestamp": 2, "data": click(3) }]).to_string(),
    ]
    .join("\n");
    let decoded = decode_with(body.as_bytes(), Limits::default()).unwrap();
    let recording = Recording::from_snapshot_bodies(&[body], Limits::default()).unwrap();
    let vocabulary = vocabulary();
    let compiled = compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap();
    assert_eq!(compiled.coverage.malformed["snapshot_line"], 2);
    assert_eq!(compiled.coverage.events, 4);
    assert_eq!(compiled.actions.len(), 1);
    assert_eq!(
        compiled.actions[0].target().as_deref(),
        Some("button \"Save\"")
    );
    assert_eq!(decoded.malformed_snapshot_lines(), 2);
    let empty = Recording::from_snapshot_bodies(&["{".into()], Limits::default()).unwrap();
    let coverage = compile(&empty, &Matcher::new(&vocabulary), "demo")
        .unwrap()
        .coverage;
    assert_eq!(coverage.events, 1);
    assert_eq!(coverage.malformed["snapshot_line"], 1);
}

#[test]
fn corrupt_compressed_fields_count_as_malformed_without_losing_valid_events() {
    let bad_gzip: String = [0x1f, 0x8b, 0x00].into_iter().map(char::from).collect();
    let bad_zstd: String = [0x28, 0xb5, 0x2f, 0xfd, 0x00]
        .into_iter()
        .map(char::from)
        .collect();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(b"not json").unwrap();
    let bad_json: String = gzip.finish().unwrap().into_iter().map(char::from).collect();
    let body = [
        json!(["tab", { "type": 2, "timestamp": 1, "cv": "2024-10", "data": bad_gzip }]),
        json!(["tab", { "type": 2, "timestamp": 2, "data": page("Save") }]),
        json!(["tab", { "type": 3, "timestamp": 3, "cv": "2024-10", "data": { "source": 0, "adds": bad_zstd } }]),
        json!(["tab", { "type": 3, "timestamp": 4, "cv": "2024-10", "data": { "source": 0, "texts": bad_json } }]),
        json!(["tab", { "type": 3, "timestamp": 4.5, "cv": "2024-10", "data": { "source": 8, "adds": bad_zstd } }]),
        json!(["tab", { "type": 3, "timestamp": 5, "data": click(3) }]),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let recording = Recording::from_snapshot_bodies(&[body], Limits::default()).unwrap();
    let vocabulary = vocabulary();
    let compiled = compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap();
    assert_eq!(compiled.coverage.malformed["full_snapshot"], 1);
    assert_eq!(compiled.coverage.malformed["mutation"], 2);
    assert_eq!(compiled.coverage.malformed["style_sheet_rule"], 1);
    assert_eq!(compiled.actions.len(), 1);
    assert_eq!(
        compiled.actions[0].target().as_deref(),
        Some("button \"Save\"")
    );
    // Serializing the fetched recording must not fail merely because a field is corrupt.
    let artifact = RecordingArtifact::new(
        RecordingSource::File {
            sha256: String::new(),
        },
        recording,
    );
    let bytes = serde_json::to_vec(&artifact).unwrap();
    let roundtrip = decode_with(&bytes, Limits::default()).unwrap();
    let roundtrip = compile(&roundtrip, &Matcher::new(&vocabulary), "demo").unwrap();
    assert_eq!(roundtrip.coverage.malformed, compiled.coverage.malformed);
    assert_eq!(roundtrip.actions.len(), 1);
}

#[test]
fn unknown_compression_versions_do_not_decompress_fields() {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&serde_json::to_vec(&page("Never decode")).unwrap())
        .unwrap();
    let packed_full: String = gzip.finish().unwrap().into_iter().map(char::from).collect();
    let packed_mutation: String = zstd::encode_all(b"[]".as_slice(), 0)
        .unwrap()
        .into_iter()
        .map(char::from)
        .collect();
    let body = [
        json!(["tab", { "type": 2, "timestamp": 1, "cv": "future", "data": packed_full }]),
        json!(["tab", { "type": 3, "timestamp": 2, "cv": null, "data": { "source": 0, "adds": packed_mutation } }]),
        json!(["tab", { "type": 3, "timestamp": 2.5, "cv": "future", "data": { "source": 8, "adds": packed_mutation } }]),
        json!(["tab", { "type": 2, "timestamp": 3, "data": page("Save") }]),
        json!(["tab", { "type": 3, "timestamp": 4, "data": click(3) }]),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let recording = Recording::from_snapshot_bodies(&[body], Limits::default()).unwrap();
    let vocabulary = vocabulary();
    let compiled = compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap();
    assert_eq!(compiled.coverage.malformed["full_snapshot"], 1);
    assert_eq!(compiled.coverage.malformed["mutation"], 1);
    assert_eq!(compiled.coverage.malformed["style_sheet_rule"], 1);
    assert_eq!(
        compiled.actions[0].target().as_deref(),
        Some("button \"Save\"")
    );
}

#[test]
fn falsey_window_ids_coalesce_across_tuple_batch_and_bare_events() {
    let event = |timestamp| json!({ "type": 4, "timestamp": timestamp, "data": { "href": "https://example.test/page" } });
    let lines = [
        json!([null, event(1)]),
        json!(["", event(2)]),
        json!({ "data": [event(3)] }),
        json!({ "window_id": null, "data": [event(4)] }),
        event(5),
        json!({ "windowId": "", "type": 4, "timestamp": 6, "data": { "href": "https://example.test/page" } }),
        json!([9, event(7)]),
        json!({ "windowId": 9, "data": [event(8)] }),
        json!({ "windowId": 9, "type": 4, "timestamp": 9, "data": { "href": "https://example.test/page" } }),
    ];
    let recording = Recording::from_snapshot_bodies(
        &[lines
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")],
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        recording
            .events()
            .map(|event| event.win)
            .collect::<Vec<_>>(),
        ["", "", "", "", "", "", "9", "9", "9"]
    );
}

#[test]
fn compressed_fields_and_separate_bodies_decode() {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&serde_json::to_vec(&page("Compressed")).unwrap())
        .unwrap();
    let packed: String = gzip.finish().unwrap().into_iter().map(char::from).collect();
    let full =
        json!(["tab", { "type": 2, "timestamp": 1, "cv": "2024-10", "data": packed }]).to_string();
    let adds = json!([{ "parentId": 2, "nextId": null, "node": { "id": 5, "type": 3, "textContent": "Added" } }]);
    let packed_adds: String = zstd::encode_all(serde_json::to_vec(&adds).unwrap().as_slice(), 0)
        .unwrap()
        .into_iter()
        .map(char::from)
        .collect();
    let mutation = json!({ "windowId": "tab", "data": [
        { "type": 3, "timestamp": 2, "cv": "2024-10", "data": { "source": 0, "adds": packed_adds } }
    ]})
    .to_string();
    let recording = Recording::from_snapshot_bodies(&[full, mutation], Limits::default()).unwrap();
    assert_eq!(recording.len(), 2);
    let events: Vec<_> = recording.events().collect();
    let snapshot: Value =
        serde_json::from_str(&recording.expanded_data(&events[0]).unwrap()).unwrap();
    let added: Value = serde_json::from_str(&recording.expanded_data(&events[1]).unwrap()).unwrap();
    assert_eq!(snapshot["node"]["id"], 1);
    assert_eq!(added["adds"][0]["node"]["textContent"], "Added");
    // Compiled lazily, the compressed fields expand when read.
    let vocabulary = vocabulary();
    let compiled = compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap();
    assert!(compiled.coverage.malformed.is_empty());
}

#[test]
fn decompression_is_bounded_by_the_recording_budget() {
    // 16 MiB of zeros compresses to a few KiB: a field that small must not be trusted.
    let bomb: String = zstd::encode_all(vec![b' '; 16 << 20].as_slice(), 19)
        .unwrap()
        .into_iter()
        .map(char::from)
        .collect();
    let line = json!(["tab", { "type": 3, "timestamp": 1, "cv": "2024-10", "data": { "source": 0, "adds": bomb } }]).to_string();
    let limits = Limits { max_bytes: 1 << 20 };
    // Decompression happens when the event is read, so the budget trips during compilation.
    let recording = Recording::from_snapshot_bodies(&[line], limits).unwrap();
    let vocabulary = vocabulary();
    let error = compile(&recording, &Matcher::new(&vocabulary), "demo").unwrap_err();
    assert!(
        matches!(error, DecodeError::TooLarge { max_bytes } if max_bytes == 1 << 20),
        "{error}"
    );
    let whole = zstd::encode_all(vec![b' '; 4 << 20].as_slice(), 19).unwrap();
    assert!(matches!(
        decode_with(&whole, limits),
        Err(DecodeError::TooLarge { .. })
    ));
    assert!(decode_with(&whole, Limits { max_bytes: 8 << 20 }).is_ok());
}

#[test]
fn grid_labels_and_row_keys_come_from_the_vocabulary() {
    // Stage | Name, one keyed row. Without rules the first column labels the row.
    let cell = |id: i64, col: &str, text: &str, text_id: i64| {
        json!({ "id": id, "type": 2, "tagName": "td", "attributes": { "data-col": col }, "childNodes": [
            { "id": text_id, "type": 3, "textContent": text }
        ]})
    };
    let snapshot = json!({ "node": { "id": 1, "type": 0, "childNodes": [
        { "id": 2, "type": 2, "tagName": "body", "childNodes": [
            { "id": 3, "type": 2, "tagName": "table", "attributes": {}, "childNodes": [
                { "id": 4, "type": 2, "tagName": "thead", "childNodes": [{ "id": 5, "type": 2, "tagName": "tr", "childNodes": [
                    { "id": 6, "type": 2, "tagName": "th", "childNodes": [{ "id": 7, "type": 3, "textContent": "Stage" }] },
                    { "id": 8, "type": 2, "tagName": "th", "childNodes": [{ "id": 9, "type": 3, "textContent": "Name" }] }
                ]}]},
                { "id": 10, "type": 2, "tagName": "tbody", "childNodes": [
                    { "id": 11, "type": 2, "tagName": "tr", "attributes": { "data-row-id": "r1" }, "childNodes": [
                        cell(12, "0", "Seed", 13), cell(14, "1", "Notebook", 15)
                    ]}
                ]}
            ]}
        ]}
    ]}});
    let events = [
        event("a", 0.0, 2, snapshot),
        event("a", 100.0, 3, click(12)),
    ];
    let recording = Recording::from_events(&events).unwrap();
    let with = |grid: Value| {
        let vocabulary = Vocabulary::from_document(json!({
            "version": 1,
            "apps": { "demo": { "project": 1, "host": "example.test", "audience": "users" } },
            "surfaces": [], "grid": grid,
        }))
        .unwrap();
        compile(&recording, &Matcher::new(&vocabulary), "demo")
            .unwrap()
            .actions[0]
            .target()
            .map(|t| t.into_owned())
    };
    assert_eq!(
        with(json!({})).as_deref(),
        Some("cell[Stage] \"Seed\": \"Seed\"")
    );
    assert_eq!(
        with(json!({ "label_columns": [["name"]], "row_keys": [{ "attribute": "data-row-id" }] }))
            .as_deref(),
        Some("cell[Stage] \"Notebook\": \"Seed\"")
    );
}

#[test]
fn long_absences_split_a_trace_into_visits() {
    let minutes = |m: f64| m * 60_000.0;
    let events = [
        event("a", 0.0, 2, page("Save")),
        event("a", minutes(1.0), 3, click(3)),
        event("a", minutes(20.0), 3, click(3)),
        // Back after 40 minutes away: a new visit, even though the tab stayed open.
        event("a", minutes(60.0), 3, click(3)),
    ];
    let actions = run(&events);
    let visits = spoiler_core::trace::visits(&actions, &Default::default());
    assert_eq!(visits.len(), 2);
    assert_eq!((visits[0].start, visits[1].end), (0, actions.len()));
    assert_eq!(visits[0].end, visits[1].start);
    assert_eq!(
        actions[visits[1].start..]
            .iter()
            .filter(|a| a.kind() == ActionKind::Click)
            .count(),
        1
    );
    // The idle marker for the absence stays with the visit it ended.
    assert!(
        actions[..visits[0].end]
            .iter()
            .any(|a| a.kind() == ActionKind::Idle)
    );
}

#[test]
fn line_breaks_inside_a_cell_stay_on_one_row() {
    let mut action = Action::new(
        Detail::ConsoleError {
            message: "first\r\nsecond\rthird".into(),
        },
        Millis(0.0),
        1,
    );
    action.reference = Ref(1);
    let tsv = to_tsv(&[action]);
    assert!(!tsv.contains('\r'), "{tsv:?}");
    assert_eq!(tsv.lines().count(), 2, "header and one row: {tsv:?}");
}

#[test]
fn every_member_of_a_gzip_file_is_decoded() {
    let member = |events: &[Event]| {
        let mut text = String::new();
        for event in events {
            text.push_str(&serde_json::to_string(event).unwrap());
            text.push('\n');
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(text.as_bytes()).unwrap();
        gzip.finish().unwrap()
    };
    // `cat a.jsonl.gz b.jsonl.gz`: a valid gzip file with two members.
    let mut file = member(&[event("tab", 1.0, 2, page("Save"))]);
    file.extend(member(&[event("tab", 2.0, 3, click(3))]));
    let recording = decode_with(&file, Limits::default()).unwrap();
    assert_eq!(recording.len(), 2);
}

#[test]
fn a_lone_surrogate_in_recorded_text_does_not_fail_the_recording() {
    // A browser can record text cut through an emoji: "\ud83d" alone is a valid JavaScript
    // string but not valid JSON Unicode. It reads as U+FFFD, one UTF-16 unit like the original.
    let page = page("Save")
        .to_string()
        .replace(r#""Save""#, r#""Save \ud83d""#);
    let meta = json!({ "type": 4, "timestamp": 0, "win": "tab", "data": { "href": "https://example.test/page" } });
    let plain = format!("{meta}\n{{\"type\":2,\"timestamp\":0,\"win\":\"tab\",\"data\":{page}}}\n");
    let recording = decode_with(plain.as_bytes(), Limits::default()).unwrap();
    let events: Vec<_> = recording.events().collect();
    let snapshot: Value =
        serde_json::from_str(&recording.expanded_data(&events[1]).unwrap()).unwrap();
    assert_eq!(
        snapshot["node"]["childNodes"][0]["childNodes"][0]["childNodes"][0]["textContent"],
        "Save \u{fffd}"
    );

    // The same inside a posthog-js compressed field.
    let adds = r#"[{"parentId":2,"nextId":null,"node":{"id":5,"type":3,"textContent":"\ud83d"}}]"#;
    let packed: String = zstd::encode_all(adds.as_bytes(), 0)
        .unwrap()
        .into_iter()
        .map(char::from)
        .collect();
    let line = json!(["tab", { "type": 3, "timestamp": 1, "cv": "2024-10", "data": { "source": 0, "adds": packed } }]).to_string();
    let recording = Recording::from_snapshot_bodies(&[line], Limits::default()).unwrap();
    let event = recording.events().next().unwrap();
    let added: Value = serde_json::from_str(&recording.expanded_data(&event).unwrap()).unwrap();
    assert_eq!(added["adds"][0]["node"]["textContent"], "\u{fffd}");
}

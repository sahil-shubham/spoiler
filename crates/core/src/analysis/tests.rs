//! The prompt files against the code that reads the model's answer. The prompt is edited as
//! prose; these keep it saying what the parser and validator actually enforce.
#![allow(clippy::unwrap_used)]

use super::{FrictionKind, ModelSummary, SYSTEM_PROMPT, response_schema, validate::signal_for};
use crate::time::Millis;
use crate::trace::{Action, ActionKind, Control, Detail, Entry, Flag, Press, TargetClass};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const KINDS: [ActionKind; 10] = [
    ActionKind::Click,
    ActionKind::Dblclick,
    ActionKind::Contextmenu,
    ActionKind::Input,
    ActionKind::Nav,
    ActionKind::Hidden,
    ActionKind::Visible,
    ActionKind::ConsoleError,
    ActionKind::NetError,
    ActionKind::Idle,
];

const FLAGS: [Flag; 7] = [
    Flag::Dead,
    Flag::Unresponsive,
    Flag::Rage,
    Flag::Slow,
    Flag::ErrorAfter,
    Flag::ErrorShown,
    Flag::Thrash,
];

const FRICTION: [FrictionKind; 7] = [
    FrictionKind::DeadClick,
    FrictionKind::RageClick,
    FrictionKind::Error,
    FrictionKind::ConfusionLoop,
    FrictionKind::Slow,
    FrictionKind::Abandonment,
    FrictionKind::Other,
];

/// Fails to compile when a variant is added, until the lists above include it.
#[allow(dead_code)]
fn lists_are_exhaustive(kind: ActionKind, flag: Flag, friction: FrictionKind) {
    use ActionKind as K;
    use FrictionKind as F;
    match kind {
        K::Click | K::Dblclick | K::Contextmenu | K::Input | K::Nav => {}
        K::Hidden | K::Visible | K::ConsoleError | K::NetError | K::Idle => {}
    }
    match flag {
        Flag::Dead | Flag::Unresponsive | Flag::Rage | Flag::Slow => {}
        Flag::ErrorAfter | Flag::ErrorShown | Flag::Thrash => {}
    }
    match friction {
        F::DeadClick | F::RageClick | F::Error | F::ConfusionLoop => {}
        F::Slow | F::Abandonment | F::Other => {}
    }
}

/// An action of `kind`, with placeholder details.
fn action_of(kind: ActionKind) -> Action {
    let control = Control {
        node: 1,
        target: String::new(),
        feature: None,
        element: None,
        reaction: None,
    };
    let press = || Press {
        control: control.clone(),
        point: None,
        class: TargetClass::Interactive,
    };
    let detail = match kind {
        ActionKind::Click => Detail::Click(press()),
        ActionKind::Dblclick => Detail::Dblclick(press()),
        ActionKind::Contextmenu => Detail::Contextmenu(press()),
        ActionKind::Input => Detail::Input(Entry {
            control: control.clone(),
            typed: String::new(),
        }),
        ActionKind::Nav => Detail::Nav,
        ActionKind::Hidden => Detail::Hidden,
        ActionKind::Visible => Detail::Visible,
        ActionKind::ConsoleError => Detail::ConsoleError {
            message: String::new(),
        },
        ActionKind::NetError => Detail::NetError {
            status: 500,
            request: String::new(),
        },
        ActionKind::Idle => Detail::Idle {
            idle_ms: Millis::ZERO,
        },
    };
    let action = Action::new(detail, Millis::ZERO, 1);
    assert_eq!(action.kind(), kind);
    action
}

/// `- `name`: …` bullets of one `### heading` section: each name with the backticked names
/// after its colon.
fn legend(heading: &str) -> Vec<(String, BTreeSet<String>)> {
    let marker = format!("### {heading}\n");
    let start = SYSTEM_PROMPT
        .find(&marker)
        .unwrap_or_else(|| panic!("the prompt has no {heading:?} section"))
        + marker.len();
    let section = &SYSTEM_PROMPT[start..];
    let section = &section[..section.find("\n#").unwrap_or(section.len())];
    let ticked = |text: &str| -> Vec<String> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    };
    let bullets: Vec<_> = section
        .lines()
        .filter_map(|line| line.strip_prefix("- `"))
        .filter_map(|line| line.split_once("`:"))
        .map(|(name, rest)| (name.to_owned(), ticked(rest).into_iter().collect()))
        .collect();
    assert!(!bullets.is_empty(), "parsed no bullets under {heading:?}");
    bullets
}

fn names(bullets: &[(String, BTreeSet<String>)]) -> BTreeSet<&str> {
    bullets.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn the_prompt_explains_every_trace_kind_and_flag() {
    let kinds: BTreeSet<&str> = KINDS.iter().map(|k| k.as_str()).collect();
    assert_eq!(names(&legend("Kinds")), kinds);
    let flags: BTreeSet<&str> = FLAGS.iter().map(|f| f.as_str()).collect();
    assert_eq!(names(&legend("Flags")), flags);
}

/// The evidence the prompt demands for each friction kind is exactly what validation accepts:
/// a stricter validator silently drops correct answers, a looser one keeps invented ones.
#[test]
fn the_prompt_states_the_evidence_validation_requires() {
    let stated = legend("Friction evidence");
    for kind in FRICTION {
        let listed = stated.iter().find(|(name, _)| name == kind.as_str());
        let Some(signal) = signal_for(kind) else {
            assert!(
                listed.is_none(),
                "{} needs no evidence but the prompt lists some",
                kind.as_str()
            );
            continue;
        };
        let mut accepted = BTreeSet::new();
        for flag in FLAGS {
            let mut action = action_of(ActionKind::Click);
            action.flag(flag);
            if signal(&action) {
                accepted.insert(flag.as_str().to_owned());
            }
        }
        for action_kind in KINDS {
            if signal(&action_of(action_kind)) {
                accepted.insert(action_kind.as_str().to_owned());
            }
        }
        let listed = listed.unwrap_or_else(|| panic!("the prompt omits {}", kind.as_str()));
        assert_eq!(listed.1, accepted, "evidence for {}", kind.as_str());
    }
}

/// Strict structured output requires every object to close its properties and require them all.
#[test]
fn the_response_schema_is_strict() {
    fn check(schema: &Value, path: &str, objects: &mut usize) {
        if schema["type"] == "object" {
            *objects += 1;
            assert_eq!(schema["additionalProperties"], false, "{path} is open");
            let properties = schema["properties"].as_object().unwrap();
            let required: BTreeSet<&str> = schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap())
                .collect();
            let declared: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
            assert_eq!(required, declared, "{path} must require every property");
            for (name, property) in properties {
                check(property, &format!("{path}/{name}"), objects);
            }
        }
        if let Some(items) = schema.get("items") {
            check(items, &format!("{path}/items"), objects);
        }
    }
    let mut objects = 0;
    check(response_schema(), "", &mut objects);
    assert!(objects >= 5, "walked only {objects} objects");
}

/// An answer built from the schema, taking `choose` for each enum.
fn answer(schema: &Value, choose: &mut dyn FnMut(&[Value]) -> Value) -> Value {
    if let Some(values) = schema["enum"].as_array() {
        return choose(values);
    }
    match &schema["type"] {
        Value::Array(types) if types.contains(&json!("null")) => Value::Null,
        Value::String(t) if t == "object" => {
            let mut object = Map::new();
            for (name, property) in schema["properties"].as_object().unwrap() {
                object.insert(name.clone(), answer(property, choose));
            }
            Value::Object(object)
        }
        Value::String(t) if t == "array" => json!([answer(&schema["items"], choose)]),
        Value::String(t) if t == "number" => json!(0.5),
        _ => json!("text"),
    }
}

/// Every answer the schema lets the model give, the parser accepts: each enum value in turn.
#[test]
fn every_answer_the_schema_allows_parses() {
    let schema = response_schema();
    let mut enums = Vec::new();
    answer(schema, &mut |values| {
        enums.push(values.to_vec());
        values[0].clone()
    });
    assert!(enums.len() >= 4, "found only {} enums", enums.len());
    for (index, values) in enums.iter().enumerate() {
        for value in values {
            let mut seen = 0;
            let candidate = answer(schema, &mut |values| {
                seen += 1;
                if seen == index + 1 {
                    value.clone()
                } else {
                    values[0].clone()
                }
            });
            ModelSummary::from_value(candidate.clone())
                .unwrap_or_else(|error| panic!("{value} does not parse: {error:#}\n{candidate}"));
        }
    }
}

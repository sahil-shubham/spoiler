use crate::{
    artifact::PromptId,
    model::{Message, Role},
    trace::{Action, render_effect},
    vocab::Vocabulary,
};
use serde_json::Value;
use std::{collections::HashSet, sync::LazyLock};

/// The built-in narration instructions (`prompts/narrate/system.md`).
pub const SYSTEM_PROMPT: &str = include_str!("../../../../prompts/narrate/system.md");

/// The response schema (`prompts/narrate/response.schema.json`). Its descriptions steer the
/// model as much as the system prompt does, so it lives beside it.
const RESPONSE_SCHEMA: &str = include_str!("../../../../prompts/narrate/response.schema.json");

/// Strict JSON schema for the model's answer. Field order is load-bearing: reasoning comes before
/// verdicts, so the model works before it concludes.
pub fn response_schema() -> &'static Value {
    static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(RESPONSE_SCHEMA).expect("the response schema is valid JSON (tested)")
    });
    &SCHEMA
}

/// The instructions a narration runs with: the built-in prompt, or a caller's replacement.
#[derive(Clone, Copy, Debug)]
pub struct Instructions<'a> {
    /// Recorded in provenance: `narrate` for the built-in prompt.
    pub name: &'a str,
    pub system: &'a str,
}

impl Default for Instructions<'_> {
    fn default() -> Self {
        Self {
            name: "narrate",
            system: SYSTEM_PROMPT,
        }
    }
}

impl Instructions<'_> {
    /// The system prompt and response schema together: both are what the model is told.
    pub fn id(&self) -> PromptId {
        PromptId::of(self.name, &[self.system, RESPONSE_SCHEMA])
    }
}

/// The system prompt plus a user message holding the vocabulary context, the caller's session
/// header (who, which account), and the trace.
pub fn build_messages(
    instructions: Instructions<'_>,
    vocabulary: &Vocabulary,
    app: &str,
    session_header: &str,
    actions: &[Action],
    tsv: &str,
) -> Vec<Message> {
    let mut user = glossary(vocabulary, app, actions);
    if !session_header.trim().is_empty() {
        user.push_str(&format!("\n\nSession: {session_header}"));
    }
    user.push_str(&format!("\n\nTrace:\n{tsv}"));
    vec![
        Message::new(Role::System, instructions.system),
        Message::new(Role::User, user),
    ]
}

/// The vocabulary a session needs: its app's surfaces, the features it touched, the glossary,
/// and only the statuses whose labels are on screen (a hundred values would drown the trace).
fn glossary(vocabulary: &Vocabulary, app: &str, actions: &[Action]) -> String {
    let on_screen = actions
        .iter()
        .map(|a| {
            let effects: Vec<String> = a.effects.iter().map(render_effect).collect();
            format!("{} {}", a.target().unwrap_or_default(), effects.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let used: HashSet<&str> = actions.iter().filter_map(Action::feature).collect();
    let visited: HashSet<&str> = actions
        .iter()
        .filter_map(|a| a.surface.as_deref())
        .collect();

    let surfaces = lines(
        vocabulary
            .surfaces
            .iter()
            .filter(|s| s.app == app && visited.contains(s.id.as_str()))
            .map(|s| {
                let purpose = s.purpose.as_deref().filter(|p| !p.is_empty());
                let purpose = purpose.map_or_else(String::new, |p| format!(" — {p}"));
                format!("- {} ({}): {}{purpose}", s.id, s.route, s.name)
            }),
    );
    let features = lines(
        vocabulary
            .features
            .iter()
            .filter(|f| used.contains(f.id.as_str()))
            .map(|f| {
                let note = f.note.as_deref().filter(|n| !n.is_empty());
                let note = note.map_or_else(String::new, |n| format!(" ({n})"));
                format!("- {}: {}{note}", f.id, f.name)
            }),
    );
    let terms = lines(vocabulary.terms.iter().map(|t| {
        let labels = t.ui_labels.as_deref().filter(|l| !l.is_empty());
        let labels = labels.map_or_else(String::new, |l| format!(" [shown as: {}]", l.join(", ")));
        format!("- {}: {}{labels}", t.term, t.means)
    }));
    let statuses = lines(vocabulary.statuses.iter().filter_map(|s| {
        let label = s.label.as_deref().filter(|l| !l.is_empty())?;
        on_screen
            .contains(&label.to_lowercase())
            .then(|| format!("- {}: \"{label}\" = {}", s.kind, s.value))
    }));

    let audience = vocabulary
        .apps
        .get(app)
        .map_or("", |app| app.audience.as_str());
    let mut sections = vec![format!("App: {app} — {audience}")];
    for (heading, body) in [
        ("Surfaces visited in this session", surfaces),
        ("Features seen in this session", features),
        ("Glossary", terms),
        ("Statuses visible in this session", statuses),
    ] {
        if !body.is_empty() {
            sections.push(format!("{heading}:\n{body}"));
        }
    }
    sections.join("\n\n")
}

fn lines(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join("\n")
}

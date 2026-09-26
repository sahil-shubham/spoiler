//! Asking a model for a vocabulary: its instructions, the request, and holding the answer to the
//! product config. Reading sources and calling the model belong to the caller.

use super::{App, Vocabulary, VocabularyError};
use crate::{
    artifact::PromptId,
    model::{Message, Role},
};
use indexmap::IndexMap;
use serde_json::Value;

/// The vocabulary-building instructions (`prompts/vocabulary/system.md`).
pub const SYSTEM_PROMPT: &str = include_str!("../../prompts/vocabulary/system.md");

pub fn prompt_id() -> PromptId {
    PromptId::of("vocabulary", &[SYSTEM_PROMPT])
}

/// One source file given to the model, by name.
pub struct Source<'a> {
    pub name: &'a str,
    pub text: &'a str,
}

/// The apps a product config defines: what every vocabulary built from it must declare.
pub fn config_apps(config: &Value) -> Result<IndexMap<String, App>, VocabularyError> {
    serde_json::from_value(config["apps"].clone()).map_err(VocabularyError::ConfigApps)
}

pub fn build_messages(config: &Value, sources: &[Source<'_>]) -> Vec<Message> {
    let mut user = format!("Config:\n{config}\nSources:\n");
    for source in sources {
        user.push_str(&format!("\nFile: {}\n{}\n", source.name, source.text));
    }
    vec![
        Message::new(Role::System, SYSTEM_PROMPT),
        Message::new(Role::User, user),
    ]
}

/// A model's answer as a vocabulary.
pub fn parse_answer(content: &str) -> Result<Vocabulary, VocabularyError> {
    let document: Value = serde_json::from_str(content).map_err(VocabularyError::AnswerNotJson)?;
    Vocabulary::from_document(document)
}

/// A built vocabulary, generated or prepared, must declare exactly the config's apps.
pub fn ensure_matches_config(
    vocabulary: &Vocabulary,
    apps: &IndexMap<String, App>,
) -> Result<(), VocabularyError> {
    if vocabulary.apps != *apps {
        return Err(VocabularyError::AppsDiffer);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SYSTEM_PROMPT;
    use crate::vocab::Matchers;
    use std::collections::BTreeSet;

    /// The prompt lists every matcher key the vocabulary reads, and only those.
    #[test]
    fn the_prompt_lists_every_matcher_key() {
        let one = || vec!["x".to_owned()];
        // No `..Default::default()`: a new matcher field must be added here, and to the prompt.
        let all = Matchers {
            testid: one(),
            data_attr: one(),
            aria: one(),
            title: one(),
            placeholder: one(),
            href: one(),
            text: one(),
            role: one(),
            class_contains: one(),
            aria_template: one(),
            text_template: one(),
            title_template: one(),
        };
        let keys: BTreeSet<String> = serde_json::to_value(all)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let section = SYSTEM_PROMPT
            .split_once("### Matchers\n")
            .expect("the prompt has a Matchers section")
            .1;
        let listed: BTreeSet<String> = section
            .lines()
            .filter_map(|line| line.strip_prefix("- `")?.strip_suffix('`'))
            .map(str::to_owned)
            .collect();
        assert!(!listed.is_empty(), "parsed no matcher keys");
        assert_eq!(listed, keys);
    }
}

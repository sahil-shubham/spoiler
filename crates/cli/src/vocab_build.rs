//! Building a vocabulary snapshot: from a model reading explicit source files, or by validating
//! a prepared candidate. Consumers never build implicitly; they pin a snapshot.

use crate::{
    io::read,
    openrouter::{OpenRouter, ResponseFormat},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use spoiler_core::{
    artifact::{SourceDigest, VocabularyProvenance, VocabularySnapshot, sha256_hex},
    vocab::{
        Vocabulary,
        build::{self, Source},
    },
};
use std::path::PathBuf;

pub struct Build {
    pub config: PathBuf,
    pub sources: Vec<PathBuf>,
    pub candidate: Option<PathBuf>,
    pub source_revision: Option<String>,
}

pub fn build(inputs: &Build, model: Option<&OpenRouter>) -> Result<VocabularySnapshot> {
    let config_bytes = read(&inputs.config)?;
    let config: Value = serde_yaml::from_slice(&config_bytes).context("config is not YAML/JSON")?;
    let apps = build::config_apps(&config)?;

    let mut files = Vec::new();
    for path in &inputs.sources {
        let bytes = read(path)?;
        let name = path
            .file_name()
            .context("source path has no file name")?
            .to_string_lossy()
            .into_owned();
        let text =
            String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("source {name} is not UTF-8"))?;
        files.push((name, text));
    }
    let sources = files
        .iter()
        .map(|(name, text)| SourceDigest {
            name: name.clone(),
            sha256: sha256_hex(text.as_bytes()),
        })
        .collect();

    let (vocabulary, generator) = match (&inputs.candidate, model) {
        (Some(candidate), _) => (
            Vocabulary::parse(&read(candidate)?)?,
            json!({ "mode": "validated_candidate" }),
        ),
        (None, Some(model)) => {
            let given: Vec<Source<'_>> = files
                .iter()
                .map(|(name, text)| Source { name, text })
                .collect();
            let messages = build::build_messages(&config, &given);
            let completion = model.complete(&messages, ResponseFormat::JsonObject)?;
            let generator = json!({
                "mode": "model",
                "requested_model": model.model,
                "model": completion.model,
                "usage": completion.usage,
                "prompt": build::prompt_id(),
            });
            (build::parse_answer(&completion.into_content()?)?, generator)
        }
        (None, None) => anyhow::bail!("--model is required without --candidate"),
    };
    build::ensure_matches_config(&vocabulary, &apps)?;

    Ok(VocabularySnapshot::new(
        vocabulary,
        VocabularyProvenance {
            config_digest: sha256_hex(&config_bytes),
            source_revision: inputs.source_revision.clone(),
            sources,
            generator,
        },
    ))
}

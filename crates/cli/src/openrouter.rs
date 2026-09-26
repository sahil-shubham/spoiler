//! OpenRouter: structured-output chat completions, routed only to zero-data-retention providers.

use crate::http::{Http, credential};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spoiler_core::{
    analysis::{self, Assessment, Check, MAX_ATTEMPTS, SessionSummary},
    artifact::ModelUsage,
    model::{Message, Role},
    trace::Action,
    vocab::Vocabulary,
};

const TOKEN: &str = "OPENROUTER_API_KEY";

pub struct OpenRouter {
    http: Http,
    base_url: String,
    pub model: String,
}

#[derive(Clone, Copy)]
pub enum ResponseFormat<'a> {
    /// Strict JSON schema output.
    Schema {
        name: &'static str,
        schema: &'a Value,
    },
    /// Any JSON object.
    JsonObject,
}

/// Token counts and cost as OpenRouter reports them (`usage: { include: true }`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

pub struct Completion {
    pub content: String,
    pub model: Option<String>,
    pub usage: Usage,
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

impl OpenRouter {
    pub fn new(base_url: &str, model: &str, timeout_seconds: u64) -> Result<Self> {
        Ok(Self {
            http: Http::new(timeout_seconds)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
        })
    }

    pub fn complete(&self, messages: &[Message], format: ResponseFormat<'_>) -> Result<Completion> {
        let response_format = match format {
            ResponseFormat::Schema { name, schema } => json!({
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema },
            }),
            ResponseFormat::JsonObject => json!({ "type": "json_object" }),
        };
        let body = json!({
            "model": self.model,
            "messages": messages,
            "response_format": response_format,
            // No sampling parameters: ZDR endpoints may reject them, and with
            // require_parameters that routes to nothing.
            "provider": { "require_parameters": true, "zdr": true },
            "usage": { "include": true },
        });
        let url = format!("{}/chat/completions", self.base_url);
        let text = self.http.post_json(&url, &credential(TOKEN)?, &body)?;
        let response: Response =
            serde_json::from_str(&text).context("completion response has an unexpected shape")?;
        let content = response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .context("completion has no content")?;
        Ok(Completion {
            content,
            model: response.model,
            usage: response.usage.unwrap_or_default(),
        })
    }

    /// Ask for an analysis, correcting the model once if its answer is malformed or cites refs
    /// that do not exist. Usage accumulates across attempts.
    pub fn narrate(
        &self,
        mut messages: Vec<Message>,
        actions: &[Action],
        vocabulary: &Vocabulary,
    ) -> Result<(SessionSummary, Check, ModelUsage)> {
        let mut usage = ModelUsage {
            model: self.model.clone(),
            ..ModelUsage::default()
        };
        let schema = ResponseFormat::Schema {
            name: "session_summary",
            schema: analysis::response_schema(),
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let completion = self.complete(&messages, schema)?;
            usage.attempts = attempt;
            usage.tokens_in += completion.usage.prompt_tokens;
            usage.tokens_out += completion.usage.completion_tokens;
            usage.cost_usd += completion.usage.cost;
            if let Some(model) = completion.model {
                usage.model = model;
            }
            match analysis::assess(&completion.content, actions, vocabulary) {
                Assessment::Accepted { summary, check } => return Ok((summary, check, usage)),
                Assessment::Rejected { reason } if attempt < MAX_ATTEMPTS => {
                    messages.push(Message::new(Role::Assistant, completion.content));
                    messages.push(Message::new(Role::User, Assessment::feedback(&reason)));
                }
                Assessment::Rejected { reason } => {
                    bail!("model answer rejected after {attempt} attempts: {reason}")
                }
            }
        }
        bail!("no model attempts were made")
    }
}

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
    token: String,
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
    /// Absent when the provider answered without content (its `finish_reason` says why).
    pub content: Option<String>,
    pub finish_reason: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
}

impl Completion {
    /// The content, or an error naming why there is none.
    pub fn into_content(self) -> Result<String> {
        match self.content {
            Some(content) => Ok(content),
            None => bail!(
                "completion has no content (finish reason: {})",
                self.finish_reason.as_deref().unwrap_or("none given")
            ),
        }
    }
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
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

impl OpenRouter {
    /// Fails without the API key, before any work that would need a model is started.
    pub fn new(base_url: &str, model: &str, timeout_seconds: u64) -> Result<Self> {
        Ok(Self {
            http: Http::new(timeout_seconds)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: credential(TOKEN)?,
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
        let text = self.http.post_json(&url, &self.token, &body)?;
        let response: Response =
            serde_json::from_str(&text).context("completion response has an unexpected shape")?;
        let (content, finish_reason) = response
            .choices
            .into_iter()
            .next()
            .map(|choice| (choice.message.content, choice.finish_reason))
            .unwrap_or_default();
        Ok(Completion {
            content: content.filter(|content| !content.trim().is_empty()),
            finish_reason,
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
    ) -> Result<(SessionSummary, Check, ModelUsage, String)> {
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
            // An empty answer (a provider hiccup) is asked again as it was; it was still paid for.
            let Some(content) = completion.content else {
                if attempt < MAX_ATTEMPTS {
                    continue;
                }
                bail!(
                    "model gave no answer after {attempt} attempts (finish reason: {})",
                    completion.finish_reason.as_deref().unwrap_or("none given")
                );
            };
            match analysis::assess(&content, actions, vocabulary) {
                Assessment::Accepted { summary, check } => {
                    return Ok((summary, check, usage, content));
                }
                Assessment::Rejected { reason } if attempt < MAX_ATTEMPTS => {
                    messages.push(Message::new(Role::Assistant, content));
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spoiler_core::{recording, trace, vocab::Matcher};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        path::Path,
    };

    /// Answer each request with the next body, closing the connection after each.
    fn serve(bodies: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for body in bodies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, server)
    }

    #[test]
    fn an_empty_answer_is_asked_again_and_still_counted() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let vocabulary =
            Vocabulary::parse(&std::fs::read(root.join("corpus/vocabulary.yaml")).unwrap())
                .unwrap();
        let recording =
            recording::decode(&std::fs::read(root.join("corpus/click_changes_text.json")).unwrap())
                .unwrap();
        let actions = trace::compile(&recording, &Matcher::new(&vocabulary), "demo")
            .unwrap()
            .actions;
        let answer =
            std::fs::read_to_string(root.join("examples/click_changes_text.response.json"))
                .unwrap();
        let completion = |content: Value, prompt_tokens: u64| {
            json!({
                "choices": [{ "message": { "content": content }, "finish_reason": "error" }],
                "model": "test/model",
                "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": 1, "cost": 0.5 },
            })
            .to_string()
        };
        let (url, server) = serve(vec![
            completion(Value::Null, 10),
            completion(json!(answer), 20),
        ]);
        let client = OpenRouter {
            http: Http::new(5).unwrap(),
            base_url: url,
            token: "test".into(),
            model: "test/model".into(),
        };
        let (_, _, usage, _) = client
            .narrate(
                vec![Message::new(Role::User, "trace")],
                &actions,
                &vocabulary,
            )
            .unwrap();
        server.join().unwrap();
        assert_eq!(usage.attempts, 2);
        assert_eq!(
            usage.tokens_in, 30,
            "the empty answer's tokens are billed too"
        );
        assert_eq!(usage.cost_usd, 1.0);
    }
}

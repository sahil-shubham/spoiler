//! Analysis: asking a model what a trace means, and holding its answer to the trace.
//!
//! The model writes prose and groupings. Code writes every fact the trace already holds — times,
//! surfaces, persisted changes, task durations and paths — and drops claims the trace does not
//! support. [`assess`] turns one model response into an accepted summary or feedback for a retry;
//! the caller owns the model call itself.

mod prompt;
#[cfg(test)]
mod tests;
mod validate;

use crate::{
    trace::{Action, Flag, Ref},
    vocab::Vocabulary,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub use prompt::{Instructions, SYSTEM_PROMPT, build_messages, response_schema};
pub use validate::validate;

/// Responses citing more nonexistent refs than this share are rejected.
pub const MAX_BAD_REF_RATIO: f64 = 0.15;
/// Model calls per analysis: the first answer plus one corrected retry.
pub const MAX_ATTEMPTS: u32 = 2;

/// A field that must be present but may be `null` (serde otherwise treats absence as `None`).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Progressed,
    NoEffect,
    Error,
    Abandoned,
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Done,
    Workaround,
    GaveUp,
    Unclear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrictionKind {
    DeadClick,
    RageClick,
    Error,
    ConfusionLoop,
    Slow,
    Abandonment,
    Other,
}

impl FrictionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeadClick => "dead_click",
            Self::RageClick => "rage_click",
            Self::Error => "error",
            Self::ConfusionLoop => "confusion_loop",
            Self::Slow => "slow",
            Self::Abandonment => "abandonment",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Degrading,
    Cosmetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Success {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub text: String,
    pub confidence: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub success: Success,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub refs: Vec<String>,
    pub action: String,
    pub response: String,
    #[serde(deserialize_with = "nullable")]
    pub feature: Option<String>,
    pub outcome: StepOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub goal: String,
    pub refs: Vec<String>,
    pub outcome: TaskOutcome,
    #[serde(deserialize_with = "nullable")]
    pub obstacle: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Friction {
    pub refs: Vec<String>,
    pub kind: FrictionKind,
    pub what: String,
    #[serde(deserialize_with = "nullable")]
    pub why: Option<String>,
    pub severity: Severity,
}

/// What the model returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSummary {
    pub reasoning: String,
    pub who: String,
    pub intent: Intent,
    pub tasks: Vec<Task>,
    pub steps: Vec<Step>,
    pub friction: Vec<Friction>,
    pub outcome: Outcome,
    pub summary: String,
}

/// Why a model's answer is not a [`ModelSummary`].
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error(transparent)]
    Shape(#[from] serde_json::Error),
    #[error("intent.confidence must be within [0, 1]")]
    Confidence,
}

impl ModelSummary {
    pub fn from_value(value: Value) -> Result<Self, AnswerError> {
        let summary: Self = serde_json::from_value(value)?;
        if !(0.0..=1.0).contains(&summary.intent.confidence) {
            return Err(AnswerError::Confidence);
        }
        Ok(summary)
    }

    pub fn from_json(content: &str) -> Result<Self, AnswerError> {
        Self::from_value(serde_json::from_str(content)?)
    }
}

/// A step as stored: the model's text plus facts derived from the refs it cites.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidatedStep {
    #[serde(flatten)]
    pub step: Step,
    pub at_s: f64,
    pub surface: Option<String>,
    /// Persisted changes on the cited actions (cell before → after, rows added/removed).
    pub changes: Vec<String>,
    /// Durations quoted in `response` that no cited action carries.
    pub unverified: Vec<String>,
}

/// A task as stored: the model's text plus its measured span over the trace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidatedTask {
    #[serde(flatten)]
    pub task: Task,
    pub start_s: f64,
    pub end_s: f64,
    /// Time on a visible tab between the first and last cited action (idle capped, hidden excluded).
    pub active_s: f64,
    /// Surfaces (or paths) the user moved through, in order.
    pub path: Vec<String>,
    pub actions: usize,
    pub changes: usize,
}

/// An action carrying code-detected flags, and the friction items that explain it. Code lists
/// every one, so a signal the model passed over is still in the analysis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlaggedAction {
    #[serde(rename = "ref")]
    pub reference: Ref,
    pub at_s: f64,
    pub surface: Option<String>,
    pub target: Option<String>,
    pub flags: Vec<Flag>,
    /// Indexes into the summary's `friction` of the items citing this action.
    pub explained_by: Vec<usize>,
}

/// What is stored and shown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub reasoning: String,
    pub who: String,
    pub intent: Intent,
    pub tasks: Vec<ValidatedTask>,
    pub steps: Vec<ValidatedStep>,
    pub friction: Vec<Friction>,
    /// Every flagged action in the analyzed actions, in time order.
    pub signals: Vec<FlaggedAction>,
    pub outcome: Outcome,
    pub summary: String,
}

/// What validation found wrong with a model response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub bad_refs: Vec<String>,
    pub bad_ref_ratio: f64,
    /// Friction claiming a signal no cited action carries.
    pub dropped_friction: Vec<String>,
    pub unknown_features: Vec<String>,
    pub corrected: Vec<String>,
    /// Durations quoted in step responses that no cited action carries.
    pub unverified_numbers: usize,
    /// Refs of flagged actions no friction item cites.
    pub unexplained_signals: Vec<Ref>,
    /// Refs of user gestures no step cites (the prompt asks for one step per action).
    pub uncited_gestures: Vec<Ref>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "one value per model attempt; boxing would only add indirection"
)]
pub enum Assessment {
    Accepted {
        summary: SessionSummary,
        check: Check,
    },
    /// Not an analysis: `reason` says why.
    Rejected { reason: String },
}

impl Assessment {
    /// What to tell the model when asking again after a rejection.
    pub fn feedback(reason: &str) -> String {
        format!("{reason} Answer again.")
    }
}

/// Parse and validate one model response against the trace it describes.
pub fn assess(content: &str, actions: &[Action], vocabulary: &Vocabulary) -> Assessment {
    let summary = match ModelSummary::from_json(content) {
        Ok(summary) => summary,
        Err(error) => {
            let error = crate::text::utf16_slice(&error.to_string(), 0, Some(500));
            return Assessment::Rejected {
                reason: format!("That does not match the schema: {error}."),
            };
        }
    };
    let (summary, check) = validate(summary, actions, vocabulary);
    if check.bad_ref_ratio > MAX_BAD_REF_RATIO {
        return Assessment::Rejected {
            reason: format!(
                "These refs do not exist in the trace: {}. Cite only refs from the trace.",
                check.bad_refs.join(", ")
            ),
        };
    }
    Assessment::Accepted { summary, check }
}

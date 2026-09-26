//! Versioned artifact envelopes: the contracts between pipeline stages and with callers.
//!
//! Every artifact carries `schema_version` and a `kind` discriminator and names the digests of
//! the inputs it was derived from, so a stored result can always be traced to exactly what
//! produced it. Versions are per kind: changing a field's shape or meaning bumps that kind's
//! [`Kind::schema_version`] and leaves artifacts of other kinds readable.

use crate::{
    analysis::{Check, SessionSummary},
    model::Message,
    recording::Recording,
    trace::{Action, Coverage, Visit},
    vocab::{App, Vocabulary},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Why an artifact cannot be used by this build.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("expected a {expected:?} artifact, got {found:?}")]
    WrongKind { expected: Kind, found: Kind },
    #[error("unsupported {kind:?} schema version {found} (this build reads {supported})")]
    SchemaVersion {
        kind: Kind,
        found: u32,
        supported: u32,
    },
    #[error("trace was compiled by compiler version {found}; this build is version {supported}")]
    CompilerVersion { found: u32, supported: u32 },
    #[error("vocabulary snapshot content does not match its digest")]
    DigestMismatch,
    #[error("trace visit {index} has invalid bounds or refs")]
    InvalidVisit { index: usize },
    #[error("trace TSV does not match its actions")]
    TsvMismatch,
    #[error("invalid {0:?} artifact")]
    Json(Kind, #[source] serde_json::Error),
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Recording,
    Trace,
    AnalysisRequest,
    Analysis,
    VocabularySnapshot,
    VocabularyCheck,
    RecordingPage,
    Session,
}

impl Kind {
    /// The shape this build writes and reads for artifacts of this kind.
    pub fn schema_version(self) -> u32 {
        match self {
            // 2: prompt identity, request digest, signal ledger, integer token counts.
            Self::AnalysisRequest | Self::Analysis => 2,
            // 3: coverage reports suppressed extension content and unlocated snapshots.
            Self::Trace => 3,
            // 2: `next_cursor` is an opaque token (formerly a JSON object).
            Self::RecordingPage => 2,
            Self::Recording | Self::VocabularySnapshot | Self::VocabularyCheck | Self::Session => 1,
        }
    }
}

/// Which instructions a model was given: a name, and the SHA-256 of the instruction files.
///
/// Prompts live as files under `prompts/`. Editing one changes its digest, so its identity
/// cannot go stale the way a hand-bumped version number can.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptId {
    pub name: String,
    pub sha256: String,
}

impl PromptId {
    /// Digest of `parts` in order, each terminated by a NUL so that parts cannot run together.
    pub fn of(name: &str, parts: &[&str]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        Self {
            name: name.to_owned(),
            sha256: hex(&hasher.finalize()),
        }
    }
}

/// The common header; `check` rejects artifacts this build does not understand.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Header {
    pub schema_version: u32,
    pub kind: Kind,
}

impl Header {
    pub fn new(kind: Kind) -> Self {
        Self {
            schema_version: kind.schema_version(),
            kind,
        }
    }

    pub fn check(&self, expected: Kind) -> Result<(), ArtifactError> {
        if self.kind != expected {
            return Err(ArtifactError::WrongKind {
                expected,
                found: self.kind,
            });
        }
        if self.schema_version != expected.schema_version() {
            return Err(ArtifactError::SchemaVersion {
                kind: expected,
                found: self.schema_version,
                supported: expected.schema_version(),
            });
        }
        Ok(())
    }

    /// Inspect the small header before attempting to deserialize the artifact body.
    pub(crate) fn read_json(bytes: &[u8], expected: Kind) -> Result<Self, ArtifactError> {
        let header: Self =
            serde_json::from_slice(bytes).map_err(|e| ArtifactError::Json(expected, e))?;
        header.check(expected)?;
        Ok(header)
    }

    /// Vocabulary YAML is already parsed into a JSON value. Only clone its two header scalars.
    pub(crate) fn read_value(document: &Value, expected: Kind) -> Result<Self, ArtifactError> {
        let header: Self = serde_json::from_value(serde_json::json!({
            "kind": document.get("kind"),
            "schema_version": document.get("schema_version"),
        }))
        .map_err(|e| ArtifactError::Json(expected, e))?;
        header.check(expected)?;
        Ok(header)
    }
}

/// Where a recording came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum RecordingSource {
    Posthog {
        host: String,
        project: u64,
        session_id: String,
        blob_keys: Vec<String>,
    },
    File {
        sha256: String,
    },
}

/// A recording normalized to decoded events (compression undone). Read back with
/// [`crate::recording::decode`].
#[derive(Serialize)]
pub struct RecordingArtifact {
    #[serde(flatten)]
    pub header: Header,
    pub source: RecordingSource,
    #[serde(serialize_with = "serialize_recording")]
    pub events: Recording,
}

fn serialize_recording<S: serde::Serializer>(
    recording: &Recording,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    recording.serialize_events(serializer)
}

impl RecordingArtifact {
    pub fn new(source: RecordingSource, events: Recording) -> Self {
        Self {
            header: Header::new(Kind::Recording),
            source,
            events,
        }
    }
}

/// What a user did in a recording, as compiled against one vocabulary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceArtifact {
    #[serde(flatten)]
    pub header: Header,
    pub compiler_version: u32,
    pub app: String,
    pub recording_digest: String,
    pub vocab_digest: String,
    pub actions: Vec<Action>,
    /// What the actions could not account for.
    #[serde(default)]
    pub coverage: Coverage,
    /// The actions split where the user was away; analyze one with `--visit`.
    #[serde(default)]
    pub visits: Vec<Visit>,
    /// The actions rendered for people and the narrator.
    pub tsv: String,
}

impl TraceArtifact {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ArtifactError> {
        Header::read_json(bytes, Kind::Trace)?;
        let trace: Self =
            serde_json::from_slice(bytes).map_err(|e| ArtifactError::Json(Kind::Trace, e))?;
        if trace.compiler_version != crate::trace::COMPILER_VERSION {
            return Err(ArtifactError::CompilerVersion {
                found: trace.compiler_version,
                supported: crate::trace::COMPILER_VERSION,
            });
        }
        for (index, visit) in trace.visits.iter().enumerate() {
            if !visit.matches(&trace.actions) {
                return Err(ArtifactError::InvalidVisit { index });
            }
        }
        if trace.tsv != crate::trace::to_tsv(&trace.actions) {
            return Err(ArtifactError::TsvMismatch);
        }
        Ok(trace)
    }
}

/// Everything needed to ask a model for an analysis, without asking it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisRequest {
    #[serde(flatten)]
    pub header: Header,
    #[serde(flatten)]
    pub provenance: AnalysisProvenance,
    pub messages: Vec<Message>,
    pub response_schema: Value,
}

/// The inputs an analysis was derived from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisProvenance {
    pub app: String,
    pub trace_digest: String,
    pub vocab_digest: String,
    /// The instructions: the built-in narration prompt or a `--system-prompt` replacement.
    pub prompt: PromptId,
    /// SHA-256 of the exact messages and response schema, as spoiler builds them from these
    /// inputs. Equal digests ask the model the same question: a scheduler's idempotency key.
    pub request_digest: String,
    /// The visit analyzed (index into the trace's `visits`); absent for the whole trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit: Option<usize>,
}

/// A validated analysis: the model's interpretation plus facts code derived from the trace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisArtifact {
    #[serde(flatten)]
    pub header: Header,
    #[serde(flatten)]
    pub provenance: AnalysisProvenance,
    pub summary: SessionSummary,
    pub check: Check,
    /// Absent when an existing model response was validated offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelUsage>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    /// The model that answered, as reported by the provider.
    pub model: String,
    pub attempts: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
}

/// A narration of one trace or visit: the model request alone, or the validated analysis.
// A handful per run, each serialized once: boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Narration {
    Request(AnalysisRequest),
    Analysis(AnalysisArtifact),
}

/// One recording taken end to end by `spoiler run`: where it came from, its trace, and a
/// narration of each visit with user gestures.
#[derive(Clone, Debug, Serialize)]
pub struct SessionArtifact {
    #[serde(flatten)]
    pub header: Header,
    pub source: RecordingSource,
    pub trace: TraceArtifact,
    pub visits: Vec<VisitNarration>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VisitNarration {
    /// Index into the trace's `visits`.
    pub visit: usize,
    #[serde(flatten)]
    pub narration: Narration,
}

/// What `spoiler vocab check` reports about a vocabulary file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VocabularyCheck {
    #[serde(flatten)]
    pub header: Header,
    /// SHA-256 of the file: what traces compiled against it pin.
    pub sha256: String,
    pub apps: IndexMap<String, App>,
    pub surfaces: usize,
    pub features: usize,
    /// Parts of the vocabulary that can never match.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VocabularySnapshot {
    #[serde(flatten)]
    pub header: Header,
    /// SHA-256 of `vocabulary` serialized as JSON.
    pub content_digest: String,
    pub provenance: VocabularyProvenance,
    pub vocabulary: Vocabulary,
}

impl VocabularySnapshot {
    pub fn new(vocabulary: Vocabulary, provenance: VocabularyProvenance) -> Self {
        Self {
            header: Header::new(Kind::VocabularySnapshot),
            content_digest: content_digest(&vocabulary),
            provenance,
            vocabulary,
        }
    }

    pub fn check(&self) -> Result<(), ArtifactError> {
        self.header.check(Kind::VocabularySnapshot)?;
        if content_digest(&self.vocabulary) != self.content_digest {
            return Err(ArtifactError::DigestMismatch);
        }
        Ok(())
    }
}

/// SHA-256 of a vocabulary serialized as JSON.
fn content_digest(vocabulary: &Vocabulary) -> String {
    // Vocabulary types have string keys and no custom serializers that can fail.
    let json = serde_json::to_vec(vocabulary).expect("a vocabulary serializes to JSON");
    sha256_hex(&json)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VocabularyProvenance {
    pub config_digest: String,
    pub source_revision: Option<String>,
    pub sources: Vec<SourceDigest>,
    /// How the vocabulary was produced: model and prompt, or an offline candidate. Free-form so
    /// that snapshots made by other generators stay readable.
    pub generator: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceDigest {
    pub name: String,
    pub sha256: String,
}

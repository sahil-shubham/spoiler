//! Product vocabulary: which pages exist, what their controls are called, and the domain terms
//! the narrator must use.
//!
//! A vocabulary is data produced ahead of time (by hand, or by `spoiler vocab build`) and pinned by
//! consumers; nothing here infers it at runtime. [`Matcher`] compiles one for lookups.

pub mod build;
mod matcher;

use crate::{
    artifact::{ArtifactError, Header, Kind, VocabularySnapshot},
    time::Millis,
};
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use matcher::{Matcher, TargetDesc, path_and_query, pathname};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vocabulary {
    pub version: u32,
    pub apps: IndexMap<String, App>,
    pub surfaces: Vec<Surface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_surfaces: Option<Vec<ExcludedSurface>>,
    #[serde(default)]
    pub features: Vec<Feature>,
    #[serde(default)]
    pub terms: Vec<Term>,
    /// Backend-only values (no UI label) are legitimate: the narrator still needs their meaning.
    #[serde(default)]
    pub statuses: Vec<Status>,
    #[serde(default)]
    pub events: Vec<ProductEvent>,
    #[serde(default)]
    pub gaps: Vec<String>,
    /// How this product's data grids identify and label rows.
    #[serde(default, skip_serializing_if = "GridRules::is_empty")]
    pub grid: GridRules,
    /// Request URL fragments that are telemetry, not the product (on top of common monitoring).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub telemetry: Vec<String>,
    /// Patterns (regular expressions) for error messages shown on screen; English defaults
    /// when empty. Short visible text a gesture brings up that matches is `error_shown`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_text: Vec<Pattern>,
    /// Timing and distance thresholds; any subset may be overridden.
    #[serde(default, skip_serializing_if = "Thresholds::is_default")]
    pub thresholds: Thresholds,
}

/// A regular expression, compiled when the vocabulary is read: an invalid one fails the load, and
/// matching never compiles again.
#[derive(Clone, Debug)]
pub struct Pattern(Regex);

impl Pattern {
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        Regex::new(pattern).map(Self)
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.0.is_match(text)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Serialize for Pattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let pattern = String::deserialize(deserializer)?;
        Self::new(&pattern).map_err(|error| {
            serde::de::Error::custom(format!("invalid pattern {pattern:?}: {error}"))
        })
    }
}

/// The numbers the compiler's rules turn on. Products with different interaction patterns
/// (slow-by-design exports, long forms) can override the defaults here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    /// How long after a click (or keystroke) its effects are collected.
    pub effect_window_ms: Millis,
    /// A click adopts the mouse-down that preceded it within this long.
    pub gesture_ms: Millis,
    /// An input on an element mounted this recently is the page initializing its form.
    pub programmatic_input_ms: Millis,
    /// A → B → A navigation within this long is thrash.
    pub thrash_ms: Millis,
    /// At least this many clicks, within `rage_window_ms` of the first and `rage_px` of it, is rage.
    pub rage_clicks: usize,
    pub rage_window_ms: Millis,
    pub rage_px: f64,
    /// A first visible reaction this late is slow.
    pub slow_ms: Millis,
    /// A gap this long on a visible page is idle.
    pub idle_ms: Millis,
    /// No action for this long ends a visit (PostHog's session inactivity limit).
    pub visit_gap_ms: Millis,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            effect_window_ms: Millis(2000.0),
            gesture_ms: Millis(1000.0),
            programmatic_input_ms: Millis(100.0),
            thrash_ms: Millis(10_000.0),
            rage_clicks: 3,
            rage_window_ms: Millis(1000.0),
            rage_px: 30.0,
            slow_ms: Millis(1000.0),
            idle_ms: Millis(30_000.0),
            visit_gap_ms: Millis(30.0 * 60.0 * 1000.0),
        }
    }
}

impl Thresholds {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Product conventions for data grids. All optional: without them rows are told apart by
/// label, the first column labels a row, and a cell takes its column's feature only when it
/// has none of its own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GridRules {
    /// Row attributes carrying the row's own identity, tried in order. Without one, rows are
    /// matched by label, which cannot tell a re-sort from an edit when labels repeat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_keys: Vec<RowKeyRule>,
    /// Header words choosing the column that labels a row: groups tried in order, each matching
    /// any header containing one of its words (case-insensitive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label_columns: Vec<Vec<String>>,
    /// Feature ids too generic for a grid cell: a cell resolving to one takes its column
    /// header's feature instead.
    #[serde(default, skip_serializing_if = "GenericFeatures::is_empty")]
    pub generic_cell_features: GenericFeatures,
}

impl GridRules {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RowKeyRule {
    pub attribute: String,
    /// Prepended to the attribute value, keeping keys from different attributes apart.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenericFeatures {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suffixes: Vec<String>,
}

impl GenericFeatures {
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty() && self.suffixes.is_empty()
    }

    pub fn includes(&self, feature: &str) -> bool {
        self.prefixes
            .iter()
            .any(|p| feature.starts_with(p.as_str()))
            || self.suffixes.iter().any(|s| feature.ends_with(s.as_str()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct App {
    /// PostHog project id.
    pub project: u64,
    pub host: String,
    pub audience: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Surface {
    pub id: String,
    pub app: String,
    /// Path template; `:name` segments match one path segment.
    pub route: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<SurfaceState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_url: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExcludedSurface {
    pub app: String,
    pub route: String,
    pub reason: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Which surfaces a feature appears on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SurfaceSelector {
    /// A surface id, or `"*"`: shared chrome of the feature's `app`, on every one of its surfaces.
    One(String),
    Many(Vec<String>),
}

impl SurfaceSelector {
    pub fn is_app_chrome(&self) -> bool {
        matches!(self, Self::One(id) if id == "*")
    }

    pub fn names(&self, surface: &str) -> bool {
        match self {
            Self::One(id) => id == surface,
            Self::Many(ids) => ids.iter().any(|id| id == surface),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub surface: SurfaceSelector,
    /// Required when `surface` is `"*"`: the app whose chrome this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matchers: Option<Matchers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How to recognize a feature's control. Keys are listed from most to least specific.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matchers {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub testid: Vec<String>,
    /// `"attr"` or `"attr=value"`, with or without the `data-` prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_attr: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aria: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placeholder: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub href: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub role: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_contains: Vec<String>,
    /// `"Open {document}"`: literal parts verbatim, `{name}` placeholders match any text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aria_template: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_template: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_template: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Term {
    pub term: String,
    pub means: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub kind: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductEvent {
    pub name: String,
    pub app: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Why a vocabulary (or a model's answer, or a config, when building one) is unusable.
#[derive(Debug, thiserror::Error)]
pub enum VocabularyError {
    #[error("vocabulary is not YAML/JSON")]
    Syntax(#[source] serde_yaml::Error),
    #[error("invalid vocabulary")]
    Invalid(#[source] serde_json::Error),
    #[error("invalid vocabulary snapshot")]
    Snapshot(#[source] ArtifactError),
    #[error("config must define apps")]
    ConfigApps(#[source] serde_json::Error),
    #[error("model vocabulary is not JSON")]
    AnswerNotJson(#[source] serde_json::Error),
    #[error("vocabulary apps differ from the config's")]
    AppsDiffer,
    #[error("unsupported vocabulary version {found} (this build reads {supported})")]
    Version { found: u32, supported: u32 },
    #[error("surface id {0} is declared more than once")]
    DuplicateSurface(String),
}

impl Vocabulary {
    /// Parse a vocabulary file: YAML or JSON, bare or wrapped in a vocabulary snapshot artifact.
    pub fn parse(bytes: &[u8]) -> Result<Self, VocabularyError> {
        // Through JSON values rather than straight into the types: YAML keys such as `true`
        // must stay strings, as they do in JavaScript's YAML reader.
        let document: Value = serde_yaml::from_slice(bytes).map_err(VocabularyError::Syntax)?;
        Self::from_document(document)
    }

    pub fn from_document(document: Value) -> Result<Self, VocabularyError> {
        let vocabulary = if document.get("vocabulary").is_none() {
            serde_json::from_value(document).map_err(VocabularyError::Invalid)?
        } else {
            Header::read_value(&document, Kind::VocabularySnapshot)
                .map_err(VocabularyError::Snapshot)?;
            let snapshot: VocabularySnapshot = serde_json::from_value(document).map_err(|e| {
                VocabularyError::Snapshot(ArtifactError::Json(Kind::VocabularySnapshot, e))
            })?;
            snapshot.check().map_err(VocabularyError::Snapshot)?;
            snapshot.vocabulary
        };
        vocabulary.validate()?;
        Ok(vocabulary)
    }

    fn validate(&self) -> Result<(), VocabularyError> {
        const SUPPORTED_VERSION: u32 = 1;
        if self.version != SUPPORTED_VERSION {
            return Err(VocabularyError::Version {
                found: self.version,
                supported: SUPPORTED_VERSION,
            });
        }
        let mut seen = std::collections::HashSet::new();
        for surface in &self.surfaces {
            if !seen.insert(&surface.id) {
                return Err(VocabularyError::DuplicateSurface(surface.id.clone()));
            }
        }
        Ok(())
    }

    /// Problems that do not make the vocabulary unusable but make parts of it inert.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for feature in &self.features {
            if feature.surface.is_app_chrome() && feature.app.is_none() {
                warnings.push(format!(
                    "feature {} is app chrome (surface \"*\") without an app: it never matches",
                    feature.id
                ));
            }
        }
        warnings
    }
}

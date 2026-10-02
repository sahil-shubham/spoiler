//! What an app's source says about its pages and controls, read by a parser rather than a
//! model, and the checks that hold a vocabulary to it.
//!
//! An extract names every route the router defines and every literal a person can see or a
//! matcher can key on (`aria-label`, `placeholder`, `title`, `data-testid`, link targets, text
//! children), each with the file and line it came from and the routes that render it. `vocab
//! check --extract` then reports what a vocabulary claims that the source does not say: pages
//! with no surface, surfaces with no page, matchers no source writes, citations to nothing.

use crate::artifact::{Header, SourceDigest};
use crate::vocab::{Matchers, Vocabulary};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// An app's routes, controls and tracked events, as its source declares them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VocabularyExtract {
    #[serde(flatten)]
    pub header: Header,
    pub app: String,
    /// How routes were found, e.g. `react-router-flat-routes`.
    pub routes_from: String,
    /// Every file read, by path relative to the extract's root.
    pub files: Vec<SourceDigest>,
    pub routes: Vec<ExtractedRoute>,
    pub literals: Vec<ExtractedLiteral>,
    pub events: Vec<ExtractedEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedRoute {
    /// In the vocabulary's route syntax: `/fundraise/:pipelineUri`.
    pub route: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
    pub file: String,
    /// Has a default export: a page, not a loader- or action-only resource route.
    pub renders: bool,
    /// What a visitor lands on at `route`: it renders, and is neither a pathless layout nor a
    /// layout whose index child renders that path instead. Every page needs a surface.
    pub page: bool,
    /// The layout route files it renders inside, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layouts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralKind {
    Testid,
    Aria,
    Title,
    Placeholder,
    /// A link's `href` or router `to`.
    Href,
    /// Text a JSX element renders as its children.
    Text,
    /// A string passed to a component's `label`, `title`, `tooltip` or similar prop: often
    /// rendered as one of the above, by a component the parser does not follow.
    Prop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedLiteral {
    /// Stable across unrelated edits: a digest of kind, file and value.
    pub id: String,
    pub kind: LiteralKind,
    /// The literal, or for a template literal its text with `{name}` for each interpolation.
    pub value: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub template: bool,
    /// `file:line`.
    pub at: String,
    /// Route templates whose modules (or layouts) import this file. Empty: imported by no route.
    pub routes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedEvent {
    pub at: String,
    /// The call that sent it: `capture` (product analytics) or `tryAddCustomEvent` (replay).
    pub call: String,
    pub name: String,
}

impl VocabularyExtract {
    /// JSON with one record per line, so a source change diffs as the lines it changed.
    pub fn to_json_lines(&self) -> String {
        fn lines<T: Serialize>(items: &[T]) -> String {
            if items.is_empty() {
                return "[]".into();
            }
            let rows: Vec<String> = items
                .iter()
                .map(|item| serde_json::to_string(item).expect("extract records serialize"))
                .collect();
            format!("[\n{}\n]", rows.join(",\n"))
        }
        format!(
            "{{\"schema_version\":{},\"kind\":{},\"app\":{},\"routes_from\":{},\n\"files\":{},\n\"routes\":{},\n\"literals\":{},\n\"events\":{}}}\n",
            self.header.schema_version,
            serde_json::to_string(&self.header.kind).expect("kind serializes"),
            serde_json::to_string(&self.app).expect("string serializes"),
            serde_json::to_string(&self.routes_from).expect("string serializes"),
            lines(&self.files),
            lines(&self.routes),
            lines(&self.literals),
            lines(&self.events),
        )
    }
}

/// Something a vocabulary claims that its extract does not support.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Finding {
    pub rule: Rule,
    /// The surface, feature, route or file it is about.
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// A page the router renders that no surface (or exclusion) covers.
    RouteWithoutSurface,
    /// A surface whose route the router does not define.
    SurfaceWithoutRoute,
    /// A matcher value no source file writes.
    MatcherNotInSource,
    /// A `source` citation naming a file the extract did not read.
    CitationNotInSource,
    /// A declared event no source sends.
    EventNotInSource,
}

/// Matcher keys whose values are source literals, and the literal kinds that can supply them.
fn literal_matchers(
    matchers: &Matchers,
) -> Vec<(&'static str, &[String], &'static [LiteralKind], bool)> {
    use LiteralKind::*;
    vec![
        ("testid", &matchers.testid, &[Testid, Prop], false),
        ("aria", &matchers.aria, &[Aria, Prop], false),
        ("title", &matchers.title, &[Title, Prop], false),
        (
            "placeholder",
            &matchers.placeholder,
            &[Placeholder, Prop],
            false,
        ),
        ("href", &matchers.href, &[Href], false),
        ("text", &matchers.text, &[Text, Prop], false),
        (
            "aria_template",
            &matchers.aria_template,
            &[Aria, Prop],
            true,
        ),
        (
            "text_template",
            &matchers.text_template,
            &[Text, Prop],
            true,
        ),
        (
            "title_template",
            &matchers.title_template,
            &[Title, Prop],
            true,
        ),
    ]
}

/// `Open {doc}` and `Open {name}` are the same template.
fn template_shape(template: &str) -> String {
    let mut shape = String::new();
    let mut inside = false;
    for c in template.chars() {
        match c {
            '{' => {
                inside = true;
                shape.push_str("{}");
            }
            '}' if inside => inside = false,
            _ if inside => {}
            _ => shape.push(c),
        }
    }
    shape
}

/// The path part of a `source` citation (`path/to/file.jsx:42` → `path/to/file.jsx`).
fn cited_file(citation: &str) -> &str {
    match citation.rsplit_once(':') {
        Some((file, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => file,
        _ => citation,
    }
}

/// What `vocabulary` claims about `extract.app` that the extract does not support.
pub fn check(vocabulary: &Vocabulary, extract: &VocabularyExtract) -> Vec<Finding> {
    let app = extract.app.as_str();
    let mut findings = BTreeSet::new();
    let mut find = |rule, subject: &str, detail: String| {
        findings.insert(Finding {
            rule,
            subject: subject.to_owned(),
            detail,
        });
    };

    let surfaces: Vec<_> = vocabulary
        .surfaces
        .iter()
        .filter(|s| s.app == app)
        .collect();
    let covered: HashSet<&str> = surfaces
        .iter()
        .map(|s| s.route.as_str())
        .chain(
            vocabulary
                .excluded_surfaces
                .iter()
                .flatten()
                .filter(|e| e.app == app)
                .map(|e| e.route.as_str()),
        )
        .collect();
    let defined: HashSet<&str> = extract.routes.iter().map(|r| r.route.as_str()).collect();
    for route in extract.routes.iter().filter(|r| r.page) {
        if !covered.contains(route.route.as_str()) {
            find(
                Rule::RouteWithoutSurface,
                &route.route,
                format!(
                    "{} renders this page; no surface or exclusion covers it",
                    route.file
                ),
            );
        }
    }
    for surface in &surfaces {
        if !defined.contains(surface.route.as_str()) {
            find(
                Rule::SurfaceWithoutRoute,
                &surface.id,
                format!("the router defines no route {}", surface.route),
            );
        }
    }

    // Literal values by kind, for the matcher check.
    let mut values: BTreeMap<LiteralKind, HashSet<&str>> = BTreeMap::new();
    let mut templates: BTreeMap<LiteralKind, HashSet<String>> = BTreeMap::new();
    for literal in &extract.literals {
        if literal.template {
            templates
                .entry(literal.kind)
                .or_default()
                .insert(template_shape(&literal.value));
        } else {
            values
                .entry(literal.kind)
                .or_default()
                .insert(literal.value.as_str());
        }
    }
    let surface_ids: HashSet<&str> = surfaces.iter().map(|s| s.id.as_str()).collect();
    let files: HashSet<&str> = extract.files.iter().map(|f| f.name.as_str()).collect();
    for feature in &vocabulary.features {
        let of_app = match (&feature.app, &feature.surface) {
            (Some(feature_app), _) => feature_app == app,
            (None, selector) => surface_ids.iter().any(|id| selector.names(id)),
        };
        if !of_app {
            continue;
        }
        if let Some(matchers) = &feature.matchers {
            for (key, list, kinds, template) in literal_matchers(matchers) {
                for value in list {
                    let found = kinds.iter().any(|kind| {
                        if template {
                            templates
                                .get(kind)
                                .is_some_and(|set| set.contains(&template_shape(value)))
                        } else {
                            values
                                .get(kind)
                                .is_some_and(|set| set.contains(value.as_str()))
                        }
                    });
                    if !found {
                        find(
                            Rule::MatcherNotInSource,
                            &feature.id,
                            format!("{key} {value:?} is written by no source file"),
                        );
                    }
                }
            }
        }
        if let Some(source) = &feature.source
            && !files.contains(cited_file(source))
        {
            find(
                Rule::CitationNotInSource,
                &feature.id,
                format!("cites {source}, which no route imports"),
            );
        }
    }
    for surface in &surfaces {
        if let Some(source) = &surface.source
            && !files.contains(cited_file(source))
        {
            find(
                Rule::CitationNotInSource,
                &surface.id,
                format!("cites {source}, which no route imports"),
            );
        }
    }

    let sent: HashSet<&str> = extract.events.iter().map(|e| e.name.as_str()).collect();
    for event in vocabulary.events.iter().filter(|e| e.app == app) {
        if !sent.contains(event.name.as_str()) {
            find(
                Rule::EventNotInSource,
                &event.name,
                "no source sends this event".into(),
            );
        }
    }
    findings.into_iter().collect()
}

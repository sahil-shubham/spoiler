use super::{Feature, Pattern, Surface, Vocabulary};
use crate::text::utf16_len;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A clicked or typed-into element, as matched against feature matchers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetDesc {
    pub tag: String,
    /// Visible text (empty for the page background: `<html>`/`<body>`).
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aria: Option<String>,
    /// `data-testid`, else `data-attr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// `title`, else `alt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `data-*` attributes, without the prefix.
    #[serde(default)]
    pub data: IndexMap<String, String>,
}

/// JS `new URL(href)`: pathname plus search; `href` itself when it is not an absolute URL.
pub fn path_and_query(href: &str) -> String {
    url::Url::parse(href).map_or_else(|_| href.to_owned(), |url| pathname_and_search(&url))
}

/// The pathname of a recorded `path?query`.
pub fn pathname(path: &str) -> &str {
    path.split_once('?').map_or(path, |(pathname, _)| pathname)
}

/// JS `new URL(href, "https://x")`: relative links resolve against a dummy origin.
fn href_path(href: &str) -> String {
    let base = url::Url::parse("https://x").expect("valid base URL");
    base.join(href)
        .map_or_else(|_| href.to_owned(), |url| pathname_and_search(&url))
}

/// `url.pathname + url.search`; JavaScript's `search` is empty for a bare `?`.
fn pathname_and_search(url: &url::Url) -> String {
    match url.query().filter(|query| !query.is_empty()) {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

/// Matching precedence: most specific key first.
#[derive(Clone, Copy, Debug)]
enum Tier {
    TestId,
    DataAttr,
    Aria,
    Title,
    Placeholder,
    Href,
    Text,
    Role,
    Class,
}

impl Tier {
    const ALL: [Tier; 9] = [
        Tier::TestId,
        Tier::DataAttr,
        Tier::Aria,
        Tier::Title,
        Tier::Placeholder,
        Tier::Href,
        Tier::Text,
        Tier::Role,
        Tier::Class,
    ];

    /// Text-like keys match the resolved target only: an ancestor container's text starts with
    /// whatever its first child says. Structural keys may match up the ancestor chain.
    fn target_only(self) -> bool {
        matches!(
            self,
            Tier::Aria | Tier::Title | Tier::Placeholder | Tier::Text
        )
    }
}

/// `data-row` / `data-col` alone are on every grid cell: they identify nothing.
fn is_positional(attribute: &str) -> bool {
    matches!(attribute, "row" | "col")
}

/// Visible text may carry a trailing shortcut hint rendered in a `<kbd>` ("Search notes Ctrl+ 1").
const MAX_SHORTCUT_SUFFIX: usize = 12;

fn text_matches(text: &str, expected: &str) -> bool {
    text == expected
        || text
            .strip_prefix(expected)
            .is_some_and(|rest| rest.starts_with(' ') && utf16_len(rest) - 1 <= MAX_SHORTCUT_SUFFIX)
}

/// `"Open {document}"` → `^Open .+$` (JavaScript's `.` excludes all line terminators).
fn template_regex(template: &str) -> Regex {
    static PLACEHOLDER: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\{[^}]+\}").expect("valid regex"));
    let pattern = PLACEHOLDER
        .split(template)
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"[^\n\r\u{2028}\u{2029}]+");
    Regex::new(&format!("^{pattern}$")).expect("escaped template is a valid regex")
}

/// Per-feature matchers prepared once.
struct CompiledFeature {
    aria_templates: Vec<Regex>,
    text_templates: Vec<Regex>,
    title_templates: Vec<Regex>,
    href_paths: Vec<String>,
}

/// A vocabulary compiled for lookups.
pub struct Matcher<'v> {
    vocabulary: &'v Vocabulary,
    /// Surface indexes with their route regexes, static routes before parameterized ones.
    routes: Vec<(usize, Regex)>,
    compiled: Vec<CompiledFeature>,
    /// Feature indexes that can match on each surface: the page's own features first, then its
    /// app's chrome, so a page control sharing a label with the top bar is never read as chrome.
    candidates: HashMap<&'v str, Vec<usize>>,
    error_text: &'v [Pattern],
}

/// Error language, when a vocabulary gives none.
static DEFAULT_ERROR_TEXT: std::sync::LazyLock<[Pattern; 1]> = std::sync::LazyLock::new(|| {
    [Pattern::new(
        r"(?i)\b(error|failed|failure|could not|couldn['’]t|unable to|went wrong|try again)\b",
    )
    .expect("valid regex")]
});

impl<'v> Matcher<'v> {
    pub fn new(vocabulary: &'v Vocabulary) -> Self {
        let mut routes: Vec<_> = vocabulary
            .surfaces
            .iter()
            .enumerate()
            .map(|(index, surface)| (index, route_regex(&surface.route)))
            .collect();
        // Stable: equal parameter counts keep file order. /items/create beats /items/:slug.
        routes.sort_by_key(|(index, _)| vocabulary.surfaces[*index].route.matches(':').count());

        let empty_matchers = super::Matchers::default();
        let compiled = vocabulary
            .features
            .iter()
            .map(|feature| {
                let matchers = feature.matchers.as_ref().unwrap_or(&empty_matchers);
                let templates = |list: &[String]| list.iter().map(|t| template_regex(t)).collect();
                CompiledFeature {
                    aria_templates: templates(&matchers.aria_template),
                    text_templates: templates(&matchers.text_template),
                    title_templates: templates(&matchers.title_template),
                    href_paths: matchers.href.iter().map(|h| href_path(h)).collect(),
                }
            })
            .collect();

        let mut candidates = HashMap::new();
        for surface in &vocabulary.surfaces {
            let app = surface.app.as_str();
            let features = &vocabulary.features;
            let on_page = features
                .iter()
                .enumerate()
                .filter(|(_, f)| !f.surface.is_app_chrome() && f.surface.names(&surface.id));
            let chrome = features
                .iter()
                .enumerate()
                .filter(|(_, f)| f.surface.is_app_chrome() && f.app.as_deref() == Some(app));
            let indexes = on_page.chain(chrome).map(|(index, _)| index).collect();
            candidates.insert(surface.id.as_str(), indexes);
        }

        let error_text = if vocabulary.error_text.is_empty() {
            &DEFAULT_ERROR_TEXT[..]
        } else {
            &vocabulary.error_text
        };
        Self {
            vocabulary,
            routes,
            compiled,
            candidates,
            error_text,
        }
    }

    /// Whether on-screen text reads as an error message.
    pub fn is_error_text(&self, text: &str) -> bool {
        self.error_text.iter().any(|pattern| pattern.is_match(text))
    }

    pub fn vocabulary(&self) -> &'v Vocabulary {
        self.vocabulary
    }

    /// The surface an app's pathname is on.
    pub fn surface(&self, app: &str, pathname: &str) -> Option<&'v Surface> {
        self.routes.iter().find_map(|(index, route)| {
            let surface = &self.vocabulary.surfaces[*index];
            (surface.app == app && route.is_match(pathname)).then_some(surface)
        })
    }

    /// The feature a target is, given the target first and then its ancestors.
    pub fn feature(&self, surface: Option<&str>, chain: &[TargetDesc]) -> Option<&'v Feature> {
        let candidates = self.candidates.get(surface?)?;
        for tier in Tier::ALL {
            let descs = if tier.target_only() {
                &chain[..chain.len().min(1)]
            } else {
                chain
            };
            for desc in descs {
                if let Some(&index) = candidates.iter().find(|&&i| self.matches(i, tier, desc)) {
                    return Some(&self.vocabulary.features[index]);
                }
            }
        }
        None
    }

    fn matches(&self, index: usize, tier: Tier, desc: &TargetDesc) -> bool {
        let Some(matchers) = &self.vocabulary.features[index].matchers else {
            return false;
        };
        let compiled = &self.compiled[index];
        let listed = |list: &[String], value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|v| list.iter().any(|item| item == v))
        };
        let templated = |templates: &[Regex], value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|v| templates.iter().any(|t| t.is_match(v)))
        };
        match tier {
            Tier::TestId => listed(&matchers.testid, &desc.testid),
            Tier::DataAttr => matchers.data_attr.iter().any(|spec| {
                let spec = spec.strip_prefix("data-").unwrap_or(spec);
                // `attr=value`: the value is everything after the first `=`.
                match spec.split_once('=') {
                    None if is_positional(spec) => false,
                    None => desc.data.contains_key(spec),
                    Some((name, value)) => desc.data.get(name).is_some_and(|v| v == value),
                }
            }),
            Tier::Aria => {
                listed(&matchers.aria, &desc.aria)
                    || templated(&compiled.aria_templates, &desc.aria)
            }
            Tier::Title => {
                listed(&matchers.title, &desc.title)
                    || templated(&compiled.title_templates, &desc.title)
            }
            Tier::Placeholder => listed(&matchers.placeholder, &desc.placeholder),
            Tier::Href => desc.href.as_deref().is_some_and(|href| {
                let path = href_path(href);
                compiled.href_paths.contains(&path)
            }),
            Tier::Text => {
                !desc.text.is_empty()
                    && (matchers.text.iter().any(|t| text_matches(&desc.text, t))
                        || compiled
                            .text_templates
                            .iter()
                            .any(|t| t.is_match(&desc.text)))
            }
            Tier::Role => listed(&matchers.role, &desc.role),
            Tier::Class => desc.classes.as_deref().is_some_and(|classes| {
                matchers
                    .class_contains
                    .iter()
                    .any(|c| classes.contains(c.as_str()))
            }),
        }
    }
}

/// Route template → anchored regex; `:param` segments match one path segment.
fn route_regex(route: &str) -> Regex {
    let body = route
        .split('/')
        .map(|segment| {
            if segment.starts_with(':') {
                "[^/]+".to_owned()
            } else {
                regex::escape(segment)
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    Regex::new(&format!("^{body}/?$")).expect("escaped route is a valid regex")
}

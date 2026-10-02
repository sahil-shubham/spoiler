//! `vocab extract`: an app's routes, literals and tracked events, read with a parser.
//!
//! Routes come from a React Router flat-routes directory; everything else from the files those
//! routes import, transitively (tsconfig paths and workspace packages resolved as the bundler
//! would). A literal belongs to every route whose import closure — its own module, its layout
//! routes and the root module — reaches the file it is written in.

use anyhow::{Context, Result, bail};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, AssignmentPattern, BindingPattern, CallExpression, ExportAllDeclaration,
    ExportDefaultDeclaration, ExportFromDeclaration, ExportNamedDeclaration, Expression,
    ImportDeclaration, ImportExpression, JSXAttributeItem, JSXAttributeName, JSXAttributeValue,
    JSXChild, JSXElement, JSXElementName, JSXExpression, JSXFragment, ModuleExportName,
    ObjectProperty, PropertyKey, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use oxc_span::SourceType;
use spoiler_core::{
    artifact::{Header, Kind, SourceDigest, sha256_hex},
    vocab::extract::{
        ExtractedEvent, ExtractedLiteral, ExtractedRoute, LiteralKind, VocabularyExtract,
    },
};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

const EXTENSIONS: [&str; 4] = ["tsx", "ts", "jsx", "js"];
/// Longest text child kept: longer text is prose, not a control's label.
const MAX_TEXT: usize = 120;

pub struct Request<'a> {
    pub app: &'a str,
    /// Paths in the extract are relative to this (usually the repository root).
    pub root: &'a Path,
    /// The flat-routes directory, e.g. `apps/web/app/routes`.
    pub routes: &'a Path,
}

/// One parsed source file.
#[derive(Default)]
struct Parsed {
    imports: Vec<String>,
    default_export: bool,
    literals: Vec<(LiteralKind, String, bool, u32)>,
    events: Vec<(String, String, u32)>,
}

pub fn extract(request: &Request<'_>) -> Result<VocabularyExtract> {
    let root = request
        .root
        .canonicalize()
        .with_context(|| format!("reading {}", request.root.display()))?;
    let routes_dir = root.join(request.routes);
    if !routes_dir.is_dir() {
        bail!("no routes directory at {}", routes_dir.display());
    }
    let modules = route_modules(&routes_dir)?;
    if modules.is_empty() {
        bail!("{} holds no route modules", routes_dir.display());
    }
    let root_module = routes_dir.parent().and_then(|app| {
        EXTENSIONS
            .iter()
            .map(|e| app.join(format!("root.{e}")))
            .find(|p| p.is_file())
    });

    let resolver = Resolver::new(ResolveOptions {
        extensions: EXTENSIONS.iter().map(|e| format!(".{e}")).collect(),
        condition_names: ["import", "module", "browser", "default"]
            .map(String::from)
            .to_vec(),
        tsconfig: Some(TsconfigDiscovery::Auto),
        ..ResolveOptions::default()
    });

    // Parse everything the route modules reach, breadth first.
    let mut parsed: BTreeMap<PathBuf, Parsed> = BTreeMap::new();
    let mut edges: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut queue: VecDeque<PathBuf> = modules
        .iter()
        .map(|(file, _)| file.clone())
        .chain(root_module.clone())
        .collect();
    while let Some(file) = queue.pop_front() {
        if parsed.contains_key(&file) {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let result = parse(&file, &text);
        let mut targets = Vec::new();
        for specifier in &result.imports {
            let Ok(resolution) = resolver.resolve_file(&file, specifier) else {
                continue;
            };
            let target = resolution.into_path_buf();
            if is_source(&root, &target) {
                targets.push(target.clone());
                queue.push_back(target);
            }
        }
        edges.insert(file.clone(), targets);
        parsed.insert(file, result);
    }

    let relative = |path: &Path| -> String {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let names: BTreeMap<&str, &PathBuf> = modules
        .iter()
        .map(|(file, name)| (name.as_str(), file))
        .collect();
    let mut routes = Vec::new();
    let mut closures: Vec<(String, BTreeSet<PathBuf>)> = Vec::new();
    for (file, name) in &modules {
        let (route, params) = route_path(name);
        let segments: Vec<&str> = name.split('.').collect();
        let mut layouts: Vec<&PathBuf> = root_module.iter().collect();
        layouts.extend(
            (1..segments.len())
                .filter_map(|i| names.get(segments[..i].join(".").as_str()).copied()),
        );
        let mut closure = BTreeSet::new();
        let mut stack: Vec<&PathBuf> = layouts.iter().copied().chain([file]).collect();
        while let Some(next) = stack.pop() {
            if closure.insert(next.clone()) {
                stack.extend(edges.get(next).into_iter().flatten());
            }
        }
        closures.push((route.clone(), closure));
        let renders = parsed.get(file).is_some_and(|p| p.default_export);
        let pathless = segments
            .last()
            .is_some_and(|last| last.starts_with('_') && *last != "_index");
        let has_index = names.contains_key(format!("{name}._index").as_str());
        routes.push(ExtractedRoute {
            route,
            params,
            file: relative(file),
            renders,
            page: renders && !pathless && !has_index,
            layouts: layouts.iter().map(|l| relative(l)).collect(),
        });
    }

    let mut literals = Vec::new();
    let mut events = Vec::new();
    let mut files = Vec::new();
    for (file, result) in &parsed {
        let name = relative(file);
        let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        files.push(SourceDigest {
            name: name.clone(),
            sha256: sha256_hex(&bytes),
        });
        let rendered_by: Vec<String> = {
            let mut set: BTreeSet<&str> = BTreeSet::new();
            for (route, closure) in &closures {
                if closure.contains(file) {
                    set.insert(route);
                }
            }
            set.into_iter().map(str::to_owned).collect()
        };
        let mut seen = BTreeSet::new();
        for (kind, value, template, line) in &result.literals {
            // One record per literal per file: the first place it is written.
            if !seen.insert((*kind, value.clone())) {
                continue;
            }
            let key = serde_json::to_string(kind).expect("kind serializes");
            literals.push(ExtractedLiteral {
                id: sha256_hex(format!("{key}\0{name}\0{value}").as_bytes())[..12].to_owned(),
                kind: *kind,
                value: value.clone(),
                template: *template,
                at: format!("{name}:{line}"),
                routes: rendered_by.clone(),
            });
        }
        for (call, event, line) in &result.events {
            events.push(ExtractedEvent {
                at: format!("{name}:{line}"),
                call: call.clone(),
                name: event.clone(),
            });
        }
    }
    routes.sort_by(|a, b| a.route.cmp(&b.route).then(a.file.cmp(&b.file)));
    literals.sort_by(|a, b| (a.kind, &a.value, &a.at).cmp(&(b.kind, &b.value, &b.at)));
    Ok(VocabularyExtract {
        header: Header::new(Kind::VocabularyExtract),
        app: request.app.to_owned(),
        routes_from: "react-router-flat-routes".into(),
        files,
        routes,
        literals,
        events,
    })
}

/// A file the extract reads: under the root, not a dependency, not a test.
fn is_source(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let text = relative.to_string_lossy();
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e))
        && !relative
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("node_modules" | "__tests__")))
        && !text.contains(".test.")
        && !text.contains(".spec.")
}

/// Route module files and their flat-route names: files directly in the directory, and
/// folders holding a `route.*` entry. Tests and `__`-prefixed folders are not routes.
fn route_modules(directory: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut modules = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(directory)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_file() {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if EXTENSIONS.contains(&extension)
                && !file_name.contains(".test.")
                && !file_name.contains(".spec.")
            {
                let stem = &file_name[..file_name.len() - extension.len() - 1];
                modules.push((path.clone(), stem.to_owned()));
            }
        } else if path.is_dir()
            && !file_name.starts_with("__")
            && let Some(entry) = EXTENSIONS
                .iter()
                .map(|e| path.join(format!("route.{e}")))
                .find(|p| p.is_file())
        {
            modules.push((entry, file_name.to_owned()));
        }
    }
    Ok(modules)
}

/// A flat-routes name as a path template: `_app.fundraise.$pipelineUri` → `/fundraise/:pipelineUri`.
/// Pathless layouts (`_x`), index routes and optional groups add no segment; `x_` escapes its
/// parent layout but keeps its segment; `$` alone is a splat.
pub fn route_path(name: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut params = Vec::new();
    for part in name.split('.') {
        if part == "_index" || part.is_empty() || (part.starts_with('(') && part.ends_with(')')) {
            continue;
        }
        if part.starts_with('_') {
            continue;
        }
        let part = part.strip_suffix('_').unwrap_or(part);
        if part == "$" {
            parts.push("*".to_owned());
        } else if let Some(param) = part.strip_prefix('$') {
            params.push(param.to_owned());
            parts.push(format!(":{param}"));
        } else {
            parts.push(part.to_owned());
        }
    }
    (format!("/{}", parts.join("/")), params)
}

fn parse(path: &Path, text: &str) -> Parsed {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path)
        .unwrap_or_default()
        .with_jsx(true);
    let program = Parser::new(&allocator, text, source_type).parse().program;
    let mut collector = Collector {
        lines: line_starts(text),
        parsed: Parsed::default(),
    };
    collector.visit_program(&program);
    collector.parsed
}

fn line_starts(text: &str) -> Vec<u32> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i as u32 + 1))
        .collect()
}

struct Collector {
    lines: Vec<u32>,
    parsed: Parsed,
}

impl Collector {
    fn line(&self, offset: u32) -> u32 {
        self.lines.partition_point(|&start| start <= offset) as u32
    }

    fn literal(&mut self, kind: LiteralKind, value: (String, bool), offset: u32) {
        let text = collapse(&value.0);
        if text.chars().count() < 2 || !text.chars().any(char::is_alphanumeric) {
            return;
        }
        let line = self.line(offset);
        self.parsed.literals.push((kind, text, value.1, line));
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A template literal as text with `{name}` per interpolation.
fn template(literal: &TemplateLiteral<'_>) -> (String, bool) {
    let mut text = String::new();
    for (index, quasi) in literal.quasis.iter().enumerate() {
        text.push_str(
            quasi
                .value
                .cooked
                .as_ref()
                .map_or(quasi.value.raw.as_str(), |c| c.as_str()),
        );
        if let Some(expression) = literal.expressions.get(index) {
            let name = match expression {
                Expression::Identifier(id) => id.name.as_str(),
                Expression::StaticMemberExpression(member) => member.property.name.as_str(),
                _ => "value",
            };
            text.push('{');
            text.push_str(name);
            text.push('}');
        }
    }
    (text, !literal.expressions.is_empty())
}

/// The strings an expression can evaluate to, through conditionals and `&&`/`||`/`??`:
/// `{busy ? "Saving" : "Save"}` writes both.
fn strings_in(expression: &Expression<'_>, out: &mut Vec<(String, bool)>) {
    match expression {
        Expression::StringLiteral(literal) => out.push((literal.value.to_string(), false)),
        Expression::TemplateLiteral(literal) => out.push(template(literal)),
        Expression::ConditionalExpression(conditional) => {
            strings_in(&conditional.consequent, out);
            strings_in(&conditional.alternate, out);
        }
        Expression::LogicalExpression(logical) => {
            strings_in(&logical.left, out);
            strings_in(&logical.right, out);
        }
        Expression::ParenthesizedExpression(inner) => strings_in(&inner.expression, out),
        _ => {}
    }
}

fn jsx_strings(expression: &JSXExpression<'_>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if let Some(expression) = expression.as_expression() {
        strings_in(expression, &mut out);
    }
    out
}

fn string_values(value: &JSXAttributeValue<'_>) -> Vec<(String, bool)> {
    match value {
        JSXAttributeValue::StringLiteral(literal) => vec![(literal.value.to_string(), false)],
        JSXAttributeValue::ExpressionContainer(container) => jsx_strings(&container.expression),
        _ => Vec::new(),
    }
}

/// Names that, on an object key or a variable, hold a label a component renders.
fn is_label_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "label",
        "placeholder",
        "title",
        "tooltip",
        "heading",
        "text",
    ]
    .iter()
    .any(|word| name == *word || name.ends_with(word))
}

fn element_name(name: &JSXElementName<'_>) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::MemberExpression(member) => member.property.name.to_string(),
        _ => String::new(),
    }
}

fn exports_default(name: &ModuleExportName<'_>) -> bool {
    match name {
        ModuleExportName::IdentifierName(name) => name.name == "default",
        ModuleExportName::IdentifierReference(name) => name.name == "default",
        ModuleExportName::StringLiteral(name) => name.value == "default",
    }
}

impl<'a> Visit<'a> for Collector {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.parsed.imports.push(it.source.value.to_string());
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        if it.specifiers.iter().any(|s| exports_default(&s.exported)) {
            self.parsed.default_export = true;
        }
        walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        self.parsed.imports.push(it.source.value.to_string());
        if it.specifiers.iter().any(|s| exports_default(&s.exported)) {
            self.parsed.default_export = true;
        }
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.parsed.imports.push(it.source.value.to_string());
    }

    fn visit_export_default_declaration(&mut self, it: &ExportDefaultDeclaration<'a>) {
        self.parsed.default_export = true;
        walk::walk_export_default_declaration(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &it.source {
            self.parsed.imports.push(source.value.to_string());
        }
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = &it.callee {
            let call = member.property.name.as_str();
            if matches!(call, "capture" | "tryAddCustomEvent")
                && let Some(Argument::StringLiteral(name)) = it.arguments.first()
            {
                let line = self.line(it.span.start);
                self.parsed
                    .events
                    .push((call.to_owned(), name.value.to_string(), line));
            }
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_jsx_element(&mut self, it: &JSXElement<'a>) {
        let opening = &it.opening_element;
        let component = element_name(&opening.name)
            .chars()
            .next()
            .is_some_and(char::is_uppercase);
        for item in &opening.attributes {
            let JSXAttributeItem::Attribute(attribute) = item else {
                continue;
            };
            let JSXAttributeName::Identifier(name) = &attribute.name else {
                continue;
            };
            let Some(value) = &attribute.value else {
                continue;
            };
            for value in string_values(value) {
                let kind = match name.name.as_str() {
                    "data-testid" | "testId" => LiteralKind::Testid,
                    "aria-label" | "ariaLabel" => LiteralKind::Aria,
                    "placeholder" => LiteralKind::Placeholder,
                    "title" if !component => LiteralKind::Title,
                    "href" | "to" if value.0.starts_with('/') || value.0.starts_with("http") => {
                        LiteralKind::Href
                    }
                    "title" | "label" | "tooltip" | "heading" | "text" if component => {
                        LiteralKind::Prop
                    }
                    _ => continue,
                };
                self.literal(kind, value, attribute.span.start);
            }
        }
        self.children(&it.children);
        walk::walk_jsx_element(self, it);
    }

    fn visit_jsx_fragment(&mut self, it: &JSXFragment<'a>) {
        self.children(&it.children);
        walk::walk_jsx_fragment(self, it);
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if let PropertyKey::StaticIdentifier(key) = &it.key
            && is_label_name(&key.name)
        {
            let mut values = Vec::new();
            strings_in(&it.value, &mut values);
            for value in values {
                self.literal(LiteralKind::Prop, value, it.span.start);
            }
        }
        walk::walk_object_property(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &it.id
            && is_label_name(&id.name)
            && let Some(init) = &it.init
        {
            let mut values = Vec::new();
            strings_in(init, &mut values);
            for value in values {
                self.literal(LiteralKind::Prop, value, it.span.start);
            }
        }
        walk::walk_variable_declarator(self, it);
    }

    /// A default value: `function Search({ searchPlaceholder = "Search firms" })`.
    fn visit_assignment_pattern(&mut self, it: &AssignmentPattern<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &it.left
            && is_label_name(&id.name)
        {
            let mut values = Vec::new();
            strings_in(&it.right, &mut values);
            for value in values {
                self.literal(LiteralKind::Prop, value, it.span.start);
            }
        }
        walk::walk_assignment_pattern(self, it);
    }
}

impl Collector {
    /// Text an element or fragment renders: text children, and the strings its `{…}`
    /// children can evaluate to.
    fn children(&mut self, children: &[JSXChild<'_>]) {
        for child in children {
            match child {
                JSXChild::Text(text) if text.value.trim().chars().count() <= MAX_TEXT => {
                    self.literal(
                        LiteralKind::Text,
                        (text.value.to_string(), false),
                        text.span.start,
                    );
                }
                JSXChild::ExpressionContainer(container) => {
                    for value in jsx_strings(&container.expression) {
                        if value.0.trim().chars().count() <= MAX_TEXT {
                            self.literal(LiteralKind::Text, value, container.span.start);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn flat_route_names_become_path_templates() {
        let cases = [
            ("_app", "/"),
            ("_app._index", "/"),
            ("_app.fundraise.$pipelineUri", "/fundraise/:pipelineUri"),
            ("_app.activities_.$uri", "/activities/:uri"),
            (
                "_app.utils.session-replays_.$uri.recording",
                "/utils/session-replays/:uri/recording",
            ),
            ("files.$", "/files/*"),
            ("($lang).about", "/about"),
            ("login", "/login"),
        ];
        for (name, path) in cases {
            assert_eq!(route_path(name).0, path, "{name}");
        }
        assert_eq!(route_path("_app.a.$id.b.$key").1, ["id", "key"]);
    }

    #[test]
    fn literals_imports_and_events_are_read_from_jsx() {
        let source = r#"
            import { Button } from "@/ui/button";
            export { Thing } from "./thing";
            const Lazy = () => import("./lazy");
            export default function Page({ doc, busy, wiki, emptyText = "Nothing yet" }) {
              posthog.capture("doc.opened");
              const searchPlaceholder = wiki ? "Search this wiki" : "Search everything";
              const options = [{ value: "strong", label: "Know well" }, { value: "x", id: "skip me" }];
              return (
                <main title="Workspace">
                  <Button label="Save draft" data-testid="save" />
                  <input placeholder="Search people" aria-label={`Open ${doc.name}`} />
                  <a href="/inbox">Inbox</a>
                  <p>{doc.name}</p>
                  <span>
                    Many   words here
                  </span>
                  <i>x</i>
                  <button>{busy ? "Regenerating" : "Regenerate"}</button>
                  {busy ? <>Refreshing</> : <>Start refresh</>}
                </main>
              );
            }
        "#;
        let parsed = parse(Path::new("page.jsx"), source);
        assert_eq!(parsed.imports, ["@/ui/button", "./thing", "./lazy"]);
        assert!(parsed.default_export);
        assert_eq!(
            parsed.events,
            [("capture".to_owned(), "doc.opened".to_owned(), 6)]
        );
        let literals: Vec<(LiteralKind, &str, bool)> = parsed
            .literals
            .iter()
            .map(|(kind, value, template, _)| (*kind, value.as_str(), *template))
            .collect();
        use LiteralKind::*;
        assert_eq!(
            literals,
            [
                (Prop, "Nothing yet", false),
                (Prop, "Search this wiki", false),
                (Prop, "Search everything", false),
                (Prop, "Know well", false),
                (Title, "Workspace", false),
                (Prop, "Save draft", false),
                (Testid, "save", false),
                (Placeholder, "Search people", false),
                (Aria, "Open {name}", true),
                (Href, "/inbox", false),
                (Text, "Inbox", false),
                (Text, "Many words here", false),
                (Text, "Regenerating", false),
                (Text, "Regenerate", false),
                (Text, "Refreshing", false),
                (Text, "Start refresh", false),
            ],
            "a one-letter text, an interpolated child and a non-label key are not labels"
        );
    }
}

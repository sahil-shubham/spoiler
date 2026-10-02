# Vocabulary Plan

Owner: Sahil · Scope: `crates/core/src/vocab`, `crates/cli` (`vocab build`, `vocab check`, a new `vocab extract`), `crates/core/src/analysis/prompt.rs` · First consumer: SPC (`spc: omni/replays/vocab/{pulse,pando}.yaml`)
Status: § 1 (`vocab extract`) and § 4 (`vocab check --extract`) built for React Router flat routes (release 0.1.2); drafting from the extract (§ 2), extract-backed matching (§ 3) and the field cleanup not started · Design source: SPC's two pinned vocabularies, their source tree, and 20 PostHog recordings compiled against them, 2026-10-01
Citations: paths are this repo unless prefixed `spc:`.

The vocabulary is the only place Spoiler learns what a product *is*: its pages, its controls, its words. Today a model writes it from a lossy digest, half of it is never read, and on SPC it names almost nothing. Its readers today are the compiler and the narrator; its value is wider than both (§ Applications).

---

## 0. What the ground truth changes

Survives unchanged: the snapshot artifact and its pinning (content digest, source digests, prompt identity), surface routing, the matcher tiers, grid rules.

1. **On SPC the vocabulary names almost no controls.** 20 recordings (10 Pulse, 10 Pando, 20–28 Sep 2026) compiled against the pinned vocabularies:

   | | navigations on a surface | gestures with a feature | features ever matched |
   |---|---|---|---|
   | Pulse | 79 / 86 | **12 / 841 (1.4 %)** | 3 of 24 |
   | Pando | 46 / 46 | 47 / 331 (14 %) | 16 of 50 |

   Surfaces work; features do not. The TSV's `feature` column is empty for 98.6 % of Pulse gestures, so the narrator names controls from rendered text, and every downstream grouping keys on prose.

2. **The model restates facts code already has, and loses some.** SPC builds the model's input with its own regex extractor (`spc: omni/replays/management/commands/replays_vocab.py:19-174`). It already computes every route deterministically, `:param` segments included (`:31-52`), then caps each Pulse route at 28 literal signals (`:135`) and pleads with the model: "Make one surface for EVERY route … don't stop after covering representative pages" (`:127-128`). The answer has 95 Pulse surfaces for 149 route modules with a default export (UI routes; approximate, layouts included). Its `gaps` contain "No digest or source files were supplied for the 'pando' application" — inside the *Pulse* vocabulary.

   The regex reads only one-line string literals in route files and their co-located components. Of Pulse's 431 `aria-label` attributes, 88 are template literals and 58 other expressions, so 150 (35 %) are invisible to it; and 271 of 420 non-test `aria-label`s, 174 of 351 `placeholder`s and 26 of 30 `data-testid`s live outside `app/routes/` (mostly `app/components/`), reached only through a route's imports.

3. **The source has the identifiers; the vocabulary does not.** Pulse source has 431 `aria-label`, 352 `placeholder` and 43 `data-testid` attributes; Pulse's vocabulary uses 4 `aria`, 5 `placeholder` and 2 `testid` matchers. In the 20 recordings, gestures whose target carries an aria-label, placeholder, title or testid **that appears verbatim in the app's source**, or a short text label that does: Pulse 354 / 841 (42 %), Pando 161 / 331 (49 %) — against 1.4 % and 14 % named today. [INFERENCE: a substring test over source text; it bounds what literal extraction can name, not what it will.]

4. **The vocabulary drifts the day it is built.** Both SPC vocabularies pin `source_revision: 1a447c8af` (2026-09-29). Two days later, 14 commits had touched 22 Pulse route files. Nothing reports drift: source digests are recorded, never compared (`crates/core/src/artifact.rs:379-392`).

5. **Ten schema fields have no reader.** Parsed, digested, never used: `excluded_surfaces`, `Surface.states`, `Surface.source`, `Feature.events`, `Feature.source`, `Term.backend`, `Term.source`, `Status.extra`, `events`, `gaps` — and the 12 subfields of `ExcludedSurface`, `SurfaceState` and `ProductEvent` (`crates/core/src/vocab/mod.rs:26-38`, `:213-241`, `:274-277`, `:320-342`). The custom-event compiler is hardcoded to four tags (`crates/core/src/trace/compiler/handlers.rs:581-608`), so declared `events` cannot match anything. SPC's vocabularies declare 11 and 12 of them.

6. **The narrator gets less than the vocabulary says.** The schema says "Backend-only values (no UI label) are legitimate: the narrator still needs their meaning" (`mod.rs:32`); the glossary drops every status whose label is not a substring of the session's rendered targets and effects (`crates/core/src/analysis/prompt.rs:116-121`). `Term.backend` and `gaps` never reach it. Surface and feature *names* are not TSV columns (`crates/core/src/trace/render.rs:190-228`); the model joins ids to the glossary itself.

7. **`vocab check` checks almost nothing.** Two hard failures (version, duplicate surface id) and one warning, chrome without an app (`mod.rs:390-419`). The README says it "reports invalid matchers and inert entries" (`README.md:260`). No check for dangling surface ids, matchers that match no source, routes without surfaces, stale surfaces, or citations that resolve to nothing; `source` strings are free text (`crates/cli/src/vocab_build.rs:26-84`).

8. **Eight of twelve matcher kinds have no matching test.** `aria`, `title`, `placeholder`, `href`, `role`, `class_contains`, `text_template` and `title_template` are covered only by `the_prompt_lists_every_matcher_key` (`crates/core/src/vocab/build.rs:65-102`), which keeps the prompt's list in sync with the struct and matches nothing. SPC's two vocabularies use four of those eight (`aria`, `title`, `placeholder`, `href`) in 44 feature matcher lists. SPC uses six kinds in all; none uses `data_attr`, `role`, `class_contains` or any template.

---

## Current State

**Schema.** `Vocabulary { version, apps, surfaces, excluded_surfaces, features, terms, statuses, events, gaps, grid, telemetry, error_text, thresholds }` (`crates/core/src/vocab/mod.rs:21-52`). `Surface.route` is a template whose `:name` segments match one segment (`:208-209`); `Feature.matchers` holds twelve string-list keys, most specific first (`:282-312`).

**Build.** `vocab build` sends the config and the whole text of each `--source` to a model with a 33-line instruction (`crates/core/prompts/vocabulary/system.md`), as `ResponseFormat::JsonObject`, not a strict schema (`crates/cli/src/vocab_build.rs:56-70`). The answer must parse and declare exactly the config's apps (`crates/core/src/vocab/build.rs:47-55`); nothing else about it is checked. `--candidate` skips the model.

**Match.** Routes compile to anchored regexes, static before parameterized; parameters are matched, not captured (`crates/core/src/vocab/matcher.rs:152-162`, `:302-316`). A target matches by tier first, then candidate order — the page's features, then its app's chrome — with text-like tiers on the resolved target only and structural tiers up its ancestors (`:226-242`, `:87-95`). An unknown route prints its raw pathname as the TSV `surface` and can match no feature (`crates/core/src/trace/mod.rs:377-382`).

**Narrate.** The glossary lists visited surfaces with route, name and purpose; used features with name and note; every term; statuses whose labels were seen (`prompt.rs:71-139`). The prompt tells the narrator to "analyze every page, including paths it does not name" (`crates/core/prompts/narrate/system.md`).

**Validate.** A narrated step's `feature` must be a vocabulary feature id (`crates/core/src/analysis/validate.rs:34`).

---

## Problem: The Vocabulary Is a Model's Paraphrase of Facts Code Can Read

Routes, route parameters, which routes render UI, which files a route renders, and every literal `aria-label`, `placeholder`, `title`, `data-testid`, `href` and text child are syntax. Asking a model to read them back produces a truncated, unverifiable copy that is stale on the next commit and cannot be checked in CI. The model belongs where syntax runs out: names, purposes, domain terms.

## Problem: Half the Schema Is Write-Only

A field ships with a reader, or it does not ship. Ten fields are written by models and people, pinned by digest, and read by nothing. They cost prompt tokens to generate, look like they mean something to an author, and change a snapshot's digest without changing a trace.

---

## Design

Identity is extracted, meaning is drafted, both are checked.

```text
app source ─▶ vocab extract ─▶ extract ─┬─▶ vocab build ─▶ vocabulary ─▶ compile / narrate / …
              (code: oxc)               │   (model: names, purposes, terms;
                                        │    may cite only extract entries)
                                        └─▶ vocab check --extract ─▶ CI
```

### 1. `vocab extract` (code)

Parses the app's JS/TS/JSX/TSX with `oxc_parser` (Rust; the same parser the oxc tools use) and writes a `vocabulary_extract` artifact. No model, no network, deterministic for a source tree.

```rust
/// What an app's source says about its pages and controls, read by a parser.
pub struct VocabularyExtract {
    #[serde(flatten)]
    pub header: Header,
    /// The adapter that turned files into routes (e.g. "react-router-flat-routes").
    pub routes_from: String,
    pub source_revision: Option<String>,
    /// Every file read, by repository path and SHA-256: what `vocab check` compares for drift.
    pub files: Vec<SourceDigest>,
    pub routes: Vec<ExtractedRoute>,
    pub controls: Vec<ExtractedControl>,
    pub events: Vec<ExtractedEvent>,
}

pub struct ExtractedRoute {
    /// `/fundraise/:pipelineUri`, in the vocabulary's route syntax.
    pub route: String,
    pub params: Vec<String>,
    pub file: String,
    /// Has a default export: a page, not a loader/action-only resource route.
    pub renders: bool,
    /// Files whose JSX this route renders, through its static imports.
    pub renders_files: Vec<String>,
}

pub struct ExtractedControl {
    /// `file:line`, the citation every vocabulary entry built from it carries.
    pub at: String,
    pub tag: String,
    pub kind: LiteralKind,
    /// Exact literal, or a template with `{name}` for interpolated parts:
    /// aria-label={`Open ${doc.name}`} → "Open {name}".
    pub value: String,
    /// Routes that render this file. One route: page control. Every route of an app: chrome.
    pub routes: Vec<String>,
}

pub enum LiteralKind { Testid, DataAttr, Aria, Title, Placeholder, Href, Text }

pub struct ExtractedEvent {
    pub at: String,
    /// `posthog.capture("…")` or a replay custom-event tag passed to `tryAddCustomEvent`.
    pub call: String,
    pub name: String,
}
```

- **Routes need an adapter; controls do not.** File-convention routing is framework-specific; JSX attributes and text children are JSX everywhere. One adapter ships: React Router flat routes, the convention `replays_vocab.py:31-52` implements. A `routes.ts` config adapter is the next one; each is a function from files to `ExtractedRoute`s.
- **Imports, not co-location, scope a control.** oxc returns module import/export information with the AST; `renders_files` is the static import closure of a route module. This replaces SPC's "co-located components" heuristic and its per-file signal caps (`replays_vocab.py:135-146`).
- **Templates come from template literals.** The four `*_template` matcher kinds stop being model guesses: the parser sees the interpolation.

**What the import closure reaches.** A route's controls are the literals in every module its route file imports, transitively, plus its layout routes' — so chrome is whatever a layout owns, found structurally instead of drafted as `surface: "*"`.

| case | reached? | SPC today |
|---|---|---|
| static `import` through tsconfig aliases and workspace packages | yes — resolved with `oxc_resolver` (tsconfig `paths`, `package.json` `exports`) | Pulse aliases `@/*`, `@components/*`, `@api/*` and four more (`spc: apps/pulse/tsconfig.json`); `@spc/ui`, `@spc/sheet` |
| dynamic `import("…")` with a literal specifier | yes — an edge like any import | every one of SPC's 224 client-side `import(…)` calls has a literal specifier; none is computed |
| layout routes rendering children through `<Outlet>` | yes — through the adapter's route nesting | `_app.jsx` wraps every `_app.*` page |
| labels passed as props (`<IconButton label="Close">` rendering `aria-label={label}`) | yes, one level: a string literal at the call site joins the attribute the component binds the prop to | 1,315 literal `label`/`title`/`tooltip`/`ariaLabel` props in Pulse |
| registry lookups (`registry[slug]`) | over-approximated: every registered component belongs to the route | Pando's cohort experiences (`spc: omni/replays/management/commands/replays_vocab.py:147-155` special-cases it today) |
| conditionally rendered or role-gated UI | over-approximated: present in the extract, matched only when it appears | modals, tabs, staff-only controls |
| labels from data or i18n at runtime | no | names, record titles — runtime only, as today |

Over-approximation costs nothing at match time — a matcher fires only on an element the recording shows. Its one cost is ambiguity: a bare text literal like `Save` in ten components on one surface names ten candidates. Attribute literals (`aria-label`, `placeholder`, `data-testid`) rarely collide; text literals get the ancestor chain from the trace's target to choose between them, and `vocab check` reports the collisions that remain.

**What is React Router, what is JSX, what is SPC.** Spoiler's core has no dependency on any of the three: the vocabulary is a route syntax (`:param`) plus literals, matched against recorded URLs and DOM (`crates/` has no mention of React Router, Pulse or Pando). After this plan:

| layer | tied to | lives in |
|---|---|---|
| route discovery, layout nesting, page vs resource route, query-state hooks | the router: React Router flat routes first (`spc: apps/pulse/app/routes.jsx` is `flatRoutes()`) | one adapter per router; `:param` is the output syntax whatever the input (`[param]` for Next becomes `:param`) |
| module resolution | TypeScript/Node conventions, not a framework | `oxc_resolver` |
| attribute, text and prop literals | JSX (React, Preact, Solid alike); not Vue or Svelte templates | the extractor core |
| SPC specifics (aliases, `@spc/*` packages, Pando's registry) | the app's own config files | read from `tsconfig.json` and `package.json`; nothing hardcoded |

SPC's coupling today lives outside Spoiler, in `replays_vocab.py`'s hand-written flat-routes parser, path list and Pando special case; the extractor replaces it.

### 2. `vocab build` (model, constrained)

Input is the extract, not raw source. The model writes names, purposes, terms, notes, and groups literals into features. Code then enforces:

- every surface route is an extracted `renders` route; every UI route has a surface (or an exclusion with a reason);
- every matcher value is an extracted literal reachable from the feature's surface;
- every `source` is an extract `at`.

A violation is a rejected answer with the existing one-retry pattern, not a warning. `--candidate` runs the same rules. Surfaces need no model at all: an extract-only vocabulary (`vocab build --extract X` without `--model`) has every UI route as a surface named by its route, every literal as a feature, and no terms — immediately usable, then improved by drafting.

### 3. Matching that uses the extract

- **Every literal is matchable.** A gesture whose target carries an extracted literal on its surface resolves to a feature even when no drafted feature lists it: id `<surface>[<kind>="<value>"]`, e.g. `pulse.fundraise_pipeline_detail[aria="Add investor"]`. Drafted features rename and group these; they never decide whether a control has an identity. This is what moves finding 1 toward finding 3.
- **Route parameters are captured.** `route_regex` gains named groups; the action keeps its raw `path` in JSON and the TSV prints the template with parameter names (`/fundraise/:pipelineUri`). Record ids stop reaching the narrator, so they stop reaching summaries.
- **Names are in the TSV.** The `surface` and `feature` cells print the name beside the id when one is drafted, so the narrator does not join against the glossary.

### 4. `vocab check` as a CI gate

`vocab check --vocab V --extract X [--strict]` adds, each with a fixed message and a count in the report:

| rule | catches |
|---|---|
| UI route with no surface and no exclusion | a new page the narrator will see as a raw path |
| surface route not among extracted routes | a deleted or renamed page |
| matcher value not an extracted literal on its surface | a renamed label; an invented matcher |
| `source` not an extract `at` | an unverifiable citation |
| one literal in two features on one surface | ambiguity resolved silently by file order |
| a file's digest differs from the vocabulary's `provenance.sources` | drift since the vocabulary was built |
| a declared event never extracted | a renamed or removed tracking call |

`--strict` exits non-zero on any of them. In CI this runs on every pull request against a fresh extract; the vocabulary is rebuilt when it fails, instead of when someone remembers.

### Every field gets a reader or goes

| field | today | decision |
|---|---|---|
| `excluded_surfaces` | no reader | **wire**: silences "UI route with no surface" for that route, with its `reason` in the report |
| `Surface.states` (+5 subfields) | no reader | **delete**; query-string state is a separate design ("States" below) |
| `Surface.source`, `Feature.source`, `Term.source` | no reader | **wire**: must be an extract `at` |
| `events`, `Feature.events` (+3 subfields) | no reader; compiler hardcodes tags | **wire as declared replay custom events**: `{ tag, meaning }`; the compiler emits a `custom` action for a declared tag (`docs/PLAN-timeline.md` keeps PostHog's own tags) |
| `Term.backend` | no reader | **wire**: glossary prints `backend` beside `means` |
| `Status.extra` | no reader | **delete** the flatten; `kind`, `value`, `label` stay |
| statuses without a label | dropped from the glossary | **wire**: label-less statuses are listed when their `value` appears in a request path or effect, as the schema comment promises |
| `gaps` | no reader | **wire**: in the check report; not the prompt |
| `class_contains` | no use on SPC | **delete**: classes are styling (Tailwind, component libraries), never identity; the extract has no literal kind for it |
| `role` | no use on SPC | **keep for grids only** (`role=gridcell` etc. already drive grid reading); delete as a feature matcher — a role names a kind, not a control |

Deletions bump `Vocabulary.version` 1 → 2; `parse` keeps reading version 1 by mapping wired fields and rejecting the deleted ones with a message naming them.

### Applications

Readers that exist, or that this plan adds. Each needs only a vocabulary and traces.

| use | reads | output |
|---|---|---|
| compile | surfaces, features, grid, thresholds, telemetry, error text | action `surface`/`feature`, flags |
| narrate and validate | names, purposes, terms, statuses | glossary; feature-id gate |
| redaction | route params | templates in the TSV instead of record ids |
| stable signatures | feature ids | a key that survives label and layout changes, for grouping the same problem across sessions (a rendered label is prose and drifts) |
| drift gate | extract digests and literals | CI failures on routes, labels, citations |
| usage | surfaces and features over many traces | which pages and controls are used, by whom, and which never are — dead UI is a product finding |
| instrumentation gaps | gestures with no extracted literal | the controls that need a label or testid to be nameable at all |
| player labels | surface names | tab labels and markers in a replay UI |

### Format: two files, one for machines and one for people

Maximum coverage makes the extract large and machine-shaped; a reviewer should never read it. The human-edited vocabulary stays small. They are separate files with separate owners:

| file | written by | shape | read by |
|---|---|---|---|
| `<app>.extract.jsonl` | `vocab extract`, never by hand | one record per line, sorted by (kind, route, value), each with a stable id `sha256(kind, route or file, value)[..12]` | `vocab build`, `vocab check`, `compile` |
| `<app>.vocab.yaml` | the model's draft, then people | the current schema, where every matcher and citation is an extract id or literal it names | everything the vocabulary feeds today |

- **Line-per-record, sorted, stable ids** make a source change show up as the lines it changed in a pull request — a renamed button is one removed and one added line, not a rewritten file.
- **Size.** SPC's Pulse source has 431 `aria-label`, 352 `placeholder` and 43 `data-testid` literals plus visible text; a few thousand records at ~200 bytes is under a megabyte, parsed once per `compile`.
- **Lookup is a hash, not a scan.** `Matcher` today tries tiers × candidates per target (`crates/core/src/vocab/matcher.rs:226-242`). With the extract it builds one `HashMap<(surface, kind, value), feature>` at load and resolves a target in one probe per tier; drafted features override extract ids in that map.
- **The snapshot pins both.** `VocabularySnapshot.provenance` gains the extract's digest, so a trace names exactly which extract and which overlay produced its feature ids.

**As built (0.1.2).** `spoiler vocab extract` writes one JSON artifact (`kind: vocabulary_extract`) whose `files`, `routes`, `literals` and `events` arrays hold one record per line, not a separate `.jsonl`: every command still writes one JSON artifact. Measured on SPC: Pulse 931 files, 312 routes (134 pages), 8,983 literals in 0.8 s; Pando 436 files, 94 routes (50 pages) in 0.2 s. Beyond JSX attributes and text, literals come from conditional and `&&`/`||` branches (`{busy ? "Saving" : "Save"}`), fragments, label-named object keys, variables and default values (`searchPlaceholder = "…"`), recorded as `prop`: the first SPC run without them reported six matchers as missing that the source does write. Pages exclude pathless layouts and layouts whose `_index` child is the page. Against SPC's pinned vocabularies the check finds, all real: 41 Pulse pages with no surface, 2 surfaces and 3 citations to removed routes, 1 Pando matcher of the wrong kind (`placeholder` for a command item's text), and 23 declared events that are backend events no frontend sends. The extract is not yet read by `vocab build` or the matcher (§ 2, § 3).

### What does NOT get built

- **No bundler loader or build plugin.** Extraction reads source files; it does not hook a build. A loader is needed only to stamp identifiers into rendered DOM, which extracted literals make unnecessary for every control that already has one.
- **No npm distribution in this plan.** The extractor is part of the Rust CLI.
- **No adapters beyond React Router flat routes.** One adapter per real consumer.
- **No model reading raw source.** It reads the extract; that is what makes its answer checkable.
- **No crawling built pages.** A headless DOM crawl would find runtime-only labels at the cost of a browser and auth in CI; extracted literals plus runtime traces cover the same ground.
- **No CSS-class identity.** Deleted, above.

---

## Native (iOS/Android)

Native support stays, in place, as a first-class path: `crates/core/src/recording/mobile.rs` (1,337 lines), its 16 of 60 corpus cases and 10 tests in `crates/core/tests/compile.rs`, and the README's promise (`README.md:7`, `:265-267`). SPC not exercising it says nothing about its value to Spoiler. What this plan owes it:

- Native routes are screen names (`matcher.rs:35-57`, `corpus/mobile_meta_screen_change.*`); the extract's route adapter is per platform, so a Swift/Kotlin adapter is an addition, not a redesign.
- The timeline's presence and fidelity apply unchanged: wireframe frames are full snapshots, screenshot-only content is already reported as opaque.

Two items are not native-specific and go: the `matchers` key-list test (`build.rs:65-102`) disappears when matchers come from the extract, and `SurfaceState` (`mod.rs:219-231`) is deleted above.

### States

SPC's pages carry meaning in the query string (`?session=`, `?status=`, `?tab=`). `pathname()` drops it before surface lookup (`crates/core/src/vocab/matcher.rs:40-43`). The extract can find `useSearchParams` keys per route; whether a query key makes a sub-surface, a state on an action, or nothing is its own design, and `Surface.states` is deleted rather than kept for it.

---

## Open decisions

1. ~~**MSRV.**~~ Decided: `rust-version` is 1.96, the extractor lives in the CLI crate, and `spoiler-core` (types and checks) takes no oxc dependency.
2. **Literal features by default.** Naming every extracted literal maximizes coverage and makes feature ids long; naming only drafted features keeps ids curated and coverage where it is.
3. **Vocabulary v2 now or wire-only first.** With `N-1` readers (`docs/PLAN-versioning.md`) a v1 file keeps loading after v2 ships, so deletion no longer forces every consumer to move at once; the remaining choice is whether wiring and deletion ship in one release.
4. **Where the extract lives.** Committed in the app repo next to the vocabulary (reviewable diffs, CI compares), or generated in CI only (no churn, no history). The artifact is one record per line either way, and on SPC's Pulse it is 7,000 lines for 931 files.

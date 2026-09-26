# Spoiler

Spoiler turns PostHog/rrweb session recordings into evidence you can inspect: it decodes recording data, replays browser DOM or native mobile wireframes, compiles user actions and visible effects into a deterministic trace with friction signals, and prepares or validates LLM narration through OpenRouter. A pinned product vocabulary supplies names and matching rules, so the same recording and vocabulary produce the same trace. The Rust core has no HTTP, database, or clock dependency; the CLI handles files and network access.

## Install

Requires Rust 1.88 or later. Install the CLI from GitHub:

```sh
cargo install --git https://github.com/sahil-shubham/spoiler spoiler
```

Or build from source (the repository pins a Rust toolchain):

```sh
git clone https://github.com/sahil-shubham/spoiler.git
cd spoiler
cargo build --release --locked
# Binary: target/release/spoiler
```

## Quick start (offline)

From the repository root, these commands use only committed recording, vocabulary, and response examples. Generated artifacts go under ignored `artifacts/`; no account or API key is needed.

```sh
cargo build --release --locked
mkdir -p artifacts
./target/release/spoiler compile \
  --recording corpus/click_changes_text.json --vocab corpus/vocabulary.yaml \
  --app demo --out artifacts/trace.json
./target/release/spoiler analyze \
  --trace artifacts/trace.json --vocab corpus/vocabulary.yaml \
  --prepare-only --out artifacts/request.json
./target/release/spoiler analyze \
  --trace artifacts/trace.json --vocab corpus/vocabulary.yaml \
  --response examples/click_changes_text.response.json --out artifacts/analysis.json
./target/release/spoiler vocab check --vocab corpus/vocabulary.yaml
```

`artifacts/trace.json` contains actions, effects, visits, coverage, and a TSV view. `artifacts/request.json` shows the messages and response schema an online model would receive. The example response demonstrates offline validation of trace citations; it is not a model-generated answer.

## Commands

Every input path accepts `-` for standard input, so commands compose with pipes:

```sh
spoiler recordings fetch --project 123 --session SESSION_ID \
  | spoiler compile --recording - --vocab vocab.yaml --app web \
  | spoiler analyze --trace - --vocab vocab.yaml --prepare-only
```

- `spoiler run` does all of that in one step: it fetches a recording (`--session ID --project ID`) or reads one (`--recording FILE|-`), compiles it, and narrates every visit with user gestures (`--visit N` for just one). It writes one `session` artifact holding the recording's `source`, the `trace`, and per visit either an `analysis` (`--model`) or a `request` (`--prepare-only`). `--save-recording FILE` and `--save-trace FILE` also write the intermediate artifacts, byte for byte what `recordings fetch` and `compile` would write, so analyses' `trace_digest` matches the saved trace. A failed visit fails the whole run; nothing partial is written.
- `spoiler decode --recording FILE [--out FILE]` normalizes a recording to a versioned recording artifact. Inputs include decoded rrweb event arrays, JSONL, compressed inputs, and PostHog snapshot lines.
- `spoiler compile --recording FILE --vocab FILE --app APP [--out FILE]` creates a trace without network access. `--timings` prints stage timings on stderr.
- `spoiler analyze --trace FILE --vocab FILE` prepares the model request (`--prepare-only`), validates an existing answer (`--response FILE`), or calls a model (`--model MODEL`); the first two take precedence over a model. Alternatively supply `--recording FILE --app APP` instead of `--trace`; `--visit N` selects one visit (zero-based). `--context FILE` adds a session header and `--system-prompt FILE` replaces the narration instructions. A model call requires `OPENROUTER_API_KEY` in the environment; no model is selected implicitly.
- `spoiler vocab check --vocab FILE [--out FILE]` checks matcher validity and reports inert entries. `spoiler vocab build --config PRODUCT.json --source ROUTES.txt --source CONTROLS.txt --source-revision REV --model MODEL --out VOCAB.json` builds a snapshot from explicit UTF-8 source files and a product config (`apps`, including project, host, and audience). It requires `OPENROUTER_API_KEY`, unless `--candidate FILE` supplies an already prepared vocabulary, which takes precedence over `--model`. Consumers accept YAML/JSON vocabulary files or verified snapshots; they never regenerate implicitly.
- `spoiler recordings list --project ID --since START --until END [--limit 100] [--out FILE]` discovers one page (`--cursor TOKEN` continues); `spoiler recordings fetch --project ID --session ID [--out FILE]` downloads snapshots. Both require `POSTHOG_API_KEY` in the environment. The default host is `https://eu.posthog.com`; use `--host` for another PostHog instance.

Configuration is flags only. Credentials come from the environment only, so they never appear in process listings or shell history: `POSTHOG_API_KEY` (a personal API key with read access to recordings) and `OPENROUTER_API_KEY`.

For example, with a PostHog key set:

```sh
spoiler recordings list --project 123 --since 2026-09-01T00:00:00Z \
  --until 2026-09-02T00:00:00Z --out artifacts/page.json
spoiler recordings fetch --project 123 --session SESSION_ID --out artifacts/recording.json
```

Discovery queries `raw_session_replay_events` and paginates by start time and session id. A page's `next_cursor` is an opaque token; pass it back as `--cursor TOKEN` to get the next page. The token carries the project, host, and window, so `spoiler recordings list --cursor TOKEN` needs no other flags, and any given alongside it must match. Store it as a string; its contents are not a stable interface. Recordings ending after `--until` are excluded: choose a settled window and overlap syncs.

Fetch requests `blob_v2` sources and downloads bounded Snappy snapshot blocks, at most 20 blob keys per range. `--max-requests` caps the listing plus planned ranges (default 50, maximum 59); it is not a cross-invocation rate limiter. PostHog counts **both** the listing and each range against the per-key snapshot throttle: free 12/min and 60/hour, paid 60/min and 300/hour, enterprise 100/min and 400/hour. A typical four-call recording therefore limits a paid key to about **75 recordings/hour**, shared with other users of that key.

On a 429, fetch waits for `Retry-After` and retries only that request while the cumulative wait stays within `--max-wait` (default 60 seconds; 0 disables retries); otherwise it exits 75 with the retry time. Callers still own aggregate rate admission. `run --session` has the same fetch options. `--timeout` applies per request. Redirects are disabled; credentials travel only over HTTPS, except to loopback test servers.

Discovery binds window and cursor values in a named HogQL query. It bounds metadata aggregation to segments within 24 hours of the window, and checks session IDs up to seven days outside it: some live IDs exceed PostHog's documented 24-hour cutoff. Recordings with segments farther than seven days outside the window can still appear complete incorrectly. It lists non-deleted recordings that started at least 24 hours ago and ended by `--until` within those bounds. Rows include `snapshot_source`, `snapshot_library`, and `retention_period_days` for capture-type and expiry decisions. PostHog's ad-hoc `/query` endpoint has rate and byte-read budgets; budget 429s are retryable, and callers should honor the reported `Retry-After`.

## Native mobile recordings

`spoiler compile` reads PostHog native iOS and Android events: screen-name Meta (`type: 4`), wireframe full snapshots (`type: 2`, `wireframes` and `initialOffset`), Android wireframe add/update/remove mutations, TouchStart/TouchEnd coordinates, and keyboard show/hide events. iOS can send a new full wireframe snapshot for **every frame**, including an action's visible response; these frames are compared for effects. Screenshot-mode frames use `type: screenshot` wireframes instead of an inspectable view tree. Flutter and React Native record screenshot-only replays upstream; when their snapshots use this mobile event format, the same screenshot limits apply. Supply the mobile app id, not the demo web app id:

```sh
python3 scripts/corpus.py
spoiler compile --recording corpus/mobile_ios_button_text_effect.json \
  --vocab corpus/vocabulary.yaml --app mobile --out mobile-trace.json
```

For native screens, Meta `href` is a screen name such as `SettingsScreen`, **not** necessarily an HTTP URL. Define its vocabulary `route` as that exact screen name (case-sensitive, with no invented leading slash); `--app` selects which app's surfaces may match. Browser URL paths still use ordinary path routes such as `/page`. Native touch coordinates are absolute within the recorded viewport; a wireframe hit can name a target, whereas screenshot pixels cannot be OCR'd into controls or text. A screenshot-only tap is reported as `screen (x,y)` with no invented button or pixel-change effect. Screenshot frame mounts are counted under `coverage.opaque_mounts.mobile_screenshot`; `coverage.screenshot_only` indicates that no labelled wireframe was available. Keyboard events report visibility, not typed characters. Check trace `coverage` for uninterpreted or malformed events before relying on a narration.

## Vocabulary and prompts

A vocabulary names apps, surfaces, and features, and may specify grid identities, telemetry URL fragments, error-message patterns, and timing thresholds. The corpus vocabulary is a minimal example. An optional grid configuration might look like:

```yaml
grid:
  row_keys: [{ attribute: data-row-id }]
  label_columns: [[name, title]]
  generic_cell_features: { prefixes: [grid.column.], suffixes: [-cell] }
telemetry: [/rum]
error_text: ['(?i)\b(could not|failure)\b']
thresholds: { slow_ms: 3000 }
```

Without grid conventions, rows are identified by their label, the first column labels a row, and a grid cell inherits its column feature only if it has no feature of its own. English error-message patterns and common monitoring-request filters apply by default. Review model-generated matchers before promoting a vocabulary; `vocab check` warns about app-chrome features (`surface: "*"`) with no app.

The binary embeds `crates/core/prompts/narrate/system.md`, `crates/core/prompts/narrate/response.schema.json`, and `crates/core/prompts/vocabulary/system.md`. A custom narration prompt can be supplied via `--system-prompt`. Prompt provenance records its name and digest; `request_digest` covers the exact model messages and response schema. Prompt changes should be reviewed against prepared requests and real answers: automated checks do not measure narration quality.

## Artifacts, validation, and limits

Every artifact has a `kind` and per-kind `schema_version`: `trace` uses version 3; `analysis_request`, `analysis`, and `recording_page` use version 2; the other kinds (`recording`, `session`, `vocabulary_snapshot`, `vocabulary_check`) use version 1. A trace records its compiler version and SHA-256 digests of the recording and vocabulary. Readers reject incompatible kinds, schemas, compiler versions, invalid trace bounds/refs, and vocabulary mismatches. Trace refs (`e1`, `e2`, …) identify actions within one trace, including when analyzing a single visit. Visits split after 30 minutes without actions across tabs; a scheduler can analyze each visit with gestures separately.

The trace's `coverage` reports duplicate, uninterpreted, or malformed events and opaque mounts such as iframes and canvases. It also includes `extensions` when browser-extension content was suppressed: keys are public Chrome extension IDs, known custom-element families, or URL-scheme names; counts include mounted or newly marked extension roots, dropped console errors/requests, and excluded gestures (up to 50 named keys, then `other`). `unlocated_snapshots` counts full snapshots on tabs with no known URL and no `$pageview`/`$url_changed` href in that window; those paths remain unknown rather than guessed. A missing Meta on an already located tab leaves its location intact. Both new coverage fields are omitted when zero or empty. Validated analyses retain only trace-supported claims, derive signal lists and uncited gestures, and report unsupported duration claims. OpenRouter calls request zero-data-retention routing and required parameter support. Invalid model answers can receive one corrected retry, and an empty answer is asked again once (its tokens are still counted); `--response` applies the same acceptance bar offline without retrying.

Recordings are **untrusted input**. `--max-input-mib` (default 512) bounds local recording reads and aggregate downloaded snapshots, including decompressed streams and fields. HogQL and model JSON responses have a separate 32 MiB cap. `--out` writes to a synced `.spoiler-*.tmp` file before renaming, so failures leave an existing output intact; interrupted writes may leave the temporary file. Without `--out`, artifacts go to stdout. Failures are JSON on stderr with `error` and `retryable` fields:

| Exit code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Invalid input, configuration, or a non-transient upstream failure |
| `2` | Invalid CLI invocation |
| `75` | Transient upstream failure (such as 429, 5xx, timeout, or connection error) |

## Behavior corpus and development

The synthetic cases in `corpus/` define compiler behavior: each recording has a description and committed `*.expected.tsv` and `*.expected.json` goldens. To edit cases and review intended output changes:

```sh
python3 scripts/corpus.py
cargo test --test corpus
SPOILER_BLESS=1 cargo test --test corpus  # only for intended changes; review the golden diff
```

Compiler-rule changes must bump `COMPILER_VERSION`. For general development:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contributions and [SECURITY.md](SECURITY.md) for private vulnerability reports. Licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).

## Limitations

The browser DOM mirror cannot see iframe documents, canvas pixels, shadow-root internals, or arbitrary unrecognized rrweb plugins. Native wireframes contain only what the SDK captured; screenshots are opaque images, not reconstructible view hierarchies, and touch coordinates do not establish which control a user intended when the target is absent. Inspect `coverage` when judging a trace. Live PostHog query/pagination behavior and model answer quality are not covered by automated tests. Avoid committing actual session recordings or credentials.

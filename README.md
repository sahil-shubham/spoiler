# Spoiler

Spoiler turns PostHog/rrweb session recordings into evidence you can inspect: it decodes recording data, replays a per-tab DOM mirror, compiles user actions and visible effects into a deterministic trace with friction signals, and prepares or validates LLM narration through OpenRouter. A pinned product vocabulary supplies names and matching rules, so the same recording and vocabulary produce the same trace. The Rust core has no HTTP, database, or clock dependency; the CLI handles files and network access.

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

Discovery queries `raw_session_replay_events` and paginates by start time and session id. A page's `next_cursor` is an opaque token; pass it back as `--cursor TOKEN` to get the next page. The token carries the project, host, and window, so `spoiler recordings list --cursor TOKEN` needs no other flags, and any given alongside it must match. Store it as a string; its contents are not a stable interface. Recordings ending after `--until` are excluded: choose a settled window and overlap syncs. Fetch supports `blob_v2` sources and limits requests (default `--max-requests 50`, maximum 59, with 20 blob keys per request). Commands do not retry; the caller owns retry policy and aggregate rate admission. `--timeout` applies per request. Redirects are disabled; credentials travel only over HTTPS, except to loopback test servers.

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

Every artifact has a `kind` and per-kind `schema_version`: `trace`, `analysis_request`, `analysis`, and `recording_page` use schema version 2; the other kinds (`recording`, `session`, `vocabulary_snapshot`, `vocabulary_check`) use version 1. A trace records its compiler version and SHA-256 digests of the recording and vocabulary. Readers reject incompatible kinds, schemas, compiler versions, invalid trace bounds/refs, and vocabulary mismatches. Trace refs (`e1`, `e2`, …) identify actions within one trace, including when analyzing a single visit. Visits split after 30 minutes without actions across tabs; a scheduler can analyze each visit with gestures separately.

The trace's `coverage` reports duplicate, uninterpreted, or malformed events and opaque mounts such as iframes and canvases. Validated analyses retain only trace-supported claims, derive signal lists and uncited gestures, and report unsupported duration claims. OpenRouter calls request zero-data-retention routing and required parameter support. Invalid model answers can receive one corrected retry; `--response` applies the same acceptance bar offline without retrying.

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

The DOM mirror cannot see iframe documents, canvas pixels, shadow-root internals, or arbitrary unrecognized rrweb plugins; inspect `coverage` when judging a trace. Live PostHog query/pagination behavior and model answer quality are not covered by automated tests. Avoid committing actual session recordings or credentials.

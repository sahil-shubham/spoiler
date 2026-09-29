# Spoiler

Spoiler reads session recordings and reports what users tried, how it ended, and what blocked them.

Code measures what happened. A model explains it. Code checks the explanation.

- **In:** any rrweb recording, from a file or fetched from PostHog. Web, native iOS and Android.
- **Out:** versioned JSON: a deterministic trace, and an analysis that cites it.
- **Model:** any OpenRouter model, one call per visit. Compiling needs no model and no network.

## Example

A user spends 27 seconds deleting a project. The analysis:

| | |
| --- | --- |
| **Task** | Delete the Website refresh project. |
| **Outcome** | `workaround` · 25.3 s active · 9 actions · 2 data changes |
| **Friction** `error` e2 e4 e8 | Delete project failed three times with 409 and "Couldn't delete: this project has an active task."<br>*Hypothesis: the message says what blocks the delete but gives no path to the Tasks tab, where the task can be completed.* |
| **Friction** `confusion_loop` e7 e8 | Changed the project's status to Archived, then retried Delete project, which failed the same way. |
| **Dropped** `slow` | "Delete project was slow to respond." No cited action carries a `slow` flag. |

Refs, timings and counts come from code. This demo's prose is hand-written; it passed the same gate.

The trace behind those refs (excerpt, columns selected):

```text
ref  t_s   target                                    flags                    effect
e2   2.0   button[delete-project] "Delete project"   error_after,error_shown  net 409 /api/projects/42 180ms; +alert "Couldn't delete: this project has an ac…"
e4   6.5   button[delete-project] "Delete project"   error_after,error_shown  net 409 /api/projects/42 180ms; +alert "Couldn't delete: this project has an ac…"
e7   12.1  div[role=menuitemradio] "Archived"                                 -menu "Active Archived"; req /api/projects/42 200 140ms; text "Status: Active" → "Status: Archived"
e8   14.1  button[delete-project] "Delete project"   error_after,error_shown  net 409 /api/projects/42 180ms; +alert "Couldn't delete: this project has an ac…"
e10  18.1  button[role=tab] "Tasks"                                           aria-selected:true→false; aria-selected:false→true; -text "Refresh the public website."; +row "T-1043"
e12  24.6  button[confirm-complete] "Complete task"                           -dialog "Complete task T-1043?"; req /api/tasks/1043/complete 200 210ms; cell Status "T-1043": "Active" → "Completed"
e13  27.1  button[delete-project] "Delete project"                            req /api/projects/42 200 190ms; → /projects; +status "Project deleted"
```

## How it works

```text
source files ─▶ vocab build ─▶ vocabulary ─┐
                (model, per release)       ├─▶ compile ─▶ trace ─▶ analyze ─▶ analysis
recording ─────────────────────────────────┘   (code only)         (model, then code)
```

1. **Vocabulary.** `vocab build` has a model read source files you name.
   It drafts surfaces, controls, domain terms and rules, each citing its source line.
   What the sources don't cover is listed under `gaps`, not guessed.
   The snapshot pins every source by SHA-256 and is never regenerated implicitly.
   `--candidate` packages a vocabulary you wrote instead, with no model call.
2. **Compile.** `compile` replays the recording's DOM log, one mirror per tab.
   Each click resolves to an element, a vocabulary feature, what changed, and how fast.
   Rules raise flags: `dead`, `unresponsive`, `rage`, `slow`, `error_after`, `error_shown`, `thrash`.
   The same recording, vocabulary and compiler version always give the same trace.
3. **Narrate.** `analyze` sends the trace as TSV, plus the vocabulary it touched.
   The recording itself is never sent. `run` makes one call per visit.
   The model returns tasks with a goal, outcome, obstacle and friction, all citing refs.
   Outcomes are `done`, `workaround`, `gave_up` or `unclear`.
4. **Gate.** Code checks every answer before accepting it:
   - Off-schema, or over 15% of cited refs missing: rejected. A live call gets one corrected retry.
   - `dead_click`, `rage_click`, `error` and `slow` friction is dropped without a matching flag.
   - Timestamps, durations, paths and data changes are written from the trace, never by the model.
   - `check` lists everything dropped, unexplained, or uncited.

## Install

```sh
pip install spoiler              # or: uv tool install spoiler
cargo install spoiler --locked   # Rust 1.88+
curl -fsSL https://github.com/sahil-shubham/spoiler/releases/latest/download/spoiler-aarch64-apple-darwin.tar.gz | tar -xz
```

- Builds: Linux x86_64/aarch64 (glibc 2.28+, or static musl) and macOS arm64/x86_64.
- The wheel only puts `spoiler` on `PATH`. There is no Python API.
- Each [release](https://github.com/sahil-shubham/spoiler/releases) archive has a `.sha256` beside it.
- From a checkout: `cargo install --path crates/cli --locked`.

## Quick start

Offline, from a checkout, using committed fixtures. No account or API key.

```sh
cargo build --release --locked && mkdir -p artifacts
./target/release/spoiler compile --recording corpus/click_changes_text.json \
  --vocab corpus/vocabulary.yaml --app demo --out artifacts/trace.json
jq -r .tsv artifacts/trace.json
./target/release/spoiler analyze --trace artifacts/trace.json --vocab corpus/vocabulary.yaml \
  --response examples/click_changes_text.response.json --out artifacts/analysis.json
```

`--response` validates a stored answer. `--prepare-only` writes the exact model request instead.
Neither sends anything.

## On your product

```sh
export POSTHOG_API_KEY=… OPENROUTER_API_KEY=…
spoiler vocab build --config product.json --model "$MODEL" \
  --source src/routes.tsx --source src/pages/project.tsx \
  --source-revision "$(git rev-parse HEAD)" --out vocab.json
spoiler vocab check --vocab vocab.json
spoiler run --project 123 --session "$SESSION_ID" --vocab vocab.json --app web \
  --model "$MODEL" --out session.json
```

- `product.json` names each app: `{"apps": {"web": {"project": 123, "host": "app.example.com",
  "audience": "workspace admins"}}}`.
- Review model-drafted matchers before relying on a vocabulary.
- Rebuild the vocabulary when you ship. Each trace records the digest it was compiled against.
- `run` narrates every visit with user gestures. `--visit N` picks one.

## Commands

| Command | Reads → writes | Network |
| --- | --- | --- |
| `run` | recording or PostHog session → `session` (trace + per-visit analyses) | PostHog with `--session`, OpenRouter with `--model` |
| `compile` | recording + vocabulary → `trace` | none |
| `analyze` | trace → `analysis_request` or `analysis` | OpenRouter with `--model` |
| `vocab build` | product config + sources → `vocabulary_snapshot` | OpenRouter, unless `--candidate` |
| `vocab check` | vocabulary → `vocabulary_check` | none |
| `decode` | recording file → normalized `recording` | none |
| `recordings list`, `fetch` | PostHog project → `recording_page`, `recording` | PostHog |

- One JSON artifact per command, to stdout or `--out` (written atomically).
- Any input path accepts `-` for stdin, so commands pipe.
- Configuration is flags only. Credentials come only from the environment.
- No model is ever chosen for you.
- `spoiler <command> --help` lists every flag.

```sh
spoiler recordings fetch --project 123 --session "$SESSION_ID" \
  | spoiler compile --recording - --vocab vocab.json --app web \
  | spoiler analyze --trace - --vocab vocab.json --prepare-only
```

Failures are JSON on stderr: `{"error": "…", "retryable": false}`.

| Exit | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Invalid input or configuration, or a permanent upstream failure |
| `2` | Invalid invocation |
| `75` | Transient upstream failure: 429, 5xx, timeout, or connection error |

## Reference

### Vocabulary

```yaml
version: 1
apps:
  projects: { project: 1, host: projects.test, audience: "workspace members" }
surfaces:
  - { id: projects.detail, app: projects, route: "/projects/:projectId", name: "Project" }
features:
  - id: projects.project.delete
    surface: projects.detail
    name: "Delete project"
    matchers: { testid: [delete-project] }
    source: "project.tsx:13"
terms:
  - term: active task
    means: "A project with an active task cannot be deleted: the server answers 409."
    source: "projects.server.ts:5"
gaps: ["Status menu options: status-menu.tsx is not among the sources."]
```

- Matchers, most specific first: `testid`, `data_attr`, `aria`, `title`, `placeholder`, `href`,
  `text`, `role`, `class_contains`. `aria`, `text` and `title` also take `*_template` forms.
- `surface: "*"` marks app chrome and requires `app`.
- Optional: `statuses`, `grid` (row identity), `telemetry` (URLs to ignore), `error_text`, `thresholds`.
- Defaults: English error patterns, common monitoring requests ignored, `slow_ms: 1000`.
- `vocab check` reports invalid matchers and inert entries.

### Recordings

- Accepted: rrweb event arrays, JSONL, compressed input, and PostHog snapshot lines.
- Native iOS and Android: taps resolve against PostHog wireframes.
  Native routes are screen names such as `SettingsScreen`, matched case-sensitively.
- Screenshot-mode frames, including Flutter and React Native, are opaque images.
  A tap on one is reported as `screen (x,y)`, with no invented control.
- A visit ends after 30 minutes without actions (`thresholds.visit_gap_ms`).

### PostHog

- `recordings list` returns one page. Pass its `next_cursor` back as `--cursor`.
- It lists recordings that started over 24 hours ago and ended by `--until`.
  Use settled windows, and overlap consecutive syncs.
- Segments more than 7 days outside the window can make a recording look complete.
- The default host is `https://eu.posthog.com`. Set `--host` for US or self-hosted.
- Listings and snapshot ranges share PostHog's per-key throttle (paid: 60/min, 300/h).
  A paid key fetches about 75 typical recordings an hour.
- On 429, fetch honours `Retry-After` for up to `--max-wait` seconds (60), then exits `75`.
- `--max-requests` caps requests per fetch (default 50, at most 59).

### Data and security

- Recordings are untrusted input. `--max-input-mib` (512) bounds decoded size.
  HogQL and model responses are capped at 32 MiB.
- Recordings are sensitive. Review what you send to PostHog and OpenRouter.
- Model calls request zero-data-retention routing. Each analysis records model, tokens and cost.
- Credentials travel only over HTTPS, and redirects are disabled.
- Report vulnerabilities privately through
  [GitHub security advisories](https://github.com/sahil-shubham/spoiler/security/advisories/new).

### Limits

- The DOM mirror can't see iframe documents, canvas pixels, or shadow-root internals.
  Events from unrecognized rrweb plugins are not interpreted. `coverage` counts all of these.
- rrweb records no key presses, so a keyboard-shortcut change has no gesture.
- Masked inputs reveal only their length.
- Tests cover the compiler and the gate, not narration quality or live PostHog behaviour.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- `corpus/` holds synthetic recordings with golden traces. They define compiler behaviour.
- Edit cases in `scripts/corpus.py`, then run `python3 scripts/corpus.py`.
- Accept an intended output change with `SPOILER_BLESS=1 cargo test --test corpus`.
  Review every golden diff.
- A compile-rule change bumps `COMPILER_VERSION`. Readers reject traces from other versions.
- Fixtures stay synthetic: no real recordings, credentials, or customer data.
- Releasing is documented at the top of `.github/workflows/release.yml`.

## License

[Apache-2.0](LICENSE)

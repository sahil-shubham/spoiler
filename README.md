# Spoiler

Spoiler reads session recordings and reports what users tried, how it ended, and what blocked them.

Code measures what happened. A model explains it. Code checks the explanation.

- **In:** any rrweb recording, from a file or fetched from PostHog. Web, native iOS and Android.
- **Out:** versioned JSON: a deterministic trace, and an analysis that cites it.
- **Model:** any OpenRouter model, one call per visit. Compiling needs no model and no network.

## Example

An admin invites a teammate, but every seat is taken. From the 22-second recording, Spoiler reports:

**Task:** Invite `priya@example.com` to the workspace. **Outcome:** `workaround`.

> Send invite was refused three times because all 5 seats were in use.
> Deactivating Ben Ortiz did not free his seat. Removing him did.
>
> *Hypothesis: the message doesn't say that deactivated members still hold seats.*

The *why* comes from the vocabulary term `seat`, drafted from `members.server.ts:4`.
It reads: "Every member holds a seat until removed, deactivated members included."

Each claim cites refs into the trace, which code compiles from the recording with no model:

| ref | t_s | target | effect | flags |
| --- | --- | --- | --- | --- |
| e2 | 1.0 | `input[invite-email] "name@company.com"` | `typed "priya@example.com"` | |
| e3 | 3.0 | `button[send-invite] "Send invite"` | `net 402 /api/invites 170ms; +alert "Couldn't send invite: all 5 seats are i…"` | `error_after,error_shown` |
| e5 | 6.5 | `button[send-invite] "Send invite"` | `net 402 /api/invites 170ms; +alert "Couldn't send invite: all 5 seats are i…"` | `error_after,error_shown` |
| e7 | 10.6 | `button[deactivate-member] "Deactivate" in cell[Actions] "Ben Ortiz"` | `req /api/members/m-ben/deactivate 200 150ms; cell Status "Ben Ortiz": "Active" → "Deactivated"; cell Actions "Ben Ortiz": "Deactivate Remove" → "Remove"` | |
| e8 | 13.1 | `button[send-invite] "Send invite"` | `net 402 /api/invites 170ms; +alert "Couldn't send invite: all 5 seats are i…"` | `error_after,error_shown` |
| e11 | 19.6 | `button[confirm-remove] "Remove"` | `-dialog "Remove Ben Ortiz?"; req /api/members/m-ben 200 160ms; -row "Ben Ortiz"` | |
| e12 | 22.1 | `button[send-invite] "Send invite"` | `req /api/invites 201 180ms; +status "Invitation sent to priya@example.com"` | |

The answer also claimed "Rage-clicked Send invite after the first refusal", citing e3 and e5.
Code dropped it: the clicks were 3.5 s apart, and neither carries a `rage` flag.

The session is synthetic and this narration is hand-written. With `--model`, a model writes it.

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
curl -fsSL https://spoiler.sh/install | sh
```

- Installs the binary for your OS and CPU to `~/.local/bin`, after checking its sha256.
- Linux gets the static musl build, which runs on any distribution.
- `… | SPOILER_VERSION=v0.1.0 sh` pins a release. `SPOILER_INSTALL_DIR` picks the directory.

From PyPI. The wheel only puts the `spoiler` binary on `PATH`; there is no Python API.

```sh
pip install spoiler
```

`uv tool install spoiler` works the same way. With Rust 1.88+, from crates.io:

```sh
cargo install spoiler --locked
```

[Releases](https://github.com/sahil-shubham/spoiler/releases) has an archive per target, each with a `.sha256`:

- `aarch64-apple-darwin`, `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (glibc 2.28+)
- `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (static; any Linux)

From a checkout: `cargo install --path crates/cli --locked`.

## Quick start

Offline, from a checkout, using committed fixtures. No account or API key.

Build, and put the binary on `PATH`:

```sh
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
mkdir -p artifacts
```

Compile a recording into a trace. No model, no network:

```sh
spoiler compile \
  --recording corpus/click_changes_text.json \
  --vocab corpus/vocabulary.yaml \
  --app demo \
  --out artifacts/trace.json
```

Read the trace the way a model does:

```sh
jq -r .tsv artifacts/trace.json
```

Validate a stored answer against the trace:

```sh
spoiler analyze \
  --trace artifacts/trace.json \
  --vocab corpus/vocabulary.yaml \
  --response examples/click_changes_text.response.json \
  --out artifacts/analysis.json
```

Swap `--response FILE` for `--prepare-only` to write the exact model request instead.
Neither sends anything.

## On your product

Set credentials, which Spoiler reads only from the environment, and two variables used below:

```sh
export POSTHOG_API_KEY=phx_…
export OPENROUTER_API_KEY=sk-or-…
MODEL=…       # any OpenRouter model id
SESSION_ID=…  # a PostHog recording id
```

Describe each app in `product.json`. `project` is its PostHog project id:

```json
{
  "apps": {
    "web": { "project": 123, "host": "app.example.com", "audience": "workspace admins" }
  }
}
```

Build a vocabulary from the source files that name your routes and controls:

```sh
spoiler vocab build \
  --config product.json \
  --source app/routes.ts \
  --source app/routes/members.tsx \
  --source app/members.server.ts \
  --source-revision "$(git rev-parse HEAD)" \
  --model "$MODEL" \
  --out vocab.json
```

Check it, and review the drafted matchers before relying on them:

```sh
spoiler vocab check --vocab vocab.json
```

Fetch, compile and narrate one PostHog session:

```sh
spoiler run \
  --project 123 \
  --session "$SESSION_ID" \
  --vocab vocab.json \
  --app web \
  --model "$MODEL" \
  --out session.json
```

- `run` narrates every visit with user gestures. `--visit N` picks one.
- Rebuild the vocabulary when you ship. Each trace records the digest it was compiled against.

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

Each stage also runs alone, and they pipe:

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
  workspace: { project: 1, host: app.test, audience: "workspace admins" }
surfaces:
  - { id: workspace.members, app: workspace, route: /settings/members, name: "Members" }
features:
  - id: members.invite.send
    surface: workspace.members
    name: "Send invite"
    matchers: { testid: [send-invite] }
    source: "members.tsx:11"
terms:
  - term: seat
    means: "Every member holds a seat until removed, deactivated members included."
    source: "members.server.ts:4"
statuses:
  - { kind: member, value: deactivated, label: Deactivated }
gaps: ["Billing page: billing.tsx is not among the sources, so seat purchases are unnamed."]
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

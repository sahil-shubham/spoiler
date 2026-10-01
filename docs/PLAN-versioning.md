# Versioning Plan

Owner: Sahil · Scope: `crates/core/src/artifact.rs`, every artifact kind, `crates/cli` (`run`, new `versions`, `upgrade`) · First consumer: SPC omni (`spc: omni/replays`)
Status: not started · Design source: the bumps `docs/PLAN-timeline.md` and `docs/PLAN-vocabulary.md` require, 2026-10-01
Citations: paths are this repo unless prefixed `spc:`.

Spoiler has four kinds of version and one good idea. Consumers use none of them to decide anything. This plan makes "what must I recompute after upgrading Spoiler?" a question code answers.

---

## 0. What the ground truth changes

1. **Every version check is strict equality, so an upgrade orphans every stored artifact.** `Header::check` rejects any `schema_version` but the current one (`crates/core/src/artifact.rs:126-141`); `TraceArtifact::from_json` rejects any other `COMPILER_VERSION` (`:231-236`); `Vocabulary::validate` accepts only version 1 (`crates/core/src/vocab/mod.rs:390-397`). The two plans add Trace 3 → 4, Recording 1 → 2, compiler 6 → 7 and vocabulary 1 → 2. After each, a new binary cannot read what the old one wrote.

2. **The idempotency key already exists.** `AnalysisProvenance.request_digest` is the SHA-256 of the exact messages and response schema: "Equal digests ask the model the same question: a scheduler's idempotency key" (`crates/core/src/artifact.rs:266-270`, computed at `crates/cli/src/main.rs:543`). Preparing a request is free (`analyze --prepare-only`, `README.md:137`). A compiler change that leaves a visit's TSV and glossary unchanged leaves its digest unchanged, and the stored narration is still the answer to that question.

3. **The consumer records versions and acts on none.** omni stores `compiler_version` and `vocab_digest` per recording and `request_digest` per visit (`spc: omni/replays/analysis.py:101-157`, `spc: omni/replays/models.py:55-56`, `:79`), pins `spoiler>=0.1.0,<0.2` (`spc: omni/pyproject.toml:68`), and re-analyzes only by hand, one recording at a time (`spc: omni/replays/management/commands/replays_analyze.py:39-41`, `:67-70`). `spoiler run` calls the model for every visit every time.

4. **Reviews already key on the right thing.** omni's review revision is a hash of the visits' `request_digest`s (`spc: omni/replays/models.py:103-106`): a review survives exactly as long as the questions the model was asked.

Survives unchanged: per-kind `schema_version`, `COMPILER_VERSION` as "rules for actions", prompt identity by file digest (`artifact.rs:86-109`), vocabulary content digests.

---

## Problem: A Version Number Says Something Changed, Not What

`COMPILER_VERSION` 6 → 7 says traces differ. It does not say which recordings' traces differ, which visits' questions differ, or whether the change touched timing, flags or nothing a narration reads. A consumer can only redo everything (cost: a model call per visit, ~$0.01 each on SPC) or nothing.

---

`COMPILER_VERSION` 6 → 7 says traces differ. It does not say which recordings' traces differ, which visits' questions differ, or whether the change touched timing, flags or nothing a narration reads. A consumer can only redo everything (a model call per visit) or nothing.

Two rules and three commands.

### 1. Shape and meaning are versioned separately

| version | changes when | a reader of the old value |
|---|---|---|
| `schema_version` per kind (shape) | a field is added, removed or retyped | **upgrades it**: each kind keeps a reader for `N-1` that maps to `N` (`Trace` 3 → 4 fills `timeline` as absent) |
| rule versions (meaning) | output for the same input changes | **recomputes**, deciding what by digest (§ 2) |

Rule versions, each a constant beside the code it governs:

| rule | governs | free to recompute? |
|---|---|---|
| `DECODE_VERSION` | normalization, duplicates, foreign-tail repair | yes — code only |
| `COMPILER_VERSION` | actions, effects, flags | yes |
| `TIMELINE_VERSION` | presence, focus, quiet, fidelity | yes |
| narration prompt digest | the instructions | no — a model call |
| `GATE_VERSION` | validation of a model answer | yes — re-gates a stored answer (`analyze --response`) |

Every artifact records the rule versions it was produced under in `produced_by`:

```rust
/// The build and the rules an artifact was derived with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProducedBy {
    /// Package version, for humans; never compared.
    pub spoiler: String,
    /// Only the rules this kind depends on: a trace names decode, compiler and timeline.
    pub rules: BTreeMap<String, u32>,
}
```

Old artifacts without `produced_by` read as `{ rules: { compiler: <their compiler_version> } }`.

### 2. Invalidation is by digest

Recomputation runs as far as digests change and stops where they do not:

```text
recording ──decode──▶ recording' ──compile──▶ trace ──prepare──▶ request_digest per visit
                (free)                (free)           (free)         │
                                                                     ├─ same as stored → keep narration, re-gate it (free)
                                                                     └─ different     → narrate (model call)
```

A compiler or timeline bump costs one recompile per recording and a model call only for visits whose question changed. A vocabulary rebuild does the same: names that visits never touched do not enter their glossary, so their digests do not move.

### 3. Commands

- **`spoiler versions`** prints the current shape and rule versions as JSON — what a consumer compares stored `produced_by` against.
- **`spoiler upgrade <artifact>`** rewrites a stored artifact to the current shape without recomputing it (schema `N-1` → `N`). Fails, naming the rule, when the artifact's meaning is stale rather than its shape.
- **`spoiler run --previous-analyses DIR`** takes the stored analyses of the recording; for each visit whose new `request_digest` matches one, it re-gates the stored answer instead of calling the model, and reports per visit `reused` or `narrated`.

For omni this is one management command after any Spoiler upgrade: recompile archived recordings, `run --previous-analyses`, store what changed. Reviews reset exactly where the question changed, as they already do.

### Release policy

- The package version is semver over the CLI and artifact shapes: a shape bump is a minor release while 0.x, and every reader reads `N-1`.
- Each rule bump is a changelog line naming what it invalidates — "compiler 7: idle rows and dead/unresponsive flags; recompile, narrations re-asked where TSV changed".
- A consumer pins a minor range (`<0.2` today) and upgrades deliberately; nothing in an upgrade requires reading the changelog to stay correct, only to estimate cost.

### What does NOT get built

- **No migration of narrations.** A model answer is evidence for one question; it is reused when the question is identical and discarded otherwise. Rewriting answers to fit new traces would invent evidence.
- **No reading of shapes older than `N-1`.** One step back covers a consumer that upgrades at least once per release; older artifacts go through `upgrade` one release at a time, or are recomputed.
- **No version negotiation between binaries.** One binary, one current shape per kind.

---

## Open decisions

1. **`produced_by` on vocabulary snapshots.** A vocabulary is input, not output; its content digest already identifies it. Adding rule versions there matters only once extraction (`docs/PLAN-vocabulary.md`) makes part of it derived.
2. **Re-gate on `GATE_VERSION` alone.** Re-gating is free, so every stored narration can be re-checked on upgrade; it can also drop friction a reviewer already judged, which changes what omni shows without a new question.
3. **Where reuse lives.** In Spoiler (`--previous-analyses`), every consumer gets it; in omni, Spoiler stays stateless and omni matches digests itself.

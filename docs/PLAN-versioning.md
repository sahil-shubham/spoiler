# Versioning Plan

Owner: Sahil · Scope: `crates/core/src/artifact.rs`, `crates/core/src/analysis`, `crates/cli` (`run --previous`, `versions`) · First consumer: SPC omni (`spc: omni/replays`)
Status: built (A1) · Design source: the bumps `docs/PLAN-timeline.md` and `docs/PLAN-vocabulary.md` require, 2026-10-01; revised against the code 2026-10-02
Citations: paths are this repo unless prefixed `spc:`.

"What must I recompute after upgrading Spoiler?" is a question code answers: shapes evolve without orphaning stored artifacts, rule versions name what changed, and a rerun reuses every analysis whose question did not change.

---

## 0. What the ground truth changes

1. **Additive evolution is already free.** No artifact type denies unknown fields, and optional fields carry `#[serde(default)]`: a reader ignores what it does not know and defaults what an older writer did not write. Every bump the timeline and vocabulary plans need — a `timeline` on traces, a `prefix` on recordings, wired vocabulary fields — is additive. The first draft of this plan proposed per-kind `N-1` readers and an `upgrade` command for them; nothing would use either. **Shape versions now count breaking changes only**, and `Header::check`'s equality (`crates/core/src/artifact.rs`) stays.

2. **The idempotency key already exists.** `AnalysisProvenance.request_digest` is the SHA-256 of the exact messages and response schema: "Equal digests ask the model the same question: a scheduler's idempotency key". A compiler change that leaves a visit's TSV and glossary unchanged leaves its digest unchanged, and the stored narration is still the answer to that question.

3. **Re-judging needs the answer, which was not kept.** An analysis stored the validated summary but not the model's text, so a stricter gate could only re-ask. Analyses now keep `answer`.

4. **Strict compiler equality on traces is right.** `TraceArtifact::from_json` rejects another `COMPILER_VERSION`; a trace from older rules should be recompiled, not read. Recordings are the source of truth and stay readable across compiler versions.

5. **The consumer records versions and acts on none.** omni stores `compiler_version` and `vocab_digest` per recording and `request_digest` per visit (`spc: omni/replays/analysis.py:101-157`), and `spoiler run` asked the model for every visit every time. omni's review revision hashes the visits' `request_digest`s (`spc: omni/replays/models.py:103-106`), so a review survives exactly as long as the questions do.

---

## Design

### 1. Shape and meaning are versioned separately

| version | changes when | reader of an older value |
|---|---|---|
| `schema_version` per kind | a field is removed, retyped or re-meant | rejects it (`ArtifactError::SchemaVersion`) |
| — | a field is added | reads it: unknown fields ignored, missing ones defaulted |
| `COMPILER_VERSION` | traces of the same recording and vocabulary change | rejects the trace; recompile from the recording |
| `GATE_VERSION` (new) | the rules accepting a model answer change | re-judges the stored answer (`run --previous`) |
| narration prompt digest | the built-in prompt or response schema changes | every request digest changes; re-asked |

`GATE_VERSION` is 1. Analyses written before it was recorded default to 1: every analysis so far came from the v0.1.0 gate, unchanged since.

### 2. Invalidation is by request digest

```text
recording ──compile──▶ trace ──prepare──▶ request_digest per visit
            (free)              (free)          │
                                                ├─ stored, same gate     → reused
                                                ├─ stored, older gate    → regated (assess the stored answer)
                                                └─ new, or regate refused → narrated (model call)
```

`run --previous <session.json>` reads an earlier session artifact's analyses (its trace is not read) and applies this per visit. Each visit reports `via: narrated | reused | regated`. Reuse costs no model call, so it applies to `--prepare-only` too: what is left as a request is exactly what a full run would pay for.

### 3. Commands

- **`spoiler versions`** prints the package version, the shape version per kind, `compiler`, `gate` and the narration prompt's identity — what a consumer compares stored artifacts against.
- **`spoiler run --previous FILE`**, above.

For omni, an upgrade is: recompile archived recordings, `run --previous` with each recording's last session, store what changed. Reviews reset exactly where the question changed, as they already do.

### Release policy

- semver over the CLI and artifact shapes. While 0.x, a breaking shape bump is a minor release; additive fields and new commands are patch releases.
- Each rule bump is a changelog line naming what it invalidates — "compiler 7: idle rows and dead/unresponsive flags; recompile, re-asked where the TSV changed".
- A consumer pins a minor range (`<0.2` today) and upgrades deliberately; `--previous` keeps an upgrade correct without reading the changelog, which only estimates cost.

### What does NOT get built

- **No `N-1` readers and no `upgrade` command.** Every planned change is additive (§ 0.1). A breaking change gets its reader when one is made.
- **No `produced_by` envelope.** The rule versions that decide anything are already where they apply: `compiler_version` on traces, `gate_version` on analyses, the prompt digest inside `request_digest`. A decode version arrives with the first decode rule that changes stored recordings (foreign-tail repair, `docs/PLAN-timeline.md`).
- **No migration of narrations.** A model answer is evidence for one question; it is reused when the question is identical and judged again by a newer gate, never rewritten to fit a new trace.

---

## Open decisions

1. **Re-gating can drop what a reviewer judged.** A newer gate may refuse friction a reviewer already confirmed; omni shows the regated analysis without a new question. Keeping verdicts keyed by `request_digest` (not by friction index) would survive it.
2. **Where omni keeps the previous session.** Its archive holds each recording's analyses as rows; it can rebuild a session artifact for `--previous`, or store the last one whole.

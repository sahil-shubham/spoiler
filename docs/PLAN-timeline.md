# Timeline Plan

Owner: Sahil · Scope: `crates/core` (recording, trace), `crates/cli` (PostHog fetch) · First consumer: the SPC Pulse replay player (outside this repo)
Status: part 1 and the timeline half of part 2 built (release A, `crates/core/src/trace/timeline.rs`); compiler rules and stitching (release B) not started · Design source: SPC replay review notes; 12 Pulse + 10 Pando PostHog recordings (15–28 Sep 2026); posthog-js recorder source, 2026-10-01
Citations: paths are this repo unless prefixed `spc:` (the SPC monorepo). Recorder citations are the build SPC browsers load today — `https://eu-assets.i.posthog.com/static/posthog-recorder.js`, `LIB_VERSION 1.435.6`, read from its published sourcemap: `rec:N` is `lazy-loaded-session-recorder.ts`, `rrweb:N` is `rrweb-record.js`, `throttler:N` is `mutation-throttler.ts`, `net:N` is `network-plugin.ts`, `console:N` is `rrweb-plugin-console-record.js`, `utils:N` is `sessionrecording-utils.ts`. Session and window ids are managed by the main bundle, which the app ships from npm: `npm-sessionid:N` and `npm-recording:N` are `sessionid.ts` and `session-recording.ts` of posthog-js 1.393.0 (Pulse's pin), from its sourcemap.

A trace says what the user did. It does not say *when the user was there*, *which tab was in front*, or *whether the page Spoiler reconstructed was the page the user saw*. Every consumer re-derives those, differently. This plan makes them a compiled, versioned part of the trace.

---

## 0. What the ground truth changes

Survives unchanged: the action model, refs, effects, flags, visits, the TSV the narrator reads.

1. **Attention is defined four times, inconsistently.**

   | concept | definition | where |
   |---|---|---|
   | idle | ≥ `idle_ms` (30 s) between *actions*, unless the previous action's tab is hidden | `crates/core/src/trace/signals.rs:56-80` |
   | active time | consecutive-action gaps, each capped at 30 s, while any tab is visible (its own visible/hidden sets) | `crates/core/src/analysis/validate.rs:19`, `:346-373` |
   | inactivity (playback) | 10 s × playback speed without an rrweb interaction, per tab | rrweb-player `Replayer` (consumer side) |
   | user activity (recorder) | any of `MouseMove, MouseInteraction, Scroll, ViewportResize, Input, TouchMove, MediaInteraction, Drag` | posthog-js `rec:184-193` |

   Spoiler counts scrolling and mouse movement as *nothing*: they are read past (`Coverage.uninterpreted`, `crates/core/src/trace/mod.rs:34-35`). A user reading and scrolling for 40 s gets an `idle` row, and the prompt says "Idle and hidden gaps are time away".

2. **The recorder SPC runs is not the one SPC pins, and it changes weekly.** posthog-js lazy-loads `posthog-recorder.js` from PostHog's CDN unversioned (`$posthog_config.strict_script_versioning: false` in SPC's own recordings). Today that file is 1.435.6 (a 3,059-line recorder); Pulse pins posthog-js 1.393.0 (1,994 lines) and Pando 1.372.4 (`spc: apps/{pulse,pando}/package.json`). The browser changelog lists about forty replay fixes between 1.398 and 1.435 (8 Jul – 1 Oct 2026), several to the exact behaviours below. No recording carries the recorder's version; `$session_options.sessionRecordingOptions` is the only fingerprint (SPC's lists `inlineStylesheetBudgetRules` and `maskAllElementAttributes`, so ≥ 1.413). **Spoiler must detect recorder behaviour from the stream, never from a version.**

   Verified against the CDN on 2026-10-01: the legacy URL the SDK uses by default, `/static/posthog-recorder.js?v=<version>`, serves 1.435.6 for both `?v=1.372.4` and `?v=1.393.0` (`cache-control: max-age=14400`); the versioned URL `/static/<version>/posthog-recorder.js` serves that exact build (`immutable`). The SDK picks the versioned URL only under `strict_script_versioning: true` (`external-scripts-loader.ts:100-119`, posthog-js 1.393.0). The 1.372.4 and 1.393.0 builds carry neither `inlineStylesheetBudgetRules` nor `maskAllElementAttributes`, which identifies the archive's oldest fingerprint as a ≤ 1.393 recorder; why only Pando loads one is not established.

3. **Blind starts are a rotation that files the new session's first snapshot under the old session.** Observed, not inferred. Recording `01a0c19b…` (Pulse, 21 Sep) has incrementals from its first event and no Meta or FullSnapshot for 298.0 s. Its own `$autocapture` events report `$sdk_debug_replay_full_snapshots` with a FullSnapshot for this session taken 40 ms after it started. That snapshot is in the *previous* recording, `01a0c13d…`: its last 25 events are a `$session_id_change { sessionId: "01a0c19b…", windowId: "…a2432524b9a5" }`, a Meta, the FullSnapshot, `$remote_config_received`, `$session_options`, `$posthog_config`, `$session_starting { nextSessionId: "01a0c19b…" }`, and ~2 s of incrementals — all labelled with the old session and the old window id. The new recording begins 2.1 s later (one `RECORDING_BUFFER_TIMEOUT`, `rec:136`). Across the 22 recordings: 5 blind starts (292–329 s, the periodic snapshot interval) and 4 "foreign tails" of 22–142 events carrying the next session's FullSnapshot, on Pulse and Pando, 15–22 Sep. posthog-js shipped fixes for this symptom in 1.398.4 (8 Jul), 1.407.2 and 1.430.2 (11 Sep, "a session rotation no longer files the new session's first snapshot under the old session"), and today's recorder also requests a snapshot when a session's incrementals arrive before any (`rec:1770-1795`). The sample postdates 1.430.2.

4. **The new session has no back-link; the old one has a forward link.** `$session_starting` is meant for the new session (`rec:2001-2041`) but lands in the old one with the rest of the tail. So the old recording says where its tail belongs; the new recording says nothing about where its start went.

5. **What the page looked like is lost in more ways than one, and most of them leave a marker.** The recorder's loss paths are in § The recorder. Click flags by fidelity across SPC's whole archive (below): `dead`/`unresponsive` on **17 %** of 28,050 clicks on a page built from a full snapshot, **41 %** of 408 before the tab's first snapshot, **52 %** of 771 after a return from idle and before the next snapshot.

6. **PostHog already states presence and session continuity; Spoiler ignores it.** `sessionIdle` carries `lastActivityTimestamp`, the detection time `eventTimestamp`, and (≥ 1.418.9) the `sessionId`/`windowId` that went idle (`rec:2679-2692`); the marker itself is restamped to `lastActivity + threshold` (`rec:2057-2062`). `$posthog_config` carries the client config and `$remote_config_received` the project's (`rec:3044-3050`). The compiler interprets four tags: `$pageview`, `$url_changed`, `window hidden`, `window visible` (`crates/core/src/trace/compiler/handlers.rs:581-608`).

7. **The TSV has become an API.** omni parses the narrator's TSV back into numbers — `round(float(t_s) * 1000)` and regexes over the `react` cell (`spc: omni/replays/trace.py:16-63`) — although the trace JSON already carries exact `t_ms: Millis` (`crates/core/src/trace/mod.rs:288-289`). Consumers read rendered prose because the JSON lacks what they need: `t0`, tab identity, presence.

8. **The time origin and tab identity are implicit.** `t0` is the first event (`crates/core/src/trace/compiler/mod.rs:55-67`) and is not written to the artifact (`crates/core/src/artifact.rs:208-224`). `win` is "1-based in order of first appearance" (`crates/core/src/trace/mod.rs:290`, `crates/core/src/trace/compiler/mod.rs:241-245`); the mapping to PostHog window ids exists only inside `Recording` (`crates/core/src/recording/mod.rs:217-221`). A player re-derives both and agrees by coincidence.

Sample (12 Pulse recordings, 20–28 Sep 2026, ≥ 2 min active): wall time without input in any tab for ≥ 10 s was 38–95 % (most above 75 %); 6 recordings had 2–7 tabs with up to 15 tab switches; the per-tab spans summed to up to 2.09× the recording.

**Archive-wide (all 1,327 recordings in SPC's S3 archive, 22–30 Sep 2026: 539 Pulse, 788 Pando, 1,979 tabs):**

| measure | Pulse | Pando | all |
|---|---|---|---|
| recordings with a blind tab (> 5 s before its first FullSnapshot) | 72 (13.4 %) | 88 (11.2 %) | 160 (12.1 %) |
| tabs that never get a FullSnapshot | 20 | 17 | 37 |
| blind delay, median | 298 s | 299 s | 298 s |
| recordings ending in a foreign tail | 65 (12.1 %) | 54 (6.9 %) | 119 (9.0 %) |
| wakes followed by a FullSnapshot within 1 s | 94 % of 581 | 57 % of 423 | 78 % of 1,004 |
| recordings with a mutation-throttling marker | 55 | 663 | 718 (2,189 markers) |
| clicks followed within 100 ms by an input; of those flagged | 300; 18 | 463; 10 | 763; 28 |

Of 121 foreign tails, 113 name a session that is also archived, and 107 of those start blind. Of 155 recordings whose first tab is blind, **103 are repaired by a tail elsewhere in the archive** — moving tails (Design § 3) fixes two thirds of blind starts with no PostHog request.

Three recorder builds ship every day of the window, told apart by their `$session_options` keys: with `inlineStylesheetBudgetRules` (≥ 1.412), with only `maskAllElementAttributes`, and without either. The oldest runs only on Pando (255 recordings) and heals 12 % of wakes against 76 % for Pando's newest and 94 % for Pulse's. Pando pins posthog-js 1.372.4; Pulse 1.393.0. [INFERENCE: the older main bundle loads an older recorder; the stream shows the effect, not the mechanism.]

---

## Current State

**Clock.** `Timestamp` is epoch ms, `Millis` a span; trace times are spans from the first event (`crates/core/src/time.rs:1-3`). The TSV renders `t_s` to one decimal (`crates/core/src/trace/render.rs:204`).

**Tabs.** `Compiler.tabs: IndexMap<String, Tab>` keyed by PostHog window id, numbered on first event of any kind (`crates/core/src/trace/compiler/mod.rs:241-245`). `Tab` holds the mirror, location, navigations, `hidden` and the open gesture (`crates/core/src/trace/compiler/tab.rs:61-84`). None of this leaves the compiler except through actions.

**Visibility.** `window hidden`/`window visible` become `hidden`/`visible` actions and a `Visibility` effect, and flush an open gesture on hide (`handlers.rs:588-606`).

**Idle and visits.** `finalize` inserts `idle` actions (`signals.rs:56-80`); `visits` splits at `visit_gap_ms` (30 min) of no non-idle action in any tab (`crates/core/src/trace/visits.rs:45-61`).

**Fetch.** `recordings fetch` takes one session's `blob_v2` sources, fetches 20-key ranges, decodes Snappy and normalizes (`crates/cli/src/posthog.rs:262-366`). `RecordingSource::Posthog` names one session (`crates/core/src/artifact.rs:166-176`). Nothing crosses a session boundary.

**Precedent for synthesized base state.** Mobile already inserts a synthetic minimal document before a first screenshot incremental without a full snapshot (`Data::MinimalScreenshot`, `crates/core/src/recording/mod.rs:202-203`, `:600-605`, `:750-753`).

---

## The recorder

What posthog-js does between a user's action and the stored recording, read from the build SPC loads.

**Identity.** A window id lives in the tab's `sessionStorage`: it survives reloads of that tab, a duplicated tab gets a new one, and a session rotation regenerates it. A session ends after 30 minutes without activity in any tab (configurable to 10 hours) or at 24 hours (`npm-sessionid:17-20`, `npm-sessionid:382-418`). One Spoiler tab is therefore one browser tab within one session, possibly across several page loads.

**Start.** Nothing is recorded until the project's remote config is persisted and the recorder script has loaded (`npm-recording:88-145`); rrweb then waits for `DOMContentLoaded` (`rec:2997`) and emits a Meta and a FullSnapshot. A FullSnapshot repeats every `full_snapshot_interval_millis`, default 5 minutes (`rec:1003-1015`), or every minute while a trigger is pending.

**Idle.** After 5 minutes without an `ACTIVE_SOURCES` event (`rec:134`, `rec:184-193`), the next event raises `sessionIdle` and every later event except `sessionIdle`, `$session_ending` and `$session_starting` is discarded (`rec:482-484`, `rec:2045-2050`); periodic snapshots stop (`rec:2677`). The first interaction raises `sessionNoLongerIdle`, and a FullSnapshot is taken only if a mirror-desyncing event was dropped (`rec:488-493`, `rec:2747-2759`). In the sample, 33 of 41 wakes had a FullSnapshot within 0.2 s; 8 waited ~300 s for the periodic one.

**Rotation.** `$session_ending`, `$session_id_change`, a recorder restart, `$session_starting` (`rec:1379-1453`, `rec:1607-1629`). A session born from a rotation without interaction holds its buffer until the user interacts, and a held epoch that never sees interaction never ships (`rec:1627`, `rec:2323-2325`).

**Loss paths.** Each row is something the stored recording does not contain, and how a reader can tell.

| loss | what is missing | marker in the stream | source |
|---|---|---|---|
| idle drop | every event between detection and the next interaction | `sessionIdle` (`payload.eventTimestamp` = when dropping began) … `sessionNoLongerIdle` | `rec:2045-2050`, `rec:2679-2692` |
| wake without a snapshot | mutations made while idle; the page is stale until the next FullSnapshot | `sessionNoLongerIdle` not followed by a FullSnapshot on that tab | `rec:2747-2759` |
| rotation mis-filing | the new session's Meta, FullSnapshot, config events and first ~2 s | old recording: a `$session_id_change` naming another session, followed by events; new recording: incrementals before any FullSnapshot | finding 3 |
| attribute throttling | attribute mutations on one node (or its enclosing `<svg>`) beyond a bucket of 100 refilled at 10/s | console warn `[SessionRecording] Too many mutations on node '<id>'`, once per node per snapshot | `throttler:41-44`, `throttler:126`, `rec:2946-2953`; 35 markers in 13 of 22 recordings |
| oversized mutation | one mutation over the byte budget (opt-in, off by default) | console warn `… Dropped an oversized DOM mutation …`, then a FullSnapshot | `rec:2958-2969` |
| snapshot depth | nodes deeper than 50 in a FullSnapshot | console warn `[rrweb-snapshot] DOM tree depth exceeded max depth of 50`, once | `rrweb:1826`, `rrweb:1833`; none in the sample |
| console volume | every console call after the 1,000th per recorder start; uncaught errors still pass | warn `The number of log records reached the threshold.` | `console:429`, `console:500-508` |
| console size | arguments past the 10th, strings past 2,000 characters | `...[truncated]` | `utils:152-153` |
| URL blocker | events on blocked URLs | `recording paused` … `recording resumed`; a FullSnapshot is taken on resume, so nothing is stale after it | `rec:1049-1083` |
| unstringifiable event | that event | none | `rec:1715-1718` |
| untouched epoch | a whole rotation-born or background recording | none: it never ships | `rec:2323-2325` |
| custom event before rrweb is ready | an `addCustomEvent` after 10 are queued or after 2 s | none | `rec:903-921`, `rec:992` |

**What rrweb records that the user did not do.**
- *Clicks* are captured with no `isTrusted` check (`rrweb:2130-2133`, `rrweb:3152-3190`): `element.click()` from code and a `<label>`'s activation click on its control are recorded as clicks.
- *Inputs* come from `input`/`change` events and from hooked setters for `value`, `checked`, `selectedIndex` and `selected` (`rrweb:3278-3360`). Code that clears or fills a field records as an input. rrweb can mark programmatic inputs (`userTriggeredOnInput`), but PostHog does not expose the option (`rec:2803-2832`). Measured: an `Add` click in Pando's notes grid at 46,814 ms moves focus to the note box, a programmatic input of `''` follows at 46,832 ms, the new row renders at 47,311 ms; Spoiler closes the click's window at the input (`crates/core/src/trace/compiler/handlers.rs:499-505`) and flags `Add` `unresponsive`. In the sample 29 of 1,403 clicks were followed within 100 ms by an input in the same tab; 2 of those were flagged.
- *Mouse moves* are sampled every 50 ms and emitted in batches every 500 ms; scrolls are throttled to 100 ms (`rrweb:3118-3119`, `rrweb:3240`). Presence cannot be resolved finer than 500 ms.

**SPC's capture settings, from its own recordings.** Project config: `masking.maskAllInputs: true`, `consoleLogRecordingEnabled: true`, `networkPayloadCapture.capturePerformance.network_timing: true`; plugins `rrweb/console@1` and `rrweb/network@1`; client `session_recording: {}`, `session_idle_timeout_seconds: 1800`. Client masking overrides the project's (`rec:693`). With timing-only network capture, `fetch`/XHR are not wrapped (`net:1002-1014`): requests carry no method, headers or body, and a status only through Resource Timing's `responseStatus`, which Spoiler reads (`crates/core/src/trace/compiler/handlers.rs:751-753`) [INFERENCE: browser support for `responseStatus` varies].

---

## Problem: Time and Attention Are Byproducts of the Action List

Idle is "a gap between actions"; active time is "gaps between actions, capped"; visits are "gaps between actions". Actions are what the user *did to a control*. Presence is what the user *was doing at all*. Deriving the second from the first makes reading look like absence, makes every consumer invent its own rule, and makes the player and the narration disagree about the same minute.

## Problem: The Trace Reads a Missing Page as a Page That Did Nothing

`flag_no_reaction` (`signals.rs:151-165`) raises `dead`/`unresponsive` when a click's window saw no visible change. During a dropped, stale or blind stretch the mirror cannot see changes that happened. Absence of evidence is reported as evidence of absence, and the gate then *requires* the narrator to explain it.

---

## Design

Three parts, all code; no model is involved anywhere in this plan.

### 1. `Timeline`: one pass, one owner

Computed by the compiler in the same event walk, written into the trace artifact, and the only source for idle, active time and presence everywhere.

**As built.** The types below are the design; the code is the contract (`crates/core/src/trace/timeline.rs`). Where they differ: `Page` has no `hard_load` (no reader); `Capture` keeps `mask_all_inputs`, `plugins` and `recorder_options` (the option names, which fingerprint the recorder build) and reads only `$session_options`; `SessionLink` is `{ session_id, window_id, at_ms }`; `quiet` subtracts gesture reactions, not requests (decision 3). Lifecycle tags the timeline reads leave `coverage.uninterpreted`; `$recording_started`, `$posthog_config`, `$remote_config_received`, `triggerGroupSamplingDecisionMade`, `$json_ld` and `rrweb/fullscreen` still count there. Recorder warnings (throttled nodes) are release B.

```rust
/// When the user was there, which tab was in front, and how much of each tab the recording
/// can show. Times are spans from `t0`, like every action time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    /// The recording's first event, epoch ms: the origin of every `*_ms` in this trace.
    pub t0: Timestamp,
    /// The recording's last event.
    pub end_ms: Millis,
    pub tabs: Vec<TabTimeline>,
    /// Merged runs of user input in any tab (posthog-js ACTIVE_SOURCES), split where
    /// consecutive inputs are `PRESENCE_GAP_MS` or more apart, and ended at a
    /// `sessionIdle` payload's `lastActivityTimestamp`.
    pub presence: Vec<Span>,
    /// The tab in front. No span covers a moment when every recorded tab was hidden.
    pub focus: Vec<FocusSpan>,
    /// Gaps in `presence` with no gesture window open and no request in flight, in any tab.
    /// Every gap, however short: consumers choose their own minimum.
    pub quiet: Vec<Span>,
    /// The browser reported itself offline: failed requests inside are not product errors.
    pub offline: Vec<Span>,
    /// What the recorder was configured to capture, from its own config events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Capture>,
    /// PostHog's own session links, when it recorded them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session: Option<SessionLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_session: Option<SessionLink>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start_ms: Millis,
    pub end_ms: Millis,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabTimeline {
    /// The `win` actions carry.
    pub win: usize,
    /// PostHog's window id: what a player keys its event streams by.
    pub window_id: String,
    pub first_ms: Millis,
    pub last_ms: Millis,
    pub hidden: Vec<Span>,
    /// Pathname (and search) from each navigation; the trace's nav actions carry surfaces.
    pub pages: Vec<Page>,
    /// How far the reconstructed page can be trusted, covering `[first_ms, last_ms]`.
    pub fidelity: Vec<FidelitySpan>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub at_ms: Millis,
    pub path: String,
    /// A document load (`DomContentLoaded`/`Load` or a new Meta after one), not an SPA navigation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hard_load: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusSpan {
    pub win: usize,
    #[serde(flatten)]
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Built from a full snapshot and every event since.
    Exact,
    /// No full snapshot yet: the mirror is whatever mutations happened to add.
    Blind,
    /// The recorder was discarding events (idle, or paused by a URL blocker).
    Dropped,
    /// Recording resumed after dropping, before the next full snapshot.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FidelitySpan {
    pub fidelity: Fidelity,
    #[serde(flatten)]
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub mask_all_inputs: Option<bool>,
    pub idle_threshold_ms: Option<Millis>,
    pub full_snapshot_interval_ms: Option<Millis>,
    pub plugins: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionLink {
    pub session_id: String,
    /// The window on the other side, per window id on this side.
    pub windows: Vec<(String, String)>,
    pub reason: Value,
}
```

Definitions, each stated once:

| term | rule | why this rule |
|---|---|---|
| presence | union over tabs of user-input events, sources = posthog-js `ACTIVE_SOURCES`, merged below `PRESENCE_GAP_MS = 1000` | the recorder's own definition; PostHog's active time already uses it |
| focus | the tab of the latest user input or `window visible` at or before *t*, unless it went hidden since; none when all are hidden | input is the only evidence of where the user looked; two visible windows side by side resolve by input |
| quiet | gaps between presence runs, minus each gesture's reaction (from its first to its last visible change) | a spinner the user is watching is not skippable; background polling would otherwise leave nothing quiet |
| idle (narrator row) | a quiet span ≥ `idle_ms` on a visible tab | replaces the action-gap rule in `finalize` |
| active time (task) | presence ∩ \[task start, task end\] | replaces the per-gap 30 s cap in `measure_task` |
| fidelity | `Blind` before the tab's first FullSnapshot; `Dropped` from `sessionIdle.payload.eventTimestamp` (or `recording paused`) to the next recorded event; `Stale` from a `sessionNoLongerIdle` to the tab's next FullSnapshot, when one does not follow at once; `Exact` otherwise | what the recorder actually captured (§ The recorder) |

**Where it is computed.** `Compiler::handle` (`crates/core/src/trace/compiler/mod.rs:232-297`) returns early for `Reading::Uninterpreted` (`:257`) — which is where mouse moves, scrolls and drags land. A `TimelineBuilder` on `Compiler` sees every event before that dispatch: it updates presence from the incremental source name the reading already carries, fidelity from `FullSnapshot` and the lifecycle tags (part 2), focus from input and visibility, and the per-tab first/last times. `quiet` subtracts gesture windows at `close_gesture` and requests as the network plugin records them. One pass, no second walk over the recording.

**Artifact.** `TraceArtifact` gains `timeline: Option<Timeline>` (absent on traces compiled before it); additive, so `Kind::Trace` stays schema 3 (`docs/PLAN-versioning.md`). The TSV is unchanged. Consumers read `actions` and `timeline` as JSON; the TSV stays the narrator's.

### 2. PostHog lifecycle events

Every custom tag and plugin the recorder writes into the replay stream, and what Spoiler does with it. A recorder version is never consulted: each rule keys on the event and its payload.

| tag | payload | written when | Spoiler |
|---|---|---|---|
| `$pageview` | `href` | each `$pageview` the app captures (`rec:1366`) | unchanged: navigation |
| `$url_changed` | `href` | URL change, only when `capture_pageview` is off (`rec:958-975`) | unchanged: navigation |
| `window hidden`, `window visible` | — | `visibilitychange` (`rec:2630-2638`) | unchanged actions; `TabTimeline.hidden`, focus |
| `sessionIdle` | `eventTimestamp`, `lastActivityTimestamp`, `threshold`, buffer sizes, `sessionId`, `windowId` | first non-interactive event past the idle threshold; restamped to `lastActivity + threshold` (`rec:2667-2696`, `rec:2057-2062`) | presence ends at `lastActivityTimestamp`; `Dropped` from `eventTimestamp` |
| `sessionNoLongerIdle` | `reason`, `type` | first interaction after idle (`rec:2707-2724`) | `Stale` until the tab's next FullSnapshot, unless one follows within 1 s |
| `recording paused`, `recording resumed` | `reason: "url blocker"` | entering/leaving a blocked URL (`rec:1049-1084`) | `Dropped` while paused; resume carries its own FullSnapshot |
| `$session_id_change` | `sessionId`, `windowId`, `changeReason` | rotation (`rec:1414`) | when `sessionId` is not this recording's: the start of a foreign tail (part 3) |
| `$session_ending` | `currentSessionId`, `currentWindowId`, `nextSessionId`, `nextWindowId`, `changeReason`, `lastActivityTimestamp`, `flushed_size` | linked rotation; backdated to the old session's last activity (`rec:1395-1407`, `rec:2010-2017`) | `Timeline.next_session` |
| `$session_starting` | `previousSessionId`, `previousWindowId`, `nextSessionId`, `nextWindowId`, `changeReason` | linked rotation (`rec:1437-1448`) | in its own session: `Timeline.previous_session`; in a foreign tail: part of the tail |
| `$recording_started` | `reason` | start, except rotation restarts (`rec:2640-2651`) | coverage |
| `browser offline`, `browser online` | — | network state (`rec:2622-2628`) | an `offline` span; failed requests inside it are not product errors |
| `$remote_config_received`, `$session_options`, `$posthog_config` | project config; rrweb options and plugins; client config | every recorder start (`rec:3044-3050`) | `Capture`: masking, idle threshold, snapshot interval, plugins, and `$session_options` keys as the recorder fingerprint |
| `triggerGroupSamplingDecisionMade` | `group_id`, `group_name`, `sampleRate`, `isSampled` | V2 trigger groups (`recording-strategies.ts:567`) | coverage |
| `$json_ld` | page JSON-LD | opt-in `captureJsonLd` (`json-ld.ts:137`) | coverage |
| `rrweb/fullscreen` | `id`, `enter` | fullscreen change (`rrweb:18`) | coverage |
| plugin `rrweb/console@1` | `level`, `payload`, `trace` | console calls, uncaught errors, rejections | unchanged `console_error`; the recorder's own warnings (loss table) become fidelity facts, not product errors |
| plugin `rrweb/network@1` | Resource Timing entries | requests (`net:1002-1014`) | unchanged |
| event types `DomContentLoaded`, `Load` | — | rrweb started before the page finished loading | `Page.hard_load`: a document load, not an SPA navigation |

A tag outside this table stays `custom:<tag>` in coverage, as now (`compiler/mod.rs:286-289`). Product-defined tags are declared in the vocabulary, not added here (`docs/PLAN-vocabulary.md`).

### Compiler rules that follow

**No verdict on a page Spoiler cannot see.** A click whose window lies in a non-`Exact` span, or whose target is inside a node the recorder announced as throttled, gets no `dead`/`unresponsive` flag; it gets `blind`, and the prompt says what it means: the page state at that moment is unknown. Coverage counts such gestures per fidelity. Across the archive this touches 1,179 of 29,229 clicks before counting throttled nodes; 571 of them carry `dead`/`unresponsive` today (41–52 %, against 17 % on exact pages).

**A programmatic input is the click's effect.** An input on a field other than the click's target, arriving within `programmatic_input_ms` of that field's `Focus` interaction and inside the click's gesture window, records a value change on the click instead of opening a typing window (`crates/core/src/trace/compiler/handlers.rs:499-505` today does the opposite). This is the `Add` case in § The recorder; across the archive, 763 clicks are followed within 100 ms by an input and 28 of them are flagged.

**The recorder's own console warnings are not the product's.** `[SessionRecording] …` and `[rrweb-snapshot] …` messages become fidelity facts and never `console_error` actions.

### 3. Stitching

**Across tabs, within a recording:** `focus` spans. A consumer showing one picture at a time follows them; nothing more is needed from Spoiler.

**Across sessions: move the foreign tail.** A rotation mis-files the new session's first events at the end of the old recording (finding 3). The repair is a move, not a borrowed base: those events *are* the new session's — its Meta, FullSnapshot, config events and the user's first interaction.

1. In recording A, a `$session_id_change` on window `W_A` whose `payload.sessionId` is B ≠ A starts a **foreign tail**: every later event on `W_A`.
2. **A without its tail.** Decoding A drops the tail and records it in coverage (`foreign_tail: { session, window, events }`). Without this, A gains a phantom visit: in the observed pair, `01a0c13d…` compiles to a second, one-action visit 41.8 minutes after its last activity.
3. **B with its start.** Given A, decoding B prepends A's tail, relabelled from `W_A` to `payload.windowId`. B's `t0` moves back to the `$session_id_change` (2.1 s in the observed pair); its first tab is `Exact` from its first event.
4. Idempotent and safe with newer recorders: the recorder filed those events once, so nothing in B duplicates them; a B whose recorder healed itself with its own early FullSnapshot (`rec:1770-1795`) still gains its missing first seconds.

```rust
/// Events a rotation filed under the session before this one: this recording's real start.
pub struct Prefix {
    /// The recording they were found in, and the window they were filed under there.
    pub from_session: String,
    pub from_window: String,
    /// The window they belong to here (`$session_id_change.payload.windowId`).
    pub window_id: String,
    /// Normalized events (compression undone), as in `RecordingArtifact.events`.
    pub events: Vec<Box<serde_json::value::RawValue>>,
}
```

**Where A comes from.** The old recording names the new one; the new one names nothing (finding 4). An archive that keeps every recording finds A as the recording that names B in its tail — SPC archives every discovered production recording (`spc: omni/replays/pipeline.py:124-131`), so the repair costs no PostHog request there. `spoiler compile --recording B --previous A` takes it as a file. Without an archive, `recordings fetch --session B --previous` finds A by one listing query (same `distinct_id`, ending within seconds of B's start) and fetches only A's last blob range: 2 requests against `--max-requests` (default 50, `README.md:281`). A blind start with no foreign tail anywhere (a reset, an expired A) stays `Blind`.

`RecordingArtifact` gains `prefix: Option<Prefix>` and `foreign_tail: Option<…>`; additive, so `Kind::Recording` stays schema 1.

---

### Versioning

Two compiler-visible changes, kept apart because one is free for every stored trace and one is not.

| change | `COMPILER_VERSION` | schema | goldens | consumer cost |
|---|---|---|---|---|
| `Timeline` emitted; lifecycle tags read into it; `Capture` | 6 (unchanged) | additive (`timeline`) | every `corpus/*.expected.tsv` byte-identical; `.expected.json` gains `timeline` | recompile archived recordings; `run --previous` reuses every narration |
| idle rows and `active_s` from `Timeline`; the `blind` flag; programmatic inputs as click effects; foreign tails moved | 6 → 7 | additive (`prefix`, `foreign_tail`) | `.expected.tsv` change where presence, fidelity, inputs or tails differ | recompile; `run --previous` re-asks only visits whose `request_digest` changed |

The first row's invariant is testable: the corpus harness compares TSVs (`crates/core/tests/corpus.rs:52-75`), so a timeline change that moves a single action fails it.

### Corpus

New cases, each a golden pair: `presence_reading_is_not_idle` (scrolling through 40 s), `focus_follows_input_across_tabs`, `focus_two_visible_windows`, `all_tabs_hidden_has_no_focus`, `idle_drops_then_stale_until_snapshot`, `wake_with_snapshot_is_exact`, `blind_start_flags_blind_not_dead`, `throttled_node_flags_blind`, `programmatic_input_is_the_clicks_effect`, `recorder_warning_is_not_a_console_error`, `paused_by_url_blocker`, `offline_failures_are_not_product_errors`, `capture_config_is_read`, `foreign_tail_is_cut`, `foreign_tail_becomes_the_next_start`. The two tail cases are built from the observed pair, scrubbed.

### What does NOT go in

- **No player and no playback thresholds.** `quiet` lists every gap; a consumer that skips gaps ≥ 10 s filters them. Spoiler has no opinion on how a recording should be watched.
- **No reconstruction of dropped mutations.** What the recorder discarded is gone; `Dropped`, `Stale` and throttled nodes say so instead of guessing.
- **No recorder-version table.** Behaviour is read from events and payloads; a version map would be stale within a week (finding 2).
- **No focus rows in the TSV.** `hidden`/`visible` already reach the narrator. One `render.rs` branch adds them if narration needs them.
- **No new vocabulary thresholds.** `PRESENCE_GAP_MS` is a compiler constant, versioned with the compiler; `idle_ms` keeps its meaning.
- **No session merging.** A recording stays one PostHog session; a foreign tail moves to the session it names, and links name neighbours.

---

## Open decisions

1. **`blind` as a flag or silence.** A flag tells the narrator the page is unknown and costs one prompt line; suppressing only `dead`/`unresponsive` is quieter but lets the narrator infer a dead click from "no effect".
2. ~~**`PRESENCE_GAP_MS`.**~~ Decided: 1 s. rrweb emits mouse moves in 500 ms batches and scrolls at most every 100 ms (`rrweb:3118-3119`, `rrweb:3240`), so 1 s merges continuous movement into one run and splits at the first missed batch.
3. ~~**Requests in `quiet`.**~~ Decided: none. A request the user triggered is inside its gesture's reaction already; counting every request lets background polling (Pulse has it) erase every quiet span.
4. **Where the tail repair runs.** In decode, so every consumer of a recording artifact (compiler, player) sees repaired streams; or only in compile, leaving archived recordings byte-for-byte as PostHog stored them. A player needs the repaired stream either way.
5. ~~**Timeline inside the trace.**~~ Decided: inside, as `trace.timeline` — one walk, and quiet uses gesture reactions. It does not move the trace's actions or TSV, and `run --previous` reuses every narration across the recompile.
6. **The `Stale` window after a wake.** 33 of 41 wakes were followed within 0.2 s by a FullSnapshot. "Within 1 s" treats the rest as stale; a wake whose recorder dropped nothing while idle is not stale at all, and the stream cannot tell those apart.

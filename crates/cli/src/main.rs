//! `spoiler`: headless recording analysis with explicit inputs and outputs.
//!
//! Every command reads named files (`-` for standard input), or upstream APIs for `recordings`
//! and `run --session`, writes one JSON artifact to `--out` (atomically) or stdout, and reports
//! failures on stderr as JSON. Exit codes: `0` success, `1` failure, `2` usage error, `75`
//! transient upstream failure worth retrying.
//!
//! Configuration is flags only; credentials are read from the environment only, so they never
//! appear in process listings or shell history.

mod extract;
mod http;
mod io;
mod openrouter;
mod posthog;
mod vocab_build;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use io::{
    PinnedVocabulary, digest, load_vocabulary, publish, publish_digest, publish_text, read,
    read_recording, read_text,
};
use serde_json::json;
use spoiler_core::{
    analysis::{self, Assessment, Check, GATE_VERSION, Instructions, SessionSummary},
    artifact::{
        AnalysisArtifact, AnalysisProvenance, AnalysisRequest, ExtractCheck, Header, Kind,
        ModelUsage, Narration, PreviousAnalysis, RecordingArtifact, RecordingSource,
        SessionArtifact, TraceArtifact, Versions, Via, VisitNarration, VocabularyCheck, sha256_hex,
    },
    model::Message,
    recording::{self, Limits, Recording},
    trace::{self, COMPILER_VERSION},
    vocab::{Matcher, extract::VocabularyExtract},
};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

const DEFAULT_POSTHOG_HOST: &str = "https://eu.posthog.com";
const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
/// sysexits `EX_TEMPFAIL`: the failure is transient; retrying later may succeed.
const EXIT_RETRYABLE: u8 = 75;
/// Unknown flags, missing arguments, bad values: the invocation, not the input.
const EXIT_USAGE: u8 = 2;

/// A usage error found after parsing (for example, a flag an environment variable can supply
/// is missing from both). Exits with [`EXIT_USAGE`], like the parser's own.
#[derive(Debug)]
struct Usage(&'static str);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Usage {}

#[derive(Parser)]
#[command(
    name = "spoiler",
    version,
    about = "Spoiler: headless session recording analysis"
)]
struct Cli {
    /// Most JSON a recording may decode to, in MiB (decompressed fields included).
    #[arg(long, global = true, default_value_t = 512)]
    max_input_mib: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Output {
    /// Write the artifact here (atomically) instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args)]
struct Network {
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
}

#[derive(clap::Args)]
struct Vocab {
    /// Vocabulary file (YAML, JSON, or a snapshot).
    #[arg(long)]
    vocab: PathBuf,
}

#[derive(clap::Args)]
struct PostHog {
    /// PostHog project id.
    #[arg(long)]
    project: u64,
    #[arg(long, default_value = DEFAULT_POSTHOG_HOST)]
    host: String,
}

/// How to ask a model about a trace.
#[derive(clap::Args)]
struct Ask {
    /// Session header for the model: who the user is, which account.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Replace the built-in system prompt (`crates/core/prompts/narrate/system.md`). Provenance records the
    /// file's name and digest.
    #[arg(long)]
    system_prompt: Option<PathBuf>,
    /// Emit the model request without calling a model. Takes precedence over --model.
    #[arg(long)]
    prepare_only: bool,
    /// OpenRouter model id. Requires OPENROUTER_API_KEY.
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = DEFAULT_OPENROUTER_URL)]
    openrouter_url: String,
}

#[derive(Subcommand)]
enum Command {
    /// Normalize a recording file into a recording artifact.
    Decode {
        /// Recording file, or `-` for standard input.
        #[arg(long)]
        recording: PathBuf,
        #[command(flatten)]
        output: Output,
    },
    /// Compile a recording into a trace against a pinned vocabulary. No network.
    Compile {
        /// Recording file, or `-` for standard input.
        #[arg(long)]
        recording: PathBuf,
        #[command(flatten)]
        vocab: Vocab,
        /// Vocabulary app the recording belongs to.
        #[arg(long)]
        app: String,
        /// Report stage timings on stderr.
        #[arg(long)]
        timings: bool,
        #[command(flatten)]
        output: Output,
    },
    /// Analyze a trace: prepare the model request, validate a given response, or call a model.
    Analyze(AnalyzeArgs),
    /// Fetch (or read), compile, and analyze every visit with user gestures, in one step.
    Run(RunArgs),
    /// Build or check vocabulary snapshots.
    Vocab {
        #[command(subcommand)]
        command: VocabCommand,
    },
    /// Discover and fetch recordings from PostHog. Requires POSTHOG_API_KEY.
    Recordings {
        #[command(subcommand)]
        command: RecordingsCommand,
    },
    /// Print the versions this build writes: artifact shapes, the compiler, the answer gate and
    /// the narration prompt. Compare a stored artifact's against them to decide what to redo.
    Versions,
}

#[derive(clap::Args)]
struct AnalyzeArgs {
    /// Trace file, or `-` for standard input.
    #[arg(
        long,
        conflicts_with = "recording",
        required_unless_present = "recording"
    )]
    trace: Option<PathBuf>,
    /// Compile this recording first (`-` for standard input; requires --app).
    #[arg(long, requires = "app")]
    recording: Option<PathBuf>,
    /// The trace's app; with --trace, checked against it.
    #[arg(long)]
    app: Option<String>,
    #[command(flatten)]
    vocab: Vocab,
    /// Analyze one visit of the trace (see its `visits`) instead of all of it.
    #[arg(long)]
    visit: Option<usize>,
    /// Validate this model response instead of calling a model. Takes precedence over --model.
    #[arg(long, conflicts_with = "prepare_only")]
    response: Option<PathBuf>,
    #[command(flatten)]
    ask: Ask,
    #[command(flatten)]
    network: Network,
    #[command(flatten)]
    output: Output,
}

#[derive(clap::Args)]
struct RunArgs {
    /// PostHog recording (session) id to fetch. Requires --project and POSTHOG_API_KEY.
    #[arg(
        long,
        conflicts_with = "recording",
        required_unless_present = "recording",
        requires = "project"
    )]
    session: Option<String>,
    /// Read this recording instead of fetching one (`-` for standard input).
    #[arg(long)]
    recording: Option<PathBuf>,
    /// PostHog project id, for --session.
    #[arg(long)]
    project: Option<u64>,
    #[arg(long, default_value = DEFAULT_POSTHOG_HOST)]
    host: String,
    /// Most snapshot requests the fetch may make.
    #[arg(long, default_value_t = 50)]
    max_requests: usize,
    /// Most seconds to wait across 429 Retry-After responses when fetching a session.
    #[arg(long, default_value_t = 60, requires = "session")]
    max_wait: u64,
    #[command(flatten)]
    vocab: Vocab,
    /// Vocabulary app the recording belongs to.
    #[arg(long)]
    app: String,
    /// Analyze only this visit, instead of every visit with user gestures.
    #[arg(long)]
    visit: Option<usize>,
    /// Also write the fetched recording artifact here, as `recordings fetch` would.
    #[arg(long, requires = "session")]
    save_recording: Option<PathBuf>,
    /// Also write the trace here, as `compile` would; analyses cite this file's digest.
    #[arg(long)]
    save_trace: Option<PathBuf>,
    /// A session artifact from an earlier `run`: a visit whose model request is unchanged
    /// reuses its analysis instead of asking the model again.
    #[arg(long)]
    previous: Option<PathBuf>,
    #[command(flatten)]
    ask: Ask,
    #[command(flatten)]
    network: Network,
    #[command(flatten)]
    output: Output,
}

#[derive(Subcommand)]
enum VocabCommand {
    /// Build a snapshot from a product config and explicit source files.
    Build {
        #[arg(long)]
        config: PathBuf,
        #[arg(long = "source", required = true)]
        sources: Vec<PathBuf>,
        /// Validate and package this vocabulary instead of generating one. Takes precedence
        /// over --model.
        #[arg(long)]
        candidate: Option<PathBuf>,
        /// OpenRouter model id. Requires OPENROUTER_API_KEY.
        #[arg(long, required_unless_present = "candidate")]
        model: Option<String>,
        /// Revision the sources were read at, recorded as provenance.
        #[arg(long)]
        source_revision: Option<String>,
        #[arg(long, default_value = DEFAULT_OPENROUTER_URL)]
        openrouter_url: String,
        #[command(flatten)]
        network: Network,
        #[command(flatten)]
        output: Output,
    },
    /// Validate a vocabulary and report what it contains and what is inert. With --extract,
    /// also what it claims that the app's source (as `vocab extract` read it) does not say.
    Check {
        #[command(flatten)]
        vocab: Vocab,
        /// A `vocab extract` artifact for one app of the vocabulary.
        #[arg(long)]
        extract: Option<PathBuf>,
        /// Exit 1 when the extract check finds anything (after writing the report).
        #[arg(long, requires = "extract")]
        strict: bool,
        #[command(flatten)]
        output: Output,
    },
    /// Read an app's routes, visible literals and tracked events from its source with a
    /// parser: what `vocab check --extract` holds a vocabulary to. No model, no network.
    Extract {
        /// The vocabulary app these sources are.
        #[arg(long)]
        app: String,
        /// Paths in the extract are relative to this (the repository root).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// The React Router flat-routes directory, relative to --root.
        #[arg(long)]
        routes: PathBuf,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Subcommand)]
enum RecordingsCommand {
    /// One page of recordings in a time window.
    List {
        /// PostHog project id.
        #[arg(long, required_unless_present = "cursor")]
        project: Option<u64>,
        /// PostHog host [default: https://eu.posthog.com].
        #[arg(long)]
        host: Option<String>,
        /// Earliest recording start (any format ClickHouse parses).
        #[arg(long, required_unless_present = "cursor")]
        since: Option<String>,
        /// Latest recording end.
        #[arg(long, required_unless_present = "cursor")]
        until: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Resume after a previous page: its `next_cursor` token. The token carries the
        /// project, host and window; flags given alongside it must match them.
        #[arg(long)]
        cursor: Option<String>,
        #[command(flatten)]
        network: Network,
        #[command(flatten)]
        output: Output,
    },
    /// Download one recording's snapshots.
    Fetch {
        #[command(flatten)]
        posthog: PostHog,
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 50)]
        max_requests: usize,
        /// Most seconds to wait across 429 Retry-After responses.
        #[arg(long, default_value_t = 60)]
        max_wait: u64,
        #[command(flatten)]
        network: Network,
        #[command(flatten)]
        output: Output,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and version are output, not failures.
        Err(error) if !error.use_stderr() => error.exit(),
        Err(error) => {
            let message = error.render().to_string();
            eprintln!(
                "{}",
                json!({ "error": message.trim_end(), "retryable": false })
            );
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let retryable = http::is_retryable(&error);
            eprintln!(
                "{}",
                json!({ "error": format!("{error:#}"), "retryable": retryable })
            );
            let code = if retryable {
                EXIT_RETRYABLE
            } else if error.is::<Usage>() {
                EXIT_USAGE
            } else {
                1
            };
            ExitCode::from(code)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let limits = Limits {
        max_bytes: cli
            .max_input_mib
            .checked_mul(1 << 20)
            .context("--max-input-mib is too large")?,
    };
    match cli.command {
        Command::Decode { recording, output } => {
            let raw = read_recording(&recording, limits)?;
            let artifact = RecordingArtifact::new(
                RecordingSource::File {
                    sha256: sha256_hex(&raw),
                },
                recording::decode_with(&raw, limits)?,
            );
            publish(&artifact, output.out.as_deref())
        }
        Command::Compile {
            recording,
            vocab,
            app,
            timings,
            output,
        } => {
            let vocabulary = load_vocabulary(&vocab.vocab)?;
            let (_, trace) = compile(&recording, &vocabulary, &app, timings, limits)?;
            publish(&trace, output.out.as_deref())
        }
        Command::Analyze(args) => analyze(args, limits),
        Command::Run(args) => run_session(args, limits),
        Command::Vocab { command } => vocab(command),
        Command::Recordings { command } => recordings(command, limits),
        Command::Versions => publish(&Versions::current(), None),
    }
}

fn ensure_app(vocabulary: &PinnedVocabulary, app: &str) -> Result<()> {
    ensure!(
        vocabulary.vocabulary.apps.contains_key(app),
        "the vocabulary has no app {app}"
    );
    Ok(())
}

/// Read, decode and compile a recording file. Returns the file's digest with the trace.
fn compile(
    path: &Path,
    vocabulary: &PinnedVocabulary,
    app: &str,
    timings: bool,
    limits: Limits,
) -> Result<(String, TraceArtifact)> {
    ensure_app(vocabulary, app)?;
    let started = Instant::now();
    let raw = read_recording(path, limits)?;
    let read_done = started.elapsed();
    let recording = recording::decode_with(&raw, limits)?;
    let decoded = started.elapsed();
    let recording_digest = sha256_hex(&raw);
    drop(raw);
    let trace = compile_recording(&recording, recording_digest.clone(), vocabulary, app)?;
    if timings {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        eprintln!(
            "{}",
            json!({
                "events": recording.len(),
                "actions": trace.actions.len(),
                "read_ms": ms(read_done),
                "decode_ms": ms(decoded - read_done),
                "compile_ms": ms(started.elapsed() - decoded),
            })
        );
    }
    Ok((recording_digest, trace))
}

fn compile_recording(
    recording: &Recording,
    recording_digest: String,
    vocabulary: &PinnedVocabulary,
    app: &str,
) -> Result<TraceArtifact> {
    ensure_app(vocabulary, app)?;
    let matcher = Matcher::new(&vocabulary.vocabulary);
    let trace::Compilation {
        actions,
        coverage,
        timeline,
    } = trace::compile(recording, &matcher, app)?;
    let tsv = trace::to_tsv(&actions);
    let visits = trace::visits(&actions, &vocabulary.vocabulary.thresholds);
    Ok(TraceArtifact {
        header: Header::new(Kind::Trace),
        compiler_version: COMPILER_VERSION,
        app: app.to_owned(),
        recording_digest,
        vocab_digest: vocabulary.digest.clone(),
        actions,
        coverage,
        visits,
        timeline,
        tsv,
    })
}

/// What narrating a trace needs besides the question: the trace, its identity, and the
/// instructions and session header the model is given.
struct Narrator<'a> {
    vocabulary: &'a PinnedVocabulary,
    trace: &'a TraceArtifact,
    trace_digest: &'a str,
    session_header: String,
    custom_prompt: Option<(String, String)>,
}

/// How a narration is answered.
enum Answer {
    /// Only prepare the request.
    Request,
    /// Validate an existing model response.
    Response(String),
    Model(openrouter::OpenRouter),
}

impl<'a> Narrator<'a> {
    fn new(
        vocabulary: &'a PinnedVocabulary,
        trace: &'a TraceArtifact,
        trace_digest: &'a str,
        ask: &Ask,
    ) -> Result<Self> {
        ensure!(
            trace.vocab_digest == vocabulary.digest,
            "the trace was compiled against a different vocabulary than --vocab"
        );
        let session_header = ask
            .context
            .as_deref()
            .map(read_text)
            .transpose()?
            .unwrap_or_default();
        let custom_prompt = ask
            .system_prompt
            .as_deref()
            .map(|path| {
                // The file name, not the path: provenance should not depend on where it was run.
                let name = path.file_name().unwrap_or(path.as_os_str());
                Ok::<_, anyhow::Error>((name.to_string_lossy().into_owned(), read_text(path)?))
            })
            .transpose()?;
        Ok(Self {
            vocabulary,
            trace,
            trace_digest,
            session_header,
            custom_prompt,
        })
    }

    /// Narrate one visit of the trace, or all of it. A visit's actions keep their refs, so its
    /// analysis cites the same refs as the trace's. An earlier analysis of the identical
    /// request is reused instead of asking again: kept if this gate accepted it, judged again
    /// if not.
    fn narrate(
        &self,
        visit: Option<usize>,
        answer: &Answer,
        previous: &[PreviousAnalysis],
    ) -> Result<(Narration, Via)> {
        let trace = self.trace;
        let (actions, tsv) = match visit {
            Some(index) => {
                let actions = visit_actions(trace, index)?;
                (actions, trace::to_tsv(actions))
            }
            None => (&trace.actions[..], trace.tsv.clone()),
        };
        let instructions = match &self.custom_prompt {
            Some((name, text)) => Instructions { name, system: text },
            None => Instructions::default(),
        };
        let vocabulary = &self.vocabulary.vocabulary;
        let messages = analysis::build_messages(
            instructions,
            vocabulary,
            &trace.app,
            &self.session_header,
            actions,
            &tsv,
        );
        let response_schema = analysis::response_schema();
        let provenance = AnalysisProvenance {
            visit,
            app: trace.app.clone(),
            trace_digest: self.trace_digest.to_owned(),
            vocab_digest: self.vocabulary.digest.clone(),
            prompt: instructions.id(),
            request_digest: sha256_hex(&serde_json::to_vec(&(&messages, response_schema))?),
        };
        let earlier = previous
            .iter()
            .find(|analysis| analysis.request_digest == provenance.request_digest);
        // Reuse costs no model call, so it applies to --prepare-only too: what is left as a
        // request is exactly what would be paid for.
        let reused = match earlier {
            Some(earlier) if earlier.gate_version == GATE_VERSION => Some((
                Answered {
                    summary: earlier.summary.clone(),
                    check: earlier.check.clone(),
                    model: earlier.model.clone(),
                    answer: earlier.answer.clone(),
                },
                Via::Reused,
            )),
            Some(PreviousAnalysis {
                answer: Some(text),
                model,
                ..
            }) => match analysis::assess(text, actions, vocabulary) {
                Assessment::Accepted { summary, check } => Some((
                    Answered {
                        summary,
                        check,
                        model: model.clone(),
                        answer: Some(text.clone()),
                    },
                    Via::Regated,
                )),
                // This gate refuses what an older one accepted: ask again, as for a new request.
                Assessment::Rejected { .. } => None,
            },
            _ => None,
        };
        let (answered, via) = match reused {
            Some(reused) => reused,
            None if matches!(answer, Answer::Request) => {
                let request = AnalysisRequest {
                    header: Header::new(Kind::AnalysisRequest),
                    provenance,
                    messages,
                    response_schema: response_schema.clone(),
                };
                return Ok((Narration::Request(request), Via::Narrated));
            }
            None => (self.ask(answer, messages, actions)?, Via::Narrated),
        };
        let analysis = AnalysisArtifact {
            header: Header::new(Kind::Analysis),
            provenance,
            summary: answered.summary,
            check: answered.check,
            model: answered.model,
            gate_version: GATE_VERSION,
            answer: answered.answer,
        };
        Ok((Narration::Analysis(analysis), via))
    }

    /// An answer from the response file or the model, held to this gate.
    fn ask(
        &self,
        answer: &Answer,
        messages: Vec<Message>,
        actions: &[trace::Action],
    ) -> Result<Answered> {
        let vocabulary = &self.vocabulary.vocabulary;
        match answer {
            Answer::Request => anyhow::bail!("a prepared request has no answer"),
            // Held to the same bar as a live answer: a response the retry loop would send back
            // is not an analysis.
            Answer::Response(response) => match analysis::assess(response, actions, vocabulary) {
                Assessment::Accepted { summary, check } => Ok(Answered {
                    summary,
                    check,
                    model: None,
                    answer: Some(response.clone()),
                }),
                Assessment::Rejected { reason } => {
                    anyhow::bail!("model response rejected: {reason}")
                }
            },
            Answer::Model(client) => {
                let (summary, check, usage, content) =
                    client.narrate(messages, actions, vocabulary)?;
                Ok(Answered {
                    summary,
                    check,
                    model: Some(usage),
                    answer: Some(content),
                })
            }
        }
    }
}

/// An accepted answer and what it cost.
struct Answered {
    summary: SessionSummary,
    check: Check,
    model: Option<ModelUsage>,
    answer: Option<String>,
}

fn visit_actions(trace: &TraceArtifact, index: usize) -> Result<&[trace::Action]> {
    let visit = trace.visits.get(index).with_context(|| {
        format!(
            "the trace has {} visits; --visit {index} is out of range",
            trace.visits.len()
        )
    })?;
    trace.actions.get(visit.start..visit.end).with_context(|| {
        format!(
            "visit {index} spans actions {}..{}, outside the trace's {} actions",
            visit.start,
            visit.end,
            trace.actions.len()
        )
    })
}

/// `--prepare-only` and a given response take precedence over `--model`.
fn answer(ask: &Ask, response: Option<&Path>, network: &Network) -> Result<Answer> {
    if ask.prepare_only {
        return Ok(Answer::Request);
    }
    if let Some(response) = response {
        return Ok(Answer::Response(read_text(response)?));
    }
    let model = ask.model.as_deref().ok_or(Usage(
        "--model is required unless --response or --prepare-only is given",
    ))?;
    Ok(Answer::Model(openrouter::OpenRouter::new(
        &ask.openrouter_url,
        model,
        network.timeout,
    )?))
}

fn analyze(args: AnalyzeArgs, limits: Limits) -> Result<()> {
    let vocabulary = load_vocabulary(&args.vocab.vocab)?;
    let (trace, trace_digest) = match (&args.trace, &args.recording, &args.app) {
        (Some(path), _, _) => {
            let bytes = read(path)?;
            let trace = TraceArtifact::from_json(&bytes)
                .with_context(|| format!("loading trace {}", path.display()))?;
            (trace, sha256_hex(&bytes))
        }
        (None, Some(recording), Some(app)) => {
            let (_, trace) = compile(recording, &vocabulary, app, false, limits)?;
            // The digest of the bytes `compile` would publish.
            let digest = digest(&trace)?;
            (trace, digest)
        }
        _ => anyhow::bail!("either --trace, or --recording with --app, is required"),
    };
    if let Some(app) = &args.app {
        ensure!(
            *app == trace.app,
            "--app {app} disagrees with the trace's app {}",
            trace.app
        );
    }
    let narrator = Narrator::new(&vocabulary, &trace, &trace_digest, &args.ask)?;
    let answer = answer(&args.ask, args.response.as_deref(), &args.network)?;
    let out = args.output.out.as_deref();
    match narrator.narrate(args.visit, &answer, &[])?.0 {
        Narration::Request(request) => publish(&request, out),
        Narration::Analysis(analysis) => publish(&analysis, out),
    }
}

fn run_session(args: RunArgs, limits: Limits) -> Result<()> {
    let vocabulary = load_vocabulary(&args.vocab.vocab)?;
    ensure_app(&vocabulary, &args.app)?;
    // Settle how to answer before spending a download on the recording.
    let answer = answer(&args.ask, None, &args.network)?;
    let (source, trace) = match (&args.session, &args.recording) {
        (Some(session), _) => {
            let snapshot = posthog::Snapshot {
                host: &args.host,
                project: args.project.context("--session requires --project")?,
                session,
                max_requests: args.max_requests,
                max_wait: args.max_wait,
                limits,
            };
            let artifact = posthog::fetch(&http::Http::new(args.network.timeout)?, &snapshot)?;
            // The digest `compile` would record for the file `recordings fetch` writes.
            let recording_digest = match &args.save_recording {
                Some(path) => publish_digest(&artifact, Some(path))?,
                None => digest(&artifact)?,
            };
            let trace =
                compile_recording(&artifact.events, recording_digest, &vocabulary, &args.app)?;
            (artifact.source, trace)
        }
        (None, Some(path)) => {
            let (sha256, trace) = compile(path, &vocabulary, &args.app, false, limits)?;
            (RecordingSource::File { sha256 }, trace)
        }
        (None, None) => anyhow::bail!("either --session or --recording is required"),
    };
    let trace_digest = match &args.save_trace {
        Some(path) => publish_digest(&trace, Some(path))?,
        None => digest(&trace)?,
    };
    let previous = match &args.previous {
        Some(path) => SessionArtifact::previous_analyses(&read(path)?)
            .with_context(|| format!("loading previous session {}", path.display()))?,
        None => Vec::new(),
    };
    let narrator = Narrator::new(&vocabulary, &trace, &trace_digest, &args.ask)?;
    let visits: Vec<usize> = match args.visit {
        Some(index) => vec![index],
        None => (0..trace.visits.len())
            .filter(|&index| {
                visit_actions(&trace, index)
                    .is_ok_and(|actions| actions.iter().any(|a| a.kind().is_gesture()))
            })
            .collect(),
    };
    let visits = visits
        .into_iter()
        .map(|visit| {
            let (narration, via) = narrator
                .narrate(Some(visit), &answer, &previous)
                .with_context(|| format!("narrating visit {visit}"))?;
            Ok(VisitNarration {
                visit,
                via,
                narration,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let session = SessionArtifact {
        header: Header::new(Kind::Session),
        source,
        trace,
        visits,
    };
    publish(&session, args.output.out.as_deref())
}

fn vocab(command: VocabCommand) -> Result<()> {
    match command {
        VocabCommand::Check {
            vocab,
            extract,
            strict,
            output,
        } => {
            let PinnedVocabulary { vocabulary, digest } = load_vocabulary(&vocab.vocab)?;
            let extract = match &extract {
                Some(path) => {
                    let bytes = read(path)?;
                    let parsed: VocabularyExtract = serde_json::from_slice(&bytes)
                        .with_context(|| format!("loading extract {}", path.display()))?;
                    parsed.header.check(Kind::VocabularyExtract)?;
                    ensure!(
                        vocabulary.apps.contains_key(&parsed.app),
                        "the vocabulary has no app {}",
                        parsed.app
                    );
                    Some(ExtractCheck {
                        findings: spoiler_core::vocab::extract::check(&vocabulary, &parsed),
                        app: parsed.app,
                        sha256: sha256_hex(&bytes),
                    })
                }
                None => None,
            };
            let found = extract.as_ref().map_or(0, |check| check.findings.len());
            let report = VocabularyCheck {
                header: Header::new(Kind::VocabularyCheck),
                sha256: digest,
                warnings: vocabulary.warnings(),
                surfaces: vocabulary.surfaces.len(),
                features: vocabulary.features.len(),
                apps: vocabulary.apps,
                extract,
            };
            publish(&report, output.out.as_deref())?;
            ensure!(
                !strict || found == 0,
                "the vocabulary disagrees with its source in {found} place(s); see the report"
            );
            Ok(())
        }
        VocabCommand::Extract {
            app,
            root,
            routes,
            output,
        } => {
            let extracted = extract::extract(&extract::Request {
                app: &app,
                root: &root,
                routes: &routes,
            })?;
            publish_text(&extracted.to_json_lines(), output.out.as_deref())
        }
        VocabCommand::Build {
            config,
            sources,
            candidate,
            model,
            source_revision,
            openrouter_url,
            network,
            output,
        } => {
            // A prepared candidate needs no model.
            let client = match (&candidate, model.as_deref()) {
                (Some(_), _) | (None, None) => None,
                (None, Some(model)) => Some(openrouter::OpenRouter::new(
                    &openrouter_url,
                    model,
                    network.timeout,
                )?),
            };
            let inputs = vocab_build::Build {
                config,
                sources,
                candidate,
                source_revision,
            };
            let snapshot = vocab_build::build(&inputs, client.as_ref())?;
            publish(&snapshot, output.out.as_deref())
        }
    }
}

fn recordings(command: RecordingsCommand, limits: Limits) -> Result<()> {
    match command {
        RecordingsCommand::List {
            project,
            host,
            since,
            until,
            limit,
            cursor,
            network,
            output,
        } => {
            let given = posthog::PartialWindow {
                host,
                project,
                since,
                until,
            };
            let (window, after) = match cursor.as_deref() {
                Some(token) => {
                    let cursor = posthog::Cursor::decode(token)?;
                    ensure!(
                        given.agrees_with(&cursor.window),
                        "the cursor belongs to a different discovery window than the flags given"
                    );
                    (cursor.window, Some(cursor.after))
                }
                // The parser requires project, since and until without a cursor.
                None => (given.complete(DEFAULT_POSTHOG_HOST)?, None),
            };
            let query = posthog::Discovery { window, limit };
            let page = posthog::list(&http::Http::new(network.timeout)?, &query, after.as_ref())?;
            publish(&page, output.out.as_deref())
        }
        RecordingsCommand::Fetch {
            posthog,
            session,
            max_requests,
            max_wait,
            network,
            output,
        } => {
            let snapshot = posthog::Snapshot {
                host: &posthog.host,
                project: posthog.project,
                session: &session,
                max_requests,
                max_wait,
                limits,
            };
            let recording = posthog::fetch(&http::Http::new(network.timeout)?, &snapshot)?;
            publish(&recording, output.out.as_deref())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn corpus(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpus")
            .join(name)
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn spoiler(args: &[&str]) {
        let cli = Cli::try_parse_from(std::iter::once("spoiler").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{error}"));
        run(cli).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn trace_digest_tracks_file_bytes_and_compiled_publication() {
        let vocab = corpus("vocabulary.yaml");
        let recording = corpus("click_changes_text.json");
        let (_, trace) = compile(
            &recording,
            &load_vocabulary(&vocab).unwrap(),
            "demo",
            false,
            Limits::default(),
        )
        .unwrap();
        let directory = scratch("trace-digest");
        let trace_path = directory.join("trace.json");
        let output_path = directory.join("request.json");
        let bytes = format!("  {} \n", serde_json::to_string(&trace).unwrap()).into_bytes();
        std::fs::write(&trace_path, &bytes).unwrap();

        let vocab = vocab.to_str().unwrap();
        let out = output_path.to_str().unwrap();
        let digest_of = |input: &[&str]| {
            let mut args = vec!["analyze", "--vocab", vocab, "--prepare-only", "--out", out];
            args.extend(input);
            spoiler(&args);
            read_json(&output_path)["trace_digest"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            digest_of(&["--trace", trace_path.to_str().unwrap()]),
            sha256_hex(&bytes),
            "a stored trace is identified by its exact file bytes"
        );
        let mut published = serde_json::to_vec(&trace).unwrap();
        published.push(b'\n');
        assert_eq!(
            digest_of(&["--recording", recording.to_str().unwrap(), "--app", "demo"]),
            sha256_hex(&published),
            "an in-memory compile hashes the bytes compile would publish"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn run_narrates_each_visit_with_gestures_citing_the_saved_trace() {
        // Two visits with clicks, then a third, long after, where the user only navigated.
        let mut case = read_json(&corpus("absence_splits_visits.json"));
        let events = case["events"].as_array_mut().unwrap();
        let last = events.last().unwrap()["timestamp"].as_f64().unwrap();
        events.push(serde_json::json!({
            "type": 4, "timestamp": last + 3_600_000.0, "win": "w1",
            "data": { "href": "https://demo.test/other" },
        }));
        let directory = scratch("run-visits");
        let recording = directory.join("recording.json");
        std::fs::write(&recording, serde_json::to_vec(&case).unwrap()).unwrap();
        let (trace_path, session_path) = (directory.join("trace.json"), directory.join("s.json"));
        spoiler(&[
            "run",
            "--recording",
            recording.to_str().unwrap(),
            "--vocab",
            corpus("vocabulary.yaml").to_str().unwrap(),
            "--app",
            "demo",
            "--prepare-only",
            "--save-trace",
            trace_path.to_str().unwrap(),
            "--out",
            session_path.to_str().unwrap(),
        ]);

        let session = read_json(&session_path);
        let trace_bytes = std::fs::read(&trace_path).unwrap();
        assert_eq!(session["trace"]["visits"].as_array().unwrap().len(), 3);
        let visits = session["visits"].as_array().unwrap();
        let narrated: Vec<u64> = visits
            .iter()
            .map(|v| v["visit"].as_u64().unwrap())
            .collect();
        assert_eq!(
            narrated,
            [0, 1],
            "the navigation-only visit is not narrated"
        );
        for visit in visits {
            let request = &visit["request"];
            assert_eq!(request["visit"], visit["visit"]);
            assert_eq!(request["trace_digest"], sha256_hex(&trace_bytes).as_str());
        }
        assert_eq!(
            session["trace"],
            serde_json::from_slice::<Value>(&trace_bytes).unwrap()
        );
        assert_eq!(
            session["source"]["sha256"],
            sha256_hex(&std::fs::read(&recording).unwrap()).as_str()
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn run_reuses_an_earlier_analysis_of_the_same_request_and_regates_an_older_one() {
        let directory = scratch("run-previous");
        let vocab = corpus("vocabulary.yaml");
        let recording = corpus("click_changes_text.json");
        let (analysis_path, previous_path, session_path) = (
            directory.join("analysis.json"),
            directory.join("previous.json"),
            directory.join("session.json"),
        );
        let common = [
            "--recording",
            recording.to_str().unwrap(),
            "--vocab",
            vocab.to_str().unwrap(),
            "--app",
            "demo",
        ];
        // An analysis of visit 0 from an answer on file: what an earlier run would have stored.
        let answer = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/click_changes_text.response.json");
        let mut args = vec![
            "analyze",
            "--visit",
            "0",
            "--response",
            answer.to_str().unwrap(),
        ];
        args.extend(common);
        args.extend(["--out", analysis_path.to_str().unwrap()]);
        spoiler(&args);
        let analysis = read_json(&analysis_path);

        let rerun = |previous_analysis: Value| {
            let previous = serde_json::json!({
                "schema_version": Kind::Session.schema_version(),
                "kind": "session",
                "visits": [{ "visit": 0, "analysis": previous_analysis }],
            });
            std::fs::write(&previous_path, previous.to_string()).unwrap();
            let mut args = vec!["run", "--prepare-only", "--visit", "0"];
            args.extend(common);
            args.extend([
                "--previous",
                previous_path.to_str().unwrap(),
                "--out",
                session_path.to_str().unwrap(),
            ]);
            spoiler(&args);
            read_json(&session_path)["visits"][0].clone()
        };

        let reused = rerun(analysis.clone());
        assert_eq!(
            reused["via"], "reused",
            "same request, same gate: no model call"
        );
        assert_eq!(reused["analysis"]["summary"], analysis["summary"]);
        assert_eq!(reused["analysis"]["answer"], analysis["answer"]);

        let mut older = analysis.clone();
        older["gate_version"] = serde_json::json!(GATE_VERSION - 1);
        let regated = rerun(older.clone());
        assert_eq!(
            regated["via"], "regated",
            "an older gate's answer is judged again"
        );
        assert_eq!(regated["analysis"]["gate_version"], GATE_VERSION);

        older.as_object_mut().unwrap().remove("answer");
        let asked = rerun(older);
        assert_eq!(
            asked["via"], "narrated",
            "no stored answer to judge: ask again"
        );
        assert!(asked["request"].is_object());

        // A caller that kept analyses as rows rebuilds only what reuse reads.
        let rows = serde_json::json!({
            "request_digest": analysis["request_digest"],
            "summary": analysis["summary"],
            "check": analysis["check"],
        });
        assert_eq!(
            rerun(rows)["via"],
            "reused",
            "no provenance, gate or answer needed"
        );

        let mut changed = analysis;
        changed["request_digest"] = serde_json::json!("another question");
        assert!(
            rerun(changed)["request"].is_object(),
            "a changed request is asked"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn versions_name_what_this_build_writes() {
        let versions = serde_json::to_value(Versions::current()).unwrap();
        assert_eq!(versions["compiler"], COMPILER_VERSION);
        assert_eq!(versions["gate"], GATE_VERSION);
        assert_eq!(versions["schemas"]["trace"], Kind::Trace.schema_version());
        assert_eq!(
            versions["prompt"]["sha256"],
            Instructions::default().id().sha256.as_str(),
            "the prompt identity is the one analyses record"
        );
    }
}

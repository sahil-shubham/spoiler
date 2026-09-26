//! `spoiler`: headless recording analysis with explicit inputs and outputs.
//!
//! Every command reads named files (`-` for standard input), or upstream APIs for `recordings`
//! and `run --session`, writes one JSON artifact to `--out` (atomically) or stdout, and reports
//! failures on stderr as JSON. Exit codes: `0` success, `1` failure, `2` usage error, `75`
//! transient upstream failure worth retrying.
//!
//! Settings that stay the same across invocations fall back to `SPOILER_*` environment
//! variables; a flag always wins. Credentials are read from the environment only.

mod http;
mod io;
mod openrouter;
mod posthog;
mod vocab_build;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use io::{
    PinnedVocabulary, digest, load_vocabulary, publish, publish_digest, read, read_recording,
    read_text,
};
use serde_json::json;
use spoiler_core::{
    analysis::{self, Assessment, Instructions},
    artifact::{
        AnalysisArtifact, AnalysisProvenance, AnalysisRequest, Header, Kind, Narration,
        RecordingArtifact, RecordingSource, SessionArtifact, TraceArtifact, VisitNarration,
        VocabularyCheck, sha256_hex,
    },
    recording::{self, Limits, Recording},
    trace::{self, COMPILER_VERSION},
    vocab::Matcher,
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
    #[arg(
        long,
        global = true,
        env = "SPOILER_MAX_INPUT_MIB",
        default_value_t = 512
    )]
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
    #[arg(long, env = "SPOILER_TIMEOUT", default_value_t = 120)]
    timeout: u64,
}

#[derive(clap::Args)]
struct Vocab {
    /// Vocabulary file (YAML, JSON, or a snapshot).
    #[arg(long, env = "SPOILER_VOCAB")]
    vocab: PathBuf,
}

#[derive(clap::Args)]
struct PostHog {
    /// PostHog project id.
    #[arg(long, env = "SPOILER_POSTHOG_PROJECT")]
    project: u64,
    #[arg(long, env = "SPOILER_POSTHOG_HOST", default_value = DEFAULT_POSTHOG_HOST)]
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
    #[arg(long, env = "SPOILER_MODEL")]
    model: Option<String>,
    #[arg(long, env = "SPOILER_OPENROUTER_URL", default_value = DEFAULT_OPENROUTER_URL)]
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
        #[arg(long, env = "SPOILER_APP")]
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
    #[arg(long, env = "SPOILER_APP")]
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
    #[arg(long, env = "SPOILER_POSTHOG_PROJECT")]
    project: Option<u64>,
    #[arg(long, env = "SPOILER_POSTHOG_HOST", default_value = DEFAULT_POSTHOG_HOST)]
    host: String,
    /// Most snapshot requests the fetch may make.
    #[arg(long, default_value_t = 50)]
    max_requests: usize,
    #[command(flatten)]
    vocab: Vocab,
    /// Vocabulary app the recording belongs to.
    #[arg(long, env = "SPOILER_APP")]
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
        #[arg(long, env = "SPOILER_MODEL", required_unless_present = "candidate")]
        model: Option<String>,
        /// Revision the sources were read at, recorded as provenance.
        #[arg(long)]
        source_revision: Option<String>,
        #[arg(long, env = "SPOILER_OPENROUTER_URL", default_value = DEFAULT_OPENROUTER_URL)]
        openrouter_url: String,
        #[command(flatten)]
        network: Network,
        #[command(flatten)]
        output: Output,
    },
    /// Validate a vocabulary and report what it contains and what is inert.
    Check {
        #[command(flatten)]
        vocab: Vocab,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Subcommand)]
enum RecordingsCommand {
    /// One page of recordings in a time window.
    List {
        #[command(flatten)]
        posthog: PostHog,
        /// Earliest recording start (any format ClickHouse parses).
        #[arg(long)]
        since: String,
        /// Latest recording end.
        #[arg(long)]
        until: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Resume from a previous page's `next_cursor`, saved as JSON.
        #[arg(long)]
        cursor: Option<PathBuf>,
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
    let trace::Compilation { actions, coverage } = trace::compile(recording, &matcher, app)?;
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
    /// analysis cites the same refs as the trace's.
    fn narrate(&self, visit: Option<usize>, answer: &Answer) -> Result<Narration> {
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
        let (summary, check, model) = match answer {
            Answer::Request => {
                return Ok(Narration::Request(AnalysisRequest {
                    header: Header::new(Kind::AnalysisRequest),
                    provenance,
                    messages,
                    response_schema: response_schema.clone(),
                }));
            }
            // Held to the same bar as a live answer: a response the retry loop would send back
            // is not an analysis.
            Answer::Response(response) => match analysis::assess(response, actions, vocabulary) {
                Assessment::Accepted { summary, check } => (summary, check, None),
                Assessment::Rejected { reason } => {
                    anyhow::bail!("model response rejected: {reason}")
                }
            },
            Answer::Model(client) => {
                let (summary, check, usage) = client.narrate(messages, actions, vocabulary)?;
                (summary, check, Some(usage))
            }
        };
        Ok(Narration::Analysis(AnalysisArtifact {
            header: Header::new(Kind::Analysis),
            provenance,
            summary,
            check,
            model,
        }))
    }
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

/// `--prepare-only` and a given response take precedence over a model (which may come from
/// `SPOILER_MODEL`).
fn answer(ask: &Ask, response: Option<&Path>, network: &Network) -> Result<Answer> {
    if ask.prepare_only {
        return Ok(Answer::Request);
    }
    if let Some(response) = response {
        return Ok(Answer::Response(read_text(response)?));
    }
    let model = ask.model.as_deref().ok_or(Usage(
        "--model (or SPOILER_MODEL) is required unless --response or --prepare-only is given",
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
    match narrator.narrate(args.visit, &answer)? {
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
            let narration = narrator
                .narrate(Some(visit), &answer)
                .with_context(|| format!("narrating visit {visit}"))?;
            Ok(VisitNarration { visit, narration })
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
        VocabCommand::Check { vocab, output } => {
            let PinnedVocabulary { vocabulary, digest } = load_vocabulary(&vocab.vocab)?;
            let report = VocabularyCheck {
                header: Header::new(Kind::VocabularyCheck),
                sha256: digest,
                warnings: vocabulary.warnings(),
                surfaces: vocabulary.surfaces.len(),
                features: vocabulary.features.len(),
                apps: vocabulary.apps,
            };
            publish(&report, output.out.as_deref())
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
            // A prepared candidate needs no model, even when SPOILER_MODEL is set.
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
            posthog,
            since,
            until,
            limit,
            cursor,
            network,
            output,
        } => {
            let cursor: Option<posthog::Cursor> = cursor
                .as_deref()
                .map(|path| {
                    serde_json::from_slice(&read(path)?)
                        .with_context(|| format!("invalid cursor {}", path.display()))
                })
                .transpose()?;
            let query = posthog::Discovery {
                host: &posthog.host,
                project: posthog.project,
                since: &since,
                until: &until,
                limit,
            };
            let page = posthog::list(&http::Http::new(network.timeout)?, &query, cursor.as_ref())?;
            publish(&page, output.out.as_deref())
        }
        RecordingsCommand::Fetch {
            posthog,
            session,
            max_requests,
            network,
            output,
        } => {
            let snapshot = posthog::Snapshot {
                host: &posthog.host,
                project: posthog.project,
                session: &session,
                max_requests,
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
}

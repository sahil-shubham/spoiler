//! `spoiler`: headless recording analysis with explicit inputs and outputs.
//!
//! Every command reads named files (or upstream APIs for `recordings`), writes one JSON artifact
//! to `--out` (atomically) or stdout, and reports failures on stderr as JSON. Exit codes:
//! `0` success, `1` failure, `2` usage error, `75` transient upstream failure worth retrying.

mod http;
mod io;
mod openrouter;
mod posthog;
mod vocab_build;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use io::{PinnedVocabulary, load_vocabulary, publish, read, read_recording, read_text};
use serde_json::json;
use spoiler_core::{
    analysis::{self, Assessment, Instructions},
    artifact::{
        AnalysisArtifact, AnalysisProvenance, AnalysisRequest, Header, Kind, RecordingArtifact,
        RecordingSource, TraceArtifact, VocabularyCheck, sha256_hex,
    },
    recording::{self, Limits},
    trace::{self, COMPILER_VERSION},
    vocab::Matcher,
};
use std::{path::PathBuf, process::ExitCode, time::Instant};

const DEFAULT_POSTHOG_HOST: &str = "https://eu.posthog.com";
const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
/// sysexits `EX_TEMPFAIL`: the failure is transient; retrying later may succeed.
const EXIT_RETRYABLE: u8 = 75;
/// Unknown flags, missing arguments, bad values: the invocation, not the input.
const EXIT_USAGE: u8 = 2;

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

#[derive(Subcommand)]
enum Command {
    /// Normalize a recording file into a recording artifact.
    Decode {
        #[arg(long)]
        recording: PathBuf,
        #[command(flatten)]
        output: Output,
    },
    /// Compile a recording into a trace against a pinned vocabulary. No network.
    Compile {
        #[arg(long)]
        recording: PathBuf,
        #[arg(long)]
        vocab: PathBuf,
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
    #[arg(
        long,
        conflicts_with = "recording",
        required_unless_present = "recording"
    )]
    trace: Option<PathBuf>,
    /// Compile this recording first (requires --app).
    #[arg(long, requires = "app")]
    recording: Option<PathBuf>,
    #[arg(long)]
    app: Option<String>,
    #[arg(long)]
    vocab: PathBuf,
    /// Session header for the model: who the user is, which account.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Replace the built-in system prompt (`prompts/narrate/system.md`). Provenance records the
    /// file's name and digest.
    #[arg(long)]
    system_prompt: Option<PathBuf>,
    /// Emit the model request without calling a model.
    #[arg(long, conflicts_with_all = ["response", "model"])]
    prepare_only: bool,
    /// Analyze one visit of the trace (see its `visits`) instead of all of it.
    #[arg(long)]
    visit: Option<usize>,
    /// Validate this model response instead of calling a model.
    #[arg(long, conflicts_with = "model")]
    response: Option<PathBuf>,
    /// OpenRouter model id. Requires OPENROUTER_API_KEY.
    #[arg(long, required_unless_present_any = ["prepare_only", "response"])]
    model: Option<String>,
    #[arg(long, default_value = DEFAULT_OPENROUTER_URL)]
    openrouter_url: String,
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
        /// Validate and package this vocabulary instead of generating one.
        #[arg(long, conflicts_with = "model")]
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
    /// Validate a vocabulary and report what it contains and what is inert.
    Check {
        #[arg(long)]
        vocab: PathBuf,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Subcommand)]
enum RecordingsCommand {
    /// One page of recordings in a time window.
    List {
        #[arg(long)]
        project: u64,
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
        #[arg(long, default_value = DEFAULT_POSTHOG_HOST)]
        host: String,
        #[command(flatten)]
        network: Network,
        #[command(flatten)]
        output: Output,
    },
    /// Download one recording's snapshots.
    Fetch {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 50)]
        max_requests: usize,
        #[arg(long, default_value = DEFAULT_POSTHOG_HOST)]
        host: String,
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
            ExitCode::from(if retryable { EXIT_RETRYABLE } else { 1 })
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
            let vocabulary = load_vocabulary(&vocab)?;
            let trace = compile(&recording, &vocabulary, &app, timings, limits)?;
            publish(&trace, output.out.as_deref())
        }
        Command::Analyze(args) => analyze(args, limits),
        Command::Vocab { command } => vocab(command),
        Command::Recordings { command } => recordings(command, limits),
    }
}

fn compile(
    recording: &std::path::Path,
    vocabulary: &PinnedVocabulary,
    app: &str,
    timings: bool,
    limits: Limits,
) -> Result<TraceArtifact> {
    ensure!(
        vocabulary.vocabulary.apps.contains_key(app),
        "the vocabulary has no app {app}"
    );
    let started = Instant::now();
    let raw = read_recording(recording, limits)?;
    let read_done = started.elapsed();
    let recording = recording::decode_with(&raw, limits)?;
    let decoded = started.elapsed();
    let matcher = Matcher::new(&vocabulary.vocabulary);
    let trace::Compilation { actions, coverage } = trace::compile(&recording, &matcher, app)?;
    let compiled = started.elapsed();
    let tsv = trace::to_tsv(&actions);
    let visits = trace::visits(&actions, &vocabulary.vocabulary.thresholds);
    if timings {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        eprintln!(
            "{}",
            json!({
                "events": recording.len(),
                "actions": actions.len(),
                "read_ms": ms(read_done),
                "decode_ms": ms(decoded - read_done),
                "compile_ms": ms(compiled - decoded),
                "render_ms": ms(started.elapsed() - compiled),
            })
        );
    }
    Ok(TraceArtifact {
        header: Header::new(Kind::Trace),
        compiler_version: COMPILER_VERSION,
        app: app.to_owned(),
        recording_digest: sha256_hex(&raw),
        vocab_digest: vocabulary.digest.clone(),
        actions,
        coverage,
        visits,
        tsv,
    })
}

fn analyze(args: AnalyzeArgs, limits: Limits) -> Result<()> {
    let vocabulary = load_vocabulary(&args.vocab)?;
    let (trace, trace_digest) = match (&args.trace, &args.recording, &args.app) {
        (Some(path), _, _) => {
            let bytes = read(path)?;
            let trace = TraceArtifact::from_json(&bytes)
                .with_context(|| format!("loading trace {}", path.display()))?;
            (trace, sha256_hex(&bytes))
        }
        (None, Some(recording), Some(app)) => {
            let trace = compile(recording, &vocabulary, app, false, limits)?;
            // `compile` publishes compact JSON followed by a newline, including with --out.
            let mut bytes = serde_json::to_vec(&trace)?;
            bytes.push(b'\n');
            (trace, sha256_hex(&bytes))
        }
        _ => anyhow::bail!("either --trace, or --recording with --app, is required"),
    };
    ensure!(
        trace.vocab_digest == vocabulary.digest,
        "the trace was compiled against a different vocabulary than --vocab"
    );
    if let Some(app) = &args.app {
        ensure!(
            *app == trace.app,
            "--app {app} disagrees with the trace's app {}",
            trace.app
        );
    }
    // One visit's actions keep their refs, so its analysis cites the same refs as the trace's.
    let (actions, tsv) = match args.visit {
        Some(index) => {
            let visit = trace.visits.get(index).with_context(|| {
                format!(
                    "the trace has {} visits; --visit {index} is out of range",
                    trace.visits.len()
                )
            })?;
            let actions = trace.actions.get(visit.start..visit.end).with_context(|| {
                format!(
                    "visit {index} spans actions {}..{}, outside the trace's {} actions",
                    visit.start,
                    visit.end,
                    trace.actions.len()
                )
            })?;
            (actions, trace::to_tsv(actions))
        }
        None => (&trace.actions[..], trace.tsv.clone()),
    };
    let session_header = args
        .context
        .as_deref()
        .map(read_text)
        .transpose()?
        .unwrap_or_default();
    let custom_prompt = args
        .system_prompt
        .as_deref()
        .map(|path| {
            // The file name, not the path: provenance should not depend on where it was run.
            let name = path.file_name().unwrap_or(path.as_os_str());
            Ok::<_, anyhow::Error>((name.to_string_lossy().into_owned(), read_text(path)?))
        })
        .transpose()?;
    let instructions = match &custom_prompt {
        Some((name, text)) => Instructions { name, system: text },
        None => Instructions::default(),
    };
    let messages = analysis::build_messages(
        instructions,
        &vocabulary.vocabulary,
        &trace.app,
        &session_header,
        actions,
        &tsv,
    );
    let response_schema = analysis::response_schema();
    let provenance = AnalysisProvenance {
        visit: args.visit,
        app: trace.app.clone(),
        trace_digest,
        vocab_digest: vocabulary.digest.clone(),
        prompt: instructions.id(),
        request_digest: sha256_hex(&serde_json::to_vec(&(&messages, response_schema))?),
    };
    let out = args.output.out.as_deref();

    if args.prepare_only {
        let request = AnalysisRequest {
            header: Header::new(Kind::AnalysisRequest),
            provenance,
            messages,
            response_schema: response_schema.clone(),
        };
        return publish(&request, out);
    }
    let (summary, check, model) = match (&args.response, &args.model) {
        // Held to the same bar as a live answer: a response the retry loop would send back is
        // not an analysis.
        (Some(response), _) => {
            match analysis::assess(&read_text(response)?, actions, &vocabulary.vocabulary) {
                Assessment::Accepted { summary, check } => (summary, check, None),
                Assessment::Rejected { reason } => {
                    anyhow::bail!("model response rejected: {reason}")
                }
            }
        }
        (None, Some(model)) => {
            let client =
                openrouter::OpenRouter::new(&args.openrouter_url, model, args.network.timeout)?;
            let (summary, check, usage) =
                client.narrate(messages, actions, &vocabulary.vocabulary)?;
            (summary, check, Some(usage))
        }
        (None, None) => anyhow::bail!("--model is required unless --response or --prepare-only"),
    };
    publish(
        &AnalysisArtifact {
            header: Header::new(Kind::Analysis),
            provenance,
            summary,
            check,
            model,
        },
        out,
    )
}

fn vocab(command: VocabCommand) -> Result<()> {
    match command {
        VocabCommand::Check { vocab, output } => {
            let PinnedVocabulary { vocabulary, digest } = load_vocabulary(&vocab)?;
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
            let inputs = vocab_build::Build {
                config,
                sources,
                candidate,
                source_revision,
            };
            let client = model
                .as_deref()
                .map(|model| openrouter::OpenRouter::new(&openrouter_url, model, network.timeout))
                .transpose()?;
            let snapshot = vocab_build::build(&inputs, client.as_ref())?;
            publish(&snapshot, output.out.as_deref())
        }
    }
}

fn recordings(command: RecordingsCommand, limits: Limits) -> Result<()> {
    match command {
        RecordingsCommand::List {
            project,
            since,
            until,
            limit,
            cursor,
            host,
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
                host: &host,
                project,
                since: &since,
                until: &until,
                limit,
            };
            let page = posthog::list(&http::Http::new(network.timeout)?, &query, cursor.as_ref())?;
            publish(&page, output.out.as_deref())
        }
        RecordingsCommand::Fetch {
            project,
            session,
            max_requests,
            host,
            network,
            output,
        } => {
            let snapshot = posthog::Snapshot {
                host: &host,
                project,
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
    use std::path::Path;

    #[test]
    fn trace_digest_tracks_file_bytes_and_compiled_publication() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
        let vocab = corpus.join("vocabulary.yaml");
        let recording = corpus.join("click_changes_text.json");
        let pinned = load_vocabulary(&vocab).unwrap();
        let trace = compile(&recording, &pinned, "demo", false, Limits::default()).unwrap();
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("trace-digest-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let trace_path = directory.join("trace.json");
        let output_path = directory.join("request.json");
        let bytes = format!("  {} \n", serde_json::to_string(&trace).unwrap()).into_bytes();
        std::fs::write(&trace_path, &bytes).unwrap();

        let request = |trace_path: Option<PathBuf>, recording_path: Option<PathBuf>| {
            analyze(
                AnalyzeArgs {
                    trace: trace_path,
                    recording: recording_path.clone(),
                    app: recording_path.map(|_| "demo".into()),
                    vocab: vocab.clone(),
                    context: None,
                    system_prompt: None,
                    prepare_only: true,
                    visit: None,
                    response: None,
                    model: None,
                    openrouter_url: DEFAULT_OPENROUTER_URL.into(),
                    network: Network { timeout: 120 },
                    output: Output {
                        out: Some(output_path.clone()),
                    },
                },
                Limits::default(),
            )
            .unwrap();
            let content = std::fs::read(&output_path).unwrap();
            let artifact: serde_json::Value = serde_json::from_slice(&content).unwrap();
            artifact["trace_digest"].as_str().unwrap().to_owned()
        };
        assert_eq!(
            request(Some(trace_path), None),
            sha256_hex(&bytes),
            "a stored trace is identified by its exact file bytes"
        );
        let mut published = serde_json::to_vec(&trace).unwrap();
        published.push(b'\n');
        assert_eq!(
            request(None, Some(recording)),
            sha256_hex(&published),
            "an in-memory compile hashes the bytes compile would publish"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}

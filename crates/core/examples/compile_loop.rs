//! Compile one recording repeatedly in one process, for profilers:
//! `cargo run --release -p spoiler-core --example compile_loop -- corpus/click_changes_text.json corpus/vocabulary.yaml demo [ITERATIONS]`.

use spoiler_core::{recording, trace, vocab};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, recording_path, vocab_path, app, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: compile_loop RECORDING VOCAB APP [ITERATIONS]");
    };
    let iterations: usize = rest.first().map_or(Ok(20), |n| n.parse())?;
    let raw = std::fs::read(recording_path)?;
    let vocabulary = vocab::Vocabulary::parse(&std::fs::read(vocab_path)?)?;
    let matcher = vocab::Matcher::new(&vocabulary);
    let (mut decode_ms, mut compile_ms) = (0.0, 0.0);
    for _ in 0..iterations {
        let started = Instant::now();
        let recording = recording::decode(&raw)?;
        let decoded = started.elapsed();
        let actions = trace::compile(&recording, &matcher, app)?;
        decode_ms += decoded.as_secs_f64() * 1000.0;
        compile_ms += (started.elapsed() - decoded).as_secs_f64() * 1000.0;
        std::hint::black_box(actions);
    }
    let n = iterations as f64;
    println!(
        "mean decode {:.1} ms, compile {:.1} ms",
        decode_ms / n,
        compile_ms / n
    );
    Ok(())
}

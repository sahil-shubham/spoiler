# Contributing

Use the pinned Rust toolchain and run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before submitting a pull request.

Compiler behavior is captured by the synthetic corpus. Edit a case and its description in `scripts/corpus.py`, then run `python3 scripts/corpus.py` and `cargo test --test corpus`. For an intentional output change, run `SPOILER_BLESS=1 cargo test --test corpus`; review each golden diff alongside the code. A change to compilation rules must also bump `COMPILER_VERSION` so stored traces can identify their compiler.

Never include a real recording, credentials, or customer data in tests, issues, or pull requests. Synthetic fixtures are sufficient for reproductions.

By submitting a contribution, you agree to license it under MIT OR Apache-2.0, at your choice.

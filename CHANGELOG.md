# Changelog

All notable changes to this project will be documented in this file. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - unreleased

### Added

- Decode PostHog and rrweb session recordings, including compressed snapshot data, into bounded, versioned artifacts.
- Reconstruct visible DOM changes per tab and compile actions, effects, visits, coverage, and friction signals into a deterministic trace.
- Discover and fetch recordings from PostHog; build and check pinned product vocabularies.
- Prepare narration requests, call OpenRouter, or validate model responses offline with trace-grounded evidence and provenance.
- Synthetic behavior corpus with golden traces for compiler rules.
- `spoiler run`: fetch or read, compile, and narrate every visit with gestures in one step, as one `session` artifact.
- `-` reads any input path from standard input.
- Recording discovery pages continue with an opaque `--cursor TOKEN` that carries its window.
- Releases on PyPI (`pip install spoiler`, wheels that install the binary), crates.io (`cargo install spoiler`), and GitHub Releases (binary archives for Linux glibc and static musl, and macOS, on x86_64 and arm64).

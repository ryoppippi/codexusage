# Project Purpose

This repository contains a standalone Rust CLI for analyzing OpenAI Codex session logs. The design goal is fast scans over large JSONL trees with predictable output, explicit pricing-cache behavior, and enough automation that the local workflow mirrors CI expectations.

# Engineering Rules

- Keep the scanner single-pass and streaming. Avoid whole-file reads or event materialization unless a test proves the tradeoff is necessary.
- Preserve the 24h pricing-cache contract: default to live refresh when stale, `--refresh-pricing` forces refresh, `--offline` never attempts network.
- Enforce crate-level `clippy::pedantic`, `clippy::unwrap_used`, `missing_docs`, and `rustdoc::missing_crate_level_docs`.
- Keep `xtask` and `Justfile` as the only supported quality-entry points for format, clippy, test, bench, doc, and coverage.
- Maintain at least 90% line coverage through `cargo llvm-cov` in `xtask cov`.
- Add or update benchmarks whenever scanner, grouping, or pricing-resolution hot paths change materially.
- Characterize behavior with fixture-based tests before changing parsing, fallback-model, or pricing semantics.
- Update this file when project-specific rules change.

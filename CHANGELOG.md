# Changelog

All notable changes to this project are documented in this file.

The format follows Keep a Changelog principles and this project follows SemVer.

## [Unreleased]

### Added

- Open-source governance files (`CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `SUPPORT.md`).
- Release process document (`RELEASING.md`) and GitHub templates (`ISSUE_TEMPLATE`, PR template, CODEOWNERS).
- Baseline API rustdoc for `firq-core`, `firq-async`, and `firq-tower`.
- Toolchain pin file (`rust-toolchain.toml`) for reproducible local and CI behavior.

### Changed

- README rewritten as a public usage manual for core, async, Axum, and Actix-web integrations.
- CI extended with examples compilation and staged crates.io dry-run checks.

### Removed

- Legacy planning docs that are no longer part of public release assets.

## [0.1.0] - 2026-02-09

### Added

- `firq-core`: DRR scheduling, backpressure policies, deadlines, cancellation handles, shutdown modes, and metrics.
- `firq-async`: Tokio async adapter with receiver/stream/dispatcher abstractions.
- `firq-tower`: Tower layer integration with in-flight limiting and rejection mapping.
- `firq-examples`: runnable examples for sync, async, Axum, and Actix-web usage.
- `firq-bench`: benchmark scenarios for fairness and tail-latency behavior.

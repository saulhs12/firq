# Changelog

All notable changes to this project are documented in this file.

The format follows Keep a Changelog principles and this project follows SemVer.

## [Unreleased]

No unreleased changes yet.

## [0.1.3] - 2026-02-10

### Added

- `firq-core`: deterministic stress tests for fairness under hot/cold tenant contention, deadline expiration pressure, and capacity saturation recovery.
- `firq-tower`: end-to-end integration test covering concurrent multi-tenant requests, client cancellations, and permit/deadlock safety checks.
- `firq-core`: explicit `metrics` feature flag (enabled by default) gating `firq_core::prometheus`.

### Changed

- `firq-bench`: reproducible environment overrides for quick scenario-specific runs (`FIRQ_BENCH_SCENARIO`, `FIRQ_BENCH_SECONDS`).
- Rustdoc examples refined for docs.rs onboarding (`firq-async` worker-backed recommended flow and `firq-tower` header-based tenant extraction).
- README expanded with scheduler guarantees/non-guarantees, SemVer/MSRV guidance, and benchmark run expectations.
- Publishable crate versions and internal dependency links aligned to `0.1.3`.

## [0.1.2] - 2026-02-10

### Changed

- Metadata-only release that aligned crate/docs version references from `0.1.1` to `0.1.2` (no scheduler runtime behavior changes).

## [0.1.1] - 2026-02-10

### Added

- `firq-core`: enqueue-side stale-entry compaction for cancelled/expired backlog and `dequeue_blocking_timeout`.
- `firq-async`: dedicated worker-backed dequeue path (`receiver_with_worker`, `stream_with_worker`, `AsyncWorkerReceiver`).
- `firq-tower`: integration tests for start-order under contention and cancel-before-turn behavior.
- `firq-examples`: worker-backed async example (`crates/firq-examples/src/bin/async_worker.rs`).

### Changed

- `firq-tower`: worker now acquires in-flight capacity (`OwnedSemaphorePermit`) before releasing scheduler turn.
- README and examples updated with parameter tuning guidance and tenant extraction patterns.
- CI/release workflows hardened to avoid silent no-op publish and to verify crates.io state before GitHub release creation.

## [0.1.0] - 2026-02-09

### Added

- `firq-core`: DRR scheduling, backpressure policies, deadlines, cancellation handles, shutdown modes, and metrics.
- `firq-async`: Tokio async adapter with receiver/stream/dispatcher abstractions.
- `firq-tower`: Tower layer integration with in-flight limiting and rejection mapping.
- `firq-examples`: runnable examples for sync, async, Axum, and Actix-web usage.
- `firq-bench`: benchmark scenarios for fairness and tail-latency behavior.

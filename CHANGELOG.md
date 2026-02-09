# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.1.0] - 2026-02-09

### Added
- `enqueue_with_handle` + `TaskHandle` + `cancel` in `firq-core`.
- Shutdown modes: `close_immediate`, `close_drain`, `close_with_mode`.
- `EnqueueRejectReason::Timeout`.
- `SchedulerStats` counters by cause:
  - `rejected_global`
  - `rejected_tenant`
  - `timeout_rejected`
  - `dropped_policy`
- Saturation fields in stats:
  - `max_global`
  - `queue_saturation_ratio`
- Extended Prometheus exporter with new counters and saturation gauges.
- `firq-async` API wrappers for handle/cancel/close-mode operations.
- Async tests for scheduler/receiver/stream/dispatcher, including panic resilience.
- `firq-tower` production semantics:
  - `with_in_flight_limit`
  - `with_deadline_extractor`
  - `with_rejection_mapper`
  - default stable rejection schema (`status`, `code`, `message`, `reason`).
- New benchmark suite with reproducible scenarios and p50/p95/p99 reporting.

### Changed
- Global capacity control now uses strict admission reservation semantics.
- Shard activation/deactivation state transitions were hardened to avoid temporary invisibility.
- CI test gate switched to per-crate targeted tests (`core`, `async`, `tower integration`).

### Documentation
- Updated Axum/Tower integration contract (`docs/axum_integration.md`).
- Updated metrics semantics and dashboard examples (`docs/metrics.md`).
- Added migration guide (`docs/migration_v1.md`).
- Added benchmark methodology report skeleton (`docs/benchmarks_v1.md`).

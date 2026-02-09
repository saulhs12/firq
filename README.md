# Firq

Firq is an in-process Rust scheduler for multi-tenant fairness with explicit backpressure, deadlines, and operational metrics.

## Workspace crates

- `firq-core`: runtime-agnostic scheduler (DRR, deadlines, backpressure, metrics).
- `firq-async`: Tokio adapter (`dequeue_async`, receiver/stream, dispatcher).
- `firq-tower`: Tower/Axum middleware integration.
- `firq-examples`: runnable examples.
- `firq-bench`: reproducible benchmark scenarios (Firq vs FIFO).

## Core capabilities

- Deficit Round Robin fairness per tenant.
- Strict global admission (`queue_len_estimate <= max_global` under concurrency).
- Tenant/global backpressure plus timeout/drop policies.
- Deadlines (`DropExpired` in dequeue).
- Pending-task cancellation (`enqueue_with_handle` + `cancel`).
- Shutdown modes: `close_immediate` and `close_drain`.
- Prometheus exporter and queue-time histogram percentiles.

## Quick start

```bash
cargo build --workspace
cargo test -p firq-core
cargo test -p firq-async
cargo test -p firq-tower --test integration
```

## Minimal core usage

```rust
use firq_core::{Scheduler, SchedulerConfig, TenantKey, Task, Priority, EnqueueResult};
use std::time::Instant;

let scheduler = Scheduler::new(SchedulerConfig::default());
let tenant = TenantKey::from(42);
let task = Task {
    payload: "work",
    enqueue_ts: Instant::now(),
    deadline: None,
    priority: Priority::Normal,
    cost: 1,
};

match scheduler.enqueue(tenant, task) {
    EnqueueResult::Enqueued => {}
    EnqueueResult::Rejected(reason) => eprintln!("rejected: {reason:?}"),
    EnqueueResult::Closed => eprintln!("scheduler closed"),
}
```

## Axum/Tower integration

See:

- `docs/axum_integration.md`
- `crates/firq-examples/src/bin/axum_api.rs`

## Metrics and dashboards

See:

- `docs/metrics.md`
- `crates/firq-core/src/prometheus.rs`

## Migration

See `docs/migration_v1.md` for v0.x -> v1 API updates.

# Firq — Multi-tenant Scheduler for Rust Services

[![Crates.io (firq-core)](https://img.shields.io/crates/v/firq-core.svg)](https://crates.io/crates/firq-core)
[![Crates.io (firq-async)](https://img.shields.io/crates/v/firq-async.svg)](https://crates.io/crates/firq-async)
[![Crates.io (firq-tower)](https://img.shields.io/crates/v/firq-tower.svg)](https://crates.io/crates/firq-tower)
[![Docs.rs (firq-core)](https://docs.rs/firq-core/badge.svg)](https://docs.rs/firq-core)
[![Docs.rs (firq-async)](https://docs.rs/firq-async/badge.svg)](https://docs.rs/firq-async)
[![Docs.rs (firq-tower)](https://docs.rs/firq-tower/badge.svg)](https://docs.rs/firq-tower)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Firq** is an in-process scheduler for Rust backends that need stable tail latency under contention.

Key capabilities:

- Fair scheduling across tenants (DRR).
- Explicit backpressure and bounded memory behavior.
- Deadline-aware dequeue (`DropExpired` semantics).
- Metrics for queue saturation, drops, rejections, and queue-time percentiles.

`firq-core` is runtime-agnostic. `firq-async` adds Tokio support. `firq-tower` integrates with Tower/Axum.

## Install

### From crates.io

```toml
[dependencies]
firq-core = "0.1.1"
```

Tokio integration:

```toml
[dependencies]
firq-core = "0.1.1"
firq-async = "0.1.1"
```

Tower/Axum integration:

```toml
[dependencies]
firq-core = "0.1.1"
firq-async = "0.1.1"
firq-tower = "0.1.1"
```

### From source

```bash
git clone https://github.com/saulhs12/firq.git
cd firq
cargo build --workspace
```

## Quick start

```bash
cargo test -p firq-core
cargo test -p firq-async
cargo test -p firq-tower --test integration
cargo check -p firq-examples --bins
cargo run -p firq-examples --bin async
cargo run -p firq-examples --bin async_worker
```

## Stability / SemVer

- Firq is currently in `0.x`; minor releases may include API-breaking changes.
- Breaking changes include removing/renaming public types/functions, changing enum variants, or changing behavior in a way that requires code changes.
- Patch releases (`0.1.z`) aim to be backward compatible and focus on fixes/hardening.
- For production, pin a concrete release (`=0.1.1`) or a conservative range (`~0.1.1`) and review `CHANGELOG.md` before upgrades.
- MSRV: Rust `1.85+` (`rust-version = "1.85"` in `firq-core`, `firq-async`, and `firq-tower`).

## Getting started on docs.rs

- `firq-core`: https://docs.rs/firq-core and `cargo add firq-core@0.1.1`
- `firq-async`: https://docs.rs/firq-async and `cargo add firq-async@0.1.1`
- `firq-tower`: https://docs.rs/firq-tower and `cargo add firq-tower@0.1.1`
- Minimal, copyable examples are included in each crate-level `lib.rs` docs.

## How to choose parameters

Use these as starting points, then tune with real traffic and `stats()` metrics.

- `shards`:
  Start with `min(physical_cores, 8)`. Increase when many tenants are hot concurrently and enqueue lock contention is visible.
- `max_global` and `max_per_tenant`:
  Size from memory budget first. Approximate memory as `max_global * avg_task_size`. Keep `max_per_tenant` low enough that one tenant cannot monopolize memory.
- `quantum` and `cost`:
  Treat `cost` as "work units" per task, and `quantum` as work units granted per round. If heavy jobs are starving, increase `quantum` or lower heavy-job `cost` calibration.
- `deadlines`:
  Use when stale work should be discarded (timeouts/SLO breaches). Expired items are removed lazily and should not permanently consume queue limits.
- backpressure policy:
  `Reject` for strict admission control, `DropOldestPerTenant`/`DropNewestPerTenant` for lossy workloads, `Timeout` when producers can wait briefly for capacity.

Queue limits are enforced against live pending work. Cancelled/expired entries are compacted lazily on dequeue and enqueue maintenance passes.

## Core usage (`firq-core`)

Use this when workers are synchronous (threads) or when direct control over dequeue loops is required.

```rust
use firq_core::{
    BackpressurePolicy, DequeueResult, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task,
    TenantKey,
};
use std::collections::HashMap;
use std::time::Instant;

let scheduler = Scheduler::new(SchedulerConfig {
    shards: 4,
    max_global: 10_000,
    max_per_tenant: 1_000,
    quantum: 5,
    quantum_by_tenant: HashMap::new(),
    quantum_provider: None,
    backpressure: BackpressurePolicy::Reject,
    backpressure_by_tenant: HashMap::new(),
    top_tenants_capacity: 64,
});

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
    EnqueueResult::Rejected(reason) => {
        eprintln!("rejected: {reason:?}");
    }
    EnqueueResult::Closed => {
        eprintln!("scheduler closed");
    }
}

match scheduler.dequeue_blocking() {
    DequeueResult::Task { tenant, task } => {
        println!("tenant={} payload={}", tenant.as_u64(), task.payload);
    }
    DequeueResult::Empty => {}
    DequeueResult::Closed => {}
}

let stats = scheduler.stats();
println!("enqueued={} dequeued={}", stats.enqueued, stats.dequeued);
```

## Async usage (`firq-async`, Tokio)

Use this when producers/consumers are async and run under Tokio.

```rust
use firq_async::{
    AsyncScheduler, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task, TenantKey,
};
use std::sync::Arc;
use std::time::Instant;

let core = Arc::new(Scheduler::new(SchedulerConfig::default()));
let scheduler = AsyncScheduler::new(core);

let tenant = TenantKey::from(7);
let task = Task {
    payload: "job",
    enqueue_ts: Instant::now(),
    deadline: None,
    priority: Priority::Normal,
    cost: 1,
};

match scheduler.enqueue(tenant, task) {
    EnqueueResult::Enqueued => {}
    EnqueueResult::Rejected(reason) => panic!("rejected: {reason:?}"),
    EnqueueResult::Closed => panic!("closed"),
}

// Recommended for steady consumers: dedicated worker mode.
let mut receiver = scheduler.receiver_with_worker(1024);
while let Some(item) = receiver.recv().await {
    println!(
        "tenant={} payload={}",
        item.tenant.as_u64(),
        item.task.payload
    );
}

// Fallback for one-off dequeue calls:
let _ = scheduler.dequeue_async().await;
```

## Axum usage (`firq-tower`)

`firq-tower` provides a Tower layer that handles scheduling, cancellation before turn, in-flight gating, and rejection mapping.

```rust
use axum::{extract::Request, routing::get, Router};
use firq_tower::{Firq, TenantKey};

let firq_layer = Firq::new()
    .with_shards(4)
    .with_max_global(1000)
    .with_max_per_tenant(100)
    .with_quantum(10)
    .with_in_flight_limit(128)
    .with_deadline_extractor::<Request, _>(|req| {
        req.headers()
            .get("X-Deadline-Ms")
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms))
    })
    .build(|req: &Request| {
        req.headers()
            .get("X-Tenant-ID")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| {
                req.headers()
                    .get("Authorization")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|raw| raw.strip_prefix("Bearer "))
                    .and_then(|token| token.strip_prefix("tenant:"))
                    .and_then(|claim| claim.parse::<u64>().ok())
            })
            .map(TenantKey::from)
            .unwrap_or(TenantKey::from(0))
    });

let app = Router::new()
    .route("/", get(|| async { "ok" }))
    .layer(firq_layer);
```

Default rejection mapping:

- `TenantFull` -> `429`
- `GlobalFull` -> `503`
- `Timeout` -> `503`

## Actix-web usage (manual scheduling gate)

Firq does not currently provide a first-party `firq-actix` crate.

Use `firq-async` in handlers or middleware to gate work before executing heavy logic:

```rust
use actix_web::{web, HttpRequest, HttpResponse};
use firq_async::{AsyncScheduler, DequeueResult, EnqueueResult, Priority, Task, TenantKey};
use std::time::Instant;

struct Permit;

#[derive(Clone)]
struct AppState {
    scheduler: AsyncScheduler<Permit>,
}

async fn guarded_handler(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<HttpResponse> {
    let tenant = req
        .headers()
        .get("X-Tenant-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(TenantKey::from)
        .unwrap_or(TenantKey::from(0));

    let task = Task {
        payload: Permit,
        enqueue_ts: Instant::now(),
        deadline: None,
        priority: Priority::Normal,
        cost: 1,
    };

    match state.scheduler.enqueue(tenant, task) {
        EnqueueResult::Enqueued => {}
        EnqueueResult::Rejected(_) => return Ok(HttpResponse::TooManyRequests().finish()),
        EnqueueResult::Closed => return Ok(HttpResponse::ServiceUnavailable().finish()),
    }

    match state.scheduler.dequeue_async().await {
        DequeueResult::Task { .. } => Ok(HttpResponse::Ok().body("ok")),
        DequeueResult::Closed => Ok(HttpResponse::ServiceUnavailable().finish()),
        DequeueResult::Empty => Ok(HttpResponse::InternalServerError().finish()),
    }
}
```

Runnable Actix example in this repository:

- `crates/firq-examples/src/bin/actix_web.rs`

## Benchmarks

Run reproducible scenarios:

```bash
cargo run --release -p firq-bench
```

Quick smoke run (same binary, full scenario set):

```bash
cargo build --release -p firq-bench
./target/release/firq-bench
```

Scenarios in the benchmark binary include:

- `hot_tenant_sustained`
- `burst_massive`
- `mixed_priorities`
- `deadline_expiration`
- `capacity_pressure`

What to observe (without relying on fixed numbers):

- Fairness: in `hot_tenant_sustained`, cold tenants should still be served.
- Queue-time behavior: compare p95/p99 queue-time trends between schedulers.
- Backpressure/expiry signals: in `capacity_pressure` and `deadline_expiration`, verify drop/reject/expired counters respond as load changes.

Use runs to compare parameter sets and regressions; do not treat a single run as a universal baseline.

## Repository layout

- `crates/firq-core`: scheduler engine.
- `crates/firq-async`: Tokio adapter.
- `crates/firq-tower`: Tower layer.
- `crates/firq-examples`: runnable examples (`publish = false`).
- `crates/firq-bench`: benchmark runner (`publish = false`).

## Community and governance

- Contribution guide: `CONTRIBUTING.md`
- Security policy: `SECURITY.md`
- Support channels: `SUPPORT.md`
- Release process: `RELEASING.md`

## Local quality gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p firq-core
cargo test -p firq-async
cargo test -p firq-tower --test integration
cargo check -p firq-examples --bins
```

Release dry-runs:

```bash
cargo publish --dry-run -p firq-core
# after firq-core is published on crates.io:
cargo publish --dry-run -p firq-async
# after firq-async is published on crates.io:
cargo publish --dry-run -p firq-tower
```


## License

This project is dual-licensed under:

- MIT (`LICENSE-MIT`)
- Apache-2.0 (`LICENSE-APACHE`)

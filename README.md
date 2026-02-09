# Firq — Multi-tenant Scheduler for Rust Services

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
firq-core = "0.1"
```

Tokio integration:

```toml
[dependencies]
firq-core = "0.1"
firq-async = "0.1"
```

Tower/Axum integration:

```toml
[dependencies]
firq-core = "0.1"
firq-async = "0.1"
firq-tower = "0.1"
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
cargo run -p firq-examples --bin async
```

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
    AsyncScheduler, DequeueResult, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task,
    TenantKey,
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

match scheduler.dequeue_async().await {
    DequeueResult::Task { task, .. } => {
        println!("payload={}", task.payload);
    }
    DequeueResult::Empty => {}
    DequeueResult::Closed => {}
}
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

## Benchmarks

Run reproducible scenarios:

```bash
cargo run --release -p firq-bench
```

Scenarios:

- `hot_tenant_sustained`
- `burst_massive`
- `mixed_priorities`
- `deadline_expiration`
- `capacity_pressure`

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
```


## License

This project is dual-licensed under:

- MIT (`LICENSE-MIT`)
- Apache-2.0 (`LICENSE-APACHE`)

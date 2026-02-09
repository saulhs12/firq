//! Core scheduling primitives for Firq.
//!
//! `firq-core` is runtime-agnostic and contains the scheduling engine used by
//! the async and web integration crates.
//!
//! It provides:
//! - Deficit Round Robin (DRR) fairness by tenant
//! - Global/per-tenant backpressure policies
//! - Deadline-aware dequeue semantics
//! - Queue-time metrics and saturation signals
//! - Pending task cancellation and shutdown modes
//!
//! # Basic usage
//!
//! ```rust
//! use firq_core::{EnqueueResult, Priority, Scheduler, SchedulerConfig, Task, TenantKey};
//! use std::time::Instant;
//!
//! let scheduler = Scheduler::new(SchedulerConfig::default());
//! let tenant = TenantKey::from(1);
//! let task = Task {
//!     payload: "work",
//!     enqueue_ts: Instant::now(),
//!     deadline: None,
//!     priority: Priority::Normal,
//!     cost: 1,
//! };
//!
//! match scheduler.enqueue(tenant, task) {
//!     EnqueueResult::Enqueued => {}
//!     EnqueueResult::Rejected(_) => {}
//!     EnqueueResult::Closed => {}
//! }
//! ```

mod api;
pub mod prometheus;
mod scheduler;
mod state;

pub use api::{
    BackpressurePolicy, CancelResult, CloseMode, DequeueResult, EnqueueRejectReason, EnqueueResult,
    EnqueueWithHandleResult, Priority, QueueTimeBucket, SchedulerConfig, SchedulerStats, Task,
    TaskHandle, TenantCount, TenantKey,
};
pub use scheduler::Scheduler;

#[cfg(test)]
mod tests;

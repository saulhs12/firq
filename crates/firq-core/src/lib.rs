//! Firq Core (MVP v0.1) — decisiones cerradas de arquitectura
//!
//! Objetivo: scheduler multi-tenant in-process para backends con alta concurrencia.
//! En v0.1 el alcance está deliberadamente acotado para tener un núcleo sólido y medible.
//!
//! Decisiones MVP (no negociables):
//! - Algoritmo de fairness: DRR (Deficit Round Robin) por tenant.
//! - Backpressure: solo Reject (si se exceden límites, se rechaza el enqueue).
//! - Deadlines: DropExpired al momento de dequeue (nunca se entrega una tarea expirada).
//! - Métricas mínimas: enqueued, dequeued, dropped, expired, queue_len_estimate, queue_time.
//!
//! Notas:
//! - El core será runtime-agnostic (sin Tokio). Un adaptador async vivirá en `firq-async`.
//! - La implementación posterior añadirá sharding, ring de tenants activos y señalización de “hay trabajo”.

mod api;
pub mod prometheus;
mod scheduler;
mod state;

pub use api::{
    BackpressurePolicy, DequeueResult, EnqueueRejectReason, EnqueueResult, Priority,
    QueueTimeBucket, SchedulerConfig, SchedulerStats, Task, TenantCount, TenantKey,
};
pub use scheduler::Scheduler;

#[cfg(test)]
mod tests;

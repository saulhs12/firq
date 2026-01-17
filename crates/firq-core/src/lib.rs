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

use std::marker::PhantomData;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TenantKey(String);

impl From<&str> for TenantKey {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for TenantKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Unidad de trabajo encolable.
///
/// - `enqueue_ts` se usa para medir queue_time al momento de entregar (now - enqueue_ts).
/// - `deadline` si existe, y ya pasó al intentar despachar, la tarea se descarta (DropExpired).
/// - `cost` se usa por DRR: tareas con mayor costo consumen más presupuesto.
#[derive(Clone, Debug)]
pub struct Task<T> {
    pub payload: T,
    pub enqueue_ts: Instant,
    pub deadline: Option<Instant>,
    pub cost: u64,
}

/// Configuración del scheduler.
///
/// MVP v0.1:
/// - `backpressure` solo soporta `Reject`.
/// - `quantum` define el presupuesto base por ronda DRR para todos los tenants.
/// - `max_global` y `max_per_tenant` imponen límites de memoria (backpressure).
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Número de shards internos para reducir contención (v0.1: estructura preparada).
    pub shards: usize,
    /// Capacidad máxima global de tareas encoladas.
    pub max_global: usize,
    /// Capacidad máxima por tenant.
    pub max_per_tenant: usize,
    /// Quantum base de DRR (presupuesto por ronda).
    pub quantum: u64,
    /// Política de backpressure (v0.1: solo Reject).
    pub backpressure: BackpressurePolicy,
}

#[derive(Clone, Debug)]
pub enum BackpressurePolicy {
    /// Rechaza el enqueue cuando se excede `max_global` o `max_per_tenant`.
    Reject,
}

#[derive(Clone, Debug)]
pub enum EnqueueResult {
    Enqueued,
    Rejected(EnqueueError),
    Closed,
}

#[derive(Clone, Debug)]
pub enum EnqueueError {
    Backpressure,
    Full,
    Invalid,
}

#[derive(Clone, Debug)]
pub enum DequeueResult<T> {
    /// Se entrega una tarea no expirada, seleccionada con fairness DRR por tenant.
    Task { tenant: TenantKey, task: Task<T> },
    /// No hay trabajo disponible.
    Empty,
    /// El scheduler fue cerrado y no entregará más trabajo.
    Closed,
}

#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    /// Total de tareas aceptadas en cola.
    pub total_enqueued: u64,
    /// Total de tareas entregadas a consumidores.
    pub total_dequeued: u64,
    /// Total de tareas descartadas por expiración (DropExpired en dequeue).
    pub total_expired: u64,
    /// Total de tareas rechazadas por backpressure (Reject) u otras causas de drop (futuras).
    pub total_dropped: u64,
    /// Estimación del tamaño total de cola (tareas pendientes). En v0.1 será aproximada.
    pub queue_len_estimate: u64,
    /// Suma acumulada del queue_time (nanosegundos) de tareas entregadas.
    /// Se usa para métricas básicas; histogramas/p99 vendrán después.
    pub queue_time_sum_ns: u64,
    /// Número de muestras acumuladas para `queue_time_sum_ns`.
    pub queue_time_samples: u64,
}

pub struct Scheduler<T> {
    config: SchedulerConfig,
    _marker: PhantomData<T>,
}

impl<T> Scheduler<T> {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }

    pub fn enqueue(&self, _tenant: TenantKey, _task: Task<T>) -> EnqueueResult {
        let _ = &self.config;
        unimplemented!("enqueue is not implemented yet");
    }

    pub fn try_dequeue(&self) -> DequeueResult<T> {
        let _ = &self.config;
        unimplemented!("try_dequeue is not implemented yet");
    }

    pub fn dequeue_blocking(&self) -> DequeueResult<T> {
        let _ = &self.config;
        unimplemented!("dequeue_blocking is not implemented yet");
    }

    pub fn stats(&self) -> SchedulerStats {
        let _ = &self.config;
        unimplemented!("stats is not implemented yet");
    }

    pub fn close(&self) {
        let _ = &self.config;
        unimplemented!("close is not implemented yet");
    }
}

use std::time::Instant;

/// Clave de fairness (hash estable) usada para shard y DRR.
///
/// Nota: se espera que el caller provea un hash estable (por ejemplo, hash de tenant_id).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TenantKey(u64);

impl From<u64> for TenantKey {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl TenantKey {
    pub(crate) fn as_u64(&self) -> u64 {
        self.0
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
    /// Costo de la tarea para DRR. Debe ser >= 1.
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
    Rejected(EnqueueRejectReason),
    Closed,
}

#[derive(Clone, Debug)]
pub enum EnqueueRejectReason {
    /// Capacidad global excedida.
    GlobalFull,
    /// Capacidad por tenant excedida.
    TenantFull,
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
    pub enqueued: u64,
    /// Total de tareas entregadas a consumidores.
    pub dequeued: u64,
    /// Total de tareas descartadas por expiración (DropExpired en dequeue).
    pub expired: u64,
    /// Total de tareas rechazadas por backpressure (Reject).
    pub dropped: u64,
    /// Estimación del tamaño total de cola (tareas pendientes). En v0.1 será aproximada.
    pub queue_len_estimate: u64,
    /// Suma acumulada del queue_time (nanosegundos) de tareas entregadas.
    /// Se usa para métricas básicas; histogramas/p99 vendrán después.
    pub queue_time_sum_ns: u64,
    /// Número de muestras acumuladas para `queue_time_sum_ns`.
    pub queue_time_samples: u64,
}

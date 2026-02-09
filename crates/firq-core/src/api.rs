use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    pub fn index(self) -> usize {
        match self {
            Priority::High => 0,
            Priority::Normal => 1,
            Priority::Low => 2,
        }
    }

    pub fn ordered() -> [Priority; 3] {
        [Priority::High, Priority::Normal, Priority::Low]
    }
}

/// Unidad de trabajo encolable.
///
/// - `enqueue_ts` se usa para medir queue_time al momento de entregar (now - enqueue_ts).
/// - `deadline` si existe, y ya pasó al intentar despachar, la tarea se descarta (DropExpired).
/// - `priority` controla el orden de despacho (High/Normal/Low).
/// - `cost` se usa por DRR: tareas con mayor costo consumen más presupuesto.
#[derive(Clone, Debug)]
pub struct Task<T> {
    pub payload: T,
    pub enqueue_ts: Instant,
    pub deadline: Option<Instant>,
    pub priority: Priority,
    /// Costo de la tarea para DRR. Debe ser >= 1.
    pub cost: u64,
}

pub type QuantumProvider = Arc<dyn Fn(TenantKey) -> u64 + Send + Sync>;

/// Configuración del scheduler.
///
/// MVP v0.1:
/// - `backpressure` solo soporta `Reject`.
/// - `quantum` define el presupuesto base por ronda DRR para todos los tenants.
/// - `max_global` y `max_per_tenant` imponen límites de memoria (backpressure).
#[derive(Clone)]
pub struct SchedulerConfig {
    /// Número de shards internos para reducir contención (v0.1: estructura preparada).
    pub shards: usize,
    /// Capacidad máxima global de tareas encoladas.
    pub max_global: usize,
    /// Capacidad máxima por tenant.
    pub max_per_tenant: usize,
    /// Quantum base de DRR (presupuesto por ronda).
    pub quantum: u64,
    /// Quantum por tenant (override estático).
    pub quantum_by_tenant: HashMap<TenantKey, u64>,
    /// Selector dinámico de quantum por tenant.
    pub quantum_provider: Option<QuantumProvider>,
    /// Política de backpressure (v0.1: solo Reject).
    pub backpressure: BackpressurePolicy,
    /// Overrides por tenant para políticas de backpressure.
    pub backpressure_by_tenant: HashMap<TenantKey, BackpressurePolicy>,
    /// Cantidad máxima de tenants a retener en métricas de top talkers.
    pub top_tenants_capacity: usize,
}

impl fmt::Debug for SchedulerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerConfig")
            .field("shards", &self.shards)
            .field("max_global", &self.max_global)
            .field("max_per_tenant", &self.max_per_tenant)
            .field("quantum", &self.quantum)
            .field("quantum_by_tenant", &self.quantum_by_tenant)
            .field(
                "quantum_provider",
                &self.quantum_provider.as_ref().map(|_| "<fn>"),
            )
            .field("backpressure", &self.backpressure)
            .field("backpressure_by_tenant", &self.backpressure_by_tenant)
            .field("top_tenants_capacity", &self.top_tenants_capacity)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum BackpressurePolicy {
    /// Rechaza el enqueue cuando se excede `max_global` o `max_per_tenant`.
    Reject,
    /// Descarta el task más antiguo del tenant para hacer lugar al nuevo.
    DropOldestPerTenant,
    /// Descarta el task más nuevo del tenant para hacer lugar al nuevo.
    DropNewestPerTenant,
    /// Espera hasta que haya capacidad o expire el timeout.
    Timeout { wait: Duration },
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
    /// Estimaciones de percentiles para queue_time (nanosegundos).
    pub queue_time_p95_ns: u64,
    pub queue_time_p99_ns: u64,
    /// Histograma básico de queue_time.
    pub queue_time_histogram: Vec<QueueTimeBucket>,
    /// Top talkers aproximados por tenant.
    pub top_tenants: Vec<TenantCount>,
}

#[derive(Clone, Debug, Default)]
pub struct QueueTimeBucket {
    pub le_ns: u64,
    pub count: u64,
}

#[derive(Clone, Debug)]
pub struct TenantCount {
    pub tenant: TenantKey,
    pub count: u64,
}

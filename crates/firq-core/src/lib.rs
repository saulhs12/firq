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

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
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
    fn as_u64(&self) -> u64 {
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

#[derive(Debug)]
struct TenantState<T> {
    queue: VecDeque<Task<T>>,
    deficit: i64,
    quantum: i64,
    active: bool,
}

impl<T> TenantState<T> {
    fn new(quantum: i64) -> Self {
        Self {
            queue: VecDeque::new(),
            deficit: 0,
            quantum,
            active: false,
        }
    }
}

#[derive(Debug)]
struct Shard<T> {
    tenants: HashMap<TenantKey, TenantState<T>>,
    active_ring: VecDeque<TenantKey>,
}

impl<T> Shard<T> {
    fn new() -> Self {
        Self {
            tenants: HashMap::new(),
            active_ring: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
struct StatsCounters {
    enqueued: AtomicU64,
    dequeued: AtomicU64,
    expired: AtomicU64,
    dropped: AtomicU64,
    queue_len_estimate: AtomicU64,
    queue_time_sum_ns: AtomicU64,
    queue_time_samples: AtomicU64,
}

impl StatsCounters {
    fn new() -> Self {
        Self {
            enqueued: AtomicU64::new(0),
            dequeued: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            queue_len_estimate: AtomicU64::new(0),
            queue_time_sum_ns: AtomicU64::new(0),
            queue_time_samples: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
struct WorkSignal {
    mutex: Mutex<()>,
    condvar: Condvar,
    seq: AtomicU64,
}

impl WorkSignal {
    fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
            seq: AtomicU64::new(0),
        }
    }

    fn current(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        self.condvar.notify_all();
    }

    fn wait_for_change(&self, last_seen: u64) {
        let mut guard = self.mutex.lock().expect("work signal mutex poisoned");
        while self.seq.load(Ordering::Acquire) == last_seen {
            guard = self
                .condvar
                .wait(guard)
                .expect("work signal condvar poisoned");
        }
    }
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

pub struct Scheduler<T> {
    config: SchedulerConfig,
    shards: Vec<Mutex<Shard<T>>>,
    stats: StatsCounters,
    work_signal: WorkSignal,
    closed: AtomicBool,
    _marker: PhantomData<T>,
}

impl<T> Scheduler<T> {
    pub fn new(config: SchedulerConfig) -> Self {
        let shard_count = config.shards.max(1);
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(Mutex::new(Shard::new()));
        }
        Self {
            config,
            shards,
            stats: StatsCounters::new(),
            work_signal: WorkSignal::new(),
            closed: AtomicBool::new(false),
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

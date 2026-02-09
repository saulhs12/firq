use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

#[derive(Clone, Debug)]
pub struct Task<T> {
    pub payload: T,
    pub enqueue_ts: Instant,
    pub deadline: Option<Instant>,
    pub priority: Priority,
    pub cost: u64,
}

pub type QuantumProvider = Arc<dyn Fn(TenantKey) -> u64 + Send + Sync>;

#[derive(Clone)]
pub struct SchedulerConfig {
    pub shards: usize,
    pub max_global: usize,
    pub max_per_tenant: usize,
    pub quantum: u64,
    pub quantum_by_tenant: HashMap<TenantKey, u64>,
    pub quantum_provider: Option<QuantumProvider>,
    pub backpressure: BackpressurePolicy,
    pub backpressure_by_tenant: HashMap<TenantKey, BackpressurePolicy>,
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

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            shards: 1,
            max_global: 1000,
            max_per_tenant: 100,
            quantum: 10,
            quantum_by_tenant: HashMap::new(),
            quantum_provider: None,
            backpressure: BackpressurePolicy::Reject,
            backpressure_by_tenant: HashMap::new(),
            top_tenants_capacity: 10,
        }
    }
}

#[derive(Clone, Debug)]
pub enum BackpressurePolicy {
    Reject,
    DropOldestPerTenant,
    DropNewestPerTenant,
    Timeout { wait: Duration },
}

#[derive(Clone, Debug)]
pub enum EnqueueResult {
    Enqueued,
    Rejected(EnqueueRejectReason),
    Closed,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskHandle(u64);

impl TaskHandle {
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl From<u64> for TaskHandle {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug)]
pub enum EnqueueWithHandleResult {
    Enqueued(TaskHandle),
    Rejected(EnqueueRejectReason),
    Closed,
}

#[derive(Clone, Debug)]
pub enum EnqueueRejectReason {
    GlobalFull,
    TenantFull,
    Timeout,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CloseMode {
    Immediate,
    Drain,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelResult {
    Cancelled,
    NotFound,
}

#[derive(Clone, Debug)]
pub enum DequeueResult<T> {
    Task { tenant: TenantKey, task: Task<T> },
    Empty,
    Closed,
}

#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    pub enqueued: u64,
    pub dequeued: u64,
    pub expired: u64,
    pub dropped: u64,
    pub rejected_global: u64,
    pub rejected_tenant: u64,
    pub timeout_rejected: u64,
    pub dropped_policy: u64,
    pub queue_len_estimate: u64,
    pub max_global: u64,
    pub queue_saturation_ratio: f64,
    pub queue_time_sum_ns: u64,
    pub queue_time_samples: u64,
    pub queue_time_p95_ns: u64,
    pub queue_time_p99_ns: u64,
    pub queue_time_histogram: Vec<QueueTimeBucket>,
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

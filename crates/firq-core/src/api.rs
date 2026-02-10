use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Stable fairness key used to assign work to shards and DRR queues.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TenantKey(u64);

impl From<u64> for TenantKey {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl TenantKey {
    /// Returns the raw tenant identifier.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

/// Scheduling priority for a task.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Priority {
    /// Highest scheduling priority.
    High,
    /// Default scheduling priority.
    #[default]
    Normal,
    /// Lowest scheduling priority.
    Low,
}

impl Priority {
    /// Returns a fixed index for this priority (High=0, Normal=1, Low=2).
    pub fn index(self) -> usize {
        match self {
            Priority::High => 0,
            Priority::Normal => 1,
            Priority::Low => 2,
        }
    }

    /// Returns priorities in dequeue order.
    pub fn ordered() -> [Priority; 3] {
        [Priority::High, Priority::Normal, Priority::Low]
    }
}

/// Enqueued unit of work.
#[derive(Clone, Debug)]
pub struct Task<T> {
    /// User payload.
    pub payload: T,
    /// Enqueue timestamp used to compute queue time metrics.
    pub enqueue_ts: Instant,
    /// Optional deadline for expiration before execution.
    pub deadline: Option<Instant>,
    /// Priority queue used for dispatch ordering.
    pub priority: Priority,
    /// Cost consumed by DRR when this task is dequeued.
    pub cost: u64,
}

/// Dynamic per-tenant quantum provider.
pub type QuantumProvider = Arc<dyn Fn(TenantKey) -> u64 + Send + Sync>;

/// Scheduler runtime configuration.
#[derive(Clone)]
pub struct SchedulerConfig {
    /// Number of shards used to partition tenant state.
    pub shards: usize,
    /// Global queue capacity across all tenants.
    pub max_global: usize,
    /// Maximum pending live tasks per tenant.
    ///
    /// Cancelled/expired entries are compacted lazily and are not intended to
    /// consume this limit once reclaimed.
    pub max_per_tenant: usize,
    /// Base DRR quantum for tenants without overrides.
    pub quantum: u64,
    /// Static per-tenant DRR quantum overrides.
    pub quantum_by_tenant: HashMap<TenantKey, u64>,
    /// Dynamic per-tenant DRR quantum provider.
    pub quantum_provider: Option<QuantumProvider>,
    /// Default backpressure policy.
    pub backpressure: BackpressurePolicy,
    /// Per-tenant backpressure policy overrides.
    pub backpressure_by_tenant: HashMap<TenantKey, BackpressurePolicy>,
    /// Maximum number of tenants tracked in top-talker metrics.
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

/// Behavior when enqueue capacity limits are hit.
#[derive(Clone, Debug)]
pub enum BackpressurePolicy {
    /// Reject enqueue requests when limits are exceeded.
    Reject,
    /// Drop the oldest task from the same tenant and enqueue the new task.
    DropOldestPerTenant,
    /// Drop the newest task from the same tenant and enqueue the new task.
    DropNewestPerTenant,
    /// Wait for capacity up to `wait`, otherwise reject with timeout.
    Timeout { wait: Duration },
}

/// Result of an enqueue operation.
#[derive(Clone, Debug)]
pub enum EnqueueResult {
    /// Task was accepted.
    Enqueued,
    /// Task was rejected by backpressure policy.
    Rejected(EnqueueRejectReason),
    /// Scheduler is closed.
    Closed,
}

/// Opaque identifier for a pending task.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskHandle(u64);

impl TaskHandle {
    /// Returns the raw numeric handle.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl From<u64> for TaskHandle {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Result of `enqueue_with_handle`.
#[derive(Clone, Debug)]
pub enum EnqueueWithHandleResult {
    /// Task was accepted and assigned a cancel handle.
    Enqueued(TaskHandle),
    /// Task was rejected by backpressure policy.
    Rejected(EnqueueRejectReason),
    /// Scheduler is closed.
    Closed,
}

/// Reason for enqueue rejection.
#[derive(Clone, Debug)]
pub enum EnqueueRejectReason {
    /// Rejected due to global queue capacity.
    GlobalFull,
    /// Rejected due to per-tenant queue capacity.
    TenantFull,
    /// Rejected because timeout elapsed while waiting for capacity.
    Timeout,
}

/// Scheduler shutdown mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CloseMode {
    /// Stop accepting and wake blocked consumers immediately.
    Immediate,
    /// Stop accepting and drain queued tasks before closing.
    Drain,
}

/// Result of task cancellation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelResult {
    /// Task was found and cancelled.
    Cancelled,
    /// Task handle was not found.
    NotFound,
}

/// Result of a dequeue attempt.
#[derive(Clone, Debug)]
pub enum DequeueResult<T> {
    /// Returned task with its tenant identity.
    Task { tenant: TenantKey, task: Task<T> },
    /// No task available at the time of dequeue.
    Empty,
    /// Scheduler has been closed.
    Closed,
}

/// Snapshot of scheduler metrics.
#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    /// Total accepted enqueues.
    pub enqueued: u64,
    /// Total successfully dequeued tasks.
    pub dequeued: u64,
    /// Total tasks dropped because deadlines expired before dispatch.
    pub expired: u64,
    /// Total tasks dropped by reject/drop backpressure paths.
    pub dropped: u64,
    /// Total enqueue rejections due to global capacity.
    pub rejected_global: u64,
    /// Total enqueue rejections due to per-tenant capacity.
    pub rejected_tenant: u64,
    /// Total enqueue rejections caused by timeout.
    pub timeout_rejected: u64,
    /// Total tasks dropped by replacement policies.
    pub dropped_policy: u64,
    /// Current estimated live queue length.
    pub queue_len_estimate: u64,
    /// Configured global queue capacity.
    pub max_global: u64,
    /// `queue_len_estimate / max_global`.
    pub queue_saturation_ratio: f64,
    /// Sum of queue time in nanoseconds for dequeued tasks.
    pub queue_time_sum_ns: u64,
    /// Number of queue time samples.
    pub queue_time_samples: u64,
    /// p95 queue time estimate in nanoseconds.
    pub queue_time_p95_ns: u64,
    /// p99 queue time estimate in nanoseconds.
    pub queue_time_p99_ns: u64,
    /// Histogram buckets for queue time.
    pub queue_time_histogram: Vec<QueueTimeBucket>,
    /// Top talkers by recent observed volume.
    pub top_tenants: Vec<TenantCount>,
}

/// Single histogram bucket for queue time metrics.
#[derive(Clone, Debug, Default)]
pub struct QueueTimeBucket {
    /// Upper bound of bucket in nanoseconds.
    pub le_ns: u64,
    /// Number of observations in this bucket.
    pub count: u64,
}

/// Tenant counter entry used in top-talker snapshots.
#[derive(Clone, Debug)]
pub struct TenantCount {
    /// Tenant identifier.
    pub tenant: TenantKey,
    /// Observed count for this tenant.
    pub count: u64,
}

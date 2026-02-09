use std::collections::HashMap;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::api::{
    DequeueResult, EnqueueRejectReason, EnqueueResult, QuantumProvider, SchedulerConfig,
    SchedulerStats, Task, TenantKey,
};
use crate::state::{
    ActiveRing, QUEUE_TIME_BUCKETS_NS, Shard, StatsCounters, TenantState, TopTenants, WorkSignal,
};

pub struct Scheduler<T> {
    config: SchedulerConfig,
    shards: Vec<Mutex<Shard<T>>>,
    active_shards: Mutex<ActiveRing<usize>>,
    stats: StatsCounters,
    work_signal: WorkSignal,
    closed: AtomicBool,
    shard_active: Vec<AtomicBool>,
    top_tenants: Mutex<TopTenants>,
    tenant_quantum: RwLock<HashMap<TenantKey, u64>>,
    quantum_provider: RwLock<Option<QuantumProvider>>,
}

impl<T> Scheduler<T> {
    fn shard_index(&self, tenant: TenantKey) -> usize {
        let shard_count = self.shards.len();
        let hash = tenant.as_u64();
        (hash as usize) % shard_count
    }

    fn saturating_add(counter: &AtomicU64, delta: u64) {
        if delta == 0 {
            return;
        }
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(delta);
            match counter.compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn record_queue_time(&self, queue_time_ns: u64) {
        for (idx, bound) in QUEUE_TIME_BUCKETS_NS.iter().enumerate() {
            if queue_time_ns <= *bound {
                Self::saturating_add(&self.stats.queue_time_buckets[idx], 1);
                break;
            }
        }
    }

    fn activate_shard(&self, shard_index: usize) {
        if !self.shard_active[shard_index].swap(true, Ordering::AcqRel) {
            self.active_shards.lock().push_back(shard_index);
        }
    }

    fn dec_queue_len_estimate(&self, delta: u64) {
        if delta == 0 {
            return;
        }

        let mut current = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(delta);
            match self.stats.queue_len_estimate.compare_exchange(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn quantum_for(&self, tenant: TenantKey) -> u64 {
        if let Some(value) = self.tenant_quantum.read().get(&tenant).copied() {
            return value.max(1);
        }
        if let Some(provider) = self.quantum_provider.read().as_ref() {
            return provider(tenant).max(1);
        }
        self.config.quantum.max(1)
    }

    fn refresh_tenant_quantum(&self, tenant: TenantKey) {
        let quantum = self.quantum_for(tenant) as i64;
        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index].lock();
        if let Some(state) = shard.tenants.get_mut(&tenant) {
            state.quantum = quantum;
        }
    }

    pub fn set_tenant_quantum(&self, tenant: TenantKey, quantum: u64) {
        let value = quantum.max(1);
        self.tenant_quantum.write().insert(tenant, value);
        self.refresh_tenant_quantum(tenant);
    }

    pub fn clear_tenant_quantum(&self, tenant: TenantKey) {
        self.tenant_quantum.write().remove(&tenant);
        self.refresh_tenant_quantum(tenant);
    }

    pub fn set_quantum_provider(&self, provider: Option<QuantumProvider>) {
        {
            let mut guard = self.quantum_provider.write();
            *guard = provider;
        }
        for shard_mutex in &self.shards {
            let mut shard = shard_mutex.lock();
            for (tenant, state) in shard.tenants.iter_mut() {
                let quantum = self.quantum_for(*tenant) as i64;
                state.quantum = quantum;
            }
        }
    }

    pub fn new(config: SchedulerConfig) -> Self {
        let shard_count = config.shards.max(1);
        let top_tenants_capacity = config.top_tenants_capacity;
        let mut shards = Vec::with_capacity(shard_count);
        let mut shard_active = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(Mutex::new(Shard::new()));
            shard_active.push(AtomicBool::new(false));
        }
        Self {
            tenant_quantum: RwLock::new(config.quantum_by_tenant.clone()),
            quantum_provider: RwLock::new(config.quantum_provider.clone()),
            config,
            shards,
            active_shards: Mutex::new(ActiveRing::new()),
            stats: StatsCounters::new(),
            work_signal: WorkSignal::new(),
            closed: AtomicBool::new(false),
            shard_active,
            top_tenants: Mutex::new(TopTenants::new(top_tenants_capacity)),
        }
    }

    pub fn enqueue(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        match self.backpressure_for(tenant).clone() {
            crate::api::BackpressurePolicy::Reject => self.enqueue_reject(tenant, task),
            crate::api::BackpressurePolicy::DropOldestPerTenant => {
                self.enqueue_drop(tenant, task, DropStrategy::Oldest)
            }
            crate::api::BackpressurePolicy::DropNewestPerTenant => {
                self.enqueue_drop(tenant, task, DropStrategy::Newest)
            }
            crate::api::BackpressurePolicy::Timeout { wait } => {
                self.enqueue_timeout(tenant, task, wait)
            }
        }
    }

    pub fn try_dequeue(&self) -> DequeueResult<T> {
        if self.closed.load(Ordering::Acquire) {
            return DequeueResult::Closed;
        }

        let mut remaining = { self.active_shards.lock().len() };

        while remaining > 0 {
            let shard_index = {
                let mut active_shards = self.active_shards.lock();
                match active_shards.pop_front() {
                    Some(index) => index,
                    None => break,
                }
            };
            remaining -= 1;

            let mut shard = self.shards[shard_index].lock();
            let result = self.try_dequeue_from_shard(&mut shard);
            let shard_has_work = shard.active_rings.iter().any(|ring| !ring.is_empty());
            drop(shard);

            if shard_has_work {
                let mut active_shards = self.active_shards.lock();
                active_shards.push_back(shard_index);
            } else {
                self.shard_active[shard_index].store(false, Ordering::Release);
            }

            if let Some((tenant, task)) = result {
                return DequeueResult::Task { tenant, task };
            }
        }

        if self.closed.load(Ordering::Acquire) {
            DequeueResult::Closed
        } else {
            DequeueResult::Empty
        }
    }

    fn try_dequeue_from_shard(&self, shard: &mut Shard<T>) -> Option<(TenantKey, Task<T>)> {
        for priority in crate::api::Priority::ordered() {
            let idx = priority.index();
            if shard.active_rings[idx].is_empty() {
                continue;
            }

            // Recorremos como maximo una vuelta completa del ring para evitar loops.
            let ring_len = shard.active_rings[idx].len();
            for _ in 0..ring_len {
                let tenant = match shard.active_rings[idx].pop_front() {
                    Some(tenant) => tenant,
                    None => break,
                };

                let Some(tenant_state) = shard.tenants.get_mut(&tenant) else {
                    continue;
                };

                let queue = &mut tenant_state.queues[idx];
                let deficit = &mut tenant_state.deficits[idx];

                let mut expired_count = 0u64;
                let now = Instant::now();

                while let Some(front) = queue.front() {
                    match front.deadline {
                        Some(deadline) if now > deadline => {
                            queue.pop_front();
                            expired_count += 1;
                        }
                        _ => break,
                    }
                }

                if expired_count > 0 {
                    Self::saturating_add(&self.stats.expired, expired_count);
                    self.dec_queue_len_estimate(expired_count);
                    self.work_signal.notify_all();
                }

                if queue.is_empty() {
                    tenant_state.active[idx] = false;
                    continue;
                }

                let front_cost = queue.front().map(|task| task.cost).unwrap_or(0);
                let cost = if front_cost > i64::MAX as u64 {
                    i64::MAX
                } else {
                    front_cost as i64
                };

                if *deficit < cost {
                    *deficit += tenant_state.quantum;
                    shard.active_rings[idx].push_back(tenant);
                    continue;
                }

                *deficit -= cost;
                let task = match queue.pop_front() {
                    Some(task) => task,
                    None => {
                        tenant_state.active[idx] = false;
                        continue;
                    }
                };

                if queue.is_empty() {
                    tenant_state.active[idx] = false;
                } else {
                    shard.active_rings[idx].push_back(tenant);
                }

                Self::saturating_add(&self.stats.dequeued, 1);
                self.dec_queue_len_estimate(1);
                self.work_signal.notify_all();

                let queue_time_ns = now
                    .duration_since(task.enqueue_ts)
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                Self::saturating_add(&self.stats.queue_time_sum_ns, queue_time_ns);
                Self::saturating_add(&self.stats.queue_time_samples, 1);
                self.record_queue_time(queue_time_ns);
                self.top_tenants.lock().record(tenant, 1);

                return Some((tenant, task));
            }
        }

        None
    }

    pub fn dequeue_blocking(&self) -> DequeueResult<T> {
        const BACKOFF_SPINS: u32 = 64;
        const BACKOFF_SLEEP: Duration = Duration::from_micros(200);
        let mut spins = 0u32;
        loop {
            let observed = self.work_signal.current();
            match self.try_dequeue() {
                DequeueResult::Task { tenant, task } => {
                    return DequeueResult::Task { tenant, task };
                }
                DequeueResult::Closed => return DequeueResult::Closed,
                DequeueResult::Empty => {
                    if self.closed.load(Ordering::Acquire) {
                        return DequeueResult::Closed;
                    }

                    if self.stats.queue_len_estimate.load(Ordering::Relaxed) > 0 {
                        // Hay trabajo pendiente pero aun no elegible (e.g. DRR); reintentar.
                        spins = spins.saturating_add(1);
                        if spins >= BACKOFF_SPINS {
                            let _ = self
                                .work_signal
                                .wait_for_change_timeout(observed, BACKOFF_SLEEP);
                            spins = 0;
                        } else {
                            std::hint::spin_loop();
                        }
                        continue;
                    }

                    spins = 0;
                    self.work_signal.wait_for_change(observed);
                }
            }
        }
    }

    pub fn stats(&self) -> SchedulerStats {
        let queue_time_histogram = QUEUE_TIME_BUCKETS_NS
            .iter()
            .enumerate()
            .map(|(idx, bound)| crate::api::QueueTimeBucket {
                le_ns: *bound,
                count: self.stats.queue_time_buckets[idx].load(Ordering::Relaxed),
            })
            .collect::<Vec<_>>();

        let total_samples: u64 = queue_time_histogram.iter().map(|b| b.count).sum();
        let percentile = |pct: f64| -> u64 {
            if total_samples == 0 {
                return 0;
            }
            let target = (total_samples as f64 * pct).ceil() as u64;
            let mut cumulative = 0u64;
            for bucket in &queue_time_histogram {
                cumulative = cumulative.saturating_add(bucket.count);
                if cumulative >= target {
                    return bucket.le_ns;
                }
            }
            queue_time_histogram
                .last()
                .map(|bucket| bucket.le_ns)
                .unwrap_or(0)
        };

        let top_tenants = self.top_tenants.lock().snapshot();

        SchedulerStats {
            enqueued: self.stats.enqueued.load(Ordering::Relaxed),
            dequeued: self.stats.dequeued.load(Ordering::Relaxed),
            expired: self.stats.expired.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            queue_len_estimate: self.stats.queue_len_estimate.load(Ordering::Relaxed),
            queue_time_sum_ns: self.stats.queue_time_sum_ns.load(Ordering::Relaxed),
            queue_time_samples: self.stats.queue_time_samples.load(Ordering::Relaxed),
            queue_time_p95_ns: percentile(0.95),
            queue_time_p99_ns: percentile(0.99),
            queue_time_histogram,
            top_tenants,
        }
    }

    pub fn close(&self) {
        let was_closed = self.closed.swap(true, Ordering::Release);
        if !was_closed {
            self.work_signal.notify_all();
        }
    }
}

enum DropStrategy {
    Oldest,
    Newest,
}

enum EnqueueAttempt<T> {
    Enqueued,
    Closed,
    Full(Task<T>, EnqueueRejectReason),
}

impl<T> Scheduler<T> {
    fn enqueue_reject(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        match self.enqueue_try_reject(tenant, task) {
            EnqueueAttempt::Enqueued => EnqueueResult::Enqueued,
            EnqueueAttempt::Closed => EnqueueResult::Closed,
            EnqueueAttempt::Full(_, reason) => {
                Self::saturating_add(&self.stats.dropped, 1);
                EnqueueResult::Rejected(reason)
            }
        }
    }

    fn enqueue_timeout(
        &self,
        tenant: TenantKey,
        mut task: Task<T>,
        wait: std::time::Duration,
    ) -> EnqueueResult {
        let deadline = Instant::now() + wait;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return EnqueueResult::Closed;
            }
            let observed = self.work_signal.current();
            match self.enqueue_try_reject(tenant, task) {
                EnqueueAttempt::Enqueued => return EnqueueResult::Enqueued,
                EnqueueAttempt::Closed => return EnqueueResult::Closed,
                EnqueueAttempt::Full(returned, reason) => {
                    task = returned;
                    if Instant::now() >= deadline {
                        Self::saturating_add(&self.stats.dropped, 1);
                        return EnqueueResult::Rejected(reason);
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let _ = self
                        .work_signal
                        .wait_for_change_timeout(observed, remaining);
                }
            }
        }
    }

    fn enqueue_try_reject(&self, tenant: TenantKey, task: Task<T>) -> EnqueueAttempt<T> {
        if self.closed.load(Ordering::Acquire) {
            return EnqueueAttempt::Closed;
        }

        let max_global = self.config.max_global as u64;
        let current_len = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        if current_len >= max_global {
            return EnqueueAttempt::Full(task, EnqueueRejectReason::GlobalFull);
        }

        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index].lock();

        if self.closed.load(Ordering::Acquire) {
            return EnqueueAttempt::Closed;
        }

        let shard_was_empty = shard.active_rings.iter().all(|ring| ring.is_empty());

        let tenant_state = shard
            .tenants
            .entry(tenant)
            .or_insert_with(|| TenantState::new(self.quantum_for(tenant) as i64));

        if tenant_state.total_len() >= self.config.max_per_tenant {
            return EnqueueAttempt::Full(task, EnqueueRejectReason::TenantFull);
        }

        let priority_index = task.priority.index();
        let queue = &mut tenant_state.queues[priority_index];
        let was_empty = queue.is_empty();
        queue.push_back(task);

        if was_empty && !tenant_state.active[priority_index] {
            tenant_state.active[priority_index] = true;
            shard.active_rings[priority_index].push_back(tenant);
        }

        Self::saturating_add(&self.stats.enqueued, 1);
        Self::saturating_add(&self.stats.queue_len_estimate, 1);

        drop(shard);

        if shard_was_empty {
            self.activate_shard(shard_index);
        }

        if was_empty {
            self.work_signal.notify_all();
        }

        EnqueueAttempt::Enqueued
    }

    fn enqueue_drop(
        &self,
        tenant: TenantKey,
        task: Task<T>,
        strategy: DropStrategy,
    ) -> EnqueueResult {
        if self.closed.load(Ordering::Acquire) {
            return EnqueueResult::Closed;
        }

        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index].lock();

        if self.closed.load(Ordering::Acquire) {
            return EnqueueResult::Closed;
        }

        let shard_was_empty = shard.active_rings.iter().all(|ring| ring.is_empty());

        let tenant_state = shard
            .tenants
            .entry(tenant)
            .or_insert_with(|| TenantState::new(self.quantum_for(tenant) as i64));

        let max_global = self.config.max_global as u64;
        let current_len = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        let global_full = current_len >= max_global;
        let tenant_full = tenant_state.total_len() >= self.config.max_per_tenant;

        if global_full || tenant_full {
            let dropped = self.drop_from_tenant(tenant_state, strategy, task.priority);
            if let Some(_dropped) = dropped {
                Self::saturating_add(&self.stats.dropped, 1);
                self.dec_queue_len_estimate(1);
            } else {
                Self::saturating_add(&self.stats.dropped, 1);
                return EnqueueResult::Rejected(if global_full {
                    EnqueueRejectReason::GlobalFull
                } else {
                    EnqueueRejectReason::TenantFull
                });
            }
        }

        let priority_index = task.priority.index();
        let queue = &mut tenant_state.queues[priority_index];
        let was_empty = queue.is_empty();
        queue.push_back(task);

        if was_empty && !tenant_state.active[priority_index] {
            tenant_state.active[priority_index] = true;
            shard.active_rings[priority_index].push_back(tenant);
        }

        Self::saturating_add(&self.stats.enqueued, 1);
        Self::saturating_add(&self.stats.queue_len_estimate, 1);

        drop(shard);

        if shard_was_empty {
            self.activate_shard(shard_index);
        }

        if was_empty {
            self.work_signal.notify_all();
        }

        EnqueueResult::Enqueued
    }

    fn drop_from_tenant(
        &self,
        tenant_state: &mut TenantState<T>,
        strategy: DropStrategy,
        incoming: crate::api::Priority,
    ) -> Option<Task<T>> {
        const DROP_HIGH: [crate::api::Priority; 3] = [
            crate::api::Priority::Low,
            crate::api::Priority::Normal,
            crate::api::Priority::High,
        ];
        const DROP_NORMAL: [crate::api::Priority; 2] =
            [crate::api::Priority::Low, crate::api::Priority::Normal];
        const DROP_LOW: [crate::api::Priority; 1] = [crate::api::Priority::Low];

        let drop_order: &[crate::api::Priority] = match incoming {
            crate::api::Priority::High => &DROP_HIGH,
            crate::api::Priority::Normal => &DROP_NORMAL,
            crate::api::Priority::Low => &DROP_LOW,
        };

        for &priority in drop_order {
            let idx = priority.index();
            if !tenant_state.queues[idx].is_empty() {
                return match strategy {
                    DropStrategy::Oldest => tenant_state.queues[idx].pop_front(),
                    DropStrategy::Newest => tenant_state.queues[idx].pop_back(),
                };
            }
        }
        None
    }

    fn backpressure_for(&self, tenant: TenantKey) -> &crate::api::BackpressurePolicy {
        self.config
            .backpressure_by_tenant
            .get(&tenant)
            .unwrap_or(&self.config.backpressure)
    }
}

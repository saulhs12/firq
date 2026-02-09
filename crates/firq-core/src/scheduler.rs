use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::api::{
    CancelResult, CloseMode, DequeueResult, EnqueueRejectReason, EnqueueResult,
    EnqueueWithHandleResult, Priority, QuantumProvider, SchedulerConfig, SchedulerStats, Task,
    TaskHandle, TenantKey,
};
use crate::state::{
    ActiveRing, QUEUE_TIME_BUCKETS_NS, QueueEntry, Shard, StatsCounters, TenantState, TopTenants,
    WorkSignal,
};

const SHUTDOWN_OPEN: u8 = 0;
const SHUTDOWN_DRAIN: u8 = 1;
const SHUTDOWN_IMMEDIATE: u8 = 2;

pub struct Scheduler<T> {
    config: SchedulerConfig,
    shards: Vec<Mutex<Shard<T>>>,
    active_shards: Mutex<ActiveRing<usize>>,
    stats: StatsCounters,
    work_signal: WorkSignal,
    shutdown_state: AtomicU8,
    shard_active: Vec<AtomicBool>,
    top_tenants: Mutex<TopTenants>,
    tenant_quantum: RwLock<HashMap<TenantKey, u64>>,
    quantum_provider: RwLock<Option<QuantumProvider>>,
    next_task_id: AtomicU64,
    pending_ids: Mutex<HashSet<u64>>,
    cancelled_ids: Mutex<HashSet<u64>>,
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

    fn try_reserve_slot(&self) -> bool {
        let max_global = self.config.max_global as u64;
        let mut current = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        loop {
            if current >= max_global {
                return false;
            }
            let next = current + 1;
            match self.stats.queue_len_estimate.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release_reserved_slots(&self, delta: u64) {
        if delta == 0 {
            return;
        }

        let mut current = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(delta);
            match self.stats.queue_len_estimate.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn activate_shard(&self, shard_index: usize) {
        if !self.shard_active[shard_index].swap(true, Ordering::AcqRel) {
            self.active_shards.lock().push_back(shard_index);
        }
    }

    fn shutdown_state(&self) -> u8 {
        self.shutdown_state.load(Ordering::Acquire)
    }

    fn is_accepting_enqueues(&self) -> bool {
        self.shutdown_state() == SHUTDOWN_OPEN
    }

    fn should_return_closed(&self) -> bool {
        match self.shutdown_state() {
            SHUTDOWN_IMMEDIATE => true,
            SHUTDOWN_DRAIN => self.stats.queue_len_estimate.load(Ordering::Acquire) == 0,
            _ => false,
        }
    }

    fn set_shutdown_mode(&self, mode: CloseMode) {
        let target = match mode {
            CloseMode::Immediate => SHUTDOWN_IMMEDIATE,
            CloseMode::Drain => SHUTDOWN_DRAIN,
        };

        let mut changed = false;
        loop {
            let current = self.shutdown_state();
            let next = match (current, target) {
                (SHUTDOWN_IMMEDIATE, _) => SHUTDOWN_IMMEDIATE,
                (_, SHUTDOWN_IMMEDIATE) => SHUTDOWN_IMMEDIATE,
                (SHUTDOWN_OPEN, SHUTDOWN_DRAIN) => SHUTDOWN_DRAIN,
                _ => current,
            };

            if next == current {
                break;
            }

            match self.shutdown_state.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    changed = true;
                    break;
                }
                Err(_) => continue,
            }
        }

        if changed {
            self.work_signal.notify_all();
        }
    }

    fn record_reject(&self, reason: EnqueueRejectReason) {
        Self::saturating_add(&self.stats.dropped, 1);
        match reason {
            EnqueueRejectReason::GlobalFull => {
                Self::saturating_add(&self.stats.rejected_global, 1);
            }
            EnqueueRejectReason::TenantFull => {
                Self::saturating_add(&self.stats.rejected_tenant, 1);
            }
            EnqueueRejectReason::Timeout => {
                Self::saturating_add(&self.stats.timeout_rejected, 1);
            }
        }
    }

    fn record_policy_drop(&self) {
        Self::saturating_add(&self.stats.dropped, 1);
        Self::saturating_add(&self.stats.dropped_policy, 1);
    }

    fn next_handle(&self) -> TaskHandle {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed) + 1;
        TaskHandle::from(id)
    }

    fn register_pending(&self, handle: TaskHandle) {
        self.pending_ids.lock().insert(handle.as_u64());
    }

    fn take_pending(&self, id: u64) -> bool {
        self.pending_ids.lock().remove(&id)
    }

    fn mark_cancelled(&self, id: u64) {
        self.cancelled_ids.lock().insert(id);
    }

    fn take_cancelled_marker(&self, id: u64) -> bool {
        self.cancelled_ids.lock().remove(&id)
    }

    fn clear_cancelled_marker(&self, id: u64) {
        self.cancelled_ids.lock().remove(&id);
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
            shutdown_state: AtomicU8::new(SHUTDOWN_OPEN),
            shard_active,
            top_tenants: Mutex::new(TopTenants::new(top_tenants_capacity)),
            next_task_id: AtomicU64::new(0),
            pending_ids: Mutex::new(HashSet::new()),
            cancelled_ids: Mutex::new(HashSet::new()),
        }
    }

    pub fn enqueue(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        match self.enqueue_with_handle(tenant, task) {
            EnqueueWithHandleResult::Enqueued(_) => EnqueueResult::Enqueued,
            EnqueueWithHandleResult::Rejected(reason) => EnqueueResult::Rejected(reason),
            EnqueueWithHandleResult::Closed => EnqueueResult::Closed,
        }
    }

    pub fn enqueue_with_handle(&self, tenant: TenantKey, task: Task<T>) -> EnqueueWithHandleResult {
        match self.backpressure_for(tenant).clone() {
            crate::api::BackpressurePolicy::Reject => self.enqueue_reject_with_handle(tenant, task),
            crate::api::BackpressurePolicy::DropOldestPerTenant => {
                self.enqueue_drop_with_handle(tenant, task, DropStrategy::Oldest)
            }
            crate::api::BackpressurePolicy::DropNewestPerTenant => {
                self.enqueue_drop_with_handle(tenant, task, DropStrategy::Newest)
            }
            crate::api::BackpressurePolicy::Timeout { wait } => {
                self.enqueue_timeout_with_handle(tenant, task, wait)
            }
        }
    }

    pub fn cancel(&self, handle: TaskHandle) -> CancelResult {
        let id = handle.as_u64();
        if self.take_pending(id) {
            self.mark_cancelled(id);
            self.release_reserved_slots(1);
            self.work_signal.notify_all();
            CancelResult::Cancelled
        } else {
            CancelResult::NotFound
        }
    }

    pub fn try_dequeue(&self) -> DequeueResult<T> {
        if self.shutdown_state() == SHUTDOWN_IMMEDIATE {
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

            let (result, shard_has_work) = {
                let mut shard = self.shards[shard_index].lock();
                let result = self.try_dequeue_from_shard(&mut shard);
                let has_work = shard.active_rings.iter().any(|ring| !ring.is_empty());
                if !has_work {
                    self.shard_active[shard_index].store(false, Ordering::Release);
                }
                (result, has_work)
            };

            if shard_has_work {
                self.active_shards.lock().push_back(shard_index);
            }

            if let Some((tenant, task)) = result {
                return DequeueResult::Task { tenant, task };
            }
        }

        if self.should_return_closed() {
            DequeueResult::Closed
        } else {
            DequeueResult::Empty
        }
    }

    fn try_dequeue_from_shard(&self, shard: &mut Shard<T>) -> Option<(TenantKey, Task<T>)> {
        for priority in Priority::ordered() {
            let idx = priority.index();
            if shard.active_rings[idx].is_empty() {
                continue;
            }

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
                    if self.take_cancelled_marker(front.id) {
                        queue.pop_front();
                        continue;
                    }
                    match front.task.deadline {
                        Some(deadline) if now > deadline => {
                            let expired = queue.pop_front().expect("front disappeared");
                            if self.take_pending(expired.id) {
                                expired_count = expired_count.saturating_add(1);
                                self.release_reserved_slots(1);
                            }
                        }
                        _ => break,
                    }
                }

                if expired_count > 0 {
                    Self::saturating_add(&self.stats.expired, expired_count);
                    self.work_signal.notify_all();
                }

                if queue.is_empty() {
                    tenant_state.active[idx] = false;
                    continue;
                }

                let front_cost = queue.front().map(|entry| entry.task.cost).unwrap_or(0);
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

                let entry = match queue.pop_front() {
                    Some(entry) => entry,
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

                if self.take_cancelled_marker(entry.id) {
                    continue;
                }

                if !self.take_pending(entry.id) {
                    continue;
                }

                *deficit -= cost;

                Self::saturating_add(&self.stats.dequeued, 1);
                self.release_reserved_slots(1);
                self.work_signal.notify_all();

                let queue_time_ns = Instant::now()
                    .duration_since(entry.task.enqueue_ts)
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                Self::saturating_add(&self.stats.queue_time_sum_ns, queue_time_ns);
                Self::saturating_add(&self.stats.queue_time_samples, 1);
                self.record_queue_time(queue_time_ns);
                self.top_tenants.lock().record(tenant, 1);

                return Some((tenant, entry.task));
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
                    if self.should_return_closed() {
                        return DequeueResult::Closed;
                    }

                    if self.stats.queue_len_estimate.load(Ordering::Acquire) > 0 {
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

        let queue_len_estimate = self.stats.queue_len_estimate.load(Ordering::Relaxed);
        let max_global = self.config.max_global as u64;
        let queue_saturation_ratio = if max_global == 0 {
            0.0
        } else {
            queue_len_estimate as f64 / max_global as f64
        };

        let top_tenants = self.top_tenants.lock().snapshot();

        SchedulerStats {
            enqueued: self.stats.enqueued.load(Ordering::Relaxed),
            dequeued: self.stats.dequeued.load(Ordering::Relaxed),
            expired: self.stats.expired.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            rejected_global: self.stats.rejected_global.load(Ordering::Relaxed),
            rejected_tenant: self.stats.rejected_tenant.load(Ordering::Relaxed),
            timeout_rejected: self.stats.timeout_rejected.load(Ordering::Relaxed),
            dropped_policy: self.stats.dropped_policy.load(Ordering::Relaxed),
            queue_len_estimate,
            max_global,
            queue_saturation_ratio,
            queue_time_sum_ns: self.stats.queue_time_sum_ns.load(Ordering::Relaxed),
            queue_time_samples: self.stats.queue_time_samples.load(Ordering::Relaxed),
            queue_time_p95_ns: percentile(0.95),
            queue_time_p99_ns: percentile(0.99),
            queue_time_histogram,
            top_tenants,
        }
    }

    pub fn close(&self) {
        self.close_immediate();
    }

    pub fn close_immediate(&self) {
        self.set_shutdown_mode(CloseMode::Immediate);
    }

    pub fn close_drain(&self) {
        self.set_shutdown_mode(CloseMode::Drain);
    }

    pub fn close_with_mode(&self, mode: CloseMode) {
        self.set_shutdown_mode(mode);
    }
}

enum DropStrategy {
    Oldest,
    Newest,
}

enum EnqueueAttempt<T> {
    Enqueued(TaskHandle),
    Closed,
    Full(Task<T>, EnqueueRejectReason),
}

impl<T> Scheduler<T> {
    fn enqueue_reject_with_handle(
        &self,
        tenant: TenantKey,
        task: Task<T>,
    ) -> EnqueueWithHandleResult {
        match self.enqueue_try_reject(tenant, task) {
            EnqueueAttempt::Enqueued(handle) => EnqueueWithHandleResult::Enqueued(handle),
            EnqueueAttempt::Closed => EnqueueWithHandleResult::Closed,
            EnqueueAttempt::Full(_, reason) => {
                self.record_reject(reason.clone());
                EnqueueWithHandleResult::Rejected(reason)
            }
        }
    }

    fn enqueue_timeout_with_handle(
        &self,
        tenant: TenantKey,
        mut task: Task<T>,
        wait: Duration,
    ) -> EnqueueWithHandleResult {
        let deadline = Instant::now() + wait;
        loop {
            if !self.is_accepting_enqueues() {
                return EnqueueWithHandleResult::Closed;
            }

            let observed = self.work_signal.current();
            match self.enqueue_try_reject(tenant, task) {
                EnqueueAttempt::Enqueued(handle) => {
                    return EnqueueWithHandleResult::Enqueued(handle);
                }
                EnqueueAttempt::Closed => return EnqueueWithHandleResult::Closed,
                EnqueueAttempt::Full(returned, _) => {
                    task = returned;
                    if Instant::now() >= deadline {
                        self.record_reject(EnqueueRejectReason::Timeout);
                        return EnqueueWithHandleResult::Rejected(EnqueueRejectReason::Timeout);
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
        if !self.is_accepting_enqueues() {
            return EnqueueAttempt::Closed;
        }

        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index].lock();

        if !self.is_accepting_enqueues() {
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

        if !self.try_reserve_slot() {
            return EnqueueAttempt::Full(task, EnqueueRejectReason::GlobalFull);
        }

        let handle = self.next_handle();
        self.register_pending(handle);

        let priority_index = task.priority.index();
        let queue = &mut tenant_state.queues[priority_index];
        let was_empty = queue.is_empty();
        queue.push_back(QueueEntry {
            id: handle.as_u64(),
            task,
        });

        if was_empty && !tenant_state.active[priority_index] {
            tenant_state.active[priority_index] = true;
            shard.active_rings[priority_index].push_back(tenant);
        }

        Self::saturating_add(&self.stats.enqueued, 1);

        drop(shard);

        if shard_was_empty {
            self.activate_shard(shard_index);
        }

        if was_empty {
            self.work_signal.notify_all();
        }

        EnqueueAttempt::Enqueued(handle)
    }

    fn enqueue_drop_with_handle(
        &self,
        tenant: TenantKey,
        task: Task<T>,
        strategy: DropStrategy,
    ) -> EnqueueWithHandleResult {
        if !self.is_accepting_enqueues() {
            return EnqueueWithHandleResult::Closed;
        }

        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index].lock();

        if !self.is_accepting_enqueues() {
            return EnqueueWithHandleResult::Closed;
        }

        let shard_was_empty = shard.active_rings.iter().all(|ring| ring.is_empty());

        let tenant_state = shard
            .tenants
            .entry(tenant)
            .or_insert_with(|| TenantState::new(self.quantum_for(tenant) as i64));

        let current_len = self.stats.queue_len_estimate.load(Ordering::Acquire);
        let tenant_full = tenant_state.total_len() >= self.config.max_per_tenant;
        let global_full = current_len >= self.config.max_global as u64;

        let mut reused_slot = false;
        if tenant_full || global_full {
            let dropped = self.drop_from_tenant(tenant_state, strategy, task.priority);
            let Some(dropped) = dropped else {
                let reason = if tenant_full {
                    EnqueueRejectReason::TenantFull
                } else {
                    EnqueueRejectReason::GlobalFull
                };
                self.record_reject(reason.clone());
                return EnqueueWithHandleResult::Rejected(reason);
            };

            self.clear_cancelled_marker(dropped.id);
            if self.take_pending(dropped.id) {
                self.record_policy_drop();
                reused_slot = true;
            }
        }

        if !reused_slot && !self.try_reserve_slot() {
            self.record_reject(EnqueueRejectReason::GlobalFull);
            return EnqueueWithHandleResult::Rejected(EnqueueRejectReason::GlobalFull);
        }

        let handle = self.next_handle();
        self.register_pending(handle);

        let priority_index = task.priority.index();
        let queue = &mut tenant_state.queues[priority_index];
        let was_empty = queue.is_empty();
        queue.push_back(QueueEntry {
            id: handle.as_u64(),
            task,
        });

        if was_empty && !tenant_state.active[priority_index] {
            tenant_state.active[priority_index] = true;
            shard.active_rings[priority_index].push_back(tenant);
        }

        Self::saturating_add(&self.stats.enqueued, 1);

        drop(shard);

        if shard_was_empty {
            self.activate_shard(shard_index);
        }

        if was_empty {
            self.work_signal.notify_all();
        }

        EnqueueWithHandleResult::Enqueued(handle)
    }

    fn drop_from_tenant(
        &self,
        tenant_state: &mut TenantState<T>,
        strategy: DropStrategy,
        incoming: Priority,
    ) -> Option<QueueEntry<T>> {
        const DROP_HIGH: [Priority; 3] = [Priority::Low, Priority::Normal, Priority::High];
        const DROP_NORMAL: [Priority; 2] = [Priority::Low, Priority::Normal];
        const DROP_LOW: [Priority; 1] = [Priority::Low];

        let drop_order: &[Priority] = match incoming {
            Priority::High => &DROP_HIGH,
            Priority::Normal => &DROP_NORMAL,
            Priority::Low => &DROP_LOW,
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

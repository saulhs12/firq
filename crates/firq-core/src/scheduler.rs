use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::state::{Shard, StatsCounters, TenantState, WorkSignal};
use crate::api::{
    DequeueResult, EnqueueRejectReason, EnqueueResult, SchedulerConfig, SchedulerStats, Task,
    TenantKey,
};

pub struct Scheduler<T> {
    config: SchedulerConfig,
    shards: Vec<Mutex<Shard<T>>>,
    stats: StatsCounters,
    work_signal: WorkSignal,
    closed: AtomicBool,
    _marker: PhantomData<T>,
}

impl<T> Scheduler<T> {
    fn shard_index(&self, tenant: TenantKey) -> usize {
        let shard_count = self.shards.len();
        let hash = tenant.as_u64();
        (hash as usize) % shard_count
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

    pub fn enqueue(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        if self.closed.load(Ordering::Acquire) {
            return EnqueueResult::Closed;
        }

        let max_global = self.config.max_global as u64;
        let current_len = self.stats.queue_len_estimate.load(Ordering::Acquire);
        if current_len >= max_global {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return EnqueueResult::Rejected(EnqueueRejectReason::GlobalFull);
        }

        let shard_index = self.shard_index(tenant);
        let mut shard = self.shards[shard_index]
            .lock()
            .expect("shard mutex poisoned");

        if self.closed.load(Ordering::Acquire) {
            return EnqueueResult::Closed;
        }

        let tenant_state = shard
            .tenants
            .entry(tenant)
            .or_insert_with(|| TenantState::new(self.config.quantum as i64));

        if tenant_state.queue.len() >= self.config.max_per_tenant {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return EnqueueResult::Rejected(EnqueueRejectReason::TenantFull);
        }

        let was_empty = tenant_state.queue.is_empty();
        tenant_state.queue.push_back(task);

        if was_empty {
            tenant_state.active = true;
            shard.active_ring.push_back(tenant);
        }

        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        self.stats.queue_len_estimate.fetch_add(1, Ordering::Relaxed);

        drop(shard);

        if was_empty {
            self.work_signal.notify_all();
        }

        EnqueueResult::Enqueued
    }

    pub fn try_dequeue(&self) -> DequeueResult<T> {
        if self.closed.load(Ordering::Acquire) {
            return DequeueResult::Closed;
        }

        for shard_mutex in &self.shards {
            let mut shard = shard_mutex.lock().expect("shard mutex poisoned");
            if shard.active_ring.is_empty() {
                continue;
            }

            // Recorremos como maximo una vuelta completa del ring para evitar loops.
            let ring_len = shard.active_ring.len();
            for _ in 0..ring_len {
                let tenant = match shard.active_ring.pop_front() {
                    Some(tenant) => tenant,
                    None => break,
                };

                let Some(tenant_state) = shard.tenants.get_mut(&tenant) else {
                    continue;
                };

                let mut expired_count = 0u64;
                let now = Instant::now();

                while let Some(front) = tenant_state.queue.front() {
                    match front.deadline {
                        Some(deadline) if now > deadline => {
                            tenant_state.queue.pop_front();
                            expired_count += 1;
                        }
                        _ => break,
                    }
                }

                if expired_count > 0 {
                    self.stats
                        .expired
                        .fetch_add(expired_count, Ordering::Relaxed);
                    self.dec_queue_len_estimate(expired_count);
                }

                if tenant_state.queue.is_empty() {
                    tenant_state.active = false;
                    continue;
                }

                let front_cost = tenant_state
                    .queue
                    .front()
                    .map(|task| task.cost)
                    .unwrap_or(0);
                let cost = if front_cost > i64::MAX as u64 {
                    i64::MAX
                } else {
                    front_cost as i64
                };

                if tenant_state.deficit < cost {
                    tenant_state.deficit += tenant_state.quantum;
                    shard.active_ring.push_back(tenant);
                    continue;
                }

                tenant_state.deficit -= cost;
                let task = tenant_state
                    .queue
                    .pop_front()
                    .expect("tenant queue not empty");

                if tenant_state.queue.is_empty() {
                    tenant_state.active = false;
                } else {
                    shard.active_ring.push_back(tenant);
                }

                self.stats.dequeued.fetch_add(1, Ordering::Relaxed);
                self.dec_queue_len_estimate(1);

                let queue_time_ns = now
                    .duration_since(task.enqueue_ts)
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                self.stats
                    .queue_time_sum_ns
                    .fetch_add(queue_time_ns, Ordering::Relaxed);
                self.stats
                    .queue_time_samples
                    .fetch_add(1, Ordering::Relaxed);

                return DequeueResult::Task { tenant, task };
            }
        }

        if self.closed.load(Ordering::Acquire) {
            DequeueResult::Closed
        } else {
            DequeueResult::Empty
        }
    }

    pub fn dequeue_blocking(&self) -> DequeueResult<T> {
        loop {
            let observed = self.work_signal.current();
            match self.try_dequeue() {
                DequeueResult::Task { tenant, task } => {
                    return DequeueResult::Task { tenant, task }
                }
                DequeueResult::Closed => return DequeueResult::Closed,
                DequeueResult::Empty => {
                    if self.closed.load(Ordering::Acquire) {
                        return DequeueResult::Closed;
                    }

                    if self
                        .stats
                        .queue_len_estimate
                        .load(Ordering::Acquire)
                        > 0
                    {
                        // Hay trabajo pendiente pero aun no elegible (e.g. DRR); reintentar.
                        continue;
                    }

                    self.work_signal.wait_for_change(observed);
                }
            }
        }
    }

    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            enqueued: self.stats.enqueued.load(Ordering::Relaxed),
            dequeued: self.stats.dequeued.load(Ordering::Relaxed),
            expired: self.stats.expired.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            queue_len_estimate: self.stats.queue_len_estimate.load(Ordering::Relaxed),
            queue_time_sum_ns: self.stats.queue_time_sum_ns.load(Ordering::Relaxed),
            queue_time_samples: self.stats.queue_time_samples.load(Ordering::Relaxed),
        }
    }

    pub fn close(&self) {
        let was_closed = self.closed.swap(true, Ordering::Release);
        if !was_closed {
            self.work_signal.notify_all();
        }
    }
}

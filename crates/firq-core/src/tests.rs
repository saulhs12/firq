use std::time::{Duration, Instant};

use crate::{
    BackpressurePolicy, DequeueResult, EnqueueRejectReason, EnqueueResult, Scheduler,
    SchedulerConfig, Task, TenantKey,
};

fn config(max_global: usize, max_per_tenant: usize) -> SchedulerConfig {
    SchedulerConfig {
        shards: 1,
        max_global,
        max_per_tenant,
        quantum: 1,
        backpressure: BackpressurePolicy::Reject,
    }
}

fn task(payload: u64, deadline: Option<Instant>) -> Task<u64> {
    Task {
        payload,
        enqueue_ts: Instant::now(),
        deadline,
        cost: 1,
    }
}

#[test]
fn drops_expired_tasks() {
    let scheduler = Scheduler::new(config(10, 10));
    let tenant = TenantKey::from(1);
    let expired = Instant::now() - Duration::from_secs(1);
    assert!(matches!(
        scheduler.enqueue(tenant, task(1, Some(expired))),
        EnqueueResult::Enqueued
    ));

    let result = scheduler.try_dequeue();
    assert!(matches!(result, DequeueResult::Empty));

    let stats = scheduler.stats();
    assert_eq!(stats.expired, 1);
    assert_eq!(stats.dequeued, 0);
    assert_eq!(stats.queue_len_estimate, 0);
}

#[test]
fn backpressure_global_rejects() {
    let scheduler = Scheduler::new(config(1, 10));
    let tenant_a = TenantKey::from(1);
    let tenant_b = TenantKey::from(2);

    assert!(matches!(
        scheduler.enqueue(tenant_a, task(1, None)),
        EnqueueResult::Enqueued
    ));
    assert!(matches!(
        scheduler.enqueue(tenant_b, task(2, None)),
        EnqueueResult::Rejected(EnqueueRejectReason::GlobalFull)
    ));
}

#[test]
fn backpressure_per_tenant_rejects() {
    let scheduler = Scheduler::new(config(10, 1));
    let tenant = TenantKey::from(42);

    assert!(matches!(
        scheduler.enqueue(tenant, task(1, None)),
        EnqueueResult::Enqueued
    ));
    assert!(matches!(
        scheduler.enqueue(tenant, task(2, None)),
        EnqueueResult::Rejected(EnqueueRejectReason::TenantFull)
    ));
}

#[test]
fn fairness_allows_cold_tenant_to_progress() {
    let scheduler = Scheduler::new(config(100, 100));
    let hot = TenantKey::from(10);
    let cold = TenantKey::from(20);

    for i in 0..10 {
        let _ = scheduler.enqueue(hot, task(i, None));
    }
    let _ = scheduler.enqueue(cold, task(999, None));

    let mut saw_cold = false;
    for _ in 0..6 {
        match scheduler.try_dequeue() {
            DequeueResult::Task { tenant, .. } if tenant == cold => {
                saw_cold = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_cold, "cold tenant should make progress under DRR");
}

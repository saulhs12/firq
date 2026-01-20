use std::time::{Duration, Instant};

use proptest::prelude::*;

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

#[derive(Clone, Debug)]
enum Op {
    Enqueue { tenant: u8, expired: bool },
    Dequeue,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u8..4, any::<bool>()).prop_map(|(tenant, expired)| Op::Enqueue { tenant, expired }),
        Just(Op::Dequeue),
    ]
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

proptest! {
    #[test]
    fn counters_stay_consistent(ops in prop::collection::vec(op_strategy(), 1..200)) {
        let scheduler = Scheduler::new(config(64, 32));

        for op in ops {
            match op {
                Op::Enqueue { tenant, expired } => {
                    let deadline = if expired {
                        Some(Instant::now() - Duration::from_secs(1))
                    } else {
                        None
                    };
                    let _ = scheduler.enqueue(TenantKey::from(tenant as u64), task(1, deadline));
                }
                Op::Dequeue => {
                    let _ = scheduler.try_dequeue();
                }
            }

            let stats = scheduler.stats();
            let enqueued = stats.enqueued as u128;
            let dequeued = stats.dequeued as u128;
            let expired = stats.expired as u128;
            let dropped = stats.dropped as u128;
            let queue_len = stats.queue_len_estimate as u128;

            prop_assert!(dequeued + expired <= enqueued);
            prop_assert_eq!(queue_len + dequeued + expired, enqueued);
            prop_assert!(dequeued + expired + dropped <= enqueued + dropped);
        }
    }
}

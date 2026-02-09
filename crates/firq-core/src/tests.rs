use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::{
    BackpressurePolicy, CancelResult, CloseMode, DequeueResult, EnqueueRejectReason, EnqueueResult,
    EnqueueWithHandleResult, Priority, Scheduler, SchedulerConfig, Task, TenantKey,
};

fn config(max_global: usize, max_per_tenant: usize) -> SchedulerConfig {
    config_with_policy(max_global, max_per_tenant, BackpressurePolicy::Reject)
}

fn config_with_policy(
    max_global: usize,
    max_per_tenant: usize,
    backpressure: BackpressurePolicy,
) -> SchedulerConfig {
    SchedulerConfig {
        shards: 1,
        max_global,
        max_per_tenant,
        quantum: 1,
        quantum_by_tenant: HashMap::new(),
        quantum_provider: None,
        backpressure,
        backpressure_by_tenant: HashMap::new(),
        top_tenants_capacity: 0,
    }
}

fn task(payload: u64, deadline: Option<Instant>) -> Task<u64> {
    Task {
        payload,
        enqueue_ts: Instant::now(),
        deadline,
        priority: Priority::Normal,
        cost: 1,
    }
}

fn task_with_priority(payload: u64, priority: Priority) -> Task<u64> {
    let mut task = task(payload, None);
    task.priority = priority;
    task
}

fn dequeue_task<T>(scheduler: &Scheduler<T>, attempts: usize) -> Option<(TenantKey, Task<T>)> {
    for _ in 0..attempts {
        match scheduler.try_dequeue() {
            DequeueResult::Task { tenant, task } => return Some((tenant, task)),
            DequeueResult::Closed => return None,
            DequeueResult::Empty => {}
        }
    }
    None
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
        if let Some((tenant, _)) = dequeue_task(&scheduler, 3)
            && tenant == cold
        {
            saw_cold = true;
            break;
        }
    }

    assert!(saw_cold, "cold tenant should make progress under DRR");
}

#[test]
fn drop_oldest_per_tenant_keeps_newest() {
    let scheduler = Scheduler::new(config_with_policy(
        10,
        2,
        BackpressurePolicy::DropOldestPerTenant,
    ));
    let tenant = TenantKey::from(1);

    let _ = scheduler.enqueue(tenant, task(1, None));
    let _ = scheduler.enqueue(tenant, task(2, None));
    let result = scheduler.enqueue(tenant, task(3, None));
    assert!(matches!(result, EnqueueResult::Enqueued));

    let stats = scheduler.stats();
    assert_eq!(stats.queue_len_estimate, 2);
    assert_eq!(stats.dropped, 1);

    let first = dequeue_task(&scheduler, 4);
    let second = dequeue_task(&scheduler, 4);
    match (first, second) {
        (Some((_, t1)), Some((_, t2))) => {
            assert_eq!(t1.payload, 2);
            assert_eq!(t2.payload, 3);
        }
        other => panic!("expected two tasks, got {:?}", other),
    }
}

#[test]
fn drop_newest_per_tenant_keeps_oldest() {
    let scheduler = Scheduler::new(config_with_policy(
        10,
        2,
        BackpressurePolicy::DropNewestPerTenant,
    ));
    let tenant = TenantKey::from(1);

    let _ = scheduler.enqueue(tenant, task(1, None));
    let _ = scheduler.enqueue(tenant, task(2, None));
    let result = scheduler.enqueue(tenant, task(3, None));
    assert!(matches!(result, EnqueueResult::Enqueued));

    let stats = scheduler.stats();
    assert_eq!(stats.queue_len_estimate, 2);
    assert_eq!(stats.dropped, 1);

    let first = dequeue_task(&scheduler, 4);
    let second = dequeue_task(&scheduler, 4);
    match (first, second) {
        (Some((_, t1)), Some((_, t2))) => {
            assert_eq!(t1.payload, 1);
            assert_eq!(t2.payload, 3);
        }
        other => panic!("expected two tasks, got {:?}", other),
    }
}

#[test]
fn high_priority_is_dequeued_first() {
    let scheduler = Scheduler::new(config_with_policy(10, 10, BackpressurePolicy::Reject));
    let low_tenant = TenantKey::from(1);
    let high_tenant = TenantKey::from(2);

    let _ = scheduler.enqueue(low_tenant, task_with_priority(1, Priority::Low));
    let _ = scheduler.enqueue(high_tenant, task_with_priority(2, Priority::High));

    let result = dequeue_task(&scheduler, 4);
    match result {
        Some((tenant, task)) => {
            assert_eq!(tenant, high_tenant);
            assert_eq!(task.payload, 2);
        }
        other => panic!("expected high priority task, got {:?}", other),
    }
}

#[test]
fn shed_low_priority_under_pressure() {
    let scheduler = Scheduler::new(config_with_policy(
        10,
        1,
        BackpressurePolicy::DropOldestPerTenant,
    ));
    let tenant = TenantKey::from(1);

    let _ = scheduler.enqueue(tenant, task_with_priority(1, Priority::Low));
    let _ = scheduler.enqueue(tenant, task_with_priority(2, Priority::High));

    let result = dequeue_task(&scheduler, 4);
    match result {
        Some((_, task)) => assert_eq!(task.payload, 2),
        other => panic!("expected high priority task, got {:?}", other),
    }

    let stats = scheduler.stats();
    assert_eq!(stats.dropped, 1);
}

#[test]
fn low_priority_does_not_evict_high() {
    let scheduler = Scheduler::new(config_with_policy(
        10,
        1,
        BackpressurePolicy::DropOldestPerTenant,
    ));
    let tenant = TenantKey::from(1);

    let _ = scheduler.enqueue(tenant, task_with_priority(1, Priority::High));
    let result = scheduler.enqueue(tenant, task_with_priority(2, Priority::Low));
    assert!(matches!(
        result,
        EnqueueResult::Rejected(EnqueueRejectReason::TenantFull)
    ));
}

#[test]
fn timeout_waits_for_capacity() {
    let scheduler = Arc::new(Scheduler::new(config_with_policy(
        1,
        1,
        BackpressurePolicy::Timeout {
            wait: Duration::from_millis(200),
        },
    )));
    let tenant = TenantKey::from(1);

    let _ = scheduler.enqueue(tenant, task(1, None));

    let (tx, rx) = mpsc::channel();
    let scheduler_clone = Arc::clone(&scheduler);
    thread::spawn(move || {
        let result = scheduler_clone.enqueue(tenant, task(2, None));
        let _ = tx.send(result);
    });

    thread::sleep(Duration::from_millis(40));
    assert!(rx.try_recv().is_err(), "enqueue should be waiting");

    assert!(dequeue_task(&scheduler, 4).is_some());

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("enqueue should complete after capacity frees");
    assert!(matches!(result, EnqueueResult::Enqueued));
}

#[test]
fn backpressure_override_per_tenant() {
    let tenant_drop = TenantKey::from(1);
    let tenant_reject = TenantKey::from(2);
    let mut overrides = HashMap::new();
    overrides.insert(tenant_drop, BackpressurePolicy::DropOldestPerTenant);

    let scheduler = Scheduler::new(SchedulerConfig {
        shards: 1,
        max_global: 10,
        max_per_tenant: 1,
        quantum: 1,
        quantum_by_tenant: HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: overrides,
        top_tenants_capacity: 0,
    });

    let _ = scheduler.enqueue(tenant_drop, task(1, None));
    let result = scheduler.enqueue(tenant_drop, task(2, None));
    assert!(matches!(result, EnqueueResult::Enqueued));

    let _ = scheduler.enqueue(tenant_reject, task(10, None));
    let result = scheduler.enqueue(tenant_reject, task(11, None));
    assert!(matches!(
        result,
        EnqueueResult::Rejected(EnqueueRejectReason::TenantFull)
    ));
}

#[test]
fn set_tenant_quantum_affects_scheduling() {
    let scheduler = Scheduler::new(config_with_policy(10, 10, BackpressurePolicy::Reject));
    let heavy = TenantKey::from(1);
    let light = TenantKey::from(2);

    let mut heavy_task = task(99, None);
    heavy_task.cost = 5;
    let _ = scheduler.enqueue(heavy, heavy_task);
    let _ = scheduler.enqueue(light, task(1, None));

    scheduler.set_tenant_quantum(heavy, 5);

    let mut saw_heavy = false;
    for _ in 0..2 {
        if let Some((tenant, _)) = dequeue_task(&scheduler, 2)
            && tenant == heavy
        {
            saw_heavy = true;
            break;
        }
    }

    assert!(
        saw_heavy,
        "tenant quantum update should speed up heavy task"
    );
}

#[test]
fn enqueue_wakes_blocking_dequeue() {
    let scheduler = Arc::new(Scheduler::<u64>::new(config(10, 10)));
    let tenant = TenantKey::from(1);
    let (tx, rx) = mpsc::channel();
    let scheduler_clone = Arc::clone(&scheduler);

    thread::spawn(move || {
        let result = scheduler_clone.dequeue_blocking();
        let _ = tx.send(result);
    });

    thread::sleep(Duration::from_millis(20));
    let _ = scheduler.enqueue(tenant, task(42, None));

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dequeue_blocking should be notified");

    match result {
        DequeueResult::Task { tenant: got, .. } => assert_eq!(got, tenant),
        other => panic!("expected task after enqueue, got {:?}", other),
    }
}

#[test]
fn close_wakes_blocking_dequeue() {
    let scheduler = Arc::new(Scheduler::<u64>::new(config(10, 10)));
    let (tx, rx) = mpsc::channel();
    let scheduler_clone = Arc::clone(&scheduler);

    thread::spawn(move || {
        let result = scheduler_clone.dequeue_blocking();
        let _ = tx.send(result);
    });

    thread::sleep(Duration::from_millis(20));
    scheduler.close();

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dequeue_blocking should return after close");
    assert!(matches!(result, DequeueResult::Closed));
}

#[test]
fn no_starvation_with_large_costs() {
    let scheduler = Scheduler::new(config(1_000, 1_000));
    let hot = TenantKey::from(10);
    let heavy = TenantKey::from(20);

    for i in 0..50 {
        let mut task = task(i, None);
        task.cost = 1;
        let _ = scheduler.enqueue(hot, task);
    }

    let mut heavy_task = task(9_999, None);
    heavy_task.cost = 10;
    let _ = scheduler.enqueue(heavy, heavy_task);

    let mut saw_heavy = false;
    for _ in 0..60 {
        if let Some((tenant, _)) = dequeue_task(&scheduler, 3)
            && tenant == heavy
        {
            saw_heavy = true;
            break;
        }
    }

    assert!(
        saw_heavy,
        "heavy tenant should not starve even with small quantum"
    );
}

#[test]
fn close_drain_drains_pending_work() {
    let scheduler = Scheduler::new(config(10, 10));
    let tenant = TenantKey::from(7);

    for payload in 0..3 {
        let result = scheduler.enqueue(tenant, task(payload, None));
        assert!(matches!(result, EnqueueResult::Enqueued));
    }

    scheduler.close_with_mode(CloseMode::Drain);
    assert!(matches!(
        scheduler.enqueue(tenant, task(99, None)),
        EnqueueResult::Closed
    ));

    for _ in 0..3 {
        assert!(
            dequeue_task(&scheduler, 3).is_some(),
            "drain mode should still deliver pending work"
        );
    }
    assert!(matches!(scheduler.try_dequeue(), DequeueResult::Closed));
}

#[test]
fn cancel_handle_releases_capacity() {
    let scheduler = Scheduler::new(config(1, 10));
    let tenant = TenantKey::from(1);

    let handle = match scheduler.enqueue_with_handle(tenant, task(1, None)) {
        EnqueueWithHandleResult::Enqueued(handle) => handle,
        other => panic!("expected handle, got {:?}", other),
    };

    let second = scheduler.enqueue(tenant, task(2, None));
    assert!(matches!(
        second,
        EnqueueResult::Rejected(EnqueueRejectReason::GlobalFull)
    ));

    assert!(matches!(scheduler.cancel(handle), CancelResult::Cancelled));

    let third = scheduler.enqueue(tenant, task(3, None));
    assert!(matches!(third, EnqueueResult::Enqueued));
}

#[test]
fn global_capacity_is_strict_under_race() {
    let scheduler = Arc::new(Scheduler::new(config(3, 100)));
    let tenants = [TenantKey::from(1), TenantKey::from(2), TenantKey::from(3)];
    let start = Arc::new(std::sync::Barrier::new(tenants.len()));
    let mut threads = Vec::new();

    for tenant in tenants {
        let scheduler = Arc::clone(&scheduler);
        let start = Arc::clone(&start);
        threads.push(thread::spawn(move || {
            start.wait();
            for i in 0..5_000 {
                let _ = scheduler.enqueue(tenant, task(i, None));
            }
        }));
    }

    for thread in threads {
        thread.join().expect("producer thread should finish");
    }

    let stats = scheduler.stats();
    assert!(
        stats.queue_len_estimate <= 3,
        "queue_len_estimate must not exceed max_global"
    );
}

#[test]
fn multi_shard_reactivation_makes_work_visible() {
    let scheduler = Scheduler::new(SchedulerConfig {
        shards: 4,
        max_global: 100,
        max_per_tenant: 100,
        quantum: 1,
        quantum_by_tenant: HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: HashMap::new(),
        top_tenants_capacity: 0,
    });

    let tenant_a = TenantKey::from(10);
    let tenant_b = TenantKey::from(11);

    assert!(matches!(
        scheduler.enqueue(tenant_a, task(1, None)),
        EnqueueResult::Enqueued
    ));
    assert!(dequeue_task(&scheduler, 3).is_some());
    assert!(matches!(scheduler.try_dequeue(), DequeueResult::Empty));

    assert!(matches!(
        scheduler.enqueue(tenant_b, task(2, None)),
        EnqueueResult::Enqueued
    ));
    assert!(
        dequeue_task(&scheduler, 4).is_some(),
        "work must become visible after shard reactivation"
    );
}

#[test]
fn high_cost_low_quantum_with_expirations_does_not_livelock() {
    let scheduler = Scheduler::new(config(1_000, 1_000));
    let hot = TenantKey::from(1);
    let heavy = TenantKey::from(2);

    for i in 0..200 {
        let mut t = task(i, Some(Instant::now() - Duration::from_millis(1)));
        t.cost = 1;
        let _ = scheduler.enqueue(hot, t);
    }

    for i in 0..100 {
        let mut t = task(1_000 + i, None);
        t.cost = 1;
        let _ = scheduler.enqueue(hot, t);
    }

    let mut heavy_task = task(9_999, None);
    heavy_task.cost = 50;
    let _ = scheduler.enqueue(heavy, heavy_task);

    let mut saw_heavy = false;
    for _ in 0..1_000 {
        match scheduler.try_dequeue() {
            DequeueResult::Task { tenant, .. } if tenant == heavy => {
                saw_heavy = true;
                break;
            }
            DequeueResult::Task { .. } | DequeueResult::Empty => {}
            DequeueResult::Closed => break,
        }
    }
    assert!(saw_heavy, "heavy task must eventually make progress");
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

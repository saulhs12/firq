use firq_async::{
    AsyncScheduler, BackpressurePolicy, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task,
    TenantKey,
};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(SchedulerConfig {
        shards: 2,
        max_global: 10_000,
        max_per_tenant: 2_000,
        quantum: 2,
        quantum_by_tenant: HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: HashMap::new(),
        top_tenants_capacity: 32,
    })));

    let running = Arc::new(AtomicBool::new(true));
    let produced = Arc::new(AtomicU64::new(0));
    let served = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));

    let mut producer_handles = Vec::new();
    for (tenant, interval_ms) in [(TenantKey::from(1), 5u64), (TenantKey::from(2), 30u64)] {
        let scheduler = scheduler.clone();
        let running = Arc::clone(&running);
        let produced = Arc::clone(&produced);
        let dropped = Arc::clone(&dropped);
        producer_handles.push(tokio::spawn(async move {
            let mut seq = 0u64;
            while running.load(Ordering::Relaxed) {
                seq = seq.saturating_add(1);
                produced.fetch_add(1, Ordering::Relaxed);
                let task = Task {
                    payload: seq,
                    enqueue_ts: Instant::now(),
                    deadline: None,
                    priority: Priority::Normal,
                    cost: 1,
                };
                if matches!(scheduler.enqueue(tenant, task), EnqueueResult::Rejected(_)) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        }));
    }

    let mut receiver = scheduler.receiver_with_worker(512);
    let served_consumer = Arc::clone(&served);
    let consumer = tokio::spawn(async move {
        while let Some(_item) = receiver.recv().await {
            served_consumer.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(12)).await;
        }
    });

    let run_for = Duration::from_secs(5);
    println!(
        "running worker-backed async consumer for {:.1}s...",
        run_for.as_secs_f64()
    );
    tokio::time::sleep(run_for).await;

    running.store(false, Ordering::Relaxed);
    scheduler.close();

    for handle in producer_handles {
        let _ = handle.await;
    }
    let _ = consumer.await;

    let stats = scheduler.stats();
    println!(
        "produced={} served={} dropped={} enq={} deq={} qlen={}",
        produced.load(Ordering::Relaxed),
        served.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        stats.enqueued,
        stats.dequeued,
        stats.queue_len_estimate
    );
}

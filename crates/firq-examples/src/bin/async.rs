use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use firq_async::{
    AsyncScheduler, BackpressurePolicy, DequeueItem, Dispatcher, EnqueueResult, Priority,
    Scheduler, SchedulerConfig, Task, TenantKey,
};

#[derive(Clone, Debug)]
struct TenantConfig {
    key: TenantKey,
    name: &'static str,
    interval_ms: u64,
    cost: u64,
}

fn counter_vec(size: usize) -> Vec<AtomicU64> {
    let mut values = Vec::with_capacity(size);
    for _ in 0..size {
        values.push(AtomicU64::new(0));
    }
    values
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let scheduler = Arc::new(Scheduler::new(SchedulerConfig {
        shards: 1,
        max_global: 10_000,
        max_per_tenant: 5_000,
        quantum: 2,
        quantum_by_tenant: HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: HashMap::new(),
        top_tenants_capacity: 64,
    }));
    let async_scheduler = AsyncScheduler::new(Arc::clone(&scheduler));

    let tenants = [
        TenantConfig {
            key: TenantKey::from(1),
            name: "hot",
            interval_ms: 10,
            cost: 1,
        },
        TenantConfig {
            key: TenantKey::from(2),
            name: "cold",
            interval_ms: 80,
            cost: 1,
        },
    ];

    let mut tenant_index = HashMap::new();
    for (idx, tenant) in tenants.iter().enumerate() {
        tenant_index.insert(tenant.key, idx);
    }
    let tenant_index = Arc::new(tenant_index);

    let produced = Arc::new(counter_vec(tenants.len()));
    let served = Arc::new(counter_vec(tenants.len()));
    let dropped = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));

    let mut handles = Vec::new();

    for (idx, tenant) in tenants.iter().enumerate() {
        let tenant = tenant.clone();
        let scheduler = async_scheduler.clone();
        let produced = Arc::clone(&produced);
        let dropped = Arc::clone(&dropped);
        let running = Arc::clone(&running);
        handles.push(tokio::spawn(async move {
            let mut seq = 0u64;
            while running.load(Ordering::Relaxed) {
                seq += 1;
                let task = Task {
                    payload: seq,
                    enqueue_ts: Instant::now(),
                    deadline: None,
                    priority: Priority::Normal,
                    cost: tenant.cost,
                };
                produced[idx].fetch_add(1, Ordering::Relaxed);
                match scheduler.enqueue(tenant.key, task) {
                    EnqueueResult::Enqueued => {}
                    EnqueueResult::Rejected(_) => {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    EnqueueResult::Closed => break,
                }
                tokio::time::sleep(Duration::from_millis(tenant.interval_ms)).await;
            }
        }));
    }

    let worker_count = 2;
    let dispatcher = Dispatcher::new(async_scheduler.clone(), worker_count);
    let served_dispatch = Arc::clone(&served);
    let tenant_index = Arc::clone(&tenant_index);
    handles.push(tokio::spawn(async move {
        dispatcher
            .run(move |item: DequeueItem<u64>| {
                let served = Arc::clone(&served_dispatch);
                let tenant_index = Arc::clone(&tenant_index);
                async move {
                    if let Some(index) = tenant_index.get(&item.tenant) {
                        served[*index].fetch_add(1, Ordering::Relaxed);
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
            })
            .await;
    }));

    let run_seconds = 6u64;
    println!("running async example for {}s...", run_seconds);
    tokio::time::sleep(Duration::from_secs(run_seconds)).await;

    running.store(false, Ordering::Relaxed);
    scheduler.close();

    for handle in handles {
        let _ = handle.await;
    }

    let stats = scheduler.stats();
    println!(
        "final stats: enq={} deq={} dropped={} expired={} qlen={}",
        stats.enqueued, stats.dequeued, stats.dropped, stats.expired, stats.queue_len_estimate
    );

    for (idx, tenant) in tenants.iter().enumerate() {
        let p = produced[idx].load(Ordering::Relaxed);
        let s = served[idx].load(Ordering::Relaxed);
        println!("tenant={} produced={} served={}", tenant.name, p, s);
    }
    println!("dropped_total={}", dropped.load(Ordering::Relaxed));
}

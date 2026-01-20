use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use firq_core::{BackpressurePolicy, EnqueueResult, Scheduler, SchedulerConfig, Task, TenantKey};

fn main() {
    let run_seconds = 5u64;
    let worker_count = 4usize;
    let cold_tenants = 40u64;

    let scheduler = Arc::new(Scheduler::new(SchedulerConfig {
        shards: 1,
        max_global: 50_000,
        max_per_tenant: 10_000,
        quantum: 1,
        backpressure: BackpressurePolicy::Reject,
    }));

    let running = Arc::new(AtomicBool::new(true));
    let produced_total = Arc::new(AtomicU64::new(0));
    let dropped_total = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    let hot_tenant = TenantKey::from(1);
    handles.push(spawn_producer(
        Arc::clone(&scheduler),
        Arc::clone(&running),
        Arc::clone(&produced_total),
        Arc::clone(&dropped_total),
        hot_tenant,
        1,
    ));

    for tenant_id in 2..(cold_tenants + 2) {
        let tenant = TenantKey::from(tenant_id);
        handles.push(spawn_producer(
            Arc::clone(&scheduler),
            Arc::clone(&running),
            Arc::clone(&produced_total),
            Arc::clone(&dropped_total),
            tenant,
            25,
        ));
    }

    for _ in 0..worker_count {
        handles.push(spawn_worker(Arc::clone(&scheduler), Arc::clone(&running)));
    }

    println!(
        "bench: hot tenant vs {} tenants ({} workers, {}s)",
        cold_tenants, worker_count, run_seconds
    );
    let start = Instant::now();
    thread::sleep(Duration::from_secs(run_seconds));
    let elapsed = start.elapsed().as_secs_f64();

    running.store(false, Ordering::Relaxed);
    scheduler.close();

    for handle in handles {
        let _ = handle.join();
    }

    let stats = scheduler.stats();
    let avg_queue_time_ms = if stats.queue_time_samples > 0 {
        let avg_ns = stats.queue_time_sum_ns as f64 / stats.queue_time_samples as f64;
        avg_ns / 1_000_000.0
    } else {
        0.0
    };
    let throughput = if elapsed > 0.0 {
        stats.dequeued as f64 / elapsed
    } else {
        0.0
    };

    println!(
        "stats: enq={} deq={} dropped={} expired={} qlen={}",
        stats.enqueued, stats.dequeued, stats.dropped, stats.expired, stats.queue_len_estimate
    );
    println!(
        "derived: throughput={:.1} ops/s avg_queue_time_ms={:.3}",
        throughput, avg_queue_time_ms
    );
    println!(
        "produced_total={} dropped_total={}",
        produced_total.load(Ordering::Relaxed),
        dropped_total.load(Ordering::Relaxed)
    );
}

fn spawn_producer(
    scheduler: Arc<Scheduler<u64>>,
    running: Arc<AtomicBool>,
    produced_total: Arc<AtomicU64>,
    dropped_total: Arc<AtomicU64>,
    tenant: TenantKey,
    interval_ms: u64,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut seq = 0u64;
        while running.load(Ordering::Relaxed) {
            seq += 1;
            let task = Task {
                payload: seq,
                enqueue_ts: Instant::now(),
                deadline: None,
                cost: 1,
            };
            produced_total.fetch_add(1, Ordering::Relaxed);
            match scheduler.enqueue(tenant, task) {
                EnqueueResult::Enqueued => {}
                EnqueueResult::Rejected(_) => {
                    dropped_total.fetch_add(1, Ordering::Relaxed);
                }
                EnqueueResult::Closed => break,
            }
            if interval_ms > 0 {
                thread::sleep(Duration::from_millis(interval_ms));
            }
        }
    })
}

fn spawn_worker(
    scheduler: Arc<Scheduler<u64>>,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match scheduler.dequeue_blocking() {
                firq_core::DequeueResult::Task { .. } => {}
                firq_core::DequeueResult::Closed => break,
                firq_core::DequeueResult::Empty => {}
            }
        }
    })
}

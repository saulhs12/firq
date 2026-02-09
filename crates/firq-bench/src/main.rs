use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use firq_core::{
    BackpressurePolicy, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task, TenantKey,
};

struct BenchResult {
    enqueued: u64,
    dequeued: u64,
    dropped: u64,
    expired: u64,
    avg_queue_time_ms: f64,
    throughput: f64,
    produced_total: u64,
    dropped_total: u64,
    dequeued_high: u64,
    dequeued_normal: u64,
    dequeued_low: u64,
}

fn main() {
    let run_seconds = 8u64;
    let worker_count = 1usize;
    let cold_tenants = 120u64;
    let hot_interval_ms = 0u64;
    let cold_interval_ms = 5u64;
    let work_ms = 4u64;
    let hot_priority = Priority::High;
    let cold_priority = Priority::Low;

    let firq = run_firq_bench(
        run_seconds,
        worker_count,
        cold_tenants,
        hot_interval_ms,
        cold_interval_ms,
        work_ms,
        hot_priority,
        cold_priority,
    );
    print_result(
        "firq",
        run_seconds,
        worker_count,
        cold_tenants,
        hot_interval_ms,
        cold_interval_ms,
        work_ms,
        hot_priority,
        cold_priority,
        &firq,
    );

    let fifo = run_fifo_bench(
        run_seconds,
        worker_count,
        cold_tenants,
        hot_interval_ms,
        cold_interval_ms,
        work_ms,
        hot_priority,
        cold_priority,
    );
    print_result(
        "fifo",
        run_seconds,
        worker_count,
        cold_tenants,
        hot_interval_ms,
        cold_interval_ms,
        work_ms,
        hot_priority,
        cold_priority,
        &fifo,
    );
}

fn run_firq_bench(
    run_seconds: u64,
    worker_count: usize,
    cold_tenants: u64,
    hot_interval_ms: u64,
    cold_interval_ms: u64,
    work_ms: u64,
    hot_priority: Priority,
    cold_priority: Priority,
) -> BenchResult {
    let scheduler = Arc::new(Scheduler::new(SchedulerConfig {
        shards: 1,
        max_global: 10_000,
        max_per_tenant: 10_000,
        quantum: 1,
        quantum_by_tenant: std::collections::HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: std::collections::HashMap::new(),
        top_tenants_capacity: 64,
    }));

    let running = Arc::new(AtomicBool::new(true));
    let produced_total = Arc::new(AtomicU64::new(0));
    let dropped_total = Arc::new(AtomicU64::new(0));
    let dequeued_high = Arc::new(AtomicU64::new(0));
    let dequeued_normal = Arc::new(AtomicU64::new(0));
    let dequeued_low = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    let hot_tenant = TenantKey::from(1);
    handles.push(spawn_firq_producer(
        Arc::clone(&scheduler),
        Arc::clone(&running),
        Arc::clone(&produced_total),
        Arc::clone(&dropped_total),
        hot_tenant,
        hot_interval_ms,
        hot_priority,
    ));

    for tenant_id in 2..(cold_tenants + 2) {
        let tenant = TenantKey::from(tenant_id);
        handles.push(spawn_firq_producer(
            Arc::clone(&scheduler),
            Arc::clone(&running),
            Arc::clone(&produced_total),
            Arc::clone(&dropped_total),
            tenant,
            cold_interval_ms,
            cold_priority,
        ));
    }

    for _ in 0..worker_count {
        handles.push(spawn_firq_worker(
            Arc::clone(&scheduler),
            Arc::clone(&running),
            work_ms,
            Arc::clone(&dequeued_high),
            Arc::clone(&dequeued_normal),
            Arc::clone(&dequeued_low),
        ));
    }

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

    BenchResult {
        enqueued: stats.enqueued,
        dequeued: stats.dequeued,
        dropped: stats.dropped,
        expired: stats.expired,
        avg_queue_time_ms,
        throughput,
        produced_total: produced_total.load(Ordering::Relaxed),
        dropped_total: dropped_total.load(Ordering::Relaxed),
        dequeued_high: dequeued_high.load(Ordering::Relaxed),
        dequeued_normal: dequeued_normal.load(Ordering::Relaxed),
        dequeued_low: dequeued_low.load(Ordering::Relaxed),
    }
}

fn run_fifo_bench(
    run_seconds: u64,
    worker_count: usize,
    cold_tenants: u64,
    hot_interval_ms: u64,
    cold_interval_ms: u64,
    work_ms: u64,
    hot_priority: Priority,
    cold_priority: Priority,
) -> BenchResult {
    let queue = Arc::new(FifoQueue::new(10_000));
    let running = Arc::new(AtomicBool::new(true));
    let produced_total = Arc::new(AtomicU64::new(0));
    let dropped_total = Arc::new(AtomicU64::new(0));
    let queue_time_sum_ns = Arc::new(AtomicU64::new(0));
    let queue_time_samples = Arc::new(AtomicU64::new(0));
    let dequeued_total = Arc::new(AtomicU64::new(0));
    let dequeued_high = Arc::new(AtomicU64::new(0));
    let dequeued_normal = Arc::new(AtomicU64::new(0));
    let dequeued_low = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    handles.push(spawn_fifo_producer(
        Arc::clone(&queue),
        Arc::clone(&running),
        Arc::clone(&produced_total),
        Arc::clone(&dropped_total),
        hot_interval_ms,
        hot_priority,
    ));

    for _ in 0..cold_tenants {
        handles.push(spawn_fifo_producer(
            Arc::clone(&queue),
            Arc::clone(&running),
            Arc::clone(&produced_total),
            Arc::clone(&dropped_total),
            cold_interval_ms,
            cold_priority,
        ));
    }

    for _ in 0..worker_count {
        handles.push(spawn_fifo_worker(
            Arc::clone(&queue),
            Arc::clone(&running),
            Arc::clone(&queue_time_sum_ns),
            Arc::clone(&queue_time_samples),
            Arc::clone(&dequeued_total),
            work_ms,
            Arc::clone(&dequeued_high),
            Arc::clone(&dequeued_normal),
            Arc::clone(&dequeued_low),
        ));
    }

    let start = Instant::now();
    thread::sleep(Duration::from_secs(run_seconds));
    let elapsed = start.elapsed().as_secs_f64();

    running.store(false, Ordering::Relaxed);
    queue.close();

    for handle in handles {
        let _ = handle.join();
    }

    let avg_queue_time_ms = if queue_time_samples.load(Ordering::Relaxed) > 0 {
        let avg_ns = queue_time_sum_ns.load(Ordering::Relaxed) as f64
            / queue_time_samples.load(Ordering::Relaxed) as f64;
        avg_ns / 1_000_000.0
    } else {
        0.0
    };
    let dequeued = dequeued_total.load(Ordering::Relaxed);
    let throughput = if elapsed > 0.0 {
        dequeued as f64 / elapsed
    } else {
        0.0
    };

    BenchResult {
        enqueued: produced_total.load(Ordering::Relaxed) - dropped_total.load(Ordering::Relaxed),
        dequeued,
        dropped: queue.dropped(),
        expired: 0,
        avg_queue_time_ms,
        throughput,
        produced_total: produced_total.load(Ordering::Relaxed),
        dropped_total: dropped_total.load(Ordering::Relaxed),
        dequeued_high: dequeued_high.load(Ordering::Relaxed),
        dequeued_normal: dequeued_normal.load(Ordering::Relaxed),
        dequeued_low: dequeued_low.load(Ordering::Relaxed),
    }
}

fn print_result(
    label: &str,
    run_seconds: u64,
    worker_count: usize,
    cold_tenants: u64,
    hot_interval_ms: u64,
    cold_interval_ms: u64,
    work_ms: u64,
    hot_priority: Priority,
    cold_priority: Priority,
    result: &BenchResult,
) {
    println!(
        "bench: {label} hot tenant vs {cold_tenants} tenants ({} workers, {}s, hot={}ms cold={}ms work={}ms, hot={:?} cold={:?})",
        worker_count,
        run_seconds,
        hot_interval_ms,
        cold_interval_ms,
        work_ms,
        hot_priority,
        cold_priority
    );
    println!(
        "stats: enq={} deq={} dropped={} expired={}",
        result.enqueued, result.dequeued, result.dropped, result.expired
    );
    println!(
        "derived: throughput={:.1} ops/s avg_queue_time_ms={:.3}",
        result.throughput, result.avg_queue_time_ms
    );
    println!(
        "produced_total={} dropped_total={}",
        result.produced_total, result.dropped_total
    );
    println!(
        "dequeued_by_priority: high={} normal={} low={}",
        result.dequeued_high, result.dequeued_normal, result.dequeued_low
    );
}

fn spawn_firq_producer(
    scheduler: Arc<Scheduler<u64>>,
    running: Arc<AtomicBool>,
    produced_total: Arc<AtomicU64>,
    dropped_total: Arc<AtomicU64>,
    tenant: TenantKey,
    interval_ms: u64,
    priority: Priority,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut seq = 0u64;
        while running.load(Ordering::Relaxed) {
            seq += 1;
            let task = Task {
                payload: seq,
                enqueue_ts: Instant::now(),
                deadline: None,
                priority,
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

fn spawn_firq_worker(
    scheduler: Arc<Scheduler<u64>>,
    running: Arc<AtomicBool>,
    work_ms: u64,
    dequeued_high: Arc<AtomicU64>,
    dequeued_normal: Arc<AtomicU64>,
    dequeued_low: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match scheduler.dequeue_blocking() {
                firq_core::DequeueResult::Task { task, .. } => {
                    match task.priority {
                        Priority::High => dequeued_high.fetch_add(1, Ordering::Relaxed),
                        Priority::Normal => dequeued_normal.fetch_add(1, Ordering::Relaxed),
                        Priority::Low => dequeued_low.fetch_add(1, Ordering::Relaxed),
                    };
                    if work_ms > 0 {
                        thread::sleep(Duration::from_millis(work_ms));
                    }
                }
                firq_core::DequeueResult::Closed => break,
                firq_core::DequeueResult::Empty => {}
            }
        }
    })
}

fn spawn_fifo_producer(
    queue: Arc<FifoQueue<u64>>,
    running: Arc<AtomicBool>,
    produced_total: Arc<AtomicU64>,
    dropped_total: Arc<AtomicU64>,
    interval_ms: u64,
    priority: Priority,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut seq = 0u64;
        while running.load(Ordering::Relaxed) {
            seq += 1;
            let task = Task {
                payload: seq,
                enqueue_ts: Instant::now(),
                deadline: None,
                priority,
                cost: 1,
            };
            produced_total.fetch_add(1, Ordering::Relaxed);
            if !queue.enqueue(task) {
                dropped_total.fetch_add(1, Ordering::Relaxed);
            }
            if interval_ms > 0 {
                thread::sleep(Duration::from_millis(interval_ms));
            }
        }
    })
}

fn spawn_fifo_worker(
    queue: Arc<FifoQueue<u64>>,
    running: Arc<AtomicBool>,
    queue_time_sum_ns: Arc<AtomicU64>,
    queue_time_samples: Arc<AtomicU64>,
    dequeued_total: Arc<AtomicU64>,
    work_ms: u64,
    dequeued_high: Arc<AtomicU64>,
    dequeued_normal: Arc<AtomicU64>,
    dequeued_low: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match queue.dequeue_blocking() {
                Some(task) => {
                    dequeued_total.fetch_add(1, Ordering::Relaxed);
                    match task.priority {
                        Priority::High => dequeued_high.fetch_add(1, Ordering::Relaxed),
                        Priority::Normal => dequeued_normal.fetch_add(1, Ordering::Relaxed),
                        Priority::Low => dequeued_low.fetch_add(1, Ordering::Relaxed),
                    };
                    let queue_time_ns = task
                        .enqueue_ts
                        .elapsed()
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64;
                    queue_time_sum_ns.fetch_add(queue_time_ns, Ordering::Relaxed);
                    queue_time_samples.fetch_add(1, Ordering::Relaxed);
                    if work_ms > 0 {
                        thread::sleep(Duration::from_millis(work_ms));
                    }
                }
                None => break,
            }
        }
    })
}

struct FifoQueue<T> {
    queue: Mutex<VecDeque<Task<T>>>,
    condvar: Condvar,
    closed: AtomicBool,
    max_global: usize,
    dropped: AtomicU64,
}

impl<T> FifoQueue<T> {
    fn new(max_global: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
            closed: AtomicBool::new(false),
            max_global,
            dropped: AtomicU64::new(0),
        }
    }

    fn enqueue(&self, task: Task<T>) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut guard = self.queue.lock().expect("fifo queue mutex poisoned");
        if guard.len() >= self.max_global {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        guard.push_back(task);
        drop(guard);
        self.condvar.notify_one();
        true
    }

    fn dequeue_blocking(&self) -> Option<Task<T>> {
        let mut guard = self.queue.lock().expect("fifo queue mutex poisoned");
        loop {
            if let Some(task) = guard.pop_front() {
                return Some(task);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            guard = self
                .condvar
                .wait(guard)
                .expect("fifo queue condvar poisoned");
        }
    }

    fn close(&self) {
        let was_closed = self.closed.swap(true, Ordering::Release);
        if !was_closed {
            self.condvar.notify_all();
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use firq_core::{
    BackpressurePolicy, DequeueResult, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task,
    TenantKey,
};

const QUEUE_TIME_BUCKETS_NS: [u64; 16] = [
    10_000,
    50_000,
    100_000,
    500_000,
    1_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
    5_000_000_000,
    10_000_000_000,
    30_000_000_000,
    60_000_000_000,
    120_000_000_000,
];

#[derive(Clone, Debug)]
struct BenchConfig {
    default_run_seconds: u64,
    worker_count: usize,
    max_global: usize,
    max_per_tenant: usize,
    quantum: u64,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            default_run_seconds: 8,
            worker_count: 2,
            max_global: 10_000,
            max_per_tenant: 1_000,
            quantum: 2,
        }
    }
}

#[derive(Clone, Debug)]
struct ProducerProfile {
    tenant: TenantKey,
    class: ProducerClass,
    interval_ms: u64,
    priority: Priority,
    cost: u64,
    deadline_ms: Option<u64>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProducerClass {
    Hot,
    Cold,
}

#[derive(Clone, Debug)]
struct Scenario {
    name: &'static str,
    description: &'static str,
    run_seconds: Option<u64>,
    work_ms: u64,
    max_global: Option<usize>,
    max_per_tenant: Option<usize>,
    producers: Vec<ProducerProfile>,
    report_cold_tail: bool,
}

#[derive(Clone, Debug)]
struct BenchResult {
    enqueued: u64,
    dequeued: u64,
    dropped: u64,
    expired: u64,
    produced_total: u64,
    throughput_ops: f64,
    drop_rate: f64,
    expired_rate: f64,
    p50_queue_ms: f64,
    p95_queue_ms: f64,
    p99_queue_ms: f64,
    cold_p99_queue_ms: f64,
}

#[derive(Clone, Debug)]
struct BenchPayload {
    class: ProducerClass,
}

#[derive(Debug)]
struct SharedCounters {
    produced_total: AtomicU64,
    dropped_total: AtomicU64,
}

impl SharedCounters {
    fn new() -> Self {
        Self {
            produced_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
struct ConcurrentHistogram {
    buckets: Vec<AtomicU64>,
}

impl ConcurrentHistogram {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(QUEUE_TIME_BUCKETS_NS.len());
        for _ in 0..QUEUE_TIME_BUCKETS_NS.len() {
            buckets.push(AtomicU64::new(0));
        }
        Self { buckets }
    }

    fn observe(&self, value_ns: u64) {
        for (idx, bound) in QUEUE_TIME_BUCKETS_NS.iter().enumerate() {
            if value_ns <= *bound {
                self.buckets[idx].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    fn percentile_ns(&self, pct: f64) -> u64 {
        let counts = self
            .buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect::<Vec<_>>();
        percentile_from_counts(&counts, pct)
    }
}

fn main() {
    let config = BenchConfig::default();
    let scenarios = scenarios();

    for scenario in scenarios {
        let firq = run_firq(&config, &scenario);
        let fifo = run_fifo(&config, &scenario);
        print_comparison(&config, &scenario, &firq, &fifo);
    }
}

fn scenarios() -> Vec<Scenario> {
    vec![
        hot_tenant_sustained(),
        burst_massive(),
        mixed_priorities(),
        deadline_expiration(),
        capacity_pressure(),
    ]
}

fn hot_tenant_sustained() -> Scenario {
    let mut producers = Vec::new();
    producers.push(ProducerProfile {
        tenant: TenantKey::from(1),
        class: ProducerClass::Hot,
        interval_ms: 0,
        priority: Priority::High,
        cost: 1,
        deadline_ms: None,
    });

    for tenant_id in 2..22 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 15,
            priority: Priority::Low,
            cost: 1,
            deadline_ms: None,
        });
    }

    Scenario {
        name: "hot_tenant_sustained",
        description: "Un tenant hot sostenido compite contra muchos tenants cold",
        run_seconds: Some(6),
        work_ms: 1,
        max_global: Some(50_000),
        max_per_tenant: Some(1_000),
        producers,
        report_cold_tail: true,
    }
}

fn burst_massive() -> Scenario {
    let mut producers = Vec::new();
    for tenant_id in 1..161 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 0,
            priority: Priority::Normal,
            cost: 1,
            deadline_ms: None,
        });
    }

    Scenario {
        name: "burst_massive",
        description: "Burst masivo de productores simultáneos",
        run_seconds: Some(4),
        work_ms: 2,
        max_global: Some(6_000),
        max_per_tenant: Some(200),
        producers,
        report_cold_tail: false,
    }
}

fn mixed_priorities() -> Scenario {
    let mut producers = Vec::new();

    for tenant_id in 1..21 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Hot,
            interval_ms: 2,
            priority: Priority::High,
            cost: 1,
            deadline_ms: None,
        });
    }

    for tenant_id in 21..81 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 4,
            priority: Priority::Normal,
            cost: 1,
            deadline_ms: None,
        });
    }

    for tenant_id in 81..141 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 7,
            priority: Priority::Low,
            cost: 1,
            deadline_ms: None,
        });
    }

    Scenario {
        name: "mixed_priorities",
        description: "Mezcla de prioridades High/Normal/Low bajo carga",
        run_seconds: None,
        work_ms: 2,
        max_global: None,
        max_per_tenant: None,
        producers,
        report_cold_tail: false,
    }
}

fn deadline_expiration() -> Scenario {
    let mut producers = Vec::new();
    for tenant_id in 1..81 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 1,
            priority: Priority::Normal,
            cost: 1,
            deadline_ms: Some(5),
        });
    }

    Scenario {
        name: "deadline_expiration",
        description: "Deadlines cortos para forzar expiraciones en dequeue",
        run_seconds: Some(5),
        work_ms: 6,
        max_global: Some(4_000),
        max_per_tenant: Some(128),
        producers,
        report_cold_tail: false,
    }
}

fn capacity_pressure() -> Scenario {
    let mut producers = Vec::new();
    for tenant_id in 1..121 {
        producers.push(ProducerProfile {
            tenant: TenantKey::from(tenant_id),
            class: ProducerClass::Cold,
            interval_ms: 0,
            priority: Priority::Normal,
            cost: 1,
            deadline_ms: None,
        });
    }

    Scenario {
        name: "capacity_pressure",
        description: "Presión de capacidad global y por tenant",
        run_seconds: Some(5),
        work_ms: 3,
        max_global: Some(2_000),
        max_per_tenant: Some(40),
        producers,
        report_cold_tail: false,
    }
}

fn run_firq(config: &BenchConfig, scenario: &Scenario) -> BenchResult {
    let run_seconds = scenario.run_seconds.unwrap_or(config.default_run_seconds);
    let scheduler = Arc::new(Scheduler::<BenchPayload>::new(SchedulerConfig {
        shards: 4,
        max_global: scenario.max_global.unwrap_or(config.max_global),
        max_per_tenant: scenario.max_per_tenant.unwrap_or(config.max_per_tenant),
        quantum: config.quantum,
        quantum_by_tenant: std::collections::HashMap::new(),
        quantum_provider: None,
        backpressure: BackpressurePolicy::Reject,
        backpressure_by_tenant: std::collections::HashMap::new(),
        top_tenants_capacity: 64,
    }));

    let running = Arc::new(AtomicBool::new(true));
    let counters = Arc::new(SharedCounters::new());
    let histogram = Arc::new(ConcurrentHistogram::new());
    let cold_histogram = Arc::new(ConcurrentHistogram::new());
    let mut handles = Vec::new();

    for profile in &scenario.producers {
        handles.push(spawn_firq_producer(
            Arc::clone(&scheduler),
            Arc::clone(&running),
            Arc::clone(&counters),
            profile.clone(),
        ));
    }

    for _ in 0..config.worker_count {
        handles.push(spawn_firq_worker(
            Arc::clone(&scheduler),
            Arc::clone(&running),
            Arc::clone(&histogram),
            Arc::clone(&cold_histogram),
            scenario.work_ms,
        ));
    }

    let start = Instant::now();
    thread::sleep(Duration::from_secs(run_seconds));
    let elapsed = start.elapsed().as_secs_f64();

    running.store(false, Ordering::Relaxed);
    scheduler.close_drain();

    for handle in handles {
        let _ = handle.join();
    }

    let stats = scheduler.stats();

    BenchResult {
        enqueued: stats.enqueued,
        dequeued: stats.dequeued,
        dropped: stats.dropped,
        expired: stats.expired,
        produced_total: counters.produced_total.load(Ordering::Relaxed),
        throughput_ops: if elapsed > 0.0 {
            stats.dequeued as f64 / elapsed
        } else {
            0.0
        },
        drop_rate: rate(
            stats.dropped,
            counters.produced_total.load(Ordering::Relaxed),
        ),
        expired_rate: rate(stats.expired, stats.enqueued),
        p50_queue_ms: ns_to_ms(histogram.percentile_ns(0.50)),
        p95_queue_ms: ns_to_ms(histogram.percentile_ns(0.95)),
        p99_queue_ms: ns_to_ms(histogram.percentile_ns(0.99)),
        cold_p99_queue_ms: ns_to_ms(cold_histogram.percentile_ns(0.99)),
    }
}

fn run_fifo(config: &BenchConfig, scenario: &Scenario) -> BenchResult {
    let run_seconds = scenario.run_seconds.unwrap_or(config.default_run_seconds);
    let max_global = scenario.max_global.unwrap_or(config.max_global);
    let queue = Arc::new(FifoQueue::<BenchPayload>::new(max_global));
    let running = Arc::new(AtomicBool::new(true));
    let counters = Arc::new(SharedCounters::new());
    let histogram = Arc::new(ConcurrentHistogram::new());
    let cold_histogram = Arc::new(ConcurrentHistogram::new());
    let dequeued_total = Arc::new(AtomicU64::new(0));
    let expired_total = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    for profile in &scenario.producers {
        handles.push(spawn_fifo_producer(
            Arc::clone(&queue),
            Arc::clone(&running),
            Arc::clone(&counters),
            profile.clone(),
        ));
    }

    for _ in 0..config.worker_count {
        handles.push(spawn_fifo_worker(
            Arc::clone(&queue),
            Arc::clone(&running),
            Arc::clone(&histogram),
            Arc::clone(&cold_histogram),
            Arc::clone(&dequeued_total),
            Arc::clone(&expired_total),
            scenario.work_ms,
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

    let dequeued = dequeued_total.load(Ordering::Relaxed);
    let dropped = queue.dropped();
    let produced = counters.produced_total.load(Ordering::Relaxed);
    let expired = expired_total.load(Ordering::Relaxed);

    BenchResult {
        enqueued: produced.saturating_sub(dropped),
        dequeued,
        dropped,
        expired,
        produced_total: produced,
        throughput_ops: if elapsed > 0.0 {
            dequeued as f64 / elapsed
        } else {
            0.0
        },
        drop_rate: rate(dropped, produced),
        expired_rate: rate(expired, produced.saturating_sub(dropped)),
        p50_queue_ms: ns_to_ms(histogram.percentile_ns(0.50)),
        p95_queue_ms: ns_to_ms(histogram.percentile_ns(0.95)),
        p99_queue_ms: ns_to_ms(histogram.percentile_ns(0.99)),
        cold_p99_queue_ms: ns_to_ms(cold_histogram.percentile_ns(0.99)),
    }
}

fn spawn_firq_producer(
    scheduler: Arc<Scheduler<BenchPayload>>,
    running: Arc<AtomicBool>,
    counters: Arc<SharedCounters>,
    profile: ProducerProfile,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let deadline = profile
                .deadline_ms
                .map(|deadline_ms| Instant::now() + Duration::from_millis(deadline_ms));
            let task = Task {
                payload: BenchPayload {
                    class: profile.class,
                },
                enqueue_ts: Instant::now(),
                deadline,
                priority: profile.priority,
                cost: profile.cost,
            };

            counters.produced_total.fetch_add(1, Ordering::Relaxed);
            match scheduler.enqueue(profile.tenant, task) {
                EnqueueResult::Enqueued => {}
                EnqueueResult::Rejected(_) => {
                    counters.dropped_total.fetch_add(1, Ordering::Relaxed);
                }
                EnqueueResult::Closed => break,
            }

            if profile.interval_ms > 0 {
                thread::sleep(Duration::from_millis(profile.interval_ms));
            }
        }
    })
}

fn spawn_firq_worker(
    scheduler: Arc<Scheduler<BenchPayload>>,
    running: Arc<AtomicBool>,
    histogram: Arc<ConcurrentHistogram>,
    cold_histogram: Arc<ConcurrentHistogram>,
    work_ms: u64,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match scheduler.dequeue_blocking() {
                DequeueResult::Task { task, .. } => {
                    let queue_time_ns = task
                        .enqueue_ts
                        .elapsed()
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64;
                    histogram.observe(queue_time_ns);
                    if task.payload.class == ProducerClass::Cold {
                        cold_histogram.observe(queue_time_ns);
                    }
                    if work_ms > 0 {
                        thread::sleep(Duration::from_millis(work_ms));
                    }
                }
                DequeueResult::Closed => break,
                DequeueResult::Empty => {}
            }
        }
    })
}

fn spawn_fifo_producer(
    queue: Arc<FifoQueue<BenchPayload>>,
    running: Arc<AtomicBool>,
    counters: Arc<SharedCounters>,
    profile: ProducerProfile,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let deadline = profile
                .deadline_ms
                .map(|deadline_ms| Instant::now() + Duration::from_millis(deadline_ms));
            let task = Task {
                payload: BenchPayload {
                    class: profile.class,
                },
                enqueue_ts: Instant::now(),
                deadline,
                priority: profile.priority,
                cost: profile.cost,
            };

            counters.produced_total.fetch_add(1, Ordering::Relaxed);
            if !queue.enqueue(task) {
                counters.dropped_total.fetch_add(1, Ordering::Relaxed);
            }

            if profile.interval_ms > 0 {
                thread::sleep(Duration::from_millis(profile.interval_ms));
            }
        }
    })
}

fn spawn_fifo_worker(
    queue: Arc<FifoQueue<BenchPayload>>,
    running: Arc<AtomicBool>,
    histogram: Arc<ConcurrentHistogram>,
    cold_histogram: Arc<ConcurrentHistogram>,
    dequeued_total: Arc<AtomicU64>,
    expired_total: Arc<AtomicU64>,
    work_ms: u64,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match queue.dequeue_blocking() {
                Some(task) => {
                    if matches!(task.deadline, Some(deadline) if Instant::now() > deadline) {
                        expired_total.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    dequeued_total.fetch_add(1, Ordering::Relaxed);
                    let queue_time_ns = task
                        .enqueue_ts
                        .elapsed()
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64;
                    histogram.observe(queue_time_ns);
                    if task.payload.class == ProducerClass::Cold {
                        cold_histogram.observe(queue_time_ns);
                    }

                    if work_ms > 0 {
                        thread::sleep(Duration::from_millis(work_ms));
                    }
                }
                None => break,
            }
        }
    })
}

fn print_comparison(
    config: &BenchConfig,
    scenario: &Scenario,
    firq: &BenchResult,
    fifo: &BenchResult,
) {
    println!(
        "scenario={} workers={} desc=\"{}\"",
        scenario.name, config.worker_count, scenario.description
    );
    println!(
        "firq: enq={} deq={} drop={} exp={} throughput={:.1} drop_rate={:.3}% expired_rate={:.3}% p50={:.3}ms p95={:.3}ms p99={:.3}ms cold_p99={:.3}ms",
        firq.enqueued,
        firq.dequeued,
        firq.dropped,
        firq.expired,
        firq.throughput_ops,
        firq.drop_rate * 100.0,
        firq.expired_rate * 100.0,
        firq.p50_queue_ms,
        firq.p95_queue_ms,
        firq.p99_queue_ms,
        firq.cold_p99_queue_ms,
    );
    println!(
        "fifo: enq={} deq={} drop={} exp={} throughput={:.1} drop_rate={:.3}% expired_rate={:.3}% p50={:.3}ms p95={:.3}ms p99={:.3}ms cold_p99={:.3}ms",
        fifo.enqueued,
        fifo.dequeued,
        fifo.dropped,
        fifo.expired,
        fifo.throughput_ops,
        fifo.drop_rate * 100.0,
        fifo.expired_rate * 100.0,
        fifo.p50_queue_ms,
        fifo.p95_queue_ms,
        fifo.p99_queue_ms,
        fifo.cold_p99_queue_ms,
    );

    let p99_gain = if fifo.p99_queue_ms.is_finite()
        && firq.p99_queue_ms.is_finite()
        && fifo.p99_queue_ms > 0.0
    {
        (fifo.p99_queue_ms - firq.p99_queue_ms) / fifo.p99_queue_ms * 100.0
    } else {
        0.0
    };
    println!(
        "comparison: p99_gain_vs_fifo={:.2}% throughput_gain_vs_fifo={:.2}% produced={}\n",
        p99_gain,
        gain_percent(firq.throughput_ops, fifo.throughput_ops),
        firq.produced_total,
    );

    if scenario.report_cold_tail {
        let cold_p99_gain = if fifo.cold_p99_queue_ms.is_finite()
            && firq.cold_p99_queue_ms.is_finite()
            && fifo.cold_p99_queue_ms > 0.0
        {
            (fifo.cold_p99_queue_ms - firq.cold_p99_queue_ms) / fifo.cold_p99_queue_ms * 100.0
        } else {
            0.0
        };
        println!(
            "noisy_neighbor: cold_p99_gain_vs_fifo={:.2}% (firq={:.3}ms fifo={:.3}ms)\n",
            cold_p99_gain, firq.cold_p99_queue_ms, fifo.cold_p99_queue_ms
        );
    }
}

fn percentile_from_counts(counts: &[u64], pct: f64) -> u64 {
    let total = counts.iter().copied().sum::<u64>();
    if total == 0 {
        return 0;
    }
    let target = (total as f64 * pct).ceil() as u64;
    let mut cumulative = 0u64;
    for (idx, count) in counts.iter().enumerate() {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= target {
            return QUEUE_TIME_BUCKETS_NS[idx];
        }
    }
    *QUEUE_TIME_BUCKETS_NS.last().unwrap_or(&0)
}

fn gain_percent(current: f64, baseline: f64) -> f64 {
    if baseline == 0.0 {
        return 0.0;
    }
    (current - baseline) / baseline * 100.0
}

fn rate(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

fn ns_to_ms(value_ns: u64) -> f64 {
    value_ns as f64 / 1_000_000.0
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

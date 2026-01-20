use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use firq_core::{
    BackpressurePolicy, DequeueResult, EnqueueResult, Scheduler, SchedulerConfig, Task, TenantKey,
};

// Core-only example: heavy data pipeline with 30 tenants and 4 workers.

#[derive(Clone, Copy, Debug)]
enum TenantTier {
    Batch,
    Etl,
    Api,
}

#[derive(Clone, Debug)]
struct TenantProfile {
    id: u64,
    name: String,
    tier: TenantTier,
    base_kb: u64,
    jitter_kb: u64,
    submit_every_us: u64,
    base_cost: u64,
}

#[derive(Clone, Debug)]
struct PipelineJob {
    bytes_kb: u64,
    stage_ms: [u64; 4],
}

#[derive(Clone, Debug)]
struct ExampleConfig {
    worker_count: usize,
    run_seconds: u64,
    log_every_s: u64,
    scheduler: SchedulerConfig,
}

struct ExampleState {
    scheduler: Arc<Scheduler<PipelineJob>>,
    running: Arc<AtomicBool>,
    produced_total: Arc<AtomicU64>,
    dropped_total: Arc<AtomicU64>,
    busy_ns_total: Arc<AtomicU64>,
    tenant_profiles: Vec<TenantProfile>,
    tenant_index: HashMap<TenantKey, usize>,
    produced_by_tenant: Arc<Mutex<Vec<u64>>>,
    served_by_tenant: Arc<Mutex<Vec<u64>>>,
}

fn main() {
    let cfg = example_config();
    let profiles = tenant_profiles();
    let state = Arc::new(ExampleState::new(cfg.scheduler.clone(), profiles));

    print_header(&cfg, &state.tenant_profiles);

    let mut producers = spawn_producers(Arc::clone(&state));
    let mut workers = spawn_workers(Arc::clone(&state), cfg.worker_count);
    let logger = spawn_logger(Arc::clone(&state), cfg.log_every_s);

    println!("running for {}s...", cfg.run_seconds);
    thread::sleep(Duration::from_secs(cfg.run_seconds));

    state.stop();
    println!("stopping... joining threads");

    for handle in producers.drain(..) {
        let _ = handle.join();
    }
    for handle in workers.drain(..) {
        let _ = handle.join();
    }
    let _ = logger.join();

    print_summary(&state, cfg.run_seconds);
}

fn example_config() -> ExampleConfig {
    ExampleConfig {
        worker_count: 4,
        run_seconds: 15,
        log_every_s: 1,
        scheduler: SchedulerConfig {
            shards: 1,
            max_global: 60_000,
            max_per_tenant: 2_000,
            quantum: 5,
            backpressure: BackpressurePolicy::Reject,
        },
    }
}

fn tenant_profiles() -> Vec<TenantProfile> {
    let mut profiles = Vec::with_capacity(30);
    let mut id = 1u64;

    for i in 0..10 {
        profiles.push(TenantProfile {
            id,
            name: format!("batch-{:02}", i + 1),
            tier: TenantTier::Batch,
            base_kb: 1200 + (i as u64 * 30),
            jitter_kb: 400,
            submit_every_us: 900 + (i as u64 * 20),
            base_cost: 8,
        });
        id += 1;
    }

    for i in 0..10 {
        profiles.push(TenantProfile {
            id,
            name: format!("etl-{:02}", i + 1),
            tier: TenantTier::Etl,
            base_kb: 400 + (i as u64 * 15),
            jitter_kb: 200,
            submit_every_us: 350 + (i as u64 * 10),
            base_cost: 4,
        });
        id += 1;
    }

    for i in 0..10 {
        profiles.push(TenantProfile {
            id,
            name: format!("api-{:02}", i + 1),
            tier: TenantTier::Api,
            base_kb: 60 + (i as u64 * 5),
            jitter_kb: 40,
            submit_every_us: 180 + (i as u64 * 8),
            base_cost: 1,
        });
        id += 1;
    }

    profiles
}

impl ExampleState {
    fn new(config: SchedulerConfig, tenant_profiles: Vec<TenantProfile>) -> Self {
        let tenant_index = tenant_profiles
            .iter()
            .enumerate()
            .map(|(idx, profile)| (TenantKey::from(profile.id), idx))
            .collect::<HashMap<_, _>>();
        let tenant_count = tenant_profiles.len();
        Self {
            scheduler: Arc::new(Scheduler::new(config)),
            running: Arc::new(AtomicBool::new(true)),
            produced_total: Arc::new(AtomicU64::new(0)),
            dropped_total: Arc::new(AtomicU64::new(0)),
            busy_ns_total: Arc::new(AtomicU64::new(0)),
            tenant_profiles,
            tenant_index,
            produced_by_tenant: Arc::new(Mutex::new(vec![0; tenant_count])),
            served_by_tenant: Arc::new(Mutex::new(vec![0; tenant_count])),
        }
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.scheduler.close();
    }
}

fn spawn_producers(state: Arc<ExampleState>) -> Vec<thread::JoinHandle<()>> {
    let mut handles = Vec::with_capacity(state.tenant_profiles.len());
    for profile in &state.tenant_profiles {
        let scheduler = Arc::clone(&state.scheduler);
        let running = Arc::clone(&state.running);
        let produced_total = Arc::clone(&state.produced_total);
        let dropped_total = Arc::clone(&state.dropped_total);
        let produced_by_tenant = Arc::clone(&state.produced_by_tenant);
        let tenant_key = TenantKey::from(profile.id);
        let tenant_index = *state
            .tenant_index
            .get(&tenant_key)
            .expect("tenant index missing");
        let plan = ProducerPlan::from_profile(profile);
        handles.push(thread::spawn(move || {
            let mut counter = 0u64;
            while running.load(Ordering::Relaxed) {
                counter += 1;
                let bytes_kb = plan.base_kb + (counter % (plan.jitter_kb + 1));
                let job = PipelineJob {
                    bytes_kb,
                    stage_ms: pipeline_stage_ms(bytes_kb),
                };
                let task = Task {
                    payload: job,
                    enqueue_ts: Instant::now(),
                    deadline: None,
                    cost: job_cost(plan.base_cost, bytes_kb),
                };
                produced_total.fetch_add(1, Ordering::Relaxed);
                record_count(&produced_by_tenant, tenant_index);
                if matches!(scheduler.enqueue(tenant_key, task), EnqueueResult::Rejected(_)) {
                    dropped_total.fetch_add(1, Ordering::Relaxed);
                }
                sleep_us(plan.submit_every_us);
            }
        }));
    }
    handles
}

fn spawn_workers(
    state: Arc<ExampleState>,
    worker_count: usize,
) -> Vec<thread::JoinHandle<()>> {
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let scheduler = Arc::clone(&state.scheduler);
        let running = Arc::clone(&state.running);
        let served_by_tenant = Arc::clone(&state.served_by_tenant);
        let tenant_index = Arc::new(state.tenant_index.clone());
        let busy_ns_total = Arc::clone(&state.busy_ns_total);
        workers.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match scheduler.dequeue_blocking() {
                    DequeueResult::Task { tenant, task } => {
                        let start = Instant::now();
                        run_pipeline(&task.payload);
                        let elapsed_ns = start.elapsed().as_nanos().min(u128::from(u64::MAX));
                        busy_ns_total.fetch_add(elapsed_ns as u64, Ordering::Relaxed);
                        if let Some(index) = tenant_index.get(&tenant).copied() {
                            record_count(&served_by_tenant, index);
                        }
                    }
                    DequeueResult::Closed => break,
                    DequeueResult::Empty => {}
                }
            }
        }));
    }
    workers
}

fn spawn_logger(state: Arc<ExampleState>, interval_s: u64) -> thread::JoinHandle<()> {
    let running = Arc::clone(&state.running);
    let scheduler = Arc::clone(&state.scheduler);
    let produced_total = Arc::clone(&state.produced_total);
    let dropped_total = Arc::clone(&state.dropped_total);
    let served_by_tenant = Arc::clone(&state.served_by_tenant);
    let profiles = state.tenant_profiles.clone();
    thread::spawn(move || {
        let mut tick = 0u64;
        while running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(interval_s));
            tick += interval_s;
            let stats = scheduler.stats();
            let (batch, etl, api) = tier_counts(&profiles, &served_by_tenant);
            println!(
                "[t+{}s] enq={} deq={} qlen={} served_batch={} served_etl={} served_api={}",
                tick,
                stats.enqueued,
                stats.dequeued,
                stats.queue_len_estimate,
                batch,
                etl,
                api
            );
            println!(
                "         produced={} dropped={}",
                produced_total.load(Ordering::Relaxed),
                dropped_total.load(Ordering::Relaxed)
            );
        }
    })
}

fn run_pipeline(job: &PipelineJob) {
    for stage_ms in job.stage_ms {
        thread::sleep(Duration::from_millis(stage_ms));
    }
}

fn pipeline_stage_ms(bytes_kb: u64) -> [u64; 4] {
    [
        2 + bytes_kb / 200,
        3 + bytes_kb / 160,
        2 + bytes_kb / 240,
        1 + bytes_kb / 300,
    ]
}

fn job_cost(base_cost: u64, bytes_kb: u64) -> u64 {
    let size_cost = bytes_kb / 100;
    base_cost.max(size_cost).max(1)
}

fn record_count(map: &Arc<Mutex<Vec<u64>>>, index: usize) {
    let mut map = map.lock().expect("count map poisoned");
    if let Some(count) = map.get_mut(index) {
        *count += 1;
    }
}

fn tier_counts(
    profiles: &[TenantProfile],
    counts: &Arc<Mutex<Vec<u64>>>,
) -> (u64, u64, u64) {
    let counts = counts.lock().expect("count map poisoned");
    let mut batch = 0u64;
    let mut etl = 0u64;
    let mut api = 0u64;
    for (idx, profile) in profiles.iter().enumerate() {
        let value = counts.get(idx).copied().unwrap_or(0);
        match profile.tier {
            TenantTier::Batch => batch += value,
            TenantTier::Etl => etl += value,
            TenantTier::Api => api += value,
        }
    }
    (batch, etl, api)
}

fn print_header(cfg: &ExampleConfig, profiles: &[TenantProfile]) {
    println!("core-only heavy data pipeline (30 tenants, 4 workers)");
    println!(
        "scheduler: shards={} max_global={} max_per_tenant={} quantum={} workers={}",
        cfg.scheduler.shards,
        cfg.scheduler.max_global,
        cfg.scheduler.max_per_tenant,
        cfg.scheduler.quantum,
        cfg.worker_count
    );
    println!("tenants:");
    for profile in profiles {
        println!(
            "  id={} name={} tier={:?} size={}kb..{}kb every={}us cost~{}",
            profile.id,
            profile.name,
            profile.tier,
            profile.base_kb,
            profile.base_kb + profile.jitter_kb,
            profile.submit_every_us,
            profile.base_cost
        );
    }
}

fn print_summary(state: &ExampleState, run_seconds: u64) {
    let stats = state.scheduler.stats();
    let busy_ns = state.busy_ns_total.load(Ordering::Relaxed) as u128;
    let elapsed_ns = u128::from(run_seconds) * 1_000_000_000;
    let busy_pct = if elapsed_ns > 0 {
        (busy_ns * 100) / elapsed_ns
    } else {
        0
    };

    println!("final stats:");
    println!(
        "enqueued={} dequeued={} queue_len={}",
        stats.enqueued, stats.dequeued, stats.queue_len_estimate
    );
    println!(
        "produced={} dropped={}",
        state.produced_total.load(Ordering::Relaxed),
        state.dropped_total.load(Ordering::Relaxed)
    );
    println!("workers busy: {}%", busy_pct);

    let (batch, etl, api) = tier_counts(&state.tenant_profiles, &state.served_by_tenant);
    println!(
        "served totals: batch={} etl={} api={}",
        batch, etl, api
    );

    let mut served = state
        .served_by_tenant
        .lock()
        .map(|counts| {
            counts
                .iter()
                .enumerate()
                .map(|(idx, count)| (&state.tenant_profiles[idx], *count))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    served.sort_by(|a, b| b.1.cmp(&a.1));

    println!("top tenants by served:");
    for (profile, count) in served.iter().take(8) {
        println!("  id={} name={} served={}", profile.id, profile.name, count);
    }
}

#[derive(Clone, Copy)]
struct ProducerPlan {
    base_kb: u64,
    jitter_kb: u64,
    submit_every_us: u64,
    base_cost: u64,
}

impl ProducerPlan {
    fn from_profile(profile: &TenantProfile) -> Self {
        Self {
            base_kb: profile.base_kb,
            jitter_kb: profile.jitter_kb,
            submit_every_us: profile.submit_every_us,
            base_cost: profile.base_cost,
        }
    }
}

fn sleep_us(micros: u64) {
    if micros > 0 {
        thread::sleep(Duration::from_micros(micros));
    }
}

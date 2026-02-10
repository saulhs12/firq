//! Tokio adapter for `firq-core`.
//!
//! This crate provides async wrappers around the core scheduler, including:
//! - `AsyncScheduler` for enqueue/dequeue operations
//! - `AsyncReceiver` and `AsyncStream` helpers
//! - `AsyncWorkerReceiver` for dedicated dequeue-worker mode
//! - `Dispatcher` with bounded in-flight execution

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

pub use firq_core::{
    BackpressurePolicy, CancelResult, CloseMode, DequeueResult, EnqueueRejectReason, EnqueueResult,
    EnqueueWithHandleResult, Priority, QueueTimeBucket, Scheduler, SchedulerConfig, SchedulerStats,
    Task, TaskHandle, TenantCount, TenantKey,
};
use futures_core::Stream;
use tokio::sync::{Semaphore, mpsc};

const WORKER_DEQUEUE_TIMEOUT: Duration = Duration::from_millis(25);

/// Async wrapper around [`Scheduler`].
pub struct AsyncScheduler<T> {
    inner: Arc<Scheduler<T>>,
}

impl<T> Clone for AsyncScheduler<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> AsyncScheduler<T> {
    /// Creates a new async wrapper over a shared core scheduler.
    pub fn new(inner: Arc<Scheduler<T>>) -> Self {
        Self { inner }
    }

    /// Returns the shared core scheduler.
    pub fn inner(&self) -> &Arc<Scheduler<T>> {
        &self.inner
    }

    /// Enqueues a task.
    pub fn enqueue(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        self.inner.enqueue(tenant, task)
    }

    /// Enqueues a task and returns a cancellation handle.
    pub fn enqueue_with_handle(&self, tenant: TenantKey, task: Task<T>) -> EnqueueWithHandleResult {
        self.inner.enqueue_with_handle(tenant, task)
    }

    /// Non-blocking dequeue attempt.
    pub fn try_dequeue(&self) -> DequeueResult<T> {
        self.inner.try_dequeue()
    }

    /// Attempts to cancel pending work by handle.
    pub fn cancel(&self, handle: TaskHandle) -> CancelResult {
        self.inner.cancel(handle)
    }

    /// Returns scheduler metric snapshot.
    pub fn stats(&self) -> SchedulerStats {
        self.inner.stats()
    }

    /// Alias for [`AsyncScheduler::close_immediate`].
    pub fn close(&self) {
        self.inner.close_immediate();
    }

    /// Closes immediately.
    pub fn close_immediate(&self) {
        self.inner.close_immediate();
    }

    /// Closes in drain mode.
    pub fn close_drain(&self) {
        self.inner.close_drain();
    }

    /// Closes using a specific mode.
    pub fn close_with_mode(&self, mode: CloseMode) {
        self.inner.close_with_mode(mode);
    }

    /// Returns a receiver-style async dequeue helper.
    pub fn receiver(&self) -> AsyncReceiver<T> {
        AsyncReceiver::new(self.clone())
    }

    /// Returns a `Stream` wrapper for dequeue operations.
    pub fn stream(&self) -> AsyncStream<T> {
        AsyncStream::new(self.clone())
    }
}

impl<T: Send + 'static> AsyncScheduler<T> {
    /// Performs blocking dequeue on a Tokio blocking thread.
    pub async fn dequeue_async(&self) -> DequeueResult<T> {
        let scheduler = Arc::clone(&self.inner);
        match tokio::task::spawn_blocking(move || scheduler.dequeue_blocking()).await {
            Ok(result) => result,
            Err(_) => DequeueResult::Closed,
        }
    }

    /// Returns a receiver powered by a dedicated dequeue worker thread.
    pub fn receiver_with_worker(&self, buffer: usize) -> AsyncWorkerReceiver<T> {
        AsyncWorkerReceiver::new(self.clone(), buffer)
    }

    /// Returns a stream powered by a dedicated dequeue worker thread.
    pub fn stream_with_worker(&self, buffer: usize) -> AsyncWorkerReceiver<T> {
        self.receiver_with_worker(buffer)
    }
}

/// Dequeued tenant/task pair.
pub struct DequeueItem<T> {
    /// Tenant selected by scheduler.
    pub tenant: TenantKey,
    /// Dequeued task.
    pub task: Task<T>,
}

/// Receiver-style async facade over scheduler dequeue.
#[derive(Clone)]
pub struct AsyncReceiver<T> {
    scheduler: AsyncScheduler<T>,
}

impl<T> AsyncReceiver<T> {
    /// Creates a new receiver facade.
    pub fn new(scheduler: AsyncScheduler<T>) -> Self {
        Self { scheduler }
    }
}

impl<T: Send + 'static> AsyncReceiver<T> {
    /// Waits for the next task, returning `None` once scheduler closes.
    pub async fn recv(&self) -> Option<DequeueItem<T>> {
        loop {
            match self.scheduler.dequeue_async().await {
                DequeueResult::Task { tenant, task } => {
                    return Some(DequeueItem { tenant, task });
                }
                DequeueResult::Closed => return None,
                DequeueResult::Empty => {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    /// Creates a receiver powered by a dedicated dequeue worker thread.
    pub fn new_worker(scheduler: AsyncScheduler<T>, buffer: usize) -> AsyncWorkerReceiver<T> {
        AsyncWorkerReceiver::new(scheduler, buffer)
    }
}

struct WorkerThreadHandle {
    shutdown: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl WorkerThreadHandle {
    fn new(shutdown: Arc<AtomicBool>, handle: thread::JoinHandle<()>) -> Self {
        Self {
            shutdown,
            handle: Mutex::new(Some(handle)),
        }
    }
}

impl Drop for WorkerThreadHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let mut guard = self.handle.lock().expect("worker handle mutex poisoned");
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
}

/// Receiver/stream adapter backed by a dedicated dequeue worker thread.
pub struct AsyncWorkerReceiver<T> {
    rx: mpsc::Receiver<DequeueItem<T>>,
    _worker: WorkerThreadHandle,
}

impl<T: Send + 'static> AsyncWorkerReceiver<T> {
    /// Creates a new worker-backed receiver.
    pub fn new(scheduler: AsyncScheduler<T>, buffer: usize) -> Self {
        let buffer = buffer.max(1);
        let (tx, rx) = mpsc::channel(buffer);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let core = Arc::clone(scheduler.inner());

        let handle = thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match core.dequeue_blocking_timeout(WORKER_DEQUEUE_TIMEOUT) {
                    DequeueResult::Task { tenant, task } => {
                        if tx.blocking_send(DequeueItem { tenant, task }).is_err() {
                            break;
                        }
                    }
                    DequeueResult::Closed => break,
                    DequeueResult::Empty => {}
                }
            }
        });

        Self {
            rx,
            _worker: WorkerThreadHandle::new(shutdown, handle),
        }
    }

    /// Waits for the next task, returning `None` once the worker stops.
    pub async fn recv(&mut self) -> Option<DequeueItem<T>> {
        self.rx.recv().await
    }
}

impl<T> Drop for AsyncWorkerReceiver<T> {
    fn drop(&mut self) {
        self.rx.close();
    }
}

impl<T> Stream for AsyncWorkerReceiver<T> {
    type Item = DequeueItem<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

/// `Stream` adapter for dequeue operations.
pub struct AsyncStream<T> {
    scheduler: AsyncScheduler<T>,
    pending: Option<Pin<Box<dyn Future<Output = DequeueResult<T>> + Send>>>,
}

impl<T> AsyncStream<T> {
    /// Creates a new stream adapter.
    pub fn new(scheduler: AsyncScheduler<T>) -> Self {
        Self {
            scheduler,
            pending: None,
        }
    }
}

impl<T: Send + 'static> Stream for AsyncStream<T> {
    type Item = DequeueItem<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pending.is_none() {
            let scheduler = this.scheduler.clone();
            this.pending = Some(Box::pin(async move { scheduler.dequeue_async().await }));
        }

        let pending = match this.pending.as_mut() {
            Some(pending) => pending,
            None => return Poll::Pending,
        };

        match pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.pending = None;
                match result {
                    DequeueResult::Task { tenant, task } => {
                        Poll::Ready(Some(DequeueItem { tenant, task }))
                    }
                    DequeueResult::Closed => Poll::Ready(None),
                    DequeueResult::Empty => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
        }
    }
}

pub struct Dispatcher<T> {
    scheduler: AsyncScheduler<T>,
    semaphore: Arc<Semaphore>,
    max_in_flight: usize,
}

impl<T> Dispatcher<T> {
    /// Creates a dispatcher with bounded in-flight handler executions.
    pub fn new(scheduler: AsyncScheduler<T>, max_in_flight: usize) -> Self {
        let max_in_flight = max_in_flight.max(1);
        Self {
            scheduler,
            semaphore: Arc::new(Semaphore::new(max_in_flight)),
            max_in_flight,
        }
    }
}

impl<T: Send + 'static> Dispatcher<T> {
    /// Runs the dequeue loop and executes each task with the provided async handler.
    pub async fn run<F, Fut>(&self, handler: F)
    where
        F: Fn(DequeueItem<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler = Arc::new(handler);
        loop {
            match self.scheduler.dequeue_async().await {
                DequeueResult::Task { tenant, task } => {
                    let permit = match Arc::clone(&self.semaphore).acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        handler(DequeueItem { tenant, task }).await;
                        drop(permit);
                    });
                }
                DequeueResult::Closed => break,
                DequeueResult::Empty => {
                    tokio::task::yield_now().await;
                }
            }
        }

        let _ = self.semaphore.acquire_many(self.max_in_flight as u32).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    fn config() -> SchedulerConfig {
        SchedulerConfig {
            shards: 2,
            max_global: 128,
            max_per_tenant: 128,
            quantum: 1,
            quantum_by_tenant: HashMap::new(),
            quantum_provider: None,
            backpressure: BackpressurePolicy::Reject,
            backpressure_by_tenant: HashMap::new(),
            top_tenants_capacity: 0,
        }
    }

    fn task(payload: u64) -> Task<u64> {
        Task {
            payload,
            enqueue_ts: Instant::now(),
            deadline: None,
            priority: Priority::Normal,
            cost: 1,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_scheduler_enqueue_cancel_roundtrip() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config())));
        let tenant = TenantKey::from(1);

        let handle = match scheduler.enqueue_with_handle(tenant, task(1)) {
            EnqueueWithHandleResult::Enqueued(handle) => handle,
            other => panic!("expected handle, got {:?}", other),
        };
        assert!(matches!(scheduler.cancel(handle), CancelResult::Cancelled));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_receiver_receives_items() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config())));
        let tenant = TenantKey::from(42);
        let _ = scheduler.enqueue(tenant, task(7));

        let item = scheduler.receiver().recv().await.expect("item");
        assert_eq!(item.tenant, tenant);
        assert_eq!(item.task.payload, 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_stream_yields_items() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config())));
        let tenant = TenantKey::from(3);
        let _ = scheduler.enqueue(tenant, task(11));

        let mut stream = scheduler.stream();
        let item = stream.next().await.expect("stream item");
        assert_eq!(item.tenant, tenant);
        assert_eq!(item.task.payload, 11);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_worker_receiver_receives_items() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config())));
        let tenant = TenantKey::from(6);
        let _ = scheduler.enqueue(tenant, task(12));

        let mut receiver = scheduler.receiver_with_worker(16);
        let item = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("worker recv timed out")
            .expect("worker recv should yield item");
        assert_eq!(item.tenant, tenant);
        assert_eq!(item.task.payload, 12);

        scheduler.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_worker_receiver_observes_basic_fair_order() {
        let mut cfg = config();
        cfg.shards = 1;
        cfg.quantum = 1;
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(cfg)));

        let tenant_a = TenantKey::from(1);
        let tenant_b = TenantKey::from(2);
        let _ = scheduler.enqueue(tenant_a, task(1));
        let _ = scheduler.enqueue(tenant_a, task(2));
        let _ = scheduler.enqueue(tenant_b, task(3));
        let _ = scheduler.enqueue(tenant_b, task(4));

        let mut receiver = scheduler.receiver_with_worker(16);
        let mut observed = Vec::new();
        for _ in 0..4 {
            let item = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("worker recv timed out")
                .expect("expected dequeued item");
            observed.push((item.tenant.as_u64(), item.task.payload));
        }

        assert_eq!(observed, vec![(1, 1), (2, 3), (1, 2), (2, 4)]);
        scheduler.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_worker_receiver_drop_stops_worker() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::<u64>::new(config())));

        let start = Instant::now();
        {
            let _receiver = scheduler.receiver_with_worker(8);
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "worker drop should join promptly"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatcher_recovers_permits_after_panic() {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config())));
        let tenant = TenantKey::from(5);
        let _ = scheduler.enqueue(tenant, task(1));
        let _ = scheduler.enqueue(tenant, task(2));

        let dispatcher = Dispatcher::new(scheduler.clone(), 1);
        let served = Arc::new(AtomicU64::new(0));
        let served_clone = Arc::clone(&served);

        let runner = tokio::spawn(async move {
            dispatcher
                .run(move |item| {
                    let served = Arc::clone(&served_clone);
                    async move {
                        if item.task.payload == 1 {
                            panic!("simulated panic");
                        }
                        served.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        scheduler.close();
        let _ = runner.await;

        assert_eq!(
            served.load(Ordering::Relaxed),
            1,
            "second task should execute despite panic in first task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "measurement helper for dequeue_async spawn_blocking overhead"]
    async fn measure_dequeue_async_spawn_blocking_cost() {
        let mut cfg = config();
        cfg.max_global = 1_024;
        cfg.max_per_tenant = 1_024;
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(cfg)));
        let tenant = TenantKey::from(9);
        let samples = 512u64;

        for i in 0..samples {
            let result = scheduler.enqueue(tenant, task(i));
            assert!(matches!(result, EnqueueResult::Enqueued));
        }

        let start = Instant::now();
        for _ in 0..samples {
            let result = scheduler.dequeue_async().await;
            assert!(matches!(result, DequeueResult::Task { .. }));
        }
        let elapsed = start.elapsed();
        let avg = elapsed / samples as u32;
        println!(
            "dequeue_async_spawn_blocking: samples={} total_ms={:.3} avg_us={:.3}",
            samples,
            elapsed.as_secs_f64() * 1_000.0,
            duration_to_us(avg)
        );
    }

    fn duration_to_us(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1_000_000.0
    }
}

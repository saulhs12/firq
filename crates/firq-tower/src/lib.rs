//! Tower integration for Firq scheduling.
//!
//! `firq-tower` exposes a configurable layer that:
//! - extracts a tenant key from requests
//! - enqueues requests through Firq
//! - supports cancellation before execution turn
//! - enforces in-flight limits with a semaphore
//! - maps scheduling rejections to HTTP-friendly errors
//!
//! # Example (header-style tenant extraction)
//!
//! ```rust,no_run
//! use firq_tower::{Firq, TenantKey};
//! use std::convert::Infallible;
//! use std::future::Ready;
//! use std::task::{Context, Poll};
//! use tower::{Service, ServiceBuilder};
//!
//! #[derive(Clone)]
//! struct Request {
//!     x_tenant_id_header: Option<String>,
//!     body: String,
//! }
//!
//! #[derive(Clone)]
//! struct EchoService;
//!
//! impl Service<Request> for EchoService {
//!     type Response = String;
//!     type Error = Infallible;
//!     type Future = Ready<Result<Self::Response, Self::Error>>;
//!     fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
//!         Poll::Ready(Ok(()))
//!     }
//!     fn call(&mut self, req: Request) -> Self::Future {
//!         std::future::ready(Ok(req.body))
//!     }
//! }
//!
//! let layer = Firq::new().build(|req: &Request| {
//!     req.x_tenant_id_header
//!         .as_deref()
//!         .and_then(|raw| raw.parse::<u64>().ok())
//!         .map(TenantKey::from)
//!         .unwrap_or(TenantKey::from(0))
//! });
//! let _svc = ServiceBuilder::new().layer(layer).service(EchoService);
//! ```

mod builder;
pub use builder::Firq;
pub use firq_async::{
    self, AsyncScheduler, BackpressurePolicy, CancelResult, CloseMode, DequeueResult,
    EnqueueRejectReason, EnqueueResult, EnqueueWithHandleResult, Priority, QueueTimeBucket,
    Scheduler, SchedulerConfig, SchedulerStats, Task, TaskHandle, TenantCount, TenantKey,
};

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::sync::{Semaphore, oneshot};
use tower_service::Service;

pub(crate) type ErasedDeadlineExtractor =
    Arc<dyn Fn(&dyn Any) -> Option<std::time::Instant> + Send + Sync>;

/// Function used to map enqueue rejections into HTTP-facing errors.
pub type RejectionMapper = Arc<dyn Fn(EnqueueRejectReason) -> FirqHttpRejection + Send + Sync>;

/// Structured HTTP rejection payload produced by the layer.
#[derive(Clone, Debug)]
pub struct FirqHttpRejection {
    /// HTTP status code.
    pub status: u16,
    /// Stable machine-readable error code.
    pub code: &'static str,
    /// Human-readable error message.
    pub message: &'static str,
    /// Underlying scheduling rejection reason.
    pub reason: EnqueueRejectReason,
}

/// Default mapping from scheduler rejection reasons to HTTP responses.
pub fn default_rejection_mapper(reason: EnqueueRejectReason) -> FirqHttpRejection {
    match reason {
        EnqueueRejectReason::TenantFull => FirqHttpRejection {
            status: 429,
            code: "tenant_full",
            message: "tenant queue limit reached",
            reason,
        },
        EnqueueRejectReason::GlobalFull => FirqHttpRejection {
            status: 503,
            code: "global_full",
            message: "scheduler global capacity reached",
            reason,
        },
        EnqueueRejectReason::Timeout => FirqHttpRejection {
            status: 503,
            code: "timeout",
            message: "request timed out waiting for scheduler capacity",
            reason,
        },
    }
}

/// Internal permit payload sent to the background dequeue worker.
pub struct FirqPermit {
    tx: oneshot::Sender<tokio::sync::OwnedSemaphorePermit>,
}

/// Layer/service error type.
#[derive(Debug)]
pub enum FirqError<E> {
    /// Error returned by the wrapped inner service.
    Service(E),
    /// Request rejected by scheduler policy.
    Rejected(FirqHttpRejection),
    /// Scheduler has been closed.
    Closed,
    /// Failed waiting for permit (expired/dropped/cancelled).
    PermitError,
}

impl<E: fmt::Display> fmt::Display for FirqError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirqError::Service(e) => write!(f, "Service error: {}", e),
            FirqError::Rejected(rejection) => write!(
                f,
                "Request rejected: status={} code={} reason={:?}",
                rejection.status, rejection.code, rejection.reason
            ),
            FirqError::Closed => write!(f, "Scheduler closed"),
            FirqError::PermitError => {
                write!(f, "Permit acquisition failed (task expired or dropped)")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for FirqError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FirqError::Service(e) => Some(e),
            _ => None,
        }
    }
}

pub trait KeyExtractor<Request>: Clone {
    /// Extracts a tenant key from an incoming request.
    fn extract(&self, req: &Request) -> TenantKey;
}

impl<F, Request> KeyExtractor<Request> for F
where
    F: Fn(&Request) -> TenantKey + Clone,
{
    fn extract(&self, req: &Request) -> TenantKey {
        (self)(req)
    }
}

/// Tower `Layer` implementation configured with a Firq scheduler.
pub struct FirqLayer<Request, K> {
    scheduler: AsyncScheduler<FirqPermit>,
    extractor: K,
    in_flight: Arc<Semaphore>,
    max_in_flight: usize,
    _worker: Arc<BackgroundWorker>,
    deadline_extractor: Option<ErasedDeadlineExtractor>,
    rejection_mapper: RejectionMapper,
    _marker: PhantomData<Request>,
}

impl<Request, K: Clone> Clone for FirqLayer<Request, K> {
    fn clone(&self) -> Self {
        Self {
            scheduler: self.scheduler.clone(),
            extractor: self.extractor.clone(),
            in_flight: Arc::clone(&self.in_flight),
            max_in_flight: self.max_in_flight,
            _worker: Arc::clone(&self._worker),
            deadline_extractor: self.deadline_extractor.clone(),
            rejection_mapper: Arc::clone(&self.rejection_mapper),
            _marker: PhantomData,
        }
    }
}

impl<Request, K> FirqLayer<Request, K> {
    /// Creates a new `FirqLayer`.
    pub fn new(
        scheduler: AsyncScheduler<FirqPermit>,
        extractor: K,
        in_flight_limit: usize,
        deadline_extractor: Option<ErasedDeadlineExtractor>,
        rejection_mapper: RejectionMapper,
    ) -> Self {
        let worker_scheduler = scheduler.inner().clone();
        let in_flight = Arc::new(Semaphore::new(in_flight_limit.max(1)));
        let worker_in_flight = Arc::clone(&in_flight);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("failed to build tower worker runtime");
            loop {
                if worker_shutdown.load(Ordering::Acquire) {
                    break;
                }
                match worker_scheduler.dequeue_blocking() {
                    DequeueResult::Task { task, .. } => {
                        let permit =
                            match runtime.block_on(worker_in_flight.clone().acquire_owned()) {
                                Ok(permit) => permit,
                                Err(_) => break,
                            };
                        let _ = task.payload.tx.send(permit);
                    }
                    DequeueResult::Closed => break,
                    DequeueResult::Empty => {}
                }
            }
        });

        let worker = Arc::new(BackgroundWorker::new(
            scheduler.clone(),
            Arc::clone(&in_flight),
            shutdown,
            worker_handle,
        ));

        Self {
            scheduler,
            extractor,
            in_flight,
            max_in_flight: in_flight_limit.max(1),
            _worker: worker,
            deadline_extractor,
            rejection_mapper,
            _marker: PhantomData,
        }
    }

    pub fn scheduler(&self) -> &AsyncScheduler<FirqPermit> {
        &self.scheduler
    }

    /// Returns configured in-flight execution limit.
    pub fn in_flight_limit(&self) -> usize {
        self.max_in_flight
    }

    /// Returns currently active in-flight executions.
    pub fn in_flight_active(&self) -> usize {
        self.max_in_flight
            .saturating_sub(self.in_flight.available_permits())
    }

    /// Returns current in-flight saturation ratio in `[0.0, 1.0+]`.
    pub fn in_flight_saturation_ratio(&self) -> f64 {
        if self.max_in_flight == 0 {
            0.0
        } else {
            self.in_flight_active() as f64 / self.max_in_flight as f64
        }
    }
}

impl<Request> FirqLayer<Request, ()> {
    /// Returns the `Firq` builder.
    pub fn builder() -> Firq {
        Firq::new()
    }
}

impl<S, Request, K> tower::Layer<S> for FirqLayer<Request, K>
where
    K: KeyExtractor<Request> + Clone,
{
    type Service = FirqService<S, K, Request>;

    fn layer(&self, inner: S) -> Self::Service {
        FirqService {
            inner,
            scheduler: self.scheduler.clone(),
            extractor: self.extractor.clone(),
            in_flight: Arc::clone(&self.in_flight),
            _worker: Arc::clone(&self._worker),
            deadline_extractor: self.deadline_extractor.clone(),
            rejection_mapper: Arc::clone(&self.rejection_mapper),
            _marker: PhantomData,
        }
    }
}

/// Tower `Service` produced by [`FirqLayer`].
pub struct FirqService<S, K, Request> {
    inner: S,
    scheduler: AsyncScheduler<FirqPermit>,
    extractor: K,
    in_flight: Arc<Semaphore>,
    _worker: Arc<BackgroundWorker>,
    deadline_extractor: Option<ErasedDeadlineExtractor>,
    rejection_mapper: RejectionMapper,
    _marker: PhantomData<Request>,
}

impl<S, K, Request> Clone for FirqService<S, K, Request>
where
    S: Clone,
    K: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            scheduler: self.scheduler.clone(),
            extractor: self.extractor.clone(),
            in_flight: Arc::clone(&self.in_flight),
            _worker: Arc::clone(&self._worker),
            deadline_extractor: self.deadline_extractor.clone(),
            rejection_mapper: Arc::clone(&self.rejection_mapper),
            _marker: PhantomData,
        }
    }
}

impl<S, K, Request> Service<Request> for FirqService<S, K, Request>
where
    S: Service<Request> + Clone + Send + 'static,
    S::Future: Send + 'static,
    K: KeyExtractor<Request> + Send + 'static,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = FirqError<S::Error>;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(FirqError::Service)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let tenant = self.extractor.extract(&req);
        let deadline = self
            .deadline_extractor
            .as_ref()
            .and_then(|extractor| extractor(&req as &dyn Any));

        let (tx, rx) = oneshot::channel();
        let task = Task {
            payload: FirqPermit { tx },
            enqueue_ts: std::time::Instant::now(),
            deadline,
            priority: Default::default(),
            cost: 1,
        };

        let scheduler = self.scheduler.clone();
        let mapper = Arc::clone(&self.rejection_mapper);

        match scheduler.enqueue_with_handle(tenant, task) {
            EnqueueWithHandleResult::Enqueued(handle) => {
                let mut inner = self.inner.clone();
                Box::pin(async move {
                    let mut guard = PendingCancelGuard::new(scheduler.clone(), handle);
                    let permit = rx.await.map_err(|_| FirqError::PermitError)?;
                    guard.disarm();

                    let response = inner.call(req).await.map_err(FirqError::Service)?;
                    drop(permit);
                    Ok(response)
                })
            }
            EnqueueWithHandleResult::Rejected(reason) => {
                let rejection = mapper(reason);
                Box::pin(async move { Err(FirqError::Rejected(rejection)) })
            }
            EnqueueWithHandleResult::Closed => Box::pin(async { Err(FirqError::Closed) }),
        }
    }
}

struct PendingCancelGuard {
    scheduler: AsyncScheduler<FirqPermit>,
    handle: Option<TaskHandle>,
}

impl PendingCancelGuard {
    fn new(scheduler: AsyncScheduler<FirqPermit>, handle: TaskHandle) -> Self {
        Self {
            scheduler,
            handle: Some(handle),
        }
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl Drop for PendingCancelGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.scheduler.cancel(handle);
        }
    }
}

struct BackgroundWorker {
    scheduler: AsyncScheduler<FirqPermit>,
    in_flight: Arc<Semaphore>,
    shutdown: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl BackgroundWorker {
    fn new(
        scheduler: AsyncScheduler<FirqPermit>,
        in_flight: Arc<Semaphore>,
        shutdown: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            scheduler,
            in_flight,
            shutdown,
            handle: Mutex::new(Some(handle)),
        }
    }
}

impl Drop for BackgroundWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.in_flight.close();
        self.scheduler.close_immediate();
        let mut guard = self.handle.lock().expect("worker mutex poisoned");
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
}

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
use std::task::{Context, Poll};

use tokio::sync::{Semaphore, oneshot};
use tower_service::Service;

pub(crate) type ErasedDeadlineExtractor =
    Arc<dyn Fn(&dyn Any) -> Option<std::time::Instant> + Send + Sync>;

pub type RejectionMapper = Arc<dyn Fn(EnqueueRejectReason) -> FirqHttpRejection + Send + Sync>;

#[derive(Clone, Debug)]
pub struct FirqHttpRejection {
    pub status: u16,
    pub code: &'static str,
    pub message: &'static str,
    pub reason: EnqueueRejectReason,
}

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

pub struct FirqPermit {
    tx: oneshot::Sender<()>,
}

#[derive(Debug)]
pub enum FirqError<E> {
    Service(E),
    Rejected(FirqHttpRejection),
    Closed,
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
    pub fn new(
        scheduler: AsyncScheduler<FirqPermit>,
        extractor: K,
        in_flight_limit: usize,
        deadline_extractor: Option<ErasedDeadlineExtractor>,
        rejection_mapper: RejectionMapper,
    ) -> Self {
        let worker_scheduler = scheduler.inner().clone();
        let worker_handle = std::thread::spawn(move || {
            loop {
                match worker_scheduler.dequeue_blocking() {
                    DequeueResult::Task { task, .. } => {
                        let _ = task.payload.tx.send(());
                    }
                    DequeueResult::Closed => break,
                    DequeueResult::Empty => {}
                }
            }
        });

        let worker = Arc::new(BackgroundWorker::new(scheduler.clone(), worker_handle));

        Self {
            scheduler,
            extractor,
            in_flight: Arc::new(Semaphore::new(in_flight_limit.max(1))),
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

    pub fn in_flight_limit(&self) -> usize {
        self.max_in_flight
    }

    pub fn in_flight_active(&self) -> usize {
        self.max_in_flight
            .saturating_sub(self.in_flight.available_permits())
    }

    pub fn in_flight_saturation_ratio(&self) -> f64 {
        if self.max_in_flight == 0 {
            0.0
        } else {
            self.in_flight_active() as f64 / self.max_in_flight as f64
        }
    }
}

impl<Request> FirqLayer<Request, ()> {
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
        let in_flight = Arc::clone(&self.in_flight);
        let mapper = Arc::clone(&self.rejection_mapper);

        match scheduler.enqueue_with_handle(tenant, task) {
            EnqueueWithHandleResult::Enqueued(handle) => {
                let mut inner = self.inner.clone();
                Box::pin(async move {
                    let mut guard = PendingCancelGuard::new(scheduler.clone(), handle);
                    rx.await.map_err(|_| FirqError::PermitError)?;
                    guard.disarm();

                    let permit = in_flight
                        .acquire_owned()
                        .await
                        .map_err(|_| FirqError::PermitError)?;
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
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl BackgroundWorker {
    fn new(scheduler: AsyncScheduler<FirqPermit>, handle: std::thread::JoinHandle<()>) -> Self {
        Self {
            scheduler,
            handle: Mutex::new(Some(handle)),
        }
    }
}

impl Drop for BackgroundWorker {
    fn drop(&mut self) {
        self.scheduler.close_immediate();
        let mut guard = self.handle.lock().expect("worker mutex poisoned");
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
}

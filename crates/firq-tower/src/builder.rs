use crate::{
    ErasedDeadlineExtractor, FirqHttpRejection, FirqLayer, KeyExtractor, RejectionMapper,
    default_rejection_mapper,
};
use firq_async::{
    AsyncScheduler, BackpressurePolicy, EnqueueRejectReason, Scheduler, SchedulerConfig,
};
use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

/// Builder for configuring and constructing `FirqLayer`.
pub struct Firq {
    config: SchedulerConfig,
    in_flight_limit: usize,
    deadline_extractor: Option<ErasedDeadlineExtractor>,
    rejection_mapper: RejectionMapper,
}

impl Default for Firq {
    fn default() -> Self {
        Self {
            config: SchedulerConfig::default(),
            in_flight_limit: 256,
            deadline_extractor: None,
            rejection_mapper: Arc::new(default_rejection_mapper),
        }
    }
}

impl Firq {
    /// Creates a builder with default scheduler settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets number of scheduler shards.
    pub fn with_shards(mut self, shards: usize) -> Self {
        self.config.shards = shards;
        self
    }

    /// Sets global queue capacity.
    pub fn with_max_global(mut self, max: usize) -> Self {
        self.config.max_global = max;
        self
    }

    /// Sets per-tenant queue capacity.
    pub fn with_max_per_tenant(mut self, max: usize) -> Self {
        self.config.max_per_tenant = max;
        self
    }

    /// Sets default DRR quantum.
    pub fn with_quantum(mut self, quantum: u64) -> Self {
        self.config.quantum = quantum;
        self
    }

    /// Sets default backpressure policy.
    pub fn with_backpressure(mut self, policy: BackpressurePolicy) -> Self {
        self.config.backpressure = policy;
        self
    }

    /// Sets maximum concurrent in-flight handler executions.
    pub fn with_in_flight_limit(mut self, in_flight_limit: usize) -> Self {
        self.in_flight_limit = in_flight_limit.max(1);
        self
    }

    /// Sets a request deadline extractor.
    pub fn with_deadline_extractor<Request, D>(mut self, extractor: D) -> Self
    where
        Request: 'static,
        D: Fn(&Request) -> Option<Instant> + Send + Sync + 'static,
    {
        self.deadline_extractor = Some(Arc::new(move |req: &dyn Any| {
            req.downcast_ref::<Request>().and_then(&extractor)
        }));
        self
    }

    /// Sets a custom mapper from enqueue rejection reason to HTTP rejection body.
    pub fn with_rejection_mapper<M>(mut self, mapper: M) -> Self
    where
        M: Fn(EnqueueRejectReason) -> FirqHttpRejection + Send + Sync + 'static,
    {
        self.rejection_mapper = Arc::new(mapper);
        self
    }

    /// Builds a `FirqLayer` using the provided tenant key extractor.
    pub fn build<Request, K>(self, extractor: K) -> FirqLayer<Request, K>
    where
        Request: Send + 'static,
        K: KeyExtractor<Request> + Clone,
    {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(self.config)));
        FirqLayer::new(
            scheduler,
            extractor,
            self.in_flight_limit,
            self.deadline_extractor,
            self.rejection_mapper,
        )
    }
}

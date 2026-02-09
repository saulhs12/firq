use crate::{FirqLayer, KeyExtractor};
use firq_async::{AsyncScheduler, BackpressurePolicy, Scheduler, SchedulerConfig};
use std::sync::Arc;

/// Builder principal para configurar la integración de Firq.
pub struct Firq {
    config: SchedulerConfig,
}

impl Default for Firq {
    fn default() -> Self {
        Self {
            config: SchedulerConfig::default(),
        }
    }
}

impl Firq {
    /// Crea una nueva configuración de Firq por defecto.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configura el número de shards (particiones) del scheduler.
    /// Más shards reducen la contención en sistemas con muchos cores.
    pub fn with_shards(mut self, shards: usize) -> Self {
        self.config.shards = shards;
        self
    }

    /// Configura el máximo global de tareas encoladas.
    pub fn with_max_global(mut self, max: usize) -> Self {
        self.config.max_global = max;
        self
    }

    /// Configura el máximo de tareas por tenant.
    pub fn with_max_per_tenant(mut self, max: usize) -> Self {
        self.config.max_per_tenant = max;
        self
    }

    /// Configura el quantum (peso) base para el algoritmo DRR.
    pub fn with_quantum(mut self, quantum: u64) -> Self {
        self.config.quantum = quantum;
        self
    }

    /// Configura la política de backpressure cuando la cola está llena.
    pub fn with_backpressure(mut self, policy: BackpressurePolicy) -> Self {
        self.config.backpressure = policy;
        self
    }

    /// Construye el `FirqLayer` con la configuración especificada y el extractor de Tenants.
    ///
    /// `extractor` es una función o closure que toma `&Request` y devuelve `TenantKey`.
    ///
    /// El tipo `Request` se infiere automáticamente del extractor.
    pub fn build<Request, K>(self, extractor: K) -> FirqLayer<Request, K>
    where
        K: KeyExtractor<Request> + Clone,
    {
        let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(self.config)));
        FirqLayer::new(scheduler, extractor)
    }
}

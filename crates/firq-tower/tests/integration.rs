use firq_async::{AsyncScheduler, Scheduler, SchedulerConfig, TenantKey};
use firq_tower::FirqLayer;
use tower::{Service, ServiceBuilder};

#[derive(Clone)]
struct MockService;

impl Service<String> for MockService {
    type Response = String;
    type Error = std::io::Error;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: String) -> Self::Future {
        std::future::ready(Ok(format!("Processed: {}", req)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_firq_tower_integration() {
    // 1. Configurar Scheduler con FirqPermit
    let config = SchedulerConfig {
        shards: 1,
        max_global: 10,
        max_per_tenant: 5,
        quantum: 10,
        quantum_by_tenant: Default::default(),
        quantum_provider: None,
        backpressure: firq_async::BackpressurePolicy::Reject,
        backpressure_by_tenant: Default::default(),
        top_tenants_capacity: 10,
    };
    let scheduler = AsyncScheduler::new(std::sync::Arc::new(Scheduler::new(config)));

    // 2. Crear Service con Layer
    let extractor = |req: &String| TenantKey::from(req.len() as u64);
    
    // FirqLayer ahora spawnea el background worker automáticamente.
    let layer = FirqLayer::new(scheduler, extractor);
    let mut service = ServiceBuilder::new()
        .layer(layer)
        .service(MockService);

    // 3. Enviar un request
    let req = "test".to_string();
    // poll_ready required by Tower (aunque aqui MockService siempre esta ready)
    std::future::poll_fn(|cx| service.poll_ready(cx)).await.unwrap();
    
    let result = service.call(req).await;
    
    // 4. Verificar resultado
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Processed: test");
}

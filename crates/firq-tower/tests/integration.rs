use firq_tower::{Firq, TenantKey};
use std::time::Duration;
use tower::{Service, ServiceBuilder, ServiceExt};

#[derive(Clone)]
struct MockService;

impl Service<String> for MockService {
    type Response = String;
    type Error = std::io::Error;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: String) -> Self::Future {
        std::future::ready(Ok(format!("Processed: {}", req)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_firq_tower_integration() {
    // 1. Configurar Scheduler con Builder Pattern (Mucho más limpio)
    let layer = Firq::new()
        .with_shards(1)
        .with_max_global(10)
        .with_max_per_tenant(5)
        .with_quantum(10)
        .build(|req: &String| TenantKey::from(req.len() as u64));
    let scheduler = layer.scheduler().clone();

    // 2. Crear Service con Layer
    let mut service = ServiceBuilder::new().layer(layer).service(MockService);

    // 3. Enviar un request
    let req = "test".to_string();
    let ready = tokio::time::timeout(Duration::from_secs(10), service.ready())
        .await
        .expect("service readiness timed out")
        .map_err(std::io::Error::other)
        .expect("service readiness failed");

    let result = tokio::time::timeout(Duration::from_secs(10), ready.call(req))
        .await
        .unwrap_or_else(|_| panic!("service call timed out, stats={:?}", scheduler.stats()));
    let result: Result<String, _> = result;

    // 4. Verificar resultado
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Processed: test");

    scheduler.close();
}

use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use firq_async::{AsyncScheduler, Scheduler, SchedulerConfig, TenantKey};
use firq_tower::FirqLayer;
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // 1. Configurar el Scheduler de Firq
    let config = SchedulerConfig {
        shards: 4,                    // 4 shards para reducir contención
        max_global: 1000,             // Máximo 1000 requests en cola
        max_per_tenant: 100,          // Máximo 100 requests por tenant
        quantum: 10,                  // Presupuesto base para DRR
        quantum_by_tenant: Default::default(),
        quantum_provider: None,
        backpressure: firq_async::BackpressurePolicy::Reject, // Rechazar cuando esté lleno
        backpressure_by_tenant: Default::default(),
        top_tenants_capacity: 20,     // Trackear top 20 tenants
    };

    let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config)));

    // 2. Definir cómo extraer el TenantKey de los requests
    // En este ejemplo, usamos el header "X-Tenant-ID"
    let tenant_extractor = |req: &Request| {
        req.headers()
            .get("X-Tenant-ID")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(TenantKey::from)
            .unwrap_or_else(|| {
                // Si no hay tenant ID, usar IP o valor default
                TenantKey::from(0)
            })
    };

    // 3. Crear el FirqLayer (esto automáticamente inicia el background worker)
    let firq_layer = FirqLayer::new(scheduler.clone(), tenant_extractor);

    // 4. Crear la aplicación Axum
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/api/users", get(list_users))
        .route("/api/status", get(status_handler))
        .layer(firq_layer)  // ← Aplicar Firq como middleware
        .with_state(AppState { scheduler });

    // 5. Iniciar el servidor
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("🚀 Server running on http://127.0.0.1:3000");
    println!("📊 Try with: curl -H 'X-Tenant-ID: 1' http://127.0.0.1:3000");
    
    axum::serve(listener, app).await.unwrap();
}

// Estado compartido de la aplicación
#[derive(Clone)]
struct AppState {
    scheduler: AsyncScheduler<firq_tower::FirqPermit>,
}

// Handlers de ejemplo
async fn root_handler() -> &'static str {
    "Hello from Firq-protected API!"
}

async fn list_users() -> Json<serde_json::Value> {
    // Simular trabajo
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    
    Json(json!({
        "users": [
            {"id": 1, "name": "Alice"},
            {"id": 2, "name": "Bob"}
        ]
    }))
}

async fn status_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let stats = state.scheduler.stats();
    
    Ok(Json(json!({
        "enqueued": stats.enqueued,
        "dequeued": stats.dequeued,
        "dropped": stats.dropped,
        "expired": stats.expired,
        "queue_len": stats.queue_len_estimate,
        "avg_queue_time_ms": if stats.queue_time_samples > 0 {
            (stats.queue_time_sum_ns / stats.queue_time_samples) / 1_000_000
        } else {
            0
        },
        "p95_queue_time_ms": stats.queue_time_p95_ns / 1_000_000,
        "p99_queue_time_ms": stats.queue_time_p99_ns / 1_000_000,
    })))
}

// Manejo de errores
struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0 })),
        )
            .into_response()
    }
}

impl From<firq_tower::FirqError<std::convert::Infallible>> for AppError {
    fn from(err: firq_tower::FirqError<std::convert::Infallible>) -> Self {
        match err {
            firq_tower::FirqError::Rejected(_) => AppError("Too many requests".into()),
            firq_tower::FirqError::Closed => AppError("Service unavailable".into()),
            firq_tower::FirqError::PermitError => AppError("Request expired".into()),
            firq_tower::FirqError::Service(e) => match e {},
        }
    }
}

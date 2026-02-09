// Ejemplo simplificado de integración de Firq con Axum
//
// Este ejemplo muestra cómo usar Firq para aplicar rate limiting
// y fairness multi-tenant en una API REST.
//
// Para ejecutar: cargo run --bin axum_simple
// Para probar: curl -H "X-Tenant-ID: 1" http://127.0.0.1:3000/

use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use firq_async::{AsyncScheduler, Scheduler, SchedulerConfig, TenantKey};
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // 1. Configurar Firq Scheduler
    let config = SchedulerConfig {
        shards: 4,
        max_global: 100,
        max_per_tenant: 10,
        quantum: 10,
        quantum_by_tenant: Default::default(),
        quantum_provider: None,
        backpressure: firq_async::BackpressurePolicy::Reject,
        backpressure_by_tenant: Default::default(),
        top_tenants_capacity: 20,
    };

    let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config)));

    // 2. Crear la aplicación Axum (sin firq-tower, manejamos manualmente)
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/api/users", get(list_users))
        .route("/api/stats", get(stats_handler))
        .with_state(AppState {
            scheduler: scheduler.clone(),
        });

    // 3. Iniciar servidor
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("🚀 Server running on http://127.0.0.1:3000");
    println!("📊 Stats: http://127.0.0.1:3000/api/stats");
    println!("\nEjemplos de uso:");
    println!("  curl -H 'X-Tenant-ID: 1' http://127.0.0.1:3000/");
    println!("  curl -H 'X-Tenant-ID: 2' http://127.0.0.1:3000/api/users\n");

    axum::serve(listener, app).await.unwrap();
}

// Estado de la aplicación
#[derive(Clone)]
struct AppState {
    scheduler: AsyncScheduler<WorkPermit>,
}

// Estructura para manejar permisos de trabajo
struct WorkPermit {
    tenant: TenantKey,
}

async fn root_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<String, AppError> {
    // Extraer tenant ID
    let tenant = extract_tenant(&req);

    // Solicitar permiso para procesar
    request_permission(&state.scheduler, tenant).await?;

    // Handler real
    Ok("Hello from Firq-protected API!".to_string())
}

async fn list_users(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = extract_tenant(&req);
    request_permission(&state.scheduler, tenant).await?;

    // Simular trabajo
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    Ok(Json(json!({
        "users": [
            {"id": 1, "name": "Alice"},
            {"id": 2, "name": "Bob"}
        ]
    })))
}

async fn stats_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let stats = state.scheduler.stats();

    Json(json!({
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
        "top_tenants": stats.top_tenants.iter().map(|t| json!({
            "tenant_id": t.tenant.as_u64(),
            "count": t.count
        })).collect::<Vec<_>>(),
    }))
}

// Utilidades

fn extract_tenant(req: &Request) -> TenantKey {
    req.headers()
        .get("X-Tenant-ID")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(TenantKey::from)
        .unwrap_or_else(|| TenantKey::from(0))
}

async fn request_permission(
    scheduler: &AsyncScheduler<WorkPermit>,
    tenant: TenantKey,
) -> Result<(), AppError> {
    use firq_async::{EnqueueResult, Task};

    let task = Task {
        payload: WorkPermit { tenant },
        enqueue_ts: std::time::Instant::now(),
        deadline: None,
        priority: Default::default(),
        cost: 1,
    };

    // Encolar
    match scheduler.enqueue(tenant, task) {
        EnqueueResult::Enqueued => {}
        EnqueueResult::Rejected(_) => {
            return Err(AppError::TooManyRequests);
        }
        EnqueueResult::Closed => {
            return Err(AppError::ServiceUnavailable);
        }
    }

    // Esperar nuestro turno
    match scheduler.dequeue_async().await {
        firq_async::DequeueResult::Task { .. } => Ok(()),
        firq_async::DequeueResult::Closed => Err(AppError::ServiceUnavailable),
        firq_async::DequeueResult::Empty => Err(AppError::Internal),
    }
}

// Error handling
enum AppError {
    TooManyRequests,
    ServiceUnavailable,
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "Too many requests. Please try again later."})),
            )
                .into_response(),
            AppError::ServiceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Service temporarily unavailable"})),
            )
                .into_response(),
            AppError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response(),
        }
    }
}

use axum::{
    Json, Router,
    error_handling::HandleErrorLayer,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use firq_tower::{AsyncScheduler, Firq, FirqPermit, TenantKey};
use serde_json::json;
use tower::ServiceBuilder;

#[tokio::main]
async fn main() {
    let firq_layer = Firq::new()
        .with_shards(4)
        .with_max_global(1000)
        .with_max_per_tenant(100)
        .with_quantum(10)
        .with_in_flight_limit(128)
        .with_deadline_extractor::<Request, _>(|req| {
            req.headers()
                .get("X-Deadline-Ms")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms))
        })
        .with_rejection_mapper(|reason| match reason {
            firq_tower::EnqueueRejectReason::TenantFull => firq_tower::FirqHttpRejection {
                status: 429,
                code: "tenant_full",
                message: "tenant queue saturated",
                reason,
            },
            firq_tower::EnqueueRejectReason::GlobalFull => firq_tower::FirqHttpRejection {
                status: 503,
                code: "global_full",
                message: "service queue saturated",
                reason,
            },
            firq_tower::EnqueueRejectReason::Timeout => firq_tower::FirqHttpRejection {
                status: 503,
                code: "timeout",
                message: "request timed out waiting for scheduler",
                reason,
            },
        })
        .build(|req: &Request| {
            req.headers()
                .get("X-Tenant-ID")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(TenantKey::from)
                .unwrap_or(TenantKey::from(0))
        });

    let scheduler = firq_layer.scheduler().clone();

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/api/users", get(list_users))
        .route("/api/status", get(status_handler))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_firq_error))
                .layer(firq_layer),
        )
        .with_state(AppState { scheduler });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("Server running on http://127.0.0.1:3000");
    println!("Try with: curl -H 'X-Tenant-ID: 1' http://127.0.0.1:3000");

    axum::serve(listener, app).await.unwrap();
}

#[derive(Clone)]
struct AppState {
    scheduler: AsyncScheduler<FirqPermit>,
}

async fn root_handler() -> &'static str {
    "Hello from Firq-protected API!"
}

async fn list_users() -> Json<serde_json::Value> {
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
            firq_tower::FirqError::Rejected(rejection) => {
                AppError(format!("{} ({})", rejection.message, rejection.code))
            }
            firq_tower::FirqError::Closed => AppError("Service unavailable".into()),
            firq_tower::FirqError::PermitError => AppError("Request expired".into()),
            firq_tower::FirqError::Service(e) => match e {},
        }
    }
}

async fn handle_firq_error(
    err: firq_tower::FirqError<std::convert::Infallible>,
) -> impl IntoResponse {
    AppError::from(err)
}

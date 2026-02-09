use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use firq_async::{
    AsyncScheduler, DequeueResult, EnqueueResult, Priority, Scheduler, SchedulerConfig, Task,
    TenantKey,
};
use std::sync::Arc;
use std::time::Instant;

struct Permit;

#[derive(Clone)]
struct AppState {
    scheduler: AsyncScheduler<Permit>,
}

fn tenant_from_request(req: &HttpRequest) -> TenantKey {
    req.headers()
        .get("X-Tenant-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(TenantKey::from)
        .unwrap_or(TenantKey::from(0))
}

async fn guarded_handler(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<HttpResponse> {
    let tenant = tenant_from_request(&req);
    let task = Task {
        payload: Permit,
        enqueue_ts: Instant::now(),
        deadline: None,
        priority: Priority::Normal,
        cost: 1,
    };

    match state.scheduler.enqueue(tenant, task) {
        EnqueueResult::Enqueued => {}
        EnqueueResult::Rejected(_) => return Ok(HttpResponse::TooManyRequests().finish()),
        EnqueueResult::Closed => return Ok(HttpResponse::ServiceUnavailable().finish()),
    }

    match state.scheduler.dequeue_async().await {
        DequeueResult::Task { .. } => Ok(HttpResponse::Ok().body("ok")),
        DequeueResult::Closed => Ok(HttpResponse::ServiceUnavailable().finish()),
        DequeueResult::Empty => Ok(HttpResponse::InternalServerError().finish()),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(SchedulerConfig::default())));
    let state = web::Data::new(AppState { scheduler });

    println!("Actix example running on http://127.0.0.1:3001");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/", web::get().to(guarded_handler))
    })
    .bind(("127.0.0.1", 3001))?
    .run()
    .await
}

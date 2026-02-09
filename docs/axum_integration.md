# Firq + Axum - Guía de Integración

Este documento explica cómo integrar **Firq** en tu API REST con Axum para aplicar rate limiting y fairness multi-tenant.

## Ejemplo Completo: `axum_simple.rs`

### 1. Configurar el Scheduler

```rust
use firq_async::{AsyncScheduler, Scheduler, SchedulerConfig};

let config = SchedulerConfig {
    shards: 4,                    // Shards para reducir contención
    max_global: 100,             // Máximo 100 requests en cola
    max_per_tenant: 10,          // Máximo 10 requests por tenant
    quantum: 10,                 // Presupuesto DRR
    backpressure: firq_async::BackpressurePolicy::Reject,
    ..Default::default()
};

let scheduler = AsyncScheduler::new(Arc::new(Scheduler::new(config)));
```

### 2. Crear la Aplicación Axum

```rust
let app = Router::new()
    .route("/", get(my_handler))
    .with_state(AppState { scheduler });
```

### 3. Extraer el Tenant ID de los Requests

```rust
fn extract_tenant(req: &Request) -> TenantKey {
    req.headers()
        .get("X-Tenant-ID")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(TenantKey::from)
        .unwrap_or(TenantKey::from(0))  // Default si no hay header
}
```

### 4. Solicitar permiso antes de procesar

```rust
async fn my_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<String, AppError> {
    let tenant = extract_tenant(&req);
    
    // Crear tarea y encolar
    let task = Task {
        payload: WorkPermit { tenant },
        enqueue_ts: Instant::now(),
        deadline: None,
        priority: Default::default(),
        cost: 1,
    };
    
    // Encolar
    match state.scheduler.enqueue(tenant, task) {
        EnqueueResult::Enqueued => {}
        EnqueueResult::Rejected(_) => return Err(AppError::TooManyRequests),
        EnqueueResult::Closed => return Err(AppError::ServiceUnavailable),
    }
    
    // Esperar nuestro turno
    match state.scheduler.dequeue_async().await {
        DequeueResult::Task { .. } => {
            // Ahora podemos procesar el request
            Ok("Processed!".to_string())
        }
        _ => Err(AppError::Internal)
    }
}
```

### 5. Manejo de Errores

```rust
enum AppError {
    TooManyRequests,      // 429
    ServiceUnavailable,   // 503
    Internal,             // 500
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "Too many requests"})),
            ).into_response(),
            // ...
        }
    }
}
```

## Ejecutar el Ejemplo

```bash
# Compilar
cargo build --bin axum_simple

# Ejecutar
cargo run --bin axum_simple

# Pruebas desde otra terminal
curl -H "X-Tenant-ID: 1" http://127.0.0.1:3000/
curl -H "X-Tenant-ID: 2" http://127.0.0.1:3000/api/users
curl http://127.0.0.1:3000/api/stats  # Ver métricas
```

## Simular Carga (Hot Tenant)

```bash
# Saturar tenant 1 (verás 429s)
for i in {1..50}; do curl -H "X-Tenant-ID: 1" http://127.0.0.1:3000/ & done

# Tenant 2 sigue funcionando (fairness)
curl -H "X-Tenant-ID: 2" http://127.0.0.1:3000/
```

## Métricas Disponibles

`GET /api/stats` retorna:

```json
{
  "enqueued": 150,
  "dequeued": 140,
  "dropped": 10,  // Requests rechazados por backpressure
  "expired": 0,
  "queue_len": 0,
  "avg_queue_time_ms": 5,
  "p95_queue_time_ms": 12,
  "p99_queue_time_ms": 18,
  "top_tenants": [
    {"tenant_id": 1, "count": 80},
    {"tenant_id": 2, "count": 60}
  ]
}
```

## Configuraciones Comunes

### Política Agresiva (Drop Oldest)

```rust
backpressure: BackpressurePolicy::DropOldestPerTenant,
```

En lugar de rechazar (429), descarta la tarea más antigua cuando el tenant está lleno.

### Quantums Diferenciados (Premium vs Free)

```rust
let mut quantum_by_tenant = HashMap::new();
quantum_by_tenant.insert(TenantKey::from(1), 50);  // Premium: más presupuesto
quantum_by_tenant.insert(TenantKey::from(2), 5);   // Free: menos

SchedulerConfig {
    quantum_by_tenant,
    ..config
}
```

### Timeout en lugar de Reject

```rust
backpressure: BackpressurePolicy::Timeout {
    wait: Duration::from_secs(5)
},
```

En lugar de rechazar inmediatamente, espera hasta 5 segundos por capacidad.

## Notas

- **`firq-tower`** existe pero está diseñado para escenarios específicos. Este enfoque manual te da más control.
- El scheduler es **compartido** entre todos los handlers vía `State`.
- El `dequeue_async()` **no hace polling** - usa señales internas que despiertan cuando hay trabajo.

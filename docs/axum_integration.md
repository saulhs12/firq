# Firq + Axum / Tower — Integracion de produccion

Este documento define el contrato operativo de `firq-tower` para Axum/Tower en v1:

- Fairness y backpressure pasan por `firq-core`.
- `firq-tower` integra:
  - extraccion de tenant,
  - cancelacion de request antes de turno,
  - deadlines por request,
  - mapeo HTTP estable de rechazos,
  - limite real de ejecucion concurrente del handler (`in_flight`).

## Contrato de ejecucion

1. `poll_ready`
- `FirqService::poll_ready` delega al servicio interior.
- No encola trabajo; solo refleja readiness del servicio real.

2. `call`
- Extrae `TenantKey`.
- Construye `Task` con `deadline` (si existe extractor).
- Encola usando `enqueue_with_handle`.

3. Espera de turno
- El request espera un permiso por `oneshot`.
- Si el cliente aborta antes de recibir permiso, el `Drop` del future cancela el `TaskHandle`.

4. Ejecucion del handler
- Tras recibir turno, el middleware adquiere un permiso de `Semaphore` (`in_flight_limit`).
- El handler corre manteniendo ese permiso hasta completar.

## Mapeo HTTP default de rechazo

`firq-tower` expone un mapper configurable. El default v1 es:

- `TenantFull` -> `429`
- `GlobalFull` -> `503`
- `Timeout` -> `503`

## Schema JSON estable de rechazo

Campos:

- `status`: codigo HTTP (u16)
- `code`: codigo estable (`tenant_full`, `global_full`, `timeout`)
- `message`: descripcion corta
- `reason`: enum interno (`TenantFull`, `GlobalFull`, `Timeout`)

Ejemplo:

```json
{
  "status": 429,
  "code": "tenant_full",
  "message": "tenant queue saturated",
  "reason": "TenantFull"
}
```

## Ejemplo de configuracion en Axum

```rust
use axum::extract::Request;
use firq_tower::{EnqueueRejectReason, Firq, FirqHttpRejection, TenantKey};

let layer = Firq::new()
    .with_shards(4)
    .with_max_global(1000)
    .with_max_per_tenant(100)
    .with_quantum(10)
    .with_in_flight_limit(128)
    .with_deadline_extractor::<Request, _>(|req| {
        req.headers()
            .get("X-Deadline-Ms")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms))
    })
    .with_rejection_mapper(|reason| match reason {
        EnqueueRejectReason::TenantFull => FirqHttpRejection {
            status: 429,
            code: "tenant_full",
            message: "tenant queue saturated",
            reason,
        },
        EnqueueRejectReason::GlobalFull => FirqHttpRejection {
            status: 503,
            code: "global_full",
            message: "service queue saturated",
            reason,
        },
        EnqueueRejectReason::Timeout => FirqHttpRejection {
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
```

## Notas operativas

- `close_immediate`: deja de admitir y corta dispatch inmediatamente.
- `close_drain`: deja de admitir y drena pendientes hasta vaciar cola.
- Con `close_drain`, los requests ya encolados siguen pudiendo completar.

# Migracion a v1 (desde v0.x)

## Cambios de API principales

## 1) Enqueue con cancelacion

Antes:

```rust
let result = scheduler.enqueue(tenant, task);
```

Ahora (opcional):

```rust
let result = scheduler.enqueue_with_handle(tenant, task);
// Enqueued(handle) -> puede cancelarse luego
```

Cancelacion:

```rust
if let EnqueueWithHandleResult::Enqueued(handle) = result {
    let _ = scheduler.cancel(handle);
}
```

## 2) Cierre con modo explicito

Antes:

```rust
scheduler.close();
```

Ahora:

```rust
scheduler.close_immediate(); // equivalente a close()
scheduler.close_drain();     // no admite nuevas, drena pendientes
```

`close()` se mantiene como alias de `close_immediate()` para compatibilidad.

## 3) Rechazos por timeout

`EnqueueRejectReason` ahora incluye:

- `Timeout`

Si usabas `match` exhaustivo debes agregar este caso.

## 4) SchedulerStats extendido

Nuevos campos:

- `rejected_global`
- `rejected_tenant`
- `timeout_rejected`
- `dropped_policy`
- `max_global`
- `queue_saturation_ratio`

Si serializas/deserializas `SchedulerStats`, actualiza el schema.

## 5) firq-tower builder

Nuevas opciones:

- `with_in_flight_limit(usize)`
- `with_deadline_extractor::<Request, _>(...)`
- `with_rejection_mapper(...)`

Rechazos now expose `FirqHttpRejection` with stable fields:

- `status`
- `code`
- `message`
- `reason`

## Compatibilidad recomendada

- Mantener `enqueue(...)` cuando no necesites cancelacion.
- Migrar a `close_drain()` para shutdown ordenado en servicios HTTP.
- Ajustar handling de errores Tower para mapear `FirqError::Rejected(FirqHttpRejection)`.

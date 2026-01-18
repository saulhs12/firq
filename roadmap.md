

# Firq — Roadmap

Este documento define:

1) Qué existe hoy (estado actual del repo).
2) Qué falta para completar el **MVP v0.1**.
3) Qué viene después (features, mejoras, hardening, DX).

> Nota: El MVP v0.1 está definido en `docs/design.md` como:
> - Fairness: DRR por tenant
> - Backpressure: Reject
> - Deadlines: DropExpired en dequeue
> - Métricas mínimas: enqueued, dequeued, dropped, expired, queue_len_estimate, queue_time

---

## 0) Estado actual (lo que existe hasta ahora)

### Documentación
- `docs/design.md`:
  - Arquitectura del workspace.
  - Responsabilidad de cada crate.
  - Decisiones cerradas del MVP v0.1.
  - Diseño interno del core (tenant queues, active_ring, sharding).
  - Definición técnica de DRR, DropExpired y Reject.
  - Métricas mínimas e invariantes.

### Código / scaffolding
- Workspace multi-crate (esperado):
  - `firq-core`
  - `firq-async`
  - `firq-tower`
  - `firq-examples`
  - `firq-bench`

> Si aún no están creados los crates/archivos, este roadmap asume esa estructura como base.

---

## 1) MVP v0.1 — checklist detallado

Objetivo: una librería funcional, testeada, con ejemplos y microbench básicos.

### 1.1 API pública mínima (core)
- [ ] `TenantKey` (hash estable, Copy/Clone, Eq/Hash).
- [ ] `Task<T>` con:
  - [ ] `enqueue_ts` (para queue_time)
  - [ ] `deadline: Option<Instant>`
  - [ ] `cost` (>= 1)
- [ ] `SchedulerConfig`:
  - [ ] `shards`, `max_global`, `max_per_tenant`, `quantum`, `backpressure=Reject`
- [ ] `EnqueueResult` / `DequeueResult`:
  - [ ] `enqueue` retorna rechazo explícito por saturación / tenant full / cerrado.
  - [ ] `dequeue` solo retorna `Task`, `Empty`, `Closed` (las expiradas se descartan internamente).
- [ ] `SchedulerStats` con:
  - [ ] `enqueued`, `dequeued`, `dropped`, `expired`, `queue_len_estimate`, `queue_time_*`

### 1.2 Estructuras internas (core)
- [ ] `TenantState`:
  - [ ] `VecDeque<Task<T>>`
  - [ ] `deficit` y `quantum`
  - [ ] `active` flag
- [ ] `active_ring` por shard:
  - [ ] Inserta tenant cuando pasa de vacío → no vacío.
  - [ ] Marca inactivo cuando queda vacío.
- [ ] Sharding:
  - [ ] `HashMap<TenantKey, TenantState>` por shard.
  - [ ] `tenant -> shard = hash % shards`.

### 1.3 Algoritmo DRR (core)
- [ ] Implementar selección DRR:
  - [ ] `deficit += quantum` cuando no alcanza para el `cost` del frente.
  - [ ] Entregar tarea cuando `deficit >= cost` y descontar.
  - [ ] Reinsertar tenant al final del ring si sigue con cola.
- [ ] Garantizar no-starvation (mediante tests).

### 1.4 Deadlines: DropExpired (core)
- [ ] En `try_dequeue`:
  - [ ] Antes de intentar despachar, limpiar expiradas del frente.
  - [ ] Incrementar `expired` y ajustar `queue_len_estimate`.
  - [ ] Nunca devolver una tarea expirada.

### 1.5 Backpressure: Reject (core)
- [ ] En `enqueue`:
  - [ ] Rechazar si `queue_len_estimate >= max_global`.
  - [ ] Rechazar si `tenant_queue_len >= max_per_tenant`.
  - [ ] Incrementar `dropped` cuando se rechaza.

### 1.6 Señalización sin polling (core)
- [ ] Implementar evento “hay trabajo”:
  - [ ] Notificar cuando entra trabajo (especialmente vacío → no vacío).
  - [ ] `dequeue_blocking` espera sin busy-loop.
  - [ ] Evitar “lost wakeups” (re-check antes de esperar).

### 1.7 Métricas mínimas (core)
- [ ] Contadores atómicos:
  - [ ] `enqueued`, `dequeued`, `dropped`, `expired`
  - [ ] `queue_len_estimate`
- [ ] Queue time:
  - [ ] al entregar: `now - enqueue_ts`
  - [ ] acumular `queue_time_sum_ns` y `queue_time_samples`

### 1.8 Tests (core)
- [ ] Unit tests:
  - [ ] No entrega expiradas.
  - [ ] Rechaza por `max_global`.
  - [ ] Rechaza por `max_per_tenant`.
  - [ ] Fairness básica: tenant B progresa aun si A está “hot”.
- [ ] Property tests (proptest) básicos:
  - [ ] Invariante: `queue_len_estimate` nunca negativo.
  - [ ] Invariante: `dequeued + expired + dropped <= enqueued + dropped` (según definición exacta).

### 1.9 `firq-async` (adaptador Tokio)
- [ ] `AsyncScheduler<T>` que wrappea `Arc<Scheduler<T>>`.
- [ ] `dequeue_async().await` usando la señalización del core (sin polling).
- [ ] (Opcional en v0.1) Dispatcher simple:
  - [ ] Límite de in-flight con `Semaphore`.

### 1.10 Ejemplos (`firq-examples`)
- [ ] Ejemplo sync:
  - [ ] 2+ tenants, uno hot, demostrar fairness.
  - [ ] imprimir stats al final.
- [ ] Ejemplo async:
  - [ ] productor async + consumidor async con `dequeue_async`.

### 1.11 Bench (`firq-bench`)
- [ ] Escenario “hot tenant vs many tenants”:
  - [ ] medir throughput
  - [ ] medir queue_time promedio (sum/samples)
  - [ ] registrar drops/expired

### 1.12 Calidad y DX
- [ ] `cargo fmt`.
- [ ] `cargo clippy -D warnings`.
- [ ] CI mínimo (fmt, clippy, test).

---

## 2) Post-MVP (v0.2+): features y mejoras

### 2.1 Backpressure avanzado
- [ ] `DropOldestPerTenant`.
- [ ] `DropNewestPerTenant`.
- [ ] `Timeout` (espera por capacidad con límite de tiempo).
- [ ] Políticas por tenant/plan (ej: premium vs free).

### 2.2 Pesos por tenant (QoS)
- [ ] `quantum` por tenant:
  - [ ] configurado por tabla/closure.
  - [ ] update dinámico (hot reload).

### 2.3 Priorización
- [ ] Prioridades discretas (High/Normal/Low).
- [ ] Fairness dentro de prioridad.
- [ ] Shed de baja prioridad bajo presión.

### 2.4 Métricas avanzadas
- [ ] Histogramas de queue_time.
- [ ] Percentiles p95/p99.
- [ ] Métricas por tenant (top talkers) con estructura acotada.
- [ ] Export Prometheus (posible en crate separado).

### 2.5 Optimizaciones de performance
- [ ] Evitar barrer shards en `try_dequeue` (cursor global o estrategia de selección).
- [ ] Reducción de locks (parking_lot vs std; granularidad).
- [ ] Mejoras en estructuras (ring más eficiente, reuso de allocations).
- [ ] Benchmarks comparativos (baseline FIFO vs Firq).

### 2.6 `firq-tower` completo
- [ ] Layer configurable:
  - [ ] extracción de tenant key
  - [ ] límites por ruta
  - [ ] respuestas 429/503 + headers
- [ ] Ejemplo Axum end-to-end.

### 2.7 Casos de uso “producto”
- [ ] Webhook dispatcher completo.
- [ ] Fan-out service example.
- [ ] Gate de concurrencia por endpoint.

---

## 3) Fixes / hardening típicos (a rastrear desde el inicio)

### Correctness
- [ ] Evitar starvation en casos borde (costs grandes + quantum pequeño).
- [ ] Manejo correcto de overflow en contadores.
- [ ] Cierre (`close`) seguro: no deadlocks, despertar a todos.
- [ ] Evitar livelock bajo presión.

### Concurrency
- [ ] Revisar `Ordering` de atomics (Relaxed vs Acquire/Release donde aplique).
- [ ] Confirmar que la señalización no pierde wakeups.

### API stability
- [ ] Minimizar breaking changes tras v0.1.
- [ ] Documentar semántica exacta de cada métrica.

---

## 4) Milestones sugeridos

### Milestone A — Core funcional (pre-v0.1)
- API core definida + compila.
- DRR + DropExpired + Reject implementados.
- dequeue_blocking sin polling.

### Milestone B — MVP v0.1
- Tests mínimos + ejemplos sync/async.
- Stats mínimas.
- Bench básico.
- Release v0.1.0.

### Milestone C — v0.2 (valor de producto)
- DropOldestPerTenant + pesos por tenant.
- Métricas mejoradas.
- `firq-tower` usable.

---

## 5) Definición de “MVP completo” (criterios de salida)

El MVP v0.1 se considera completo cuando:

- DRR funciona y pasa tests de fairness.
- No se entregan expiradas (DropExpired).
- Reject funciona global y por tenant.
- Señalización sin polling (sync y async).
- Stats mínimas correctas.
- Ejemplos sync/async corren y demuestran el comportamiento.
- Bench básico corre y produce números reproducibles.
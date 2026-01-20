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

Decisión arquitectónica explícita:
- `firq-async` se construye exclusivamente sobre Tokio.
- Tower NO es dependencia del core ni del async layer.
- Tower se usa solo como integración opcional en `firq-tower`.

---

## 1) MVP v0.1 — checklist detallado

Objetivo: una librería funcional, testeada, con ejemplos y microbench básicos.

### 1.1 API pública mínima (core)
- [x] `TenantKey` (hash estable, Copy/Clone, Eq/Hash).
- [x] `Task<T>` con:
  - [x] `enqueue_ts` (para queue_time)
  - [x] `deadline: Option<Instant>`
  - [x] `cost` (>= 1)
- [x] `SchedulerConfig`:
  - [x] `shards`, `max_global`, `max_per_tenant`, `quantum`, `backpressure=Reject`
- [x] `EnqueueResult` / `DequeueResult`:
  - [x] `enqueue` retorna rechazo explícito por saturación / tenant full / cerrado.
  - [x] `dequeue` solo retorna `Task`, `Empty`, `Closed` (las expiradas se descartan internamente).
- [x] `SchedulerStats` con:
  - [x] `enqueued`, `dequeued`, `dropped`, `expired`, `queue_len_estimate`, `queue_time_*`

### 1.2 Estructuras internas (core)
- [x] `TenantState`:
  - [x] `VecDeque<Task<T>>`
  - [x] `deficit` y `quantum`
  - [x] `active` flag
- [x] `active_ring` por shard:
  - [x] Inserta tenant cuando pasa de vacío → no vacío.
  - [x] Marca inactivo cuando queda vacío.
- [x] Sharding:
  - [x] `HashMap<TenantKey, TenantState>` por shard.
  - [x] `tenant -> shard = hash % shards`.

### 1.3 Algoritmo DRR (core)
- [x] Implementar selección DRR:
  - [x] `deficit += quantum` cuando no alcanza para el `cost` del frente.
  - [x] Entregar tarea cuando `deficit >= cost` y descontar.
  - [x] Reinsertar tenant al final del ring si sigue con cola.
- [ ] Garantizar no-starvation (mediante tests).

### 1.4 Deadlines: DropExpired (core)
- [x] En `try_dequeue`:
  - [x] Antes de intentar despachar, limpiar expiradas del frente.
  - [x] Incrementar `expired` y ajustar `queue_len_estimate`.
  - [x] Nunca devolver una tarea expirada.

### 1.5 Backpressure: Reject (core)
- [x] En `enqueue`:
  - [x] Rechazar si `queue_len_estimate >= max_global`.
  - [x] Rechazar si `tenant_queue_len >= max_per_tenant`.
  - [x] Incrementar `dropped` cuando se rechaza.

### 1.6 Señalización sin polling (core)
- [x] Implementar evento “hay trabajo”:
  - [x] Notificar cuando entra trabajo (especialmente vacío → no vacío).
  - [x] `dequeue_blocking` espera sin busy-loop.
  - [x] Evitar “lost wakeups” (re-check antes de esperar).

### 1.7 Métricas mínimas (core)
- [x] Contadores atómicos:
  - [x] `enqueued`, `dequeued`, `dropped`, `expired`
  - [x] `queue_len_estimate`
- [x] Queue time:
  - [x] al entregar: `now - enqueue_ts`
  - [x] acumular `queue_time_sum_ns` y `queue_time_samples`

### 1.8 Tests (core)
- [x] Unit tests:
  - [x] No entrega expiradas.
  - [x] Rechaza por `max_global`.
  - [x] Rechaza por `max_per_tenant`.
  - [x] Fairness básica: tenant B progresa aun si A está “hot”.
- [x] Property tests (proptest) básicos:
  - [x] Invariante: `queue_len_estimate` nunca negativo.
  - [x] Invariante: `dequeued + expired + dropped <= enqueued + dropped` (según definición exacta).

### 1.9 `firq-async` (adaptador Tokio, alcance explícito)

Alcance del crate:
- `firq-async` NO implementa algoritmos de scheduling.
- `firq-async` NO redefine fairness, backpressure ni deadlines.
- `firq-async` es únicamente un adaptador async sobre `firq-core`.

Decisiones técnicas cerradas (v0.1):
- Runtime obligatorio: **Tokio**.
- No dependencia de Tower en este crate.
- API async estable, mínima y predecible.

Responsabilidades:
- Exponer una API async mínima (`dequeue_async`) sobre `Scheduler`.
- Integrarse con la señalización del core sin polling.
- Facilitar ejecución concurrente controlada (in‑flight).

Componentes mínimos a implementar:
- [x] `AsyncScheduler<T>`:
      - wrappea `Arc<Scheduler<T>>`
      - clonable y `Send + Sync`
- [x] `enqueue(&self, tenant, task) -> EnqueueResult`
      - delega directamente al core (no lógica adicional)
- [x] `dequeue_async(&self) -> DequeueResult<T>`
      - espera async hasta que:
          - haya trabajo
          - el scheduler esté cerrado
      - puente a `dequeue_blocking` con `spawn_blocking` (sin polling)
- [x] `AsyncReceiver` / `AsyncStream`
      - `recv().await` y `Stream<Item = DequeueItem<T>>`
- [x] Integración con señalización del core:
      - puente a `dequeue_blocking` con `spawn_blocking`

Concurrencia (v0.1):
- [x] Dispatcher opcional:
      - workers con `tokio::spawn`
      - límite de in‑flight usando `tokio::sync::Semaphore`
      - sin retries automáticos
      - sin prioridades adicionales

Explícitamente fuera de alcance en v0.1:
- Timeouts de enqueue.
- Cancelación de tareas en ejecución.
- Integración HTTP / RPC.
- Métricas async adicionales (solo se exponen las del core).

### 1.10 Ejemplos (`firq-examples`)
- [x] Ejemplo sync:
  - [x] 2+ tenants, uno hot, demostrar fairness.
  - [x] imprimir stats al final.
- [x] Ejemplo async:
  - [x] productor async + consumidor async con `dequeue_async`.

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

### 2.6 `firq-tower` (integración Tower / HTTP – post‑MVP)

Estado:
- NO forma parte del MVP v0.1.
- Se implementa únicamente después de estabilizar `firq-core` y `firq-async`.

Rol del crate:
- Adaptador entre Tower (`Service`, `Layer`) y `firq-async`.
- Permite usar Firq como middleware en stacks HTTP/RPC (Axum, Hyper).

Responsabilidades:
- Extraer `TenantKey` desde:
      - headers
      - path
      - extensión del request
      - closure definida por el usuario
- Encolar requests como `Task`.
- Esperar dequeue y ejecutar el handler real.
- Traducir rechazos a respuestas HTTP (429 / 503).

Explícitamente fuera de alcance:
- Lógica de scheduling.
- Gestión de runtime async.
- Métricas propias (solo expone las del core).

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

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
- [x] Garantizar no-starvation (mediante tests).

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
- Re-exportar tipos comunes del core para ergonomía (uso solo `firq-async`).

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
- [x] Re-exports de tipos del core:
      - `Task`, `TenantKey`, `Scheduler`, `SchedulerConfig`, `BackpressurePolicy`, `EnqueueResult`, `DequeueResult`

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
  - [x] imports solo desde `firq-async` (sin `firq-core`).

### 1.11 Bench (`firq-bench`)
- [x] Escenario “hot tenant vs many tenants”:
  - [x] medir throughput
  - [x] medir queue_time promedio (sum/samples)
  - [x] registrar drops/expired

### 1.12 Calidad y DX
- [x] `cargo fmt`.
- [x] `cargo clippy -D warnings`.
- [x] CI mínimo (fmt, clippy, test).

---

## 2) Post-MVP (v0.2+): features y mejoras

### 2.1 Backpressure avanzado
- [x] `DropOldestPerTenant`.
- [x] `DropNewestPerTenant`.
- [x] `Timeout` (espera por capacidad con límite de tiempo).
- [x] Políticas por tenant/plan (ej: premium vs free).

### 2.2 Pesos por tenant (QoS)
- [x] `quantum` por tenant:
  - [x] configurado por tabla/closure.
  - [x] update dinámico (hot reload).

### 2.3 Priorización
- [x] Prioridades discretas (High/Normal/Low).
- [x] Fairness dentro de prioridad.
- [x] Shed de baja prioridad bajo presión.

### 2.4 Métricas avanzadas
- [x] Histogramas de queue_time.
- [x] Percentiles p95/p99.
- [x] Métricas por tenant (top talkers) con estructura acotada.
- [x] Export Prometheus (posible en crate separado).

### 2.5 Optimizaciones de performance
- [x] Evitar barrer shards en `try_dequeue` (cursor global o estrategia de selección).
- [x] Reducción de locks (parking_lot vs std; granularidad).
- [x] Mejoras en estructuras (ring más eficiente, reuso de allocations).
- [x] Benchmarks comparativos (baseline FIFO vs Firq).

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
- [x] Evitar starvation en casos borde (costs grandes + quantum pequeño).
- [x] Manejo correcto de overflow en contadores.
- [x] Cierre (`close`) seguro: no deadlocks, despertar a todos.
- [x] Evitar livelock bajo presión.

### Concurrency
- [x] Revisar `Ordering` de atomics (Relaxed vs Acquire/Release donde aplique).
- [x] Confirmar que la señalización no pierde wakeups.

### API stability
- [x] Minimizar breaking changes tras v0.1.
- [x] Documentar semántica exacta de cada métrica.

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

## 5) Cierre técnico hacia v1 (producción)

Objetivo de esta etapa:
- Pasar de “funciona en casos felices” a “correcto, estable y operable bajo carga adversa”.
- Cerrar riesgos de concurrencia que impactan fairness real, backpressure efectivo y tail latency.
- Establecer criterios de aceptación cuantitativos para validar p95/p99 y resiliencia.

### 5.1 Blockers de correctness y concurrencia (prioridad crítica)
- [x] Corregir la carrera de activación/desactivación de shard en el core (`active_shards` + `shard_active`) para eliminar la posibilidad de trabajo encolado no visible temporalmente para consumidores.
- [x] Reemplazar el control global de capacidad basado en `load` + verificación por un mecanismo de admisión estricta (CAS de reserva o semáforo global) que garantice `queue_len_estimate <= max_global` incluso con múltiples productores concurrentes.
- [x] Definir y documentar un invariante formal de estado activo por shard: “si existe al menos una cola no vacía en el shard, el shard debe estar presente en el ring global o marcado para reactivación atómica”.
- [x] Agregar pruebas de estrés multi-shard (productores/consumidores concurrentes) que reproduzcan y prevengan regresiones de los dos puntos anteriores.
- [x] Validar ausencia de livelock bajo combinación de `cost` alto + `quantum` bajo + expiraciones frecuentes.

### 5.2 Semántica de `firq-tower` para fairness/backpressure efectivo
- [x] Alinear `firq-tower` con control de in-flight real del handler para que la admisión no sea solo “permiso lógico” sino límite efectivo de ejecución concurrente.
- [x] Agregar cancelación explícita de trabajo encolado cuando el request se aborta/desconecta antes de obtener turno.
- [x] Propagar deadlines del request al `Task.deadline` y mapear expiración a respuesta HTTP configurable.
- [x] Definir mapeo configurable de rechazo por política (`GlobalFull`, `TenantFull`, timeout) a códigos HTTP (`429`, `503`) con cuerpo estructurado estable.
- [x] Documentar contrato de integración Tower/Axum respecto a `poll_ready`, backpressure y orden de ejecución.

### 5.3 Hardening de `firq-async`
- [x] Incorporar tests unitarios e integración para `AsyncScheduler`, `AsyncReceiver`, `AsyncStream` y `Dispatcher`.
- [x] Verificar semántica de cierre: `close()` debe drenar/terminar sin tareas huérfanas ni esperas indefinidas.
- [x] Definir y probar comportamiento de `Dispatcher` ante panic/fallo del handler para evitar fuga de permisos.
- [x] Medir y documentar costo de `spawn_blocking` por dequeue bajo alta concurrencia.

### 5.4 Observabilidad de operación (SLO/SLA)
- [x] Expandir `SchedulerStats` con contadores por causa: `rejected_global`, `rejected_tenant`, `dropped_policy`, `timeout_rejected`.
- [x] Agregar señal explícita de saturación (`queue_len/max_global`, `in_flight/max_in_flight`) para alerting.
- [x] Mantener histograma de queue time como fuente de verdad de percentiles y documentar precisión/limitaciones.
- [x] Añadir ejemplos de dashboard Prometheus para p50/p95/p99 y top talkers.

### 5.5 Benchmarks de cierre (evidencia de estabilidad p99)
- [x] Definir suite reproducible de escenarios: hot tenant sostenido, burst masivo, mezcla de prioridades, expiración por deadline y presión de capacidad.
- [x] Reportar `throughput`, `drop rate`, `expired rate`, `queue_time p50/p95/p99` por escenario.
- [x] Comparar Firq vs baseline FIFO bajo la misma carga y publicar metodología exacta (parámetros, duración, workers, hardware).
- [x] Establecer umbrales de aceptación para declarar estabilidad de p99 bajo carga adversa.

### 5.6 Calidad y CI (estado real vs checklist)
- [x] Reconciliar checklist de calidad con el estado real de CI y corregir cualquier ítem marcado como completado sin evidencia actual.
- [x] Dejar verde: `cargo fmt --all -- --check`.
- [x] Dejar verde: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Dejar verde: tests dirigidos por crate/escenario (`firq-core`, `firq-async`, `firq-tower` integración).
- [x] Refactorizar `firq-bench` para cumplir clippy estricto sin `allow` global.

### 5.7 Preparación de release y publicación
- [x] Completar metadata de `Cargo.toml` en todos los crates: `description`, `license`, `repository`, `documentation`, `homepage` (si aplica), `readme`, `keywords`, `categories`.
- [x] Asegurar que dependencias internas `path` tengan también `version` para empaquetado/publicación.
- [x] Validar empaquetado local por crate con `cargo package --allow-dirty --no-verify` y resolver bloqueos.
- [x] Definir orden de publicación escalonado (`firq-core` -> `firq-async` -> `firq-tower`) para evitar resolución fallida de dependencias internas no publicadas.
- [x] Definir política semver y changelog de release.
- [x] Publicar guía de migración para cambios de API entre v0.x y v1.

### 5.8 Cambios de API públicos planificados
- [x] Agregar API de cancelación de tareas pendientes (`enqueue_with_handle` + `cancel`) para integraciones HTTP y shutdown robusto.
- [x] Agregar variante de cierre configurable (`close_immediate` / `close_drain`) manteniendo `close()` por compatibilidad.
- [x] Extender builder de `firq-tower` con límites de in-flight, extractor de deadline y mapeador de rechazos.
- [x] Documentar garantías de compatibilidad y estrategia de deprecación.

### 5.9 Estrategia de pruebas de aceptación (gate de v1)
- [x] Pruebas de concurrencia intensiva con `shards > 1`, múltiples productores y consumidores.
- [x] Pruebas de no-starvation con tenants “hot” y “cold” mezclando `cost` heterogéneo.
- [x] Pruebas de exactitud de métricas para enqueue/dequeue/drop/expire bajo carrera.
- [x] Pruebas de integración Tower/Axum con cancelación de cliente y deadlines.
- [x] Pruebas de robustez de cierre con consumidores bloqueados y dispatcher async activo.

### 5.10 Definition of Done de v1
- [x] No existen violaciones observadas de invariantes de cola en pruebas de estrés prolongadas.
- [x] `clippy -D warnings`, `fmt --check` y suites dirigidas por crate (sin gate `test --workspace`) pasan de forma consistente.
- [x] Benchmarks documentados muestran mejora clara frente a FIFO en escenarios noisy-neighbor.
- [x] API pública y semántica de métricas están documentadas con precisión y ejemplos ejecutables.
- [x] Crates con metadata y versionado coherente para publicación escalonada por orden de dependencias.

### 5.11 Cambios importantes en interfaces públicas (a reflejar en roadmap)
1. `Scheduler`: APIs de cancelación y cierre con modo de drenado.
2. `SchedulerStats`: contadores desagregados por causa de rechazo/drop/timeout.
3. `firq-tower::Firq` builder: `in_flight_limit`, extractor de deadline y mapeador HTTP de rechazo.

### 5.12 Casos de prueba que deben quedar explícitos en roadmap
1. Estrés concurrente multi-shard con productores/consumidores simultáneos.
2. Invariantes de capacidad global bajo carrera.
3. No-starvation con `cost` heterogéneo y prioridades mezcladas.
4. Cancelación de request en integración Axum/Tower.
5. Cierre ordenado con workers sync y dispatcher async activos.

Estado: cerrado. El caso 4 quedó validado con `test_abort_before_handler_turn_keeps_second_request_unexecuted` en `crates/firq-tower/tests/integration.rs`.

### 5.13 Supuestos y defaults elegidos
1. Alcance objetivo: librería in-process single-node (no cola distribuida/persistente).
2. Runtime async objetivo: Tokio.
3. En v1, correctness y semántica operacional tienen prioridad sobre microoptimizaciones.
4. En este cierre se permiten breaking changes controlados para acelerar hardening técnico.

---

## 6) Milestones de cierre (nuevos)

### Milestone D — Correctness & Concurrency
- Corregidos blockers de activación de shard y admisión global estricta.
- Tests multi-shard de estrés en verde.
- Invariantes documentados y verificados.

### Milestone E — Tower/Async Production Semantics
- `firq-tower` con control de in-flight real y cancelación.
- Deadlines propagadas desde request.
- Integración y contratos HTTP documentados.

### Milestone F — Observabilidad & Performance Evidence
- Métricas extendidas por causa.
- Bench suite reproducible con p50/p95/p99.
- Reporte comparativo Firq vs FIFO publicado.

### Milestone G — Release Readiness
- CI completamente verde con gates estrictos.
- Metadata y empaquetado de crates listos.
- Changelog, semver y documentación de uso/migración completos.

---

## 7) Definición de “MVP completo” (criterios de salida)

El MVP v0.1 se considera completo cuando:

- DRR funciona y pasa tests de fairness.
- No se entregan expiradas (DropExpired).
- Reject funciona global y por tenant.
- Señalización sin polling (sync y async).
- Stats mínimas correctas.
- Ejemplos sync/async corren y demuestran el comportamiento.
- Bench básico corre y produce números reproducibles.

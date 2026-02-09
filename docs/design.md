

# Firq — Arquitectura y diseño técnico (MVP v0.1)

## Propósito

**Firq** es un *scheduler in-process* (embebible) para sistemas con alta concurrencia que necesitan:

- **Fairness multi-tenant** (evitar *noisy neighbor* / starvation).
- **Backpressure** (control explícito de memoria/cola bajo sobrecarga).
- **Deadlines** (descartar trabajo que ya no sirve).
- **Observabilidad** básica para operar (métricas del hot-path y del *queue time*).

El objetivo es mantener **estabilidad de latencia** (especialmente *tail latency*) cuando existe contención entre diferentes tenants/rutas/clientes.

---

## Decisiones cerradas del MVP v0.1

Estas decisiones son “no negociables” en la primera versión:

1) **Algoritmo de fairness:** DRR (*Deficit Round Robin*) por tenant.
2) **Backpressure:** solo `Reject`.
   - Si se exceden límites, `enqueue()` rechaza.
3) **Deadlines:** `DropExpired` en `dequeue()`.
   - Una tarea expirada **nunca** se entrega a consumidores.
4) **Métricas mínimas:**
   - `enqueued`, `dequeued`, `dropped`, `expired`
   - `queue_len_estimate`
   - `queue_time` (medición básica; histogramas/p99 vienen después).

---

## Arquitectura del workspace (crates y responsabilidades)

Firq se implementa como un **workspace multi-crate**. Cada crate tiene responsabilidades estrictas para evitar acoplamientos innecesarios.

### 1) `firq-core`

**Responsabilidad:** Contiene *toda la lógica del scheduler*.

- Runtime-agnostic: **NO** depende de Tokio.
- Implementa:
  - Enqueue con límites (backpressure Reject).
  - Dequeue con fairness DRR por tenant.
  - DropExpired en dequeue.
  - Estructuras internas (colas por tenant, ring de activos, sharding).
  - Señalización de “hay trabajo disponible” para evitar polling.
  - Métricas mínimas.

**Qué NO hace:**
- No ejecuta tareas.
- No hace I/O.
- No define transporte/servidor.
- No persiste ni distribuye (no es una job-queue distribuida).

### 2) `firq-async`

**Responsabilidad:** Adaptador Tokio para consumir el core en contextos async.

- Provee:
  - `dequeue_async().await` (sin polling) usando la señalización del core.
  - Opcional: `Stream`/dispatcher async.
  - Recomendación: control de “in-flight” con `Semaphore` para I/O masivo.

**Regla:** `firq-async` no re-implementa DRR ni la lógica de colas; solo *adapta* espera/ejecución.

### 3) `firq-tower`

**Responsabilidad:** Integración con Tower/Axum como middleware/layer.

- Calcula `TenantKey` desde el request (ej: `api_key`, `merchant_id`, `(tenant, route)`).
- Decide:
  - encolar trabajo,
  - o rechazar según backpressure,
  - o enrutar a ejecución controlada (según patrón que se defina).

**Objetivo:** hacer “plug-and-play” el uso de Firq en backends HTTP.

### 4) `firq-examples`

**Responsabilidad:** Ejemplos reproducibles y “realistas”.

- Sync worker pool.
- Async dispatcher I/O-bound.
- Casos: webhooks multi-tenant, fan-out, gate por ruta.

### 5) `firq-bench`

**Responsabilidad:** Microbench y escenarios de carga.

- Throughput (ops/s).
- Contención (shards/locks).
- Efecto de un tenant “hot”.
- Queue time básico (y posteriormente p95/p99).

---

## Modelo conceptual

### Tenant / TenantKey

Un **tenant** es la unidad de fairness y aislamiento.

Ejemplos de `TenantKey`:
- `tenant_id` (SaaS)
- `merchant_id` (pagos/webhooks)
- `api_key` (API pública)
- `(tenant_id, route_id)` (fairness por ruta)
- `provider` (downstream externo)

**Regla:** la clave debe representar la entidad que compite por recursos.

### Task

Una `Task<T>` encapsula el trabajo (payload) y metadatos necesarios para control:

- `payload: T` — trabajo real.
- `enqueue_ts` — timestamp de entrada para medir `queue_time`.
- `deadline: Option<Instant>` — si venció al despachar => `DropExpired`.
- `cost: u64` — peso de la tarea para DRR.

**Semántica de `cost`:**
- Si `cost` es mayor, esa tarea consume más presupuesto DRR.
- Esto evita que tareas “pesadas” degraden fairness.

---

## API pública (MVP v0.1)

### `SchedulerConfig`

- `shards: usize`
  - Particiona el estado para reducir contención.
- `max_global: usize`
  - Límite global de tareas encoladas.
- `max_per_tenant: usize`
  - Límite por tenant.
- `quantum: u64`
  - Presupuesto base por ronda DRR.
- `backpressure: Reject`
  - Única política en v0.1.

### `Scheduler<T>`

- `enqueue(tenant, task) -> EnqueueResult`
  - Acepta y encola o rechaza por límites/estado.
- `try_dequeue() -> DequeueResult<T>`
  - No bloquea.
  - Aplica DropExpired y DRR.
- `dequeue_blocking() -> DequeueResult<T>`
  - Bloquea esperando trabajo (sin polling) o retorna `Closed`.
- `stats() -> SchedulerStats`
  - Devuelve métricas mínimas.
- `close()`
  - Cierra el scheduler y despierta a consumidores bloqueados.

---

## Diseño interno del core (firq-core)

### Estado por tenant

Cada tenant tiene un `TenantState`:

- `queue: VecDeque<Task<T>>` — FIFO por tenant.
- `deficit: i64` — presupuesto acumulado.
- `quantum: i64` — asignación por ronda (v0.1 puede ser global).
- `active: bool` — si está en el ring de activos.

### Ring de tenants activos

`active_ring` contiene tenants con `queue` no vacía.

- Cuando un tenant pasa de cola vacía → no vacía, se inserta al ring.
- Si un tenant queda vacío tras despachar/limpiar expiradas, se marca inactivo.

### Sharding

El core mantiene `N` shards:

- `Shard[i]` contiene:
  - `HashMap<TenantKey, TenantState<T>>`
  - `active_ring` del shard

**Mapeo:** `tenant -> shard = hash(tenant) % shards`.

Objetivo: reducir contención de locks en cargas multi-tenant.

---

## Algoritmo DRR (Deficit Round Robin)

### Intuición

DRR reparte el servicio en rondas asignando un presupuesto (`quantum`) a cada tenant.

- Si un tenant no puede “pagar” el costo de su tarea actual, acumula presupuesto y cede turno.
- Con el tiempo, todos progresan.

### Pseudoflujo (alto nivel)

1) Seleccionar el siguiente tenant desde `active_ring`.
2) **DropExpired:** eliminar del frente todas las tareas expiradas.
   - Incrementar métricas `expired` y decrementar el contador de cola.
3) Si el tenant quedó vacío → desactivarlo y continuar.
4) Sea `c = cost(front_task)`.
5) Si `deficit < c`:
   - `deficit += quantum`.
   - mover el tenant al final del ring.
6) Si `deficit >= c`:
   - `deficit -= c`.
   - pop de la tarea y entregarla.
   - si aún tiene tareas → reinsertar al final del ring.
   - si quedó vacío → desactivarlo.

### Propiedades deseadas

- **No starvation:** un tenant con trabajo eventualmente progresa.
- **Fairness práctica:** un tenant “hot” no monopoliza.
- **Compatibilidad con costos:** tareas con distinto peso no rompen el reparto.

---

## Deadlines: DropExpired en dequeue

### Regla

- Si `task.deadline.is_some()` y `now > deadline` al intentar despachar, la tarea se **descarta**.
- Firq no devuelve una variante “Expired” en v0.1; simplemente incrementa métricas y continúa.

### Implicación

- Se evita hacer trabajo inútil.
- Se reduce presión downstream (menos I/O desperdiciado).

---

## Backpressure: Reject (v0.1)

### Límite global

- Si `queue_len_estimate >= max_global` ⇒ `enqueue()` rechaza.

### Límite por tenant

- Si `tenant_queue_len >= max_per_tenant` ⇒ `enqueue()` rechaza.

### Resultado

- Memoria estable.
- Degradación explícita.

---

## Señalización “hay trabajo” (sin polling)

El core expone un mecanismo para:

- despertar consumidores bloqueados (`dequeue_blocking`) cuando entra trabajo,
- permitir que el adaptador async espere (`dequeue_async`) sin polling.

**Requisito:** evitar “lost wakeups” (se re-checkea la condición antes de dormir/await).

---

## Métricas mínimas (definiciones)

- `enqueued`: tareas aceptadas.
- `dequeued`: tareas entregadas.
- `dropped`: tareas no aceptadas (backpressure Reject).
- `expired`: tareas descartadas por deadline vencido.
- `queue_len_estimate`: estimación de pendientes.
- `queue_time`:
  - al entregar una tarea: `now - enqueue_ts`.
  - v0.1: acumulación básica (sum + samples). Luego histogramas/p95/p99.
 - Semantica exacta: ver `docs/metrics.md`.

---

## Patrones de uso

### Sync (threads)

- Productores: `enqueue()` desde cualquier thread.
- Consumidores: un worker pool llama `dequeue_blocking()`.

**Mejor para:** CPU-bound, latencia estable, sistemas tradicionales.

### Async (Tokio)

- Productores: `enqueue()` desde handlers async.
- Consumidor: `dequeue_async().await` y ejecutar trabajo async.
- Recomendado: `Semaphore` para limitar in-flight (I/O).

**Mejor para:** I/O-bound, webhooks, fan-out, integraciones externas.

---

## Invariantes que deben mantenerse

1) Nunca entregar tareas expiradas (DropExpired estricto).
2) Capacidad global estricta: `queue_len_estimate <= max_global` bajo concurrencia.
3) Invariante formal de shard activo:
   si existe al menos una cola no vacia en un shard, ese shard debe estar en el ring global
   (`active_shards`) o marcado para reactivacion atomica (`shard_active=true`).
4) Un tenant activo con trabajo progresa (no starvation).
5) Al `close_immediate()`, consumidores bloqueados se despiertan y retornan `Closed`.
6) Al `close_drain()`, no se admiten nuevas tareas y se drena hasta vaciar `queue_len_estimate`.

---

## Estabilidad de API (post v0.1)

- Objetivo: minimizar cambios breaking despues de v0.1.
- Cambios preferidos: aditivos (nuevas funciones/metricas/campos opcionales).
- Cambios breaking: solo en bump mayor y con notas de migracion claras.
- Deprecaciones: primero se marca como deprecated, luego se remueve en version mayor.
- Metricas: se agregan sin renombrar ni eliminar las existentes.

---

## Roadmap inmediato posterior al MVP

- Políticas adicionales de backpressure (DropOldest, Timeout).
- Pesos por tenant (quantum por tenant/plan).
- Mejoras de selección de shard (evitar barrido completo; cursores/aleatorización).
- Histogramas de queue_time y percentiles (p95/p99).
- Integración Tower/Axum completa (middleware configurable).

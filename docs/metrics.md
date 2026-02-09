# Firq — Semantica de metricas

Este documento define la semantica exacta de las metricas expuestas por el core.

Aplica a:
- `SchedulerStats` (API sync).
- Exporter Prometheus (cuando se expone).

## Contadores principales

- `enqueued`
  - Incrementa cuando una tarea es aceptada en cola.
  - No incluye tareas rechazadas por backpressure.
- `dequeued`
  - Incrementa cuando una tarea no expirada es entregada a un consumidor.
  - Solo cuenta tareas que salen via `try_dequeue`/`dequeue_blocking`.
- `dropped`
  - Incrementa cuando un `enqueue` es rechazado por backpressure (Reject o Timeout).
  - En politicas de drop, tambien incrementa cuando se descarta una tarea existente para hacer lugar (DropOldest/DropNewest).
- `expired`
  - Incrementa cuando una tarea expira al momento de intentar despacharla.
  - Solo se cuentan expiraciones observadas en el front de la cola.

## Longitud de cola

- `queue_len_estimate`
  - Incrementa cuando una tarea es aceptada.
  - Decrementa cuando una tarea es entregada, descartada por expiracion, o removida por drop.
  - Es una estimacion bajo concurrencia (no es una lectura exacta de la cola).

## Queue time

- `queue_time_sum_ns`
  - Suma de `queue_time` (nanosegundos) de tareas entregadas.
  - `queue_time = now - enqueue_ts`, medido al entregar.
- `queue_time_samples`
  - Cantidad de tareas entregadas que aportan al `queue_time`.
- `queue_time_histogram`
  - Histograma en buckets definidos por `QUEUE_TIME_BUCKETS_NS`.
  - Cada tarea entregada incrementa un bucket segun su `queue_time`.
- `queue_time_p95_ns` / `queue_time_p99_ns`
  - Percentiles derivados del histograma.
  - Si no hay muestras, devuelven `0`.

## Top tenants

- `top_tenants`
  - Top talkers aproximado por tenant.
  - Se calcula en base a tareas entregadas (`dequeued`) y tiene capacidad acotada.
  - No es un ranking exacto cuando hay muchos tenants.

## Derivados comunes

- `avg_queue_time_ns = queue_time_sum_ns / queue_time_samples` (si hay muestras).
- `throughput = dequeued / ventana_de_tiempo`.


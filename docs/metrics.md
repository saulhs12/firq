# Firq — Semantica de metricas

Este documento define la semantica exacta de metricas de `SchedulerStats` y del exporter Prometheus.

## Contadores base

- `enqueued`
  - Incrementa cuando una tarea es aceptada.
- `dequeued`
  - Incrementa cuando una tarea no expirada es entregada al consumidor.
- `expired`
  - Incrementa cuando una tarea vence deadline al frente de cola.
- `dropped`
  - Suma total de rechazos y descartes por politica.

## Desagregacion de rechazo/drop

- `rejected_global`
  - Enqueue rechazado por capacidad global.
- `rejected_tenant`
  - Enqueue rechazado por capacidad por tenant.
- `timeout_rejected`
  - Enqueue rechazado por timeout de espera de capacidad.
- `dropped_policy`
  - Tareas removidas por politicas de reemplazo (`DropOldestPerTenant` / `DropNewestPerTenant`).

## Saturacion

- `queue_len_estimate`
  - Longitud logica de cola pendiente.
  - Se mantiene con reserva estricta de capacidad.
- `max_global`
  - Capacidad global configurada.
- `queue_saturation_ratio`
  - `queue_len_estimate / max_global`.
  - Valor esperado en `[0, 1]` cuando `max_global > 0`.

## Queue time

- `queue_time_sum_ns`
- `queue_time_samples`
- `queue_time_histogram`
- `queue_time_p95_ns`
- `queue_time_p99_ns`

Notas:

- `queue_time` se mide en dequeue como `now - enqueue_ts`.
- p95/p99 se estiman sobre histogramas (no cuantiles exactos).

## Top talkers

- `top_tenants`
  - Ranking aproximado por tareas entregadas.
  - Estructura acotada por `top_tenants_capacity`.

## Dashboard Prometheus (ejemplos)

1. Saturacion de cola

```promql
firq_queue_saturation_ratio
```

2. Rechazos por causa (rate)

```promql
sum(rate(firq_rejected_global_total[5m]))
sum(rate(firq_rejected_tenant_total[5m]))
sum(rate(firq_timeout_rejected_total[5m]))
```

3. p95/p99 de queue time (gauge exportado)

```promql
firq_queue_time_p95_ns / 1e6
firq_queue_time_p99_ns / 1e6
```

4. Top tenants

```promql
topk(10, firq_tenant_dequeued_total)
```

## Derivados recomendados

- `avg_queue_time_ns = queue_time_sum_ns / queue_time_samples`
- `throughput = rate(dequeued_total[window])`
- Alert de saturacion:
  - warning: `queue_saturation_ratio > 0.80`
  - critical: `queue_saturation_ratio > 0.95`

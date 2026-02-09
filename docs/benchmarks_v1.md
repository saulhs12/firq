# Benchmarks V1

Este documento define la metodologia reproducible de benchmarks para V1.

## Objetivo de aceptacion

Gate V1:

- Firq debe mostrar mejora frente a FIFO en p99 bajo escenario noisy-neighbor.
- Firq debe preservar progreso de tenants cold en presencia de tenants hot.

## Comando

```bash
cargo run -p firq-bench
```

## Escenarios incluidos

1. `hot_tenant_sustained`
- Tenant hot sostenido + muchos tenants cold.

2. `burst_massive`
- Burst masivo simultaneo de productores.

3. `mixed_priorities`
- Mezcla de `High/Normal/Low` bajo contencion.

4. `deadline_expiration`
- Deadlines cortos para forzar expiraciones.

5. `capacity_pressure`
- Presion de `max_global` y `max_per_tenant`.

## Metricas reportadas por escenario

- `throughput` (ops/s)
- `drop_rate`
- `expired_rate`
- `queue_time p50/p95/p99`
- comparacion `p99_gain_vs_fifo`

## Costo de `spawn_blocking` en `dequeue_async`

Medicion local con helper de test:

```bash
cargo test -p firq-async measure_dequeue_async_spawn_blocking_cost -- --ignored --nocapture
```

Resultado observado:

- `samples=512`
- `total_ms=8.522`
- `avg_us=16.643` por `dequeue_async` en este entorno.

## Parametros base

- Workers: 2
- Scheduler shards: 4
- Backpressure: `Reject`
- Quantum: 2
- Buckets de queue time: mismos que `firq-core`.

## Nota de reproducibilidad

Registrar junto al reporte:

- CPU y numero de cores
- RAM
- OS y version
- Rust toolchain (`rustc --version`)
- commit hash (`git rev-parse HEAD`)

## Resultados (ejecucion local, 2026-02-09)

Hardware/entorno de referencia para esta corrida:

- Host local de desarrollo.
- `cargo run -p firq-bench` en perfil `dev`.

Resumen:

1. `hot_tenant_sustained`
- Firq throughput: `463.3 ops/s`
- FIFO throughput: `460.6 ops/s`
- Queue-time p99: `inf` en ambos (carga extrema fuera de buckets definidos).

2. `burst_massive`
- Firq throughput: `633.5 ops/s`
- FIFO throughput: `665.4 ops/s`
- Queue-time p99: `inf` en ambos.

3. `mixed_priorities`
- Firq throughput: `680.4 ops/s`
- FIFO throughput: `685.5 ops/s`
- Queue-time p99: `inf` en ambos.

4. `deadline_expiration`
- Firq throughput: `216.4 ops/s`, `expired_rate=97.741%`, `p99=10ms`
- FIFO throughput: `229.8 ops/s`, `expired_rate=99.312%`, `p99=5ms`

5. `capacity_pressure`
- Firq throughput: `437.1 ops/s`
- FIFO throughput: `446.3 ops/s`
- Queue-time p99: `inf` en ambos.

Observaciones:

- En escenarios de sobrecarga extrema, los percentiles caen en el bucket `+Inf` del histograma (se reporta `inf`).
- Para criterio de release, estos datos deben repetirse en CI/runner dedicado y perfil `--release` para señal estable.

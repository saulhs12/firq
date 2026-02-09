# Benchmarks V1

Este documento define la metodologia reproducible de benchmarks para V1.

## Objetivo de aceptacion

Gate V1:

- Firq debe mostrar mejora frente a FIFO en p99 bajo escenario noisy-neighbor.
- Firq debe preservar progreso de tenants cold en presencia de tenants hot.

## Comando

```bash
cargo run --release -p firq-bench
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
- `cold_p99` (p99 de latencia de tenants cold en escenario noisy-neighbor)
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
- `cargo run --release -p firq-bench` en perfil `release`.

Resumen:

1. `hot_tenant_sustained`
- Firq throughput: `1595.5 ops/s`
- FIFO throughput: `1594.7 ops/s`
- Queue-time p99: `5s` (Firq) vs `10s` (FIFO), `p99_gain_vs_fifo=50.00%`.
- Cold tail (`cold_p99`): `5s` en ambos.

2. `burst_massive`
- Firq throughput: `666.7 ops/s`
- FIFO throughput: `785.0 ops/s`
- Queue-time p99: `5s` en ambos.

3. `mixed_priorities`
- Firq throughput: `805.9 ops/s`
- FIFO throughput: `806.5 ops/s`
- Queue-time p99: `10s` en ambos.

4. `deadline_expiration`
- Firq throughput: `278.7 ops/s`, `expired_rate=97.846%`, `p99=10ms`
- FIFO throughput: `276.1 ops/s`, `expired_rate=99.446%`, `p99=5ms`

5. `capacity_pressure`
- Firq throughput: `503.3 ops/s`
- FIFO throughput: `534.7 ops/s`
- Queue-time p99: `5s` en ambos.

Observaciones:

- El criterio de cierre noisy-neighbor queda cubierto por `hot_tenant_sustained` con mejora de p99 frente a FIFO en la misma carga.
- En escenarios de sobrecarga extrema, la latencia cae en buckets altos y la señal de tail se aplana; se recomienda repetir en runner dedicado para trazas estables de release.

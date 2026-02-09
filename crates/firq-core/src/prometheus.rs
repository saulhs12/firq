use crate::api::SchedulerStats;

pub fn render_stats(stats: &SchedulerStats, namespace: &str) -> String {
    let ns = if namespace.is_empty() {
        "firq"
    } else {
        namespace
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# HELP {ns}_enqueued_total Total tasks enqueued\n# TYPE {ns}_enqueued_total counter\n{ns}_enqueued_total {}\n",
        stats.enqueued
    ));
    out.push_str(&format!(
        "# HELP {ns}_dequeued_total Total tasks dequeued\n# TYPE {ns}_dequeued_total counter\n{ns}_dequeued_total {}\n",
        stats.dequeued
    ));
    out.push_str(&format!(
        "# HELP {ns}_dropped_total Total tasks dropped by backpressure\n# TYPE {ns}_dropped_total counter\n{ns}_dropped_total {}\n",
        stats.dropped
    ));
    out.push_str(&format!(
        "# HELP {ns}_expired_total Total tasks expired before dequeue\n# TYPE {ns}_expired_total counter\n{ns}_expired_total {}\n",
        stats.expired
    ));
    out.push_str(&format!(
        "# HELP {ns}_queue_len_estimate Estimated queue length\n# TYPE {ns}_queue_len_estimate gauge\n{ns}_queue_len_estimate {}\n",
        stats.queue_len_estimate
    ));
    out.push_str(&format!(
        "# HELP {ns}_queue_time_sum_ns Sum of queue time in ns\n# TYPE {ns}_queue_time_sum_ns counter\n{ns}_queue_time_sum_ns {}\n",
        stats.queue_time_sum_ns
    ));
    out.push_str(&format!(
        "# HELP {ns}_queue_time_samples Total queue time samples\n# TYPE {ns}_queue_time_samples counter\n{ns}_queue_time_samples {}\n",
        stats.queue_time_samples
    ));
    out.push_str(&format!(
        "# HELP {ns}_queue_time_p95_ns Approx p95 queue time in ns\n# TYPE {ns}_queue_time_p95_ns gauge\n{ns}_queue_time_p95_ns {}\n",
        stats.queue_time_p95_ns
    ));
    out.push_str(&format!(
        "# HELP {ns}_queue_time_p99_ns Approx p99 queue time in ns\n# TYPE {ns}_queue_time_p99_ns gauge\n{ns}_queue_time_p99_ns {}\n",
        stats.queue_time_p99_ns
    ));

    if !stats.queue_time_histogram.is_empty() {
        out.push_str(&format!(
            "# HELP {ns}_queue_time_ns Queue time histogram\n# TYPE {ns}_queue_time_ns histogram\n"
        ));
        let mut cumulative = 0u64;
        for bucket in &stats.queue_time_histogram {
            cumulative = cumulative.saturating_add(bucket.count);
            let le = if bucket.le_ns == u64::MAX {
                "+Inf".to_string()
            } else {
                bucket.le_ns.to_string()
            };
            out.push_str(&format!(
                "{ns}_queue_time_ns_bucket{{le=\"{le}\"}} {cumulative}\n"
            ));
        }
        out.push_str(&format!(
            "{ns}_queue_time_ns_count {}\n{ns}_queue_time_ns_sum {}\n",
            stats.queue_time_samples, stats.queue_time_sum_ns
        ));
    }

    if !stats.top_tenants.is_empty() {
        out.push_str(&format!(
            "# HELP {ns}_tenant_dequeued_total Approx dequeued count per tenant (top talkers)\n# TYPE {ns}_tenant_dequeued_total gauge\n"
        ));
        for entry in &stats.top_tenants {
            out.push_str(&format!(
                "{ns}_tenant_dequeued_total{{tenant=\"{}\"}} {}\n",
                entry.tenant.as_u64(),
                entry.count
            ));
        }
    }

    out
}

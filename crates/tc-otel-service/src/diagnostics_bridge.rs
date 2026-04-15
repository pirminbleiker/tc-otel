//! Bridge DiagEvents from the self-polling collector to tc-otel's
//! existing MetricEntry / OTel export pipeline.
//!
//! Emitted metric names follow `tc.rt.*` and `tc.task.*` prefixes with
//! resource attributes `ams_net_id` and — for per-task metrics — `task_port`.

use tc_otel_ads::ams::AmsNetId;
use tc_otel_ads::diagnostics::DiagEvent;
use tc_otel_core::MetricEntry;

/// Convert a single decoded diagnostic event into zero or more OTel metrics.
pub fn diag_event_to_metrics(target_net_id: AmsNetId, ev: DiagEvent) -> Vec<MetricEntry> {
    let net_id_str = target_net_id.to_string();
    match ev {
        DiagEvent::ExceedCounter { value } => {
            vec![with_ams(
                net_id_str,
                MetricEntry::sum("tc.rt.exceed_counter".into(), value as f64, true),
            )]
        }
        DiagEvent::ExceedReset => {
            // Not a metric — reset is a state change; emit a non-monotonic
            // sum with value 0 so the downstream can detect drops via
            // counter-reset semantics.
            vec![with_ams(
                net_id_str,
                MetricEntry::sum("tc.rt.exceed_counter".into(), 0.0, false),
            )]
        }
        DiagEvent::RtUsage {
            cpu_percent,
            system_latency_us,
            peak_latency_us,
        } => vec![
            with_ams(
                net_id_str.clone(),
                MetricEntry::gauge("tc.rt.cpu_usage_percent".into(), cpu_percent as f64),
            ),
            with_ams(
                net_id_str.clone(),
                MetricEntry::gauge(
                    "tc.rt.system_latency_us".into(),
                    system_latency_us as f64,
                ),
            ),
            with_ams(
                net_id_str,
                MetricEntry::gauge("tc.rt.peak_latency_us".into(), peak_latency_us as f64),
            ),
        ],
        DiagEvent::TaskStats {
            task_port,
            cpu_ticks_100ns,
            exec_ticks_100ns,
            ..
        } => {
            // CPU-time accumulators as monotonic counters in nanoseconds.
            // Prometheus rate() gives ns/s → divide by 1e9 for CPU fraction.
            // u32 wrap is handled by counter-reset semantics in PromQL.
            let cpu_ns = cpu_ticks_100ns as f64 * 100.0;
            let exec_ns = exec_ticks_100ns as f64 * 100.0;
            vec![
                with_task(
                    net_id_str.clone(),
                    task_port,
                    MetricEntry::sum("tc.task.cpu_time_ns".into(), cpu_ns, true),
                ),
                with_task(
                    net_id_str,
                    task_port,
                    MetricEntry::sum("tc.task.exec_time_ns".into(), exec_ns, true),
                ),
            ]
        }
    }
}

fn with_ams(net_id: String, mut m: MetricEntry) -> MetricEntry {
    m.ams_net_id = net_id;
    m
}

fn with_task(net_id: String, task_port: u16, mut m: MetricEntry) -> MetricEntry {
    m.ams_net_id = net_id;
    m.ams_source_port = task_port;
    m.attributes.insert(
        "task_port".into(),
        serde_json::Value::Number(task_port.into()),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> AmsNetId {
        AmsNetId::from_bytes([172, 28, 41, 37, 1, 1])
    }

    #[test]
    fn exceed_counter_maps_to_monotonic_sum() {
        let out = diag_event_to_metrics(net(), DiagEvent::ExceedCounter { value: 42 });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "tc.rt.exceed_counter");
        assert_eq!(out[0].value, 42.0);
        assert!(out[0].is_monotonic);
        assert_eq!(out[0].ams_net_id, "172.28.41.37.1.1");
    }

    #[test]
    fn rt_usage_maps_to_three_gauges() {
        let out = diag_event_to_metrics(
            net(),
            DiagEvent::RtUsage {
                cpu_percent: 5,
                system_latency_us: 300,
                peak_latency_us: 150,
            },
        );
        assert_eq!(out.len(), 3);
        let names: Vec<&str> = out.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"tc.rt.cpu_usage_percent"));
        assert!(names.contains(&"tc.rt.system_latency_us"));
        assert!(names.contains(&"tc.rt.peak_latency_us"));
    }

    #[test]
    fn task_stats_maps_to_cpu_and_exec_counters_in_ns() {
        let out = diag_event_to_metrics(
            net(),
            DiagEvent::TaskStats {
                task_port: 350,
                type_marker: 0,
                cycle_counter: 0,
                cpu_ticks_100ns: 10,
                exec_ticks_100ns: 7,
            },
        );
        assert_eq!(out.len(), 2);
        let cpu = out.iter().find(|m| m.name == "tc.task.cpu_time_ns").unwrap();
        assert_eq!(cpu.value, 1000.0, "10 × 100 ns = 1000 ns");
        assert_eq!(cpu.ams_source_port, 350);
        assert!(cpu.is_monotonic);
        let exec = out
            .iter()
            .find(|m| m.name == "tc.task.exec_time_ns")
            .unwrap();
        assert_eq!(exec.value, 700.0);
    }
}

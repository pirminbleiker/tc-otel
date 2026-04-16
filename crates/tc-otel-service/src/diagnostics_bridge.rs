//! Bridge DiagEvents from the self-polling collector to tc-otel's
//! existing MetricEntry / OTel export pipeline.
//!
//! Emitted metric names follow `tc.rt.*` and `tc.task.*` prefixes with
//! resource attributes `ams_net_id` and — for per-task metrics — `task_port`.

use std::collections::HashMap;
use tc_otel_ads::ams::AmsNetId;
use tc_otel_ads::diagnostics::DiagEvent;
use tc_otel_core::MetricEntry;

/// Convert a single decoded diagnostic event into zero or more OTel metrics.
///
/// `task_names` maps `(net_id, port)` to the discovered task name. When
/// empty or missing the port number is used as the label value.
pub fn diag_event_to_metrics(
    target_net_id: AmsNetId,
    ev: DiagEvent,
    task_names: &HashMap<(AmsNetId, u16), String>,
) -> Vec<MetricEntry> {
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
            cycle_counter,
            cpu_ticks_100ns,
            exec_ticks_100ns,
            ..
        } => {
            // CPU-time accumulators as monotonic counters in nanoseconds.
            // Prometheus rate() gives ns/s → divide by 1e9 for CPU fraction.
            // u32 wrap (every ~7 min at full rate) is handled by PromQL's
            // counter-reset semantics.
            let cpu_ns = cpu_ticks_100ns as f64 * 100.0;
            let exec_ns = exec_ticks_100ns as f64 * 100.0;
            // u16 cycle counter wraps every ~65 s at 1 kHz; PromQL's
            // rate() handles the apparent "reset" the same way. Emit raw;
            // downstream can derive actual cycle time per cycle via
            //   rate(tc_task_cpu_time_ns[1m]) / rate(tc_task_cycle_count[1m])
            // → ns per cycle (÷1000 for µs).
            let task_name = task_names
                .get(&(target_net_id, task_port))
                .cloned()
                .unwrap_or_else(|| format!("port-{task_port}"));
            vec![
                with_task(
                    net_id_str.clone(),
                    task_port,
                    &task_name,
                    MetricEntry::sum("tc.task.cpu_time_ns".into(), cpu_ns, true),
                ),
                with_task(
                    net_id_str.clone(),
                    task_port,
                    &task_name,
                    MetricEntry::sum("tc.task.exec_time_ns".into(), exec_ns, true),
                ),
                with_task(
                    net_id_str,
                    task_port,
                    &task_name,
                    MetricEntry::sum(
                        "tc.task.cycle_count".into(),
                        cycle_counter as f64,
                        true,
                    ),
                ),
            ]
        }
        DiagEvent::TaskSnapshot {
            task_port,
            task_name,
            cycle_count,
            last_exec_time_us,
            cycle_exceed_count,
            rt_violation_count,
            ..
        } => {
            // Snapshot: emit cycle count and exceed/violation counters.
            vec![
                with_task(
                    net_id_str.clone(),
                    task_port,
                    &task_name,
                    MetricEntry::sum("tc.task.cycle_count".into(), cycle_count as f64, true),
                ),
                with_task(
                    net_id_str.clone(),
                    task_port,
                    &task_name,
                    MetricEntry::sum(
                        "tc.task.cycle_exceed_count".into(),
                        cycle_exceed_count as f64,
                        true,
                    ),
                ),
                with_task(
                    net_id_str.clone(),
                    task_port,
                    &task_name,
                    MetricEntry::sum(
                        "tc.task.rt_violation_count".into(),
                        rt_violation_count as f64,
                        true,
                    ),
                ),
                with_task(
                    net_id_str,
                    task_port,
                    &task_name,
                    MetricEntry::gauge(
                        "tc.task.last_exec_time_us".into(),
                        last_exec_time_us as f64,
                    ),
                ),
            ]
        }
        DiagEvent::CycleExceedEdge {
            task_port,
            task_name,
            ..
        } => {
            // Edge event: emit a non-monotonic counter = 1 to signal the edge.
            vec![with_task(
                net_id_str,
                task_port,
                &task_name,
                MetricEntry::sum("tc.task.cycle_exceed_edge".into(), 1.0, false),
            )]
        }
        DiagEvent::RtViolationEdge {
            task_port,
            task_name,
            ..
        } => {
            // Edge event: emit a non-monotonic counter = 1 to signal the edge.
            vec![with_task(
                net_id_str,
                task_port,
                &task_name,
                MetricEntry::sum("tc.task.rt_violation_edge".into(), 1.0, false),
            )]
        }
    }
}

fn with_ams(net_id: String, mut m: MetricEntry) -> MetricEntry {
    m.ams_net_id = net_id;
    m
}

fn with_task(net_id: String, task_port: u16, task_name: &str, mut m: MetricEntry) -> MetricEntry {
    m.ams_net_id = net_id;
    m.ams_source_port = task_port;
    m.task_name = task_name.to_string();
    m.attributes.insert(
        "task_port".into(),
        serde_json::Value::Number(task_port.into()),
    );
    m.attributes
        .insert("task_name".into(), serde_json::Value::String(task_name.to_string()));
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
        let out = diag_event_to_metrics(
            net(),
            DiagEvent::ExceedCounter { value: 42 },
            &HashMap::new(),
        );
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
            &HashMap::new(),
        );
        assert_eq!(out.len(), 3);
        let names: Vec<&str> = out.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"tc.rt.cpu_usage_percent"));
        assert!(names.contains(&"tc.rt.system_latency_us"));
        assert!(names.contains(&"tc.rt.peak_latency_us"));
    }

    #[test]
    fn task_stats_maps_to_cpu_exec_and_cycle_counters_with_name() {
        let mut names = HashMap::new();
        names.insert((net(), 350_u16), "PlcTask".to_string());
        let out = diag_event_to_metrics(
            net(),
            DiagEvent::TaskStats {
                task_port: 350,
                type_marker: 0,
                cycle_counter: 12345,
                cpu_ticks_100ns: 10,
                exec_ticks_100ns: 7,
            },
            &names,
        );
        assert_eq!(out.len(), 3);
        let cpu = out.iter().find(|m| m.name == "tc.task.cpu_time_ns").unwrap();
        assert_eq!(cpu.value, 1000.0, "10 × 100 ns = 1000 ns");
        assert_eq!(cpu.ams_source_port, 350);
        assert_eq!(cpu.task_name, "PlcTask");
        assert!(cpu.is_monotonic);
        let cycle = out
            .iter()
            .find(|m| m.name == "tc.task.cycle_count")
            .unwrap();
        assert_eq!(cycle.value, 12345.0);
        assert_eq!(cycle.task_name, "PlcTask");
    }

    #[test]
    fn task_stats_falls_back_to_port_label_when_name_unknown() {
        let out = diag_event_to_metrics(
            net(),
            DiagEvent::TaskStats {
                task_port: 350,
                type_marker: 0,
                cycle_counter: 1,
                cpu_ticks_100ns: 0,
                exec_ticks_100ns: 0,
            },
            &HashMap::new(),
        );
        assert_eq!(out[0].task_name, "port-350");
    }
}

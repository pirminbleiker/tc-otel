//! Bridge DiagEvents from push-diagnostic batches and the self-polling
//! collector to tc-otel's existing MetricEntry / OTel export pipeline.
//!
//! Emitted metric names follow `tc.rt.*` and `tc.task.*` prefixes with
//! resource attributes `ams_net_id` and — for per-task metrics — `task_port`.
//! Flagged samples (cycle-exceed / RT-violation) additionally carry
//! `cycle_count` and `dc_time_ns` attributes so each event can be correlated
//! 1:1 with the PLC log entry produced on the same cycle. `dc_time_ns` is
//! FILETIME — 100-nanosecond ticks since 1601-01-01 UTC — the same format
//! log entries use for `plc_timestamp` / `clock_timestamp`.
//!
//! User-defined push metrics (Gauge, Counter, Histogram) are converted via an
//! accumulated descriptor table per `(ams_net_id, task_port)`, allowing
//! follow-up batches to reference metrics by ID without re-announcing metadata.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tc_otel_ads::ams::AmsNetId;
use tc_otel_ads::diagnostics::{
    DiagEvent, DiagSample, MetricAggregateSample, MetricBodySchema, MetricDescriptor, MetricSample,
    METRIC_FLAG_RING_OVERFLOWED, METRIC_STAT_ORDER, SAMPLE_FLAG_CYCLE_EXCEED, SAMPLE_FLAG_OVERFLOW,
    SAMPLE_FLAG_RT_VIOLATION,
};
use tc_otel_core::MetricEntry;

/// Descriptor table per `(ams_net_id, task_port)`. Maps metric_id → MetricDescriptor.
/// This cache is populated as descriptors are announced and referenced across
/// follow-up batches. Descriptors are cleared on online-change (detected by
/// TaskRegistry signal).
///
/// For now, built as a simple HashMap. In future, this could be promoted to
/// a mutable cache in the service state to survive across multiple bridge calls.
type DescriptorCache = HashMap<u16, MetricDescriptor>;

/// `DcTaskTime` (nanoseconds since 2000-01-01 UTC — TwinCAT Distributed
/// Clock epoch) → `DateTime<Utc>`.
///
/// The PLC ships DcTaskTime in each diag sample because it is stamped by
/// the runtime at the start of the task cycle and has true nanosecond
/// resolution — unlike `PRG_TaskLog.RtcTime` which is quantised at
/// ~100 ms. Using DcTaskTime lets every flagged-cycle metric point land
/// on the Grafana time axis at the exact cycle boundary, so annotations
/// and log correlation are cycle-accurate.
fn dc_time_to_datetime(dc_ns: i64) -> DateTime<Utc> {
    // Seconds between Unix epoch (1970) and DC epoch (2000).
    const UNIX_TO_DC_EPOCH_SECS: i64 = 946_684_800;
    let secs = dc_ns / 1_000_000_000;
    let nanos_in_sec = (dc_ns.rem_euclid(1_000_000_000)) as u32;
    DateTime::from_timestamp(UNIX_TO_DC_EPOCH_SECS + secs, nanos_in_sec).unwrap_or_else(Utc::now)
}

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
                MetricEntry::gauge("tc.rt.system_latency_us".into(), system_latency_us as f64),
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
            let cpu_ns = cpu_ticks_100ns as f64 * 100.0;
            let exec_ns = exec_ticks_100ns as f64 * 100.0;
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
                    MetricEntry::sum("tc.task.cycle_count".into(), cycle_counter as f64, true),
                ),
            ]
        }
        DiagEvent::TaskDiagBatch {
            task_port,
            task_name,
            window_ms,
            cycle_count_end,
            exec_time_min_us,
            exec_time_max_us,
            exec_time_avg_us,
            cycle_exceed_count,
            rt_violation_count,
            samples,
            ..
        } => batch_to_metrics(
            &net_id_str,
            task_port,
            &task_name,
            window_ms,
            cycle_count_end,
            exec_time_min_us,
            exec_time_max_us,
            exec_time_avg_us,
            cycle_exceed_count,
            rt_violation_count,
            &samples,
        ),
        DiagEvent::MetricBatch {
            descriptors,
            samples,
            dc_time_start,
            dc_time_end,
            ..
        } => {
            // Build a descriptor cache from the announced descriptors.
            let mut cache = DescriptorCache::new();
            for desc in descriptors {
                cache.insert(desc.metric_id, desc);
            }
            // Convert samples using the cache.
            metric_batch_to_entries(&net_id_str, &cache, &samples, dc_time_start, dc_time_end)
        }
        DiagEvent::MetricAggregateBatch {
            metric_id,
            task_index,
            flags,
            body_schema,
            sample_size,
            stat_mask: _,
            cycle_count_start: _,
            cycle_count_end: _,
            dc_time_start,
            dc_time_end,
            name,
            unit,
            trace_id,
            span_id,
            samples,
        } => metric_aggregate_to_entries(
            &net_id_str,
            metric_id,
            task_index,
            flags,
            body_schema,
            sample_size,
            dc_time_start,
            dc_time_end,
            &name,
            &unit,
            trace_id,
            span_id,
            &samples,
        ),
    }
}

/// Convert one FB_Metrics aggregate batch into per-sample MetricEntry values.
///
/// Non-numeric body schemas:
///   * Bool → 0.0 / 1.0 gauge
///   * Discrete (BYTE/WORD/DWORD/LWORD/ENUM) → little-endian unsigned int → f64
///   * String / Wstring → silently dropped (OTel metrics are numeric)
///
/// Per-sample timestamps aren't on the wire — the receiver linearly
/// interpolates between `dc_time_start` and `dc_time_end`. Single-sample
/// batches use `dc_time_start` directly.
///
/// trace_id / span_id are stored on each ``MetricEntry`` as raw bytes and
/// promoted to a proper OTel Exemplar on the OTLP NumberDataPoint later in
/// the export path. Backends like Grafana Tempo / Prometheus follow the
/// Exemplar field directly to render "View trace" links; plain attributes
/// would require a TraceQL query hop.
#[allow(clippy::too_many_arguments)]
fn metric_aggregate_to_entries(
    net_id: &str,
    metric_id: u32,
    task_index: u8,
    flags: u8,
    body_schema: MetricBodySchema,
    sample_size: u32,
    dc_time_start: i64,
    dc_time_end: i64,
    name: &str,
    unit: &str,
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
    samples: &[MetricAggregateSample],
) -> Vec<MetricEntry> {
    let mut out: Vec<MetricEntry> = Vec::with_capacity(samples.len());
    if samples.is_empty() {
        return out;
    }

    let span_ns = (dc_time_end - dc_time_start).max(0);
    let denom = (samples.len() as i64 - 1).max(1);
    let overflow_flag = flags & METRIC_FLAG_RING_OVERFLOWED != 0;

    for (i, sample) in samples.iter().enumerate() {
        let ts_ns = if samples.len() == 1 {
            dc_time_start
        } else {
            dc_time_start + (i as i64) * span_ns / denom
        };

        // NumericAggregated samples expand into one MetricEntry per set
        // stat bit (Min, Max, Mean, Sum, Count, StdDev) with the stat name
        // suffixed onto the metric name. Doing that here means the rest of
        // the pipeline doesn't need to know about aggregation at all.
        if let MetricAggregateSample::NumericAggregated { stat_mask, values } = sample {
            let mut k = 0;
            for (bit, suffix) in METRIC_STAT_ORDER {
                if stat_mask & bit == 0 {
                    continue;
                }
                if k >= values.len() {
                    // Defensive: decoder should have validated, but skip
                    // rather than panic if a malformed sample slips through.
                    break;
                }
                let v = values[k];
                k += 1;
                let metric_name = format!("{}.{}", name, suffix);
                out.push(build_aggregate_entry(
                    metric_name,
                    v,
                    ts_ns,
                    unit,
                    net_id,
                    task_index,
                    metric_id,
                    body_schema,
                    sample_size,
                    overflow_flag,
                    trace_id,
                    span_id,
                ));
            }
            continue;
        }

        let value: f64 = match sample {
            MetricAggregateSample::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            MetricAggregateSample::Numeric(v) => *v,
            MetricAggregateSample::Discrete(bytes) => discrete_bytes_to_f64(bytes),
            MetricAggregateSample::String(_) | MetricAggregateSample::Wstring(_) => {
                // OTel metrics are numeric; string-typed metrics need the
                // events pipeline (not yet implemented). Skip silently.
                continue;
            }
            MetricAggregateSample::NumericAggregated { .. } => unreachable!(),
        };

        let mut entry = MetricEntry::gauge(name.to_string(), value);
        entry.unit = unit.to_string();
        entry.timestamp = dc_time_to_datetime(ts_ns);
        entry.ams_net_id = net_id.to_string();
        entry.task_index = task_index as i32;

        entry.attributes.insert(
            "metric_id".into(),
            serde_json::Value::Number(metric_id.into()),
        );
        entry.attributes.insert(
            "body_schema".into(),
            serde_json::Value::String(format!("{:?}", body_schema).to_lowercase()),
        );
        entry.attributes.insert(
            "sample_size".into(),
            serde_json::Value::Number(sample_size.into()),
        );
        if overflow_flag {
            entry
                .attributes
                .insert("ring_overflowed".into(), serde_json::Value::Bool(true));
        }
        if let Some(tid) = trace_id {
            entry.trace_id = tid;
        }
        if let Some(sid) = span_id {
            entry.span_id = sid;
        }

        out.push(entry);
    }

    out
}

/// Construct the per-stat MetricEntry for an aggregated sample. Mirrors the
/// attribute set we attach to the raw-numeric path in
/// ``metric_aggregate_to_entries`` so dashboards can group both kinds the
/// same way (metric_id, body_schema, sample_size, ring_overflowed,
/// optional trace_id / span_id — the last two live on ``MetricEntry`` as
/// raw bytes and become an OTel Exemplar on the exporter side).
#[allow(clippy::too_many_arguments)]
fn build_aggregate_entry(
    name: String,
    value: f64,
    ts_ns: i64,
    unit: &str,
    net_id: &str,
    task_index: u8,
    metric_id: u32,
    body_schema: MetricBodySchema,
    sample_size: u32,
    overflow_flag: bool,
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
) -> MetricEntry {
    let mut entry = MetricEntry::gauge(name, value);
    entry.unit = unit.to_string();
    entry.timestamp = dc_time_to_datetime(ts_ns);
    entry.ams_net_id = net_id.to_string();
    entry.task_index = task_index as i32;

    entry.attributes.insert(
        "metric_id".into(),
        serde_json::Value::Number(metric_id.into()),
    );
    entry.attributes.insert(
        "body_schema".into(),
        serde_json::Value::String(format!("{:?}", body_schema).to_lowercase()),
    );
    entry.attributes.insert(
        "sample_size".into(),
        serde_json::Value::Number(sample_size.into()),
    );
    if overflow_flag {
        entry
            .attributes
            .insert("ring_overflowed".into(), serde_json::Value::Bool(true));
    }
    if let Some(tid) = trace_id {
        entry.trace_id = tid;
    }
    if let Some(sid) = span_id {
        entry.span_id = sid;
    }
    entry
}

/// Interpret a discrete-byte sample as a little-endian unsigned integer.
/// Caps at 8 bytes (LWORD); longer slices use the first 8 bytes.
fn discrete_bytes_to_f64(bytes: &[u8]) -> f64 {
    let mut buf = [0_u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(buf) as f64
}

/// Convert per-task diagnostic batch to metrics. Pre-aggregated stats (min/max/avg
/// exec-time, cycle-exceed/RT-violation counts) are emitted as gauges/sums. Per-sample
/// edges (flagged cycles) are emitted as event counters with cycle/dc-time attributes.
#[allow(clippy::too_many_arguments)]
fn batch_to_metrics(
    net_id: &str,
    task_port: u16,
    task_name: &str,
    window_ms: u16,
    cycle_count_end: u32,
    exec_time_min_us: u32,
    exec_time_max_us: u32,
    exec_time_avg_us: u32,
    cycle_exceed_count: u32,
    rt_violation_count: u32,
    samples: &[DiagSample],
) -> Vec<MetricEntry> {
    // One batch typically emits ~9 metrics plus one per flagged sample.
    let mut out = Vec::with_capacity(9 + samples.len() / 10);

    // Aggregate gauges for the window.
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge("tc.task.exec_time_min_us".into(), exec_time_min_us as f64),
    ));
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge("tc.task.exec_time_max_us".into(), exec_time_max_us as f64),
    ));
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge("tc.task.exec_time_avg_us".into(), exec_time_avg_us as f64),
    ));
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge("tc.task.window_ms".into(), window_ms as f64),
    ));
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge("tc.task.sample_count".into(), samples.len() as f64),
    ));

    // cycle_count is monotonic by construction (task cycle counter only grows);
    // emit end-of-window value as a sum so rate() gives cycles/sec.
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::sum("tc.task.cycle_count".into(), cycle_count_end as f64, true),
    ));

    // Window deltas as non-monotonic sums. Downstream aggregates via
    // sum_over_time() to reconstruct cumulative counts.
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::sum(
            "tc.task.cycle_exceed_window".into(),
            cycle_exceed_count as f64,
            false,
        ),
    ));
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::sum(
            "tc.task.rt_violation_window".into(),
            rt_violation_count as f64,
            false,
        ),
    ));

    // Per-sample edge events for flagged cycles — timestamp set to each
    // sample's real dc_time so Grafana places them at the exact cycle
    // boundary (same timebase as log entries). Two metrics per flagged
    // cycle:
    //   - `cycle_exceed_edge` = 1   (event counter, rates + annotations)
    //   - `cycle_exceed_exec_us` = exec_time_us (magnitude, Y-axis on the
    //      scatter panel — shows by HOW MUCH each cycle overran)
    // Both carry cycle_count attribute so you can click into logs.
    let mut overflow_seen = false;
    for s in samples {
        let sample_ts = dc_time_to_datetime(s.dc_time);
        if s.flags & SAMPLE_FLAG_CYCLE_EXCEED != 0 {
            let mut m = with_sample_attrs(
                with_task(
                    net_id.into(),
                    task_port,
                    task_name,
                    MetricEntry::sum("tc.task.cycle_exceed_edge".into(), 1.0, false),
                ),
                s,
            );
            m.timestamp = sample_ts;
            out.push(m);

            let mut m2 = with_sample_attrs(
                with_task(
                    net_id.into(),
                    task_port,
                    task_name,
                    MetricEntry::gauge(
                        "tc.task.cycle_exceed_exec_us".into(),
                        s.exec_time_us as f64,
                    ),
                ),
                s,
            );
            m2.timestamp = sample_ts;
            out.push(m2);
        }
        if s.flags & SAMPLE_FLAG_RT_VIOLATION != 0 {
            let mut m = with_sample_attrs(
                with_task(
                    net_id.into(),
                    task_port,
                    task_name,
                    MetricEntry::sum("tc.task.rt_violation_edge".into(), 1.0, false),
                ),
                s,
            );
            m.timestamp = sample_ts;
            out.push(m);
        }
        overflow_seen |= s.flags & SAMPLE_FLAG_OVERFLOW != 0;
    }

    // Signal overflow as a gauge once per batch — ops dashboards light up on
    // any non-zero reading and operators know samples were dropped.
    out.push(with_task(
        net_id.into(),
        task_port,
        task_name,
        MetricEntry::gauge(
            "tc.task.sample_buffer_overflow".into(),
            if overflow_seen { 1.0 } else { 0.0 },
        ),
    ));

    out
}

/// Convert a metric batch into MetricEntry values using a descriptor cache.
///
/// Each sample references a metric_id; the descriptor cache provides metadata
/// (name, unit, kind, is_monotonic for Sum, bounds for Histogram). Samples are
/// accumulated into their respective metric (e.g., counter deltas, histogram
/// observations).
fn metric_batch_to_entries(
    net_id: &str,
    cache: &DescriptorCache,
    samples: &[MetricSample],
    _dc_time_start: i64,
    _dc_time_end: i64,
) -> Vec<MetricEntry> {
    let mut out = Vec::with_capacity(samples.len());

    for sample in samples {
        let desc = match cache.get(&sample.metric_id) {
            Some(d) => d,
            None => {
                // Sample references unknown metric_id; skip (malformed batch).
                continue;
            }
        };

        let sample_ts = dc_time_to_datetime(sample.dc_time);

        let mut entry = match desc.kind {
            0 => {
                // Gauge
                MetricEntry::gauge(desc.name.clone(), sample.value as f64)
            }
            1 => {
                // Sum/Counter
                let is_monotonic = (desc.flags & 1) != 0;
                MetricEntry::sum(desc.name.clone(), sample.value as f64, is_monotonic)
            }
            2 => {
                // Histogram
                if let Some(ref bounds) = desc.histogram_bounds {
                    // Accumulate observation into bucket (linear search for simplicity).
                    let bucket_idx = bounds
                        .iter()
                        .position(|&b| sample.value < b)
                        .unwrap_or(bounds.len());
                    let mut counts = vec![0_u64; bounds.len() + 1];
                    counts[bucket_idx] = 1;
                    MetricEntry::histogram(
                        desc.name.clone(),
                        bounds.iter().map(|&f| f as f64).collect(),
                        counts,
                        1,
                        sample.value as f64,
                    )
                } else {
                    // Histogram without bounds (malformed); skip.
                    continue;
                }
            }
            _ => {
                // Unknown kind; skip.
                continue;
            }
        };

        entry.description = desc.description.clone();
        entry.unit = desc.unit.clone();
        entry.timestamp = sample_ts;
        entry.ams_net_id = net_id.to_string();

        // Add attributes from descriptor.
        for (key, val) in &desc.attributes {
            entry
                .attributes
                .insert(key.clone(), serde_json::Value::String(val.clone()));
        }

        out.push(entry);
    }

    out
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
    m.attributes.insert(
        "task_name".into(),
        serde_json::Value::String(task_name.to_string()),
    );
    m
}

fn with_sample_attrs(mut m: MetricEntry, s: &DiagSample) -> MetricEntry {
    m.attributes.insert(
        "cycle_count".into(),
        serde_json::Value::Number(s.cycle_count.into()),
    );
    m.attributes.insert(
        "dc_time_ns".into(),
        serde_json::Value::Number(s.dc_time.into()),
    );
    m.attributes.insert(
        "exec_time_us".into(),
        serde_json::Value::Number(s.exec_time_us.into()),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_otel_core::MetricKind;

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
        let cpu = out
            .iter()
            .find(|m| m.name == "tc.task.cpu_time_ns")
            .unwrap();
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
    fn batch_emits_aggregate_gauges_and_window_deltas() {
        let ev = DiagEvent::TaskDiagBatch {
            task_port: 350,
            task_name: "PlcTask".into(),
            task_obj_id: 42,
            window_ms: 100,
            cycle_count_start: 1000,
            cycle_count_end: 1099,
            dc_time_start: 0,
            dc_time_end: 99_000_000,
            exec_time_min_us: 200,
            exec_time_max_us: 500,
            exec_time_avg_us: 330,
            cycle_exceed_count: 2,
            rt_violation_count: 1,
            samples: vec![DiagSample {
                cycle_count: 1050,
                exec_time_us: 500,
                dc_time: 50_000_000,
                flags: SAMPLE_FLAG_CYCLE_EXCEED,
            }],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());
        let by_name: HashMap<String, &MetricEntry> =
            out.iter().map(|m| (m.name.clone(), m)).collect();

        assert_eq!(by_name["tc.task.exec_time_min_us"].value, 200.0);
        assert_eq!(by_name["tc.task.exec_time_max_us"].value, 500.0);
        assert_eq!(by_name["tc.task.exec_time_avg_us"].value, 330.0);
        assert_eq!(by_name["tc.task.window_ms"].value, 100.0);
        assert_eq!(by_name["tc.task.sample_count"].value, 1.0);
        assert_eq!(by_name["tc.task.cycle_count"].value, 1099.0);
        assert!(by_name["tc.task.cycle_count"].is_monotonic);
        assert_eq!(by_name["tc.task.cycle_exceed_window"].value, 2.0);
        assert!(!by_name["tc.task.cycle_exceed_window"].is_monotonic);
        assert_eq!(by_name["tc.task.rt_violation_window"].value, 1.0);
        assert_eq!(by_name["tc.task.sample_buffer_overflow"].value, 0.0);
    }

    #[test]
    fn batch_emits_edge_events_with_cycle_and_dc_time_attributes() {
        let ev = DiagEvent::TaskDiagBatch {
            task_port: 350,
            task_name: "PlcTask".into(),
            task_obj_id: 42,
            window_ms: 100,
            cycle_count_start: 1000,
            cycle_count_end: 1002,
            dc_time_start: 0,
            dc_time_end: 2_000_000,
            exec_time_min_us: 100,
            exec_time_max_us: 900,
            exec_time_avg_us: 400,
            cycle_exceed_count: 1,
            rt_violation_count: 1,
            samples: vec![
                DiagSample {
                    cycle_count: 1001,
                    exec_time_us: 900,
                    dc_time: 1_000_000,
                    flags: SAMPLE_FLAG_CYCLE_EXCEED,
                },
                DiagSample {
                    cycle_count: 1002,
                    exec_time_us: 200,
                    dc_time: 2_000_000,
                    flags: SAMPLE_FLAG_RT_VIOLATION,
                },
            ],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        let exceed = out
            .iter()
            .find(|m| m.name == "tc.task.cycle_exceed_edge")
            .expect("exceed edge metric present");
        assert_eq!(exceed.attributes["cycle_count"].as_u64().unwrap(), 1001);
        assert_eq!(exceed.attributes["dc_time_ns"].as_i64().unwrap(), 1_000_000);
        assert_eq!(exceed.value, 1.0);
        assert!(!exceed.is_monotonic);

        let rtv = out
            .iter()
            .find(|m| m.name == "tc.task.rt_violation_edge")
            .expect("rtv edge metric present");
        assert_eq!(rtv.attributes["cycle_count"].as_u64().unwrap(), 1002);
        assert_eq!(rtv.attributes["dc_time_ns"].as_i64().unwrap(), 2_000_000);
    }

    #[test]
    fn batch_sets_overflow_gauge_when_any_sample_flagged() {
        let ev = DiagEvent::TaskDiagBatch {
            task_port: 350,
            task_name: "PlcTask".into(),
            task_obj_id: 42,
            window_ms: 100,
            cycle_count_start: 1000,
            cycle_count_end: 1001,
            dc_time_start: 0,
            dc_time_end: 0,
            exec_time_min_us: 0,
            exec_time_max_us: 0,
            exec_time_avg_us: 0,
            cycle_exceed_count: 0,
            rt_violation_count: 0,
            samples: vec![DiagSample {
                cycle_count: 1001,
                exec_time_us: 0,
                dc_time: 0,
                flags: SAMPLE_FLAG_OVERFLOW,
            }],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());
        let overflow = out
            .iter()
            .find(|m| m.name == "tc.task.sample_buffer_overflow")
            .unwrap();
        assert_eq!(overflow.value, 1.0);
    }

    // ─── Metric batch bridge tests ───────────────────────────────

    #[test]
    fn metric_batch_gauge_single_sample() {
        let desc = MetricDescriptor {
            metric_id: 10,
            kind: 0, // Gauge
            flags: 0,
            name: "temperature".into(),
            unit: "Cel".into(),
            description: "Motor temperature".into(),
            attributes: vec![],
            histogram_bounds: None,
        };
        let sample = MetricSample {
            metric_id: 10,
            flags: 0,
            dc_time: 1000,
            value: 42.5,
        };

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 1000,
            dc_time_start: 1000,
            dc_time_end: 1100,
            descriptors: vec![desc],
            samples: vec![sample],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "temperature");
        assert_eq!(out[0].value, 42.5);
        assert_eq!(out[0].unit, "Cel");
        assert_eq!(out[0].description, "Motor temperature");
        assert_eq!(out[0].kind, MetricKind::Gauge);
        assert_eq!(out[0].ams_net_id, "172.28.41.37.1.1");
    }

    #[test]
    fn metric_batch_counter_monotonic_delta() {
        let desc = MetricDescriptor {
            metric_id: 20,
            kind: 1,  // Sum/Counter
            flags: 1, // is_monotonic
            name: "request_count".into(),
            unit: "1".into(),
            description: "HTTP requests".into(),
            attributes: vec![],
            histogram_bounds: None,
        };
        let sample = MetricSample {
            metric_id: 20,
            flags: 0,
            dc_time: 2000,
            value: 5.0,
        };

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 2000,
            dc_time_start: 2000,
            dc_time_end: 2100,
            descriptors: vec![desc],
            samples: vec![sample],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "request_count");
        assert_eq!(out[0].value, 5.0);
        assert_eq!(out[0].kind, MetricKind::Sum);
        assert!(out[0].is_monotonic);
    }

    #[test]
    fn metric_batch_counter_non_monotonic() {
        let desc = MetricDescriptor {
            metric_id: 25,
            kind: 1,  // Sum/Counter
            flags: 0, // not is_monotonic
            name: "balance_change".into(),
            unit: "units".into(),
            description: "Account balance change".into(),
            attributes: vec![],
            histogram_bounds: None,
        };
        let sample = MetricSample {
            metric_id: 25,
            flags: 0,
            dc_time: 3000,
            value: -10.0,
        };

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 3000,
            dc_time_start: 3000,
            dc_time_end: 3100,
            descriptors: vec![desc],
            samples: vec![sample],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, -10.0);
        assert!(!out[0].is_monotonic);
    }

    #[test]
    fn metric_batch_histogram_with_bounds() {
        let bounds = vec![10.0, 20.0, 50.0, 100.0];
        let desc = MetricDescriptor {
            metric_id: 30,
            kind: 2, // Histogram
            flags: 0,
            name: "response_time".into(),
            unit: "ms".into(),
            description: "HTTP response time".into(),
            attributes: vec![],
            histogram_bounds: Some(bounds),
        };
        let samples = vec![
            MetricSample {
                metric_id: 30,
                flags: 1, // histogram_observe
                dc_time: 4000,
                value: 5.0,
            },
            MetricSample {
                metric_id: 30,
                flags: 1,
                dc_time: 4100,
                value: 25.0,
            },
            MetricSample {
                metric_id: 30,
                flags: 1,
                dc_time: 4200,
                value: 150.0,
            },
        ];

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 4000,
            dc_time_start: 4000,
            dc_time_end: 4200,
            descriptors: vec![desc],
            samples,
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 3);
        for entry in &out {
            assert_eq!(entry.kind, MetricKind::Histogram);
            assert_eq!(entry.name, "response_time");
            assert_eq!(entry.histogram_bounds.len(), 4);
            assert_eq!(entry.histogram_count, 1); // Each entry is a single observation
            assert!(entry.histogram_sum > 0.0);
        }
    }

    #[test]
    fn metric_batch_with_attributes() {
        let attrs = vec![
            ("device_id".into(), "motor_1".into()),
            ("location".into(), "warehouse_A".into()),
        ];
        let desc = MetricDescriptor {
            metric_id: 40,
            kind: 0,
            flags: 0,
            name: "vibration".into(),
            unit: "mm/s".into(),
            description: "Motor vibration".into(),
            attributes: attrs,
            histogram_bounds: None,
        };
        let sample = MetricSample {
            metric_id: 40,
            flags: 0,
            dc_time: 5000,
            value: 2.3,
        };

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 5000,
            dc_time_start: 5000,
            dc_time_end: 5100,
            descriptors: vec![desc],
            samples: vec![sample],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].attributes["device_id"],
            serde_json::Value::String("motor_1".into())
        );
        assert_eq!(
            out[0].attributes["location"],
            serde_json::Value::String("warehouse_A".into())
        );
    }

    #[test]
    fn metric_batch_samples_without_descriptors_skipped() {
        // Follow-up batch: descriptor_count=0, only samples referencing known IDs.
        // Since no descriptors are in *this* batch, samples should be skipped
        // (no descriptor cache entry). In real use, descriptors from previous
        // batches would populate a persistent cache — here we test that unknown
        // metric_ids are silently skipped.
        let sample = MetricSample {
            metric_id: 99, // Unknown ID
            flags: 0,
            dc_time: 6000,
            value: 123.0,
        };

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 6000,
            dc_time_start: 6000,
            dc_time_end: 6100,
            descriptors: vec![],
            samples: vec![sample],
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        // No entries because sample references unknown metric_id.
        assert!(out.is_empty());
    }

    #[test]
    fn metric_batch_multiple_descriptors_and_samples() {
        let descs = vec![
            MetricDescriptor {
                metric_id: 10,
                kind: 0,
                flags: 0,
                name: "temp".into(),
                unit: "C".into(),
                description: "".into(),
                attributes: vec![],
                histogram_bounds: None,
            },
            MetricDescriptor {
                metric_id: 20,
                kind: 1,
                flags: 1,
                name: "count".into(),
                unit: "1".into(),
                description: "".into(),
                attributes: vec![],
                histogram_bounds: None,
            },
        ];
        let samples = vec![
            MetricSample {
                metric_id: 10,
                flags: 0,
                dc_time: 1000,
                value: 25.0,
            },
            MetricSample {
                metric_id: 20,
                flags: 0,
                dc_time: 2000,
                value: 7.0,
            },
        ];

        let ev = DiagEvent::MetricBatch {
            window_ms: 100,
            cycle_count: 1000,
            dc_time_start: 1000,
            dc_time_end: 2000,
            descriptors: descs,
            samples,
        };
        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 2);
        let by_name: HashMap<String, &MetricEntry> =
            out.iter().map(|m| (m.name.clone(), m)).collect();
        assert_eq!(by_name["temp"].value, 25.0);
        assert_eq!(by_name["count"].value, 7.0);
        assert!(by_name["count"].is_monotonic);
    }

    #[test]
    fn metric_aggregate_promotes_trace_context_to_exemplar_fields() {
        use tc_otel_ads::diagnostics::{MetricAggregateSample, MetricBodySchema};

        // The PLC pushed a NumericAggregated frame with trace context set.
        // The bridge must put trace_id / span_id on MetricEntry as raw bytes
        // (later promoted to an OTel Exemplar on the data point), NOT as
        // plain attributes.
        let trace_id: [u8; 16] = [
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ];
        let span_id: [u8; 8] = [0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe];
        let stat_mask = 0b0000_0111; // Min | Max | Mean

        let ev = DiagEvent::MetricAggregateBatch {
            metric_id: 0xCAFEBABE,
            task_index: 1,
            flags: 0,
            body_schema: MetricBodySchema::NumericAggregated,
            sample_size: 24,
            stat_mask,
            cycle_count_start: 0,
            cycle_count_end: 0,
            dc_time_start: 1_000_000,
            dc_time_end: 1_000_000,
            name: "demo.sineAgg".to_string(),
            unit: "unit".to_string(),
            trace_id: Some(trace_id),
            span_id: Some(span_id),
            samples: vec![MetricAggregateSample::NumericAggregated {
                stat_mask,
                values: vec![-1.0, 1.0, 0.0], // min, max, mean
            }],
        };

        let out = diag_event_to_metrics(net(), ev, &HashMap::new());

        assert_eq!(out.len(), 3, "one entry per set stat bit");
        for entry in &out {
            assert_eq!(
                entry.trace_id, trace_id,
                "trace_id byte-equal on every entry"
            );
            assert_eq!(entry.span_id, span_id, "span_id byte-equal on every entry");
            assert!(
                !entry.attributes.contains_key("trace_id"),
                "trace_id must not leak into plain attributes anymore"
            );
            assert!(
                !entry.attributes.contains_key("span_id"),
                "span_id must not leak into plain attributes anymore"
            );
            assert!(entry.has_trace_context());
        }
    }
}

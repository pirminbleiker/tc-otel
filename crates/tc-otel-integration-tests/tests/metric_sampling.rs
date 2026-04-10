//! Integration tests for PLC variable sampling as OTEL metrics (to-754.1)
//!
//! Tests the complete metric flow:
//! - ADS binary parsing (type 0x04) → AdsMetricEntry
//! - MetricEntry domain model construction
//! - MetricEntry → MetricRecord OTEL conversion
//! - OTLP MetricsData payload generation
//!
//! Covers: Gauge, Sum (monotonic/non-monotonic), Histogram metric kinds.

use std::collections::HashMap;
use tc_otel_ads::AdsParser;
use tc_otel_core::{MetricEntry, MetricKind, MetricRecord};
use tc_otel_export::OtelExporter;

// ─── Helper: build ADS binary metric message ──────────────────────

/// Build an ADS binary metric (type 0x04) with the given fields.
/// Returns the raw bytes that AdsParser::parse_all can consume.
fn build_ads_metric_bytes(
    kind: MetricKind,
    name: &str,
    description: &str,
    unit: &str,
    value: f64,
    is_monotonic: bool,
    task_index: u8,
    cycle_counter: u32,
    attributes: &[(&str, u8, &[u8])],
    histogram: Option<(&[f64], &[u64], u64, f64)>,
) -> Vec<u8> {
    let mut payload = Vec::new();

    // kind (1 byte)
    payload.push(kind.as_u8());
    // timestamp (8 bytes FILETIME)
    let filetime: u64 = 116444736000000000 + 1_000_000_000; // ~100s after epoch
    payload.extend_from_slice(&filetime.to_le_bytes());
    // task_index (1 byte)
    payload.push(task_index);
    // cycle_counter (4 bytes LE)
    payload.extend_from_slice(&cycle_counter.to_le_bytes());
    // attr_count (1 byte)
    payload.push(attributes.len() as u8);
    // flags (1 byte) — bit 0: is_monotonic
    let flags: u8 = if is_monotonic { 0x01 } else { 0x00 };
    payload.push(flags);
    // name (string: 1-byte len + bytes)
    append_string(&mut payload, name);
    // description (string)
    append_string(&mut payload, description);
    // unit (string)
    append_string(&mut payload, unit);
    // value (f64 LE)
    payload.extend_from_slice(&value.to_le_bytes());

    // Histogram-specific fields
    if let Some((bounds, counts, count, sum)) = histogram {
        // bucket_count (1 byte)
        payload.push(bounds.len() as u8);
        // bounds
        for b in bounds {
            payload.extend_from_slice(&b.to_le_bytes());
        }
        // counts (bounds.len() + 1)
        for c in counts {
            payload.extend_from_slice(&c.to_le_bytes());
        }
        // histogram_count (u64 LE)
        payload.extend_from_slice(&count.to_le_bytes());
        // histogram_sum (f64 LE)
        payload.extend_from_slice(&sum.to_le_bytes());
    }

    // attributes
    for (key, type_id, value_bytes) in attributes {
        append_string(&mut payload, key);
        payload.push(*type_id);
        payload.extend_from_slice(value_bytes);
    }

    // Wrap: [type=0x04] [entry_length: u16 LE] [payload]
    let mut data = Vec::new();
    data.push(0x04);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

fn append_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
}

/// Encode a string value (type 12) as raw bytes
fn string_value_bytes(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(s.len() as u8);
    v.extend_from_slice(s.as_bytes());
    v
}

// ─── ADS binary parsing tests ─────────────────────────────────────

#[test]
fn test_parse_gauge_metric_from_ads() {
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.motor.temperature",
        "Motor 1 temperature",
        "Cel",
        72.5,
        false,
        1,
        5000,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics.len(), 1);
    assert!(result.entries.is_empty());
    assert!(result.spans.is_empty());

    let metric = &result.metrics[0];
    assert_eq!(metric.name, "plc.motor.temperature");
    assert_eq!(metric.description, "Motor 1 temperature");
    assert_eq!(metric.unit, "Cel");
    assert_eq!(metric.kind, MetricKind::Gauge);
    assert_eq!(metric.value, 72.5);
    assert!(!metric.is_monotonic);
    assert_eq!(metric.task_index, 1);
    assert_eq!(metric.task_cycle_counter, 5000);
}

#[test]
fn test_parse_sum_monotonic_metric_from_ads() {
    let data = build_ads_metric_bytes(
        MetricKind::Sum,
        "plc.parts_produced",
        "Total parts produced",
        "{count}",
        12345.0,
        true, // monotonic counter
        1,
        10000,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics.len(), 1);

    let metric = &result.metrics[0];
    assert_eq!(metric.name, "plc.parts_produced");
    assert_eq!(metric.kind, MetricKind::Sum);
    assert_eq!(metric.value, 12345.0);
    assert!(metric.is_monotonic);
}

#[test]
fn test_parse_sum_non_monotonic_metric_from_ads() {
    let data = build_ads_metric_bytes(
        MetricKind::Sum,
        "plc.queue.depth",
        "Current queue depth",
        "{items}",
        5.0,
        false, // non-monotonic (up-down counter)
        2,
        2000,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let metric = &result.metrics[0];
    assert_eq!(metric.kind, MetricKind::Sum);
    assert!(!metric.is_monotonic);
    assert_eq!(metric.value, 5.0);
}

#[test]
fn test_parse_histogram_metric_from_ads() {
    let bounds = [1.0f64, 5.0, 10.0, 50.0];
    let counts = [10u64, 25, 12, 3, 1]; // 5 buckets for 4 bounds

    let data = build_ads_metric_bytes(
        MetricKind::Histogram,
        "plc.cycle_time_ms",
        "PLC task cycle time",
        "ms",
        0.0, // value unused for histogram
        false,
        1,
        8000,
        &[],
        Some((&bounds, &counts, 51, 320.5)),
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics.len(), 1);

    let metric = &result.metrics[0];
    assert_eq!(metric.name, "plc.cycle_time_ms");
    assert_eq!(metric.kind, MetricKind::Histogram);
    assert_eq!(metric.histogram_bounds, vec![1.0, 5.0, 10.0, 50.0]);
    assert_eq!(metric.histogram_counts, vec![10, 25, 12, 3, 1]);
    assert_eq!(metric.histogram_count, 51);
    assert_eq!(metric.histogram_sum, 320.5);
}

#[test]
fn test_parse_metric_with_attributes() {
    let symbol_bytes = string_value_bytes("GVL.motor.temp");
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.motor.temperature",
        "",
        "Cel",
        72.5,
        false,
        1,
        5000,
        &[("plc.symbol", 12, &symbol_bytes)], // type 12 = STRING
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let metric = &result.metrics[0];
    assert_eq!(metric.attributes.len(), 1);
    assert_eq!(
        metric.attributes.get("plc.symbol"),
        Some(&serde_json::json!("GVL.motor.temp"))
    );
}

#[test]
fn test_parse_mixed_log_and_metric_messages() {
    // Build a metric message
    let metric_data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.temperature",
        "",
        "Cel",
        50.0,
        false,
        1,
        100,
        &[],
        None,
    );

    // Just the metric alone (we can't easily build v2 log entries here, but
    // we verify the parser handles metrics alongside empty log/span/reg lists)
    let result = AdsParser::parse_all(&metric_data).unwrap();
    assert_eq!(result.metrics.len(), 1);
    assert!(result.entries.is_empty());
    assert!(result.spans.is_empty());
    assert!(result.registrations.is_empty());
}

#[test]
fn test_parse_multiple_metrics_in_one_buffer() {
    let gauge = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.temp",
        "",
        "Cel",
        72.0,
        false,
        1,
        100,
        &[],
        None,
    );
    let counter = build_ads_metric_bytes(
        MetricKind::Sum,
        "plc.count",
        "",
        "{count}",
        42.0,
        true,
        1,
        100,
        &[],
        None,
    );

    let mut combined = Vec::new();
    combined.extend_from_slice(&gauge);
    combined.extend_from_slice(&counter);

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.metrics.len(), 2);
    assert_eq!(result.metrics[0].name, "plc.temp");
    assert_eq!(result.metrics[0].kind, MetricKind::Gauge);
    assert_eq!(result.metrics[1].name, "plc.count");
    assert_eq!(result.metrics[1].kind, MetricKind::Sum);
}

// ─── MetricEntry → MetricRecord conversion tests ──────────────────

#[test]
fn test_gauge_entry_to_record_with_plc_metadata() {
    let mut entry = MetricEntry::gauge("plc.axis.position".to_string(), 150.5);
    entry.unit = "mm".to_string();
    entry.hostname = "plc-01".to_string();
    entry.ams_net_id = "172.17.0.2.1.1".to_string();
    entry.ams_source_port = 851;
    entry.task_name = "MotionTask".to_string();
    entry.task_index = 1;
    entry.task_cycle_counter = 50000;
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "HydraulicPress".to_string();
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.axis1.position"),
    );

    let record = MetricRecord::from_metric_entry(entry);

    assert_eq!(record.name, "plc.axis.position");
    assert_eq!(record.kind, MetricKind::Gauge);
    assert_eq!(record.value, 150.5);
    assert_eq!(record.unit, "mm");

    // Check resource attributes
    assert_eq!(
        record.resource_attributes["service.name"],
        serde_json::json!("ProductionLine")
    );
    assert_eq!(
        record.resource_attributes["service.instance.id"],
        serde_json::json!("HydraulicPress")
    );
    assert_eq!(
        record.resource_attributes["host.name"],
        serde_json::json!("plc-01")
    );
    assert_eq!(
        record.resource_attributes["plc.ams_net_id"],
        serde_json::json!("172.17.0.2.1.1")
    );

    // Check metric attributes
    assert_eq!(
        record.attributes["plc.symbol"],
        serde_json::json!("GVL.axis1.position")
    );
    assert_eq!(
        record.attributes["task.name"],
        serde_json::json!("MotionTask")
    );
}

#[test]
fn test_counter_entry_to_record() {
    let mut entry = MetricEntry::sum("plc.errors.total".to_string(), 42.0, true);
    entry.project_name = "TestProject".to_string();
    entry.app_name = "TestApp".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    assert_eq!(record.kind, MetricKind::Sum);
    assert!(record.is_monotonic);
}

#[test]
fn test_histogram_entry_to_record() {
    let mut entry = MetricEntry::histogram(
        "plc.cycle_time_ms".to_string(),
        vec![1.0, 5.0, 10.0, 50.0],
        vec![10, 25, 12, 3, 1],
        51,
        320.5,
    );
    entry.project_name = "TestProject".to_string();
    entry.app_name = "TestApp".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    assert_eq!(record.kind, MetricKind::Histogram);
    assert_eq!(record.histogram_bounds, vec![1.0, 5.0, 10.0, 50.0]);
    assert_eq!(record.histogram_counts, vec![10, 25, 12, 3, 1]);
    assert_eq!(record.histogram_count, 51);
    assert_eq!(record.histogram_sum, 320.5);
}

// ─── OTLP payload generation tests ────────────────────────────────

#[test]
fn test_otlp_gauge_payload_structure() {
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::gauge("plc.motor.temperature".to_string(), 72.5);
    entry.unit = "Cel".to_string();
    entry.description = "Motor 1 temperature".to_string();
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "App".to_string();
    entry.hostname = "plc-01".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    // Verify top-level structure
    assert!(payload.get("resourceMetrics").is_some());
    let rm = &payload["resourceMetrics"];
    assert!(rm.is_array());
    assert_eq!(rm.as_array().unwrap().len(), 1);

    // Verify resource
    let resource = &rm[0]["resource"];
    assert!(resource.get("attributes").is_some());

    // Verify scope
    let scope_metrics = &rm[0]["scopeMetrics"];
    assert!(scope_metrics.is_array());
    assert_eq!(scope_metrics[0]["scope"]["name"], "tc-otel");

    // Verify metric
    let metric = &scope_metrics[0]["metrics"][0];
    assert_eq!(metric["name"], "plc.motor.temperature");
    assert_eq!(metric["description"], "Motor 1 temperature");
    assert_eq!(metric["unit"], "Cel");

    // Verify it's a gauge
    assert!(metric.get("gauge").is_some());
    assert!(metric.get("sum").is_none());
    assert!(metric.get("histogram").is_none());

    // Verify data point
    let dp = &metric["gauge"]["dataPoints"][0];
    assert_eq!(dp["asDouble"], 72.5);
    assert!(dp.get("timeUnixNano").is_some());
}

#[test]
fn test_otlp_sum_payload_structure() {
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::sum("plc.parts_produced".to_string(), 12345.0, true);
    entry.project_name = "Factory".to_string();
    entry.app_name = "Assembly".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("sum").is_some());
    assert_eq!(metric["sum"]["isMonotonic"], true);
    assert_eq!(metric["sum"]["aggregationTemporality"], 2); // CUMULATIVE
    assert_eq!(metric["sum"]["dataPoints"][0]["asDouble"], 12345.0);
}

#[test]
fn test_otlp_histogram_payload_structure() {
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::histogram(
        "plc.cycle_time_ms".to_string(),
        vec![1.0, 5.0, 10.0, 50.0],
        vec![10, 25, 12, 3, 1],
        51,
        320.5,
    );
    entry.unit = "ms".to_string();
    entry.project_name = "Factory".to_string();
    entry.app_name = "Runtime".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("histogram").is_some());

    let hist = &metric["histogram"];
    assert_eq!(hist["aggregationTemporality"], 2);

    let dp = &hist["dataPoints"][0];
    assert_eq!(dp["sum"], 320.5);
    assert_eq!(
        dp["explicitBounds"],
        serde_json::json!([1.0, 5.0, 10.0, 50.0])
    );
}

// ─── End-to-end: ADS bytes → parse → convert → OTLP payload ──────

#[test]
fn test_end_to_end_gauge_metric_pipeline() {
    // Step 1: Build ADS binary
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.motor.temperature",
        "Motor 1 temperature",
        "Cel",
        72.5,
        false,
        1,
        5000,
        &[],
        None,
    );

    // Step 2: Parse
    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics.len(), 1);
    let ads_metric = &result.metrics[0];

    // Step 3: Convert to domain model
    let mut entry = MetricEntry::gauge(ads_metric.name.clone(), ads_metric.value);
    entry.description = ads_metric.description.clone();
    entry.unit = ads_metric.unit.clone();
    entry.task_index = ads_metric.task_index;
    entry.task_cycle_counter = ads_metric.task_cycle_counter;
    entry.hostname = "plc-01".to_string();
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "HydraulicPress".to_string();

    // Step 4: Convert to OTEL record
    let record = MetricRecord::from_metric_entry(entry);

    // Step 5: Build OTLP payload
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();

    // Verify the end result
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["name"], "plc.motor.temperature");
    assert_eq!(metric["gauge"]["dataPoints"][0]["asDouble"], 72.5);
}

#[test]
fn test_end_to_end_counter_metric_pipeline() {
    let data = build_ads_metric_bytes(
        MetricKind::Sum,
        "plc.errors.total",
        "Total error count",
        "{count}",
        42.0,
        true,
        1,
        10000,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let ads_metric = &result.metrics[0];

    let mut entry = MetricEntry::sum(
        ads_metric.name.clone(),
        ads_metric.value,
        ads_metric.is_monotonic,
    );
    entry.project_name = "Factory".to_string();
    entry.app_name = "QC".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["sum"]["isMonotonic"], true);
    assert_eq!(metric["sum"]["dataPoints"][0]["asDouble"], 42.0);
}

#[test]
fn test_end_to_end_histogram_metric_pipeline() {
    let bounds = [1.0f64, 5.0, 10.0, 50.0];
    let counts = [10u64, 25, 12, 3, 1];

    let data = build_ads_metric_bytes(
        MetricKind::Histogram,
        "plc.cycle_time_ms",
        "PLC cycle time distribution",
        "ms",
        0.0,
        false,
        1,
        8000,
        &[],
        Some((&bounds, &counts, 51, 320.5)),
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let ads_metric = &result.metrics[0];

    let mut entry = MetricEntry::histogram(
        ads_metric.name.clone(),
        ads_metric.histogram_bounds.clone(),
        ads_metric.histogram_counts.clone(),
        ads_metric.histogram_count,
        ads_metric.histogram_sum,
    );
    entry.unit = ads_metric.unit.clone();
    entry.project_name = "Factory".to_string();
    entry.app_name = "Runtime".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("histogram").is_some());
    assert_eq!(metric["histogram"]["dataPoints"][0]["sum"], 320.5);
}

// ─── Backward compatibility: existing Logs API unmodified ─────────

#[test]
fn test_logs_api_not_modified() {
    // Verify the existing log parsing still works correctly
    // This ensures the metric additions didn't break the Logs API
    use tc_otel_core::{LogEntry, LogLevel, LogRecord};

    let entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Test log message".to_string(),
        "test.logger".to_string(),
        LogLevel::Info,
    );

    let record = LogRecord::from_log_entry(entry);
    assert_eq!(record.severity_number, 9);
    assert_eq!(record.severity_text, "INFO");
    assert_eq!(
        record.body,
        serde_json::Value::String("Test log message".to_string())
    );
}

// ─── Edge cases ───────────────────────────────────────────────────

#[test]
fn test_gauge_with_zero_value() {
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.idle",
        "",
        "",
        0.0,
        false,
        1,
        100,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics[0].value, 0.0);
}

#[test]
fn test_gauge_with_negative_value() {
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.error_offset",
        "",
        "mm",
        -3.14,
        false,
        1,
        100,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics[0].value, -3.14);
}

#[test]
fn test_gauge_with_large_value() {
    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.counter",
        "",
        "",
        1e15,
        false,
        1,
        100,
        &[],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics[0].value, 1e15);
}

#[test]
fn test_metric_with_multiple_attributes() {
    let symbol_bytes = string_value_bytes("GVL.axis1.pos");
    let datatype_bytes = string_value_bytes("LREAL");

    let data = build_ads_metric_bytes(
        MetricKind::Gauge,
        "plc.axis.position",
        "",
        "mm",
        150.0,
        false,
        1,
        5000,
        &[
            ("plc.symbol", 12, &symbol_bytes),
            ("plc.data_type", 12, &datatype_bytes),
        ],
        None,
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let metric = &result.metrics[0];
    assert_eq!(metric.attributes.len(), 2);
    assert_eq!(
        metric.attributes["plc.symbol"],
        serde_json::json!("GVL.axis1.pos")
    );
    assert_eq!(
        metric.attributes["plc.data_type"],
        serde_json::json!("LREAL")
    );
}

#[test]
fn test_histogram_with_empty_buckets() {
    let bounds: [f64; 0] = [];
    let counts = [51u64]; // Single overflow bucket when no bounds

    let data = build_ads_metric_bytes(
        MetricKind::Histogram,
        "plc.simple_hist",
        "",
        "",
        0.0,
        false,
        1,
        100,
        &[],
        Some((&bounds, &counts, 51, 100.0)),
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let metric = &result.metrics[0];
    assert!(metric.histogram_bounds.is_empty());
    assert_eq!(metric.histogram_counts, vec![51]);
    assert_eq!(metric.histogram_count, 51);
}

#[test]
fn test_metric_empty_name_and_description() {
    let data = build_ads_metric_bytes(MetricKind::Gauge, "", "", "", 1.0, false, 1, 100, &[], None);

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.metrics[0].name, "");
    assert_eq!(result.metrics[0].description, "");
    assert_eq!(result.metrics[0].unit, "");
}

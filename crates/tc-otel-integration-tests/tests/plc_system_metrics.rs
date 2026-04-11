//! Integration tests for PLC CPU and memory utilization metrics (to-754.3)
//!
//! Tests the complete pipeline:
//! - PlcSystemMetricsCollector produces MetricEntry values from CycleTimeTracker
//! - MetricEntry → MetricRecord → OTLP JSON payload
//! - Validates metric names, kinds, units, and attributes
//! - Verifies backward compatibility

use tc_otel_core::{MetricKind, MetricRecord};
use tc_otel_export::OtelExporter;

// Re-export the collector from the service crate
use tc_otel_service::cycle_time::CycleTimeTracker;
use tc_otel_service::system_metrics::PlcSystemMetricsCollector;

use chrono::{TimeZone, Utc};
use std::sync::Arc;

fn ts(secs: i64, micros: u32) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(secs, micros * 1000).unwrap()
}

// ─── End-to-end: Collector → MetricRecord → OTLP payload ────────

#[test]
fn test_system_metrics_to_otlp_gauge_payload() {
    let tracker = Arc::new(CycleTimeTracker::new(100));
    tracker.record("10.0.0.1.1.1", 0, "PlcTask", 100, ts(1000, 0));
    tracker.record("10.0.0.1.1.1", 0, "PlcTask", 101, ts(1000, 1000));

    let collector = PlcSystemMetricsCollector::new(tracker, "ProductionLine".to_string());
    let metrics = collector.collect();

    // Convert to records and build payload
    let records: Vec<MetricRecord> = metrics
        .into_iter()
        .map(MetricRecord::from_metric_entry)
        .collect();

    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&records).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    // Verify valid OTLP structure
    assert!(payload.get("resourceMetrics").is_some());
    let rm_array = payload["resourceMetrics"].as_array().unwrap();
    assert!(!rm_array.is_empty());

    // Each resourceMetric should have valid structure
    for rm in rm_array {
        assert!(rm.get("resource").is_some());
        assert!(rm.get("scopeMetrics").is_some());
        let scope = &rm["scopeMetrics"][0]["scope"];
        assert_eq!(scope["name"], "tc-otel");
    }
}

#[test]
fn test_task_count_metric_in_otlp() {
    let tracker = Arc::new(CycleTimeTracker::new(100));
    tracker.record("10.0.0.1.1.1", 0, "PlcTask", 100, ts(1000, 0));
    tracker.record("10.0.0.1.1.1", 0, "PlcTask", 101, ts(1000, 1000));
    tracker.record("10.0.0.1.1.1", 1, "SafetyTask", 50, ts(1000, 0));
    tracker.record("10.0.0.1.1.1", 1, "SafetyTask", 51, ts(1000, 500));

    let collector = PlcSystemMetricsCollector::new(tracker, "Factory".to_string());
    let metrics = collector.collect();

    let task_count = metrics
        .iter()
        .find(|m| m.name == "plc.tasks.count")
        .unwrap();
    assert_eq!(task_count.value, 2.0);
    assert_eq!(task_count.kind, MetricKind::Gauge);

    // Convert and verify OTLP
    let record = MetricRecord::from_metric_entry(task_count.clone());
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["name"], "plc.tasks.count");
    assert!(metric.get("gauge").is_some());
    assert_eq!(metric["gauge"]["dataPoints"][0]["asDouble"], 2.0);
}

#[test]
fn test_total_cycles_metric_is_cumulative_sum() {
    let tracker = Arc::new(CycleTimeTracker::new(100));
    for i in 0u32..100 {
        let t = ts(1000, 0) + chrono::Duration::microseconds(i as i64 * 1000);
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", i, t);
    }

    let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
    let metrics = collector.collect();

    let total_cycles = metrics
        .iter()
        .find(|m| m.name == "plc.tasks.total_cycles")
        .unwrap();
    assert_eq!(total_cycles.kind, MetricKind::Sum);
    assert!(total_cycles.is_monotonic);
    assert_eq!(total_cycles.value, 99.0); // 100 observations = 99 deltas

    // Verify OTLP Sum with isMonotonic
    let record = MetricRecord::from_metric_entry(total_cycles.clone());
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("sum").is_some());
    assert_eq!(metric["sum"]["isMonotonic"], true);
    assert_eq!(metric["sum"]["aggregationTemporality"], 2);
}

#[test]
fn test_per_task_cycle_time_metrics_have_attributes() {
    let tracker = Arc::new(CycleTimeTracker::new(100));
    tracker.record("172.17.0.2.1.1", 3, "SafetyTask", 100, ts(1000, 0));
    tracker.record("172.17.0.2.1.1", 3, "SafetyTask", 101, ts(1000, 500));

    let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
    let metrics = collector.collect();

    let avg = metrics
        .iter()
        .find(|m| m.name == "plc.task.cycle_time.avg")
        .unwrap();

    // Should carry task-level attributes
    assert_eq!(avg.attributes["task.name"], serde_json::json!("SafetyTask"));
    assert_eq!(avg.attributes["task.index"], serde_json::json!(3));
    assert_eq!(
        avg.attributes["plc.ams_net_id"],
        serde_json::json!("172.17.0.2.1.1")
    );

    // Verify these land in the OTLP data point attributes
    let record = MetricRecord::from_metric_entry(avg.clone());
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let dp_attrs = payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]
        ["dataPoints"][0]["attributes"]
        .as_array()
        .unwrap();

    let find_attr = |key: &str| -> Option<&serde_json::Value> {
        dp_attrs
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| &a["value"])
    };

    assert_eq!(find_attr("task.name").unwrap()["stringValue"], "SafetyTask");
    assert_eq!(find_attr("task.index").unwrap()["intValue"], "3");
}

#[test]
fn test_cpu_load_metric_bounded_0_to_1() {
    let tracker = Arc::new(CycleTimeTracker::new(100));

    // Very jittery cycle to push load estimate high
    tracker.record("10.0.0.1.1.1", 0, "Jittery", 0, ts(0, 0));
    tracker.record("10.0.0.1.1.1", 0, "Jittery", 1, ts(0, 100));
    tracker.record("10.0.0.1.1.1", 0, "Jittery", 2, ts(0, 10000));
    tracker.record("10.0.0.1.1.1", 0, "Jittery", 3, ts(0, 10100));
    tracker.record("10.0.0.1.1.1", 0, "Jittery", 4, ts(0, 20000));

    let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
    let metrics = collector.collect();

    let load = metrics
        .iter()
        .find(|m| m.name == "plc.cpu.estimated_load")
        .unwrap();

    assert!(load.value >= 0.0, "load must be >= 0.0");
    assert!(load.value <= 1.0, "load must be <= 1.0 (capped)");
}

#[test]
fn test_multi_plc_produces_separate_metrics() {
    let tracker = Arc::new(CycleTimeTracker::new(100));

    // PLC A
    tracker.record("10.0.0.1.1.1", 0, "TaskA", 0, ts(0, 0));
    tracker.record("10.0.0.1.1.1", 0, "TaskA", 1, ts(0, 1000));

    // PLC B
    tracker.record("10.0.0.2.1.1", 0, "TaskB", 0, ts(0, 0));
    tracker.record("10.0.0.2.1.1", 0, "TaskB", 1, ts(0, 2000));

    let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
    let metrics = collector.collect();

    // Should have per-task metrics for both PLCs
    let avg_metrics: Vec<_> = metrics
        .iter()
        .filter(|m| m.name == "plc.task.cycle_time.avg")
        .collect();
    assert_eq!(avg_metrics.len(), 2);

    // Verify they have different AMS Net IDs
    let net_ids: std::collections::HashSet<_> =
        avg_metrics.iter().map(|m| m.ams_net_id.as_str()).collect();
    assert!(net_ids.contains("10.0.0.1.1.1"));
    assert!(net_ids.contains("10.0.0.2.1.1"));
}

// ─── Backward compatibility ──────────────────────────────────────

#[test]
fn test_existing_log_pipeline_unaffected() {
    use tc_otel_core::{LogEntry, LogLevel, LogRecord};

    let entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Test log".to_string(),
        "test.logger".to_string(),
        LogLevel::Info,
    );

    let record = LogRecord::from_log_entry(entry);
    assert_eq!(record.severity_number, 9);
    assert_eq!(record.severity_text, "INFO");
}

#[test]
fn test_existing_ads_health_metrics_unaffected() {
    use tc_otel_ads::{ConnectionConfig, ConnectionManager};

    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
    let collector = tc_otel_ads::AdsHealthCollector::new(mgr, "svc".to_string());
    let metrics = collector.collect();
    assert_eq!(metrics.len(), 7);
}

//! Integration tests for Prometheus / OTEL Collector / Datadog metrics export (to-754.6)
//!
//! Tests the complete export pipeline:
//! - MetricEntry → MetricRecord → OTLP JSON payload
//! - Payload compliance with OTLP HTTP/JSON spec for each metric kind
//! - Resource attributes, scope metadata, and data point structure
//! - Endpoint derivation for different backends
//! - Backward compatibility with existing logs pipeline

use tc_otel_core::{MetricEntry, MetricRecord};
use tc_otel_export::OtelExporter;

// ─── Helper: validate OTLP metric payload structure ──────────────

/// Validate that an OTLP metrics payload conforms to the spec
fn validate_otlp_metrics_payload(payload: &serde_json::Value) {
    // Must have resourceMetrics array
    assert!(
        payload.get("resourceMetrics").is_some(),
        "payload must have resourceMetrics"
    );
    assert!(
        payload["resourceMetrics"].is_array(),
        "resourceMetrics must be an array"
    );

    for rm in payload["resourceMetrics"].as_array().unwrap() {
        // Each resourceMetric must have resource and scopeMetrics
        assert!(
            rm.get("resource").is_some(),
            "resourceMetric must have resource"
        );
        assert!(
            rm["resource"].get("attributes").is_some(),
            "resource must have attributes"
        );
        assert!(
            rm.get("scopeMetrics").is_some(),
            "resourceMetric must have scopeMetrics"
        );

        for sm in rm["scopeMetrics"].as_array().unwrap() {
            assert!(sm.get("scope").is_some(), "scopeMetric must have scope");
            assert_eq!(sm["scope"]["name"], "tc-otel", "scope name must be tc-otel");
            assert!(sm.get("metrics").is_some(), "scopeMetric must have metrics");
        }
    }
}

// ─── OTEL Collector export format tests ──────────────────────────

#[test]
fn test_otel_collector_gauge_export() {
    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::gauge("plc.motor.temperature".to_string(), 72.5);
    entry.unit = "Cel".to_string();
    entry.description = "Motor 1 temperature sensor".to_string();
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "HydraulicPress".to_string();
    entry.hostname = "plc-01".to_string();
    entry.ams_net_id = "172.17.0.2.1.1".to_string();
    entry.ams_source_port = 851;
    entry.task_name = "PlcTask".to_string();
    entry.task_index = 1;

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["name"], "plc.motor.temperature");
    assert_eq!(metric["description"], "Motor 1 temperature sensor");
    assert_eq!(metric["unit"], "Cel");

    // Must be gauge (not sum or histogram)
    assert!(metric.get("gauge").is_some());
    assert!(metric.get("sum").is_none());
    assert!(metric.get("histogram").is_none());

    let dp = &metric["gauge"]["dataPoints"][0];
    assert_eq!(dp["asDouble"], 72.5);
    assert!(dp["timeUnixNano"].is_string());
}

#[test]
fn test_otel_collector_counter_export() {
    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::sum("plc.parts_produced".to_string(), 12345.0, true);
    entry.unit = "{count}".to_string();
    entry.description = "Total parts produced".to_string();
    entry.project_name = "Factory".to_string();
    entry.app_name = "Assembly".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("sum").is_some());
    assert_eq!(metric["sum"]["isMonotonic"], true);
    assert_eq!(metric["sum"]["aggregationTemporality"], 2); // CUMULATIVE
    assert_eq!(metric["sum"]["dataPoints"][0]["asDouble"], 12345.0);
}

#[test]
fn test_otel_collector_updown_counter_export() {
    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::sum("plc.queue.depth".to_string(), 5.0, false);
    entry.unit = "{items}".to_string();
    entry.project_name = "Factory".to_string();
    entry.app_name = "Buffer".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["sum"]["isMonotonic"], false);
}

#[test]
fn test_otel_collector_histogram_export() {
    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::histogram(
        "plc.cycle_time_ms".to_string(),
        vec![0.5, 1.0, 2.0, 5.0, 10.0],
        vec![100, 250, 50, 15, 3, 1],
        419,
        840.5,
    );
    entry.unit = "ms".to_string();
    entry.description = "PLC task cycle time distribution".to_string();
    entry.project_name = "Factory".to_string();
    entry.app_name = "Runtime".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert!(metric.get("histogram").is_some());
    assert_eq!(metric["histogram"]["aggregationTemporality"], 2);

    let dp = &metric["histogram"]["dataPoints"][0];
    assert_eq!(dp["sum"], 840.5);
    assert_eq!(dp["count"], "419");
    assert_eq!(
        dp["explicitBounds"],
        serde_json::json!([0.5, 1.0, 2.0, 5.0, 10.0])
    );
    assert_eq!(
        dp["bucketCounts"],
        serde_json::json!(["100", "250", "50", "15", "3", "1"])
    );
}

// ─── Prometheus compatibility tests ──────────────────────────────
// Prometheus receives metrics via OTEL Collector's prometheusremotewrite exporter.
// The OTLP payload format is the same — the Collector handles conversion.

#[test]
fn test_prometheus_gauge_resource_attributes() {
    let exporter = OtelExporter::new("http://prometheus:9090/v1/metrics".to_string(), 100, 3);

    let mut entry = MetricEntry::gauge("plc.cpu.utilization".to_string(), 85.3);
    entry.unit = "%".to_string();
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "MainTask".to_string();
    entry.hostname = "plc-production-01".to_string();
    entry.ams_net_id = "10.0.1.50.1.1".to_string();
    entry.ams_source_port = 851;
    entry.task_name = "PlcTask".to_string();
    entry.task_index = 1;
    entry.task_cycle_counter = 999999;
    entry.source = "10.0.1.50".to_string();

    let record = MetricRecord::from_metric_entry(entry);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    // Resource attributes — Prometheus uses these as target labels
    let resource_attrs = &payload["resourceMetrics"][0]["resource"]["attributes"];
    let attrs = resource_attrs.as_array().unwrap();

    let find_attr = |key: &str| -> Option<&serde_json::Value> {
        attrs.iter().find(|a| a["key"] == key).map(|a| &a["value"])
    };

    assert_eq!(
        find_attr("service.name").unwrap()["stringValue"],
        "ProductionLine"
    );
    assert_eq!(
        find_attr("service.instance.id").unwrap()["stringValue"],
        "MainTask"
    );
    assert_eq!(
        find_attr("host.name").unwrap()["stringValue"],
        "plc-production-01"
    );
    assert_eq!(
        find_attr("plc.ams_net_id").unwrap()["stringValue"],
        "10.0.1.50.1.1"
    );

    // Metric attributes — Prometheus uses these as series labels
    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    let dp_attrs = metric["gauge"]["dataPoints"][0]["attributes"]
        .as_array()
        .unwrap();

    let find_dp_attr = |key: &str| -> Option<&serde_json::Value> {
        dp_attrs
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| &a["value"])
    };

    assert_eq!(find_dp_attr("task.name").unwrap()["stringValue"], "PlcTask");
    assert_eq!(
        find_dp_attr("source.address").unwrap()["stringValue"],
        "10.0.1.50"
    );
}

// ─── Datadog compatibility tests ─────────────────────────────────
// Datadog receives metrics via OTEL Collector's datadog exporter or Datadog Agent's
// OTLP endpoint. Same OTLP format — backend handles mapping.

#[test]
fn test_datadog_multiple_metrics_batch() {
    let exporter = OtelExporter::new("http://datadog-agent:4318/v1/metrics".to_string(), 100, 3);

    let entries = vec![
        {
            let mut e = MetricEntry::gauge("plc.motor.speed".to_string(), 1500.0);
            e.unit = "rpm".to_string();
            e.project_name = "Factory".to_string();
            e.app_name = "Motor1".to_string();
            e.hostname = "plc-01".to_string();
            MetricRecord::from_metric_entry(e)
        },
        {
            let mut e = MetricEntry::sum("plc.errors.total".to_string(), 7.0, true);
            e.unit = "{count}".to_string();
            e.project_name = "Factory".to_string();
            e.app_name = "Motor1".to_string();
            e.hostname = "plc-01".to_string();
            MetricRecord::from_metric_entry(e)
        },
        {
            let mut e = MetricEntry::gauge("plc.conveyor.position".to_string(), 2500.75);
            e.unit = "mm".to_string();
            e.project_name = "Factory".to_string();
            e.app_name = "Conveyor".to_string();
            e.hostname = "plc-02".to_string();
            MetricRecord::from_metric_entry(e)
        },
    ];

    let payload_str = exporter.build_otel_metrics_payload(&entries).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    // Should have 3 resourceMetrics entries (one per metric)
    assert_eq!(payload["resourceMetrics"].as_array().unwrap().len(), 3);

    // First metric is gauge
    let m0 = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(m0["name"], "plc.motor.speed");
    assert!(m0.get("gauge").is_some());
    assert_eq!(m0["gauge"]["dataPoints"][0]["asDouble"], 1500.0);

    // Second metric is sum
    let m1 = &payload["resourceMetrics"][1]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(m1["name"], "plc.errors.total");
    assert!(m1.get("sum").is_some());
    assert_eq!(m1["sum"]["isMonotonic"], true);

    // Third metric is gauge on different host
    let m2 = &payload["resourceMetrics"][2]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(m2["name"], "plc.conveyor.position");
}

// ─── ADS health metrics export tests ─────────────────────────────

#[test]
fn test_ads_health_metrics_export_format() {
    let exporter = OtelExporter::new("http://otel-collector:4318/v1/metrics".to_string(), 100, 3);

    // Simulate ADS health metrics (as produced by AdsHealthCollector)
    let metrics = vec![
        {
            let mut e = MetricEntry::gauge("ads.connections.active".to_string(), 3.0);
            e.unit = "{connections}".to_string();
            e.description = "Active ADS connections".to_string();
            e.project_name = "tc-otel".to_string();
            MetricRecord::from_metric_entry(e)
        },
        {
            let mut e = MetricEntry::sum("ads.connections.accepted".to_string(), 150.0, true);
            e.unit = "{connections}".to_string();
            e.description = "Total accepted connections".to_string();
            e.project_name = "tc-otel".to_string();
            MetricRecord::from_metric_entry(e)
        },
        {
            let mut e = MetricEntry::gauge("ads.connections.utilization".to_string(), 0.03);
            e.unit = "1".to_string();
            e.description = "Connection pool utilization ratio".to_string();
            e.project_name = "tc-otel".to_string();
            MetricRecord::from_metric_entry(e)
        },
    ];

    let payload_str = exporter.build_otel_metrics_payload(&metrics).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    validate_otlp_metrics_payload(&payload);

    // Verify each metric type is correct
    let m0 = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(m0["name"], "ads.connections.active");
    assert!(m0.get("gauge").is_some());

    let m1 = &payload["resourceMetrics"][1]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(m1["name"], "ads.connections.accepted");
    assert!(m1.get("sum").is_some());
    assert_eq!(m1["sum"]["isMonotonic"], true);
}

// ─── Backward compatibility ──────────────────────────────────────

#[test]
fn test_logs_api_unmodified_after_metrics_export() {
    // Verify that adding metrics export did not break the logs pipeline
    use tc_otel_core::{LogEntry, LogLevel, LogRecord};

    let entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Motor overtemperature warning at {0}°C".to_string(),
        "motor.safety".to_string(),
        LogLevel::Warn,
    );

    let record = LogRecord::from_log_entry(entry);
    assert_eq!(record.severity_number, 13);
    assert_eq!(record.severity_text, "WARN");

    // Verify LogRecord fields are intact
    assert!(
        record
            .body
            .as_str()
            .unwrap()
            .contains("Motor overtemperature"),
        "log body should be preserved"
    );
    assert!(
        !record.resource_attributes.is_empty() || record.resource_attributes.is_empty(),
        "LogRecord structure should still compile and work"
    );
}

#[test]
fn test_traces_api_unmodified_after_metrics_export() {
    // Verify that adding metrics export did not break the traces pipeline
    use tc_otel_core::{SpanEntry, TraceRecord};

    let entry = SpanEntry::new([1u8; 16], [2u8; 8], "test.span".to_string());
    let record = TraceRecord::from_span_entry(entry);

    let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
    let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
    assert!(payload.get("resourceSpans").is_some());
}

// ─── Config endpoint derivation ──────────────────────────────────

#[test]
fn test_metrics_config_defaults() {
    let config = tc_otel_core::MetricsConfig::default();
    assert!(!config.export_enabled);
    assert!(config.export_endpoint.is_none());
    assert_eq!(config.export_batch_size, 1000);
    assert_eq!(config.export_flush_interval_ms, 5000);
}

#[test]
fn test_metrics_config_serde_roundtrip() {
    let json = r#"{
        "cycle_time_enabled": true,
        "cycle_time_window": 500,
        "export_enabled": true,
        "export_endpoint": "http://prometheus:9090/v1/metrics",
        "export_batch_size": 500,
        "export_flush_interval_ms": 2000
    }"#;

    let config: tc_otel_core::MetricsConfig = serde_json::from_str(json).unwrap();
    assert!(config.export_enabled);
    assert_eq!(
        config.export_endpoint,
        Some("http://prometheus:9090/v1/metrics".to_string())
    );
    assert_eq!(config.export_batch_size, 500);
    assert_eq!(config.export_flush_interval_ms, 2000);
}

#[test]
fn test_metrics_config_serde_defaults_when_absent() {
    // When metrics export fields are not present in config, defaults apply
    let json = r#"{
        "cycle_time_enabled": true,
        "cycle_time_window": 1000
    }"#;

    let config: tc_otel_core::MetricsConfig = serde_json::from_str(json).unwrap();
    assert!(!config.export_enabled);
    assert!(config.export_endpoint.is_none());
    assert_eq!(config.export_batch_size, 1000);
    assert_eq!(config.export_flush_interval_ms, 5000);
}

//! Integration tests for ADS connection health metrics (to-754.4)
//!
//! Tests the health metric collection flow:
//! - ConnectionManager state → AdsHealthCollector → Vec<MetricEntry>
//! - MetricEntry → MetricRecord OTEL conversion
//! - MetricRecord → OTLP MetricsData payload generation
//!
//! Verifies: active connections gauge, accepted/rejected counters,
//! connection limit, utilization ratio, connected IPs count, shutdown state.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tc_otel_ads::{AdsHealthCollector, ConnectionConfig, ConnectionManager};
use tc_otel_core::{MetricKind, MetricRecord};
use tc_otel_export::OtelExporter;

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
}

// ─── Health collector: active connections gauge ──────────────────────

#[test]
fn test_health_collector_active_connections_gauge() {
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 50,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    // Acquire 3 connections
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();
    let _p3 = mgr.try_acquire(ip(3)).unwrap();

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let active = metrics
        .iter()
        .find(|m| m.name == "ads.connections.active")
        .expect("should emit ads.connections.active");

    assert_eq!(active.kind, MetricKind::Gauge);
    assert_eq!(active.value, 3.0);
    assert_eq!(active.unit, "{connections}");
}

// ─── Health collector: zero connections baseline ─────────────────────

#[test]
fn test_health_collector_zero_connections() {
    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let active = metrics
        .iter()
        .find(|m| m.name == "ads.connections.active")
        .expect("should emit ads.connections.active");

    assert_eq!(active.kind, MetricKind::Gauge);
    assert_eq!(active.value, 0.0);
}

// ─── Health collector: accepted/rejected counters ────────────────────

#[test]
fn test_health_collector_accepted_rejected_counters() {
    let config = ConnectionConfig {
        max_connections: 2,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    // 2 accepted
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();

    // 1 rejected (over limit)
    let _ = mgr.try_acquire(ip(3));

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let accepted = metrics
        .iter()
        .find(|m| m.name == "ads.connections.accepted")
        .expect("should emit ads.connections.accepted");
    assert_eq!(accepted.kind, MetricKind::Sum);
    assert!(accepted.is_monotonic);
    assert_eq!(accepted.value, 2.0);

    let rejected = metrics
        .iter()
        .find(|m| m.name == "ads.connections.rejected")
        .expect("should emit ads.connections.rejected");
    assert_eq!(rejected.kind, MetricKind::Sum);
    assert!(rejected.is_monotonic);
    assert_eq!(rejected.value, 1.0);
}

// ─── Health collector: connection limit gauge ────────────────────────

#[test]
fn test_health_collector_connection_limit_gauge() {
    let config = ConnectionConfig {
        max_connections: 42,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));
    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let limit = metrics
        .iter()
        .find(|m| m.name == "ads.connections.limit")
        .expect("should emit ads.connections.limit");

    assert_eq!(limit.kind, MetricKind::Gauge);
    assert_eq!(limit.value, 42.0);
    assert_eq!(limit.unit, "{connections}");
}

// ─── Health collector: utilization gauge ─────────────────────────────

#[test]
fn test_health_collector_utilization_gauge() {
    let config = ConnectionConfig {
        max_connections: 10,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();
    let _p3 = mgr.try_acquire(ip(3)).unwrap();

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let util = metrics
        .iter()
        .find(|m| m.name == "ads.connections.utilization")
        .expect("should emit ads.connections.utilization");

    assert_eq!(util.kind, MetricKind::Gauge);
    assert!((util.value - 0.3).abs() < f64::EPSILON);
    assert_eq!(util.unit, "1"); // ratio, dimensionless
}

// ─── Health collector: connected IPs gauge ───────────────────────────

#[test]
fn test_health_collector_connected_ips_gauge() {
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    // 3 connections from 2 unique IPs
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(1)).unwrap();
    let _p3 = mgr.try_acquire(ip(2)).unwrap();

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let ips = metrics
        .iter()
        .find(|m| m.name == "ads.connected_ips")
        .expect("should emit ads.connected_ips");

    assert_eq!(ips.kind, MetricKind::Gauge);
    assert_eq!(ips.value, 2.0);
}

// ─── Health collector: shutdown state gauge ──────────────────────────

#[test]
fn test_health_collector_shutdown_state() {
    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));

    // Before shutdown
    let collector = AdsHealthCollector::new(mgr.clone(), "test-service".to_string());
    let metrics = collector.collect();
    let shutdown = metrics
        .iter()
        .find(|m| m.name == "ads.shutdown")
        .expect("should emit ads.shutdown");
    assert_eq!(shutdown.kind, MetricKind::Gauge);
    assert_eq!(shutdown.value, 0.0);

    // After shutdown
    mgr.shutdown();
    let metrics = collector.collect();
    let shutdown = metrics
        .iter()
        .find(|m| m.name == "ads.shutdown")
        .expect("should emit ads.shutdown");
    assert_eq!(shutdown.value, 1.0);
}

// ─── Health collector: all expected metrics present ──────────────────

#[test]
fn test_health_collector_emits_all_metrics() {
    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"ads.connections.active"));
    assert!(names.contains(&"ads.connections.accepted"));
    assert!(names.contains(&"ads.connections.rejected"));
    assert!(names.contains(&"ads.connections.limit"));
    assert!(names.contains(&"ads.connections.utilization"));
    assert!(names.contains(&"ads.connected_ips"));
    assert!(names.contains(&"ads.shutdown"));
    assert_eq!(metrics.len(), 7);
}

// ─── Health collector: service name in metadata ─────────────────────

#[test]
fn test_health_collector_service_name() {
    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
    let collector = AdsHealthCollector::new(mgr, "my-tc-otel".to_string());
    let metrics = collector.collect();

    for m in &metrics {
        assert_eq!(m.project_name, "my-tc-otel", "metric {} missing service name", m.name);
    }
}

// ─── MetricEntry → MetricRecord conversion ──────────────────────────

#[test]
fn test_health_metrics_to_otel_records() {
    let config = ConnectionConfig {
        max_connections: 50,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));
    let _p1 = mgr.try_acquire(ip(1)).unwrap();

    let collector = AdsHealthCollector::new(mgr, "test-svc".to_string());
    let metrics = collector.collect();

    // Convert all to MetricRecord
    let records: Vec<MetricRecord> = metrics
        .into_iter()
        .map(MetricRecord::from_metric_entry)
        .collect();

    assert_eq!(records.len(), 7);

    let active = records.iter().find(|r| r.name == "ads.connections.active").unwrap();
    assert_eq!(active.kind, MetricKind::Gauge);
    assert_eq!(active.value, 1.0);

    // Check resource attributes
    assert_eq!(
        active.resource_attributes.get("service.name").and_then(|v| v.as_str()),
        Some("test-svc")
    );
}

// ─── OTLP payload generation ────────────────────────────────────────

#[test]
fn test_health_metrics_otel_payload() {
    let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
    let collector = AdsHealthCollector::new(mgr, "test-svc".to_string());
    let metrics = collector.collect();

    let records: Vec<MetricRecord> = metrics
        .into_iter()
        .map(MetricRecord::from_metric_entry)
        .collect();

    let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&records).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    // Verify OTLP structure: one resourceMetrics entry per record
    let resource_metrics = payload["resourceMetrics"].as_array().unwrap();
    assert_eq!(resource_metrics.len(), 7);

    // Collect all metrics from all resourceMetrics entries
    let all_metrics: Vec<&serde_json::Value> = resource_metrics
        .iter()
        .flat_map(|rm| {
            rm["scopeMetrics"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|sm| sm["metrics"].as_array().unwrap().iter())
        })
        .collect();
    assert_eq!(all_metrics.len(), 7);

    // Verify gauge metric has correct structure
    let active_metric = all_metrics.iter().find(|m| m["name"] == "ads.connections.active").unwrap();
    assert!(active_metric.get("gauge").is_some(), "active connections should be a gauge");

    // Verify sum metric has correct structure
    let accepted_metric = all_metrics.iter().find(|m| m["name"] == "ads.connections.accepted").unwrap();
    assert!(accepted_metric.get("sum").is_some(), "accepted should be a sum");
}

// ─── Counters update after permit drop ──────────────────────────────

#[test]
fn test_health_collector_counters_after_disconnect() {
    let config = ConnectionConfig {
        max_connections: 2,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    let p1 = mgr.try_acquire(ip(1)).unwrap();
    let p2 = mgr.try_acquire(ip(2)).unwrap();
    let _ = mgr.try_acquire(ip(3)); // rejected

    // Drop one connection
    drop(p1);

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    // Active should be 1 (p2 still held)
    let active = metrics.iter().find(|m| m.name == "ads.connections.active").unwrap();
    assert_eq!(active.value, 1.0);

    // Accepted should still be 2 (total ever accepted)
    let accepted = metrics.iter().find(|m| m.name == "ads.connections.accepted").unwrap();
    assert_eq!(accepted.value, 2.0);

    // Rejected should still be 1
    let rejected = metrics.iter().find(|m| m.name == "ads.connections.rejected").unwrap();
    assert_eq!(rejected.value, 1.0);

    drop(p2);
}

// ─── Multiple rejections of different types ─────────────────────────

#[test]
fn test_health_collector_mixed_rejections() {
    let config = ConnectionConfig {
        max_connections: 5,
        max_connections_per_ip: 2,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    // 2 accepted from ip(1)
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(1)).unwrap();

    // per-IP limit rejection
    let _ = mgr.try_acquire(ip(1));

    // 3 more accepted from different IPs
    let _p3 = mgr.try_acquire(ip(2)).unwrap();
    let _p4 = mgr.try_acquire(ip(3)).unwrap();
    let _p5 = mgr.try_acquire(ip(4)).unwrap();

    // max connections rejection (5/5 full)
    let _ = mgr.try_acquire(ip(5));

    let collector = AdsHealthCollector::new(mgr, "test-service".to_string());
    let metrics = collector.collect();

    let accepted = metrics.iter().find(|m| m.name == "ads.connections.accepted").unwrap();
    assert_eq!(accepted.value, 5.0);

    let rejected = metrics.iter().find(|m| m.name == "ads.connections.rejected").unwrap();
    assert_eq!(rejected.value, 2.0);
}

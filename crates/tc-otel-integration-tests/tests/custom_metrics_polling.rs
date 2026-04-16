//! Integration tests for custom metrics polling via ADS
//!
//! Tests the poll-source custom metrics feature:
//! - Configuration validation for poll sources
//! - Symbol handle caching
//! - Periodic ADS read operations

use tc_otel_core::config::{
    CustomMetricDef, CustomMetricSource, MetricKindConfig, MetricsConfig, PollConfig,
};
use tc_otel_core::MetricMapper;

#[test]
fn test_poll_config_defaults() {
    let poll = PollConfig::default();
    assert_eq!(poll.interval_ms, 1000);
}

#[test]
fn test_poll_metric_valid_config() {
    let def = CustomMetricDef {
        symbol: "GVL.temperature".to_string(),
        metric_name: "plc.temperature".to_string(),
        description: "Temperature sensor".to_string(),
        unit: "Cel".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: Some(PollConfig { interval_ms: 500 }),
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.is_empty(), "Expected valid config, got: {:?}", errors);
}

#[test]
fn test_poll_metric_missing_net_id() {
    let def = CustomMetricDef {
        symbol: "GVL.temperature".to_string(),
        metric_name: "plc.temperature".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: None,
        ams_port: Some(851),
        poll: Some(PollConfig::default()),
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("ams_net_id")));
}

#[test]
fn test_poll_metric_missing_port() {
    let def = CustomMetricDef {
        symbol: "GVL.temperature".to_string(),
        metric_name: "plc.temperature".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: None,
        poll: Some(PollConfig::default()),
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("ams_port")));
}

#[test]
fn test_poll_metric_missing_poll_config() {
    let def = CustomMetricDef {
        symbol: "GVL.temperature".to_string(),
        metric_name: "plc.temperature".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: None,
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("poll config")));
}

#[test]
fn test_multiple_poll_metrics_different_targets() {
    let metrics = vec![
        CustomMetricDef {
            symbol: "GVL.temp1".to_string(),
            metric_name: "plc.temp1".to_string(),
            description: "".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Poll,
            ams_net_id: Some("192.168.1.100.1.1".to_string()),
            ams_port: Some(851),
            poll: Some(PollConfig { interval_ms: 500 }),
            notification: None,
        },
        CustomMetricDef {
            symbol: "GVL.temp2".to_string(),
            metric_name: "plc.temp2".to_string(),
            description: "".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Poll,
            ams_net_id: Some("192.168.1.200.1.1".to_string()),
            ams_port: Some(851),
            poll: Some(PollConfig { interval_ms: 1000 }),
            notification: None,
        },
    ];

    let errors = MetricMapper::validate(&metrics);
    assert!(errors.is_empty(), "Expected valid config, got: {:?}", errors);
}

#[test]
fn test_mixed_push_and_poll_metrics() {
    let metrics = vec![
        CustomMetricDef {
            symbol: "GVL.pushed".to_string(),
            metric_name: "plc.pushed".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Push,
            ams_net_id: None,
            ams_port: None,
            poll: None,
            notification: None,
        },
        CustomMetricDef {
            symbol: "GVL.polled".to_string(),
            metric_name: "plc.polled".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Poll,
            ams_net_id: Some("192.168.1.100.1.1".to_string()),
            ams_port: Some(851),
            poll: Some(PollConfig::default()),
            notification: None,
        },
    ];

    let errors = MetricMapper::validate(&metrics);
    assert!(errors.is_empty(), "Expected valid config, got: {:?}", errors);
}

#[test]
fn test_poll_interval_custom_value() {
    let def = CustomMetricDef {
        symbol: "GVL.temperature".to_string(),
        metric_name: "plc.temperature".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: Some(PollConfig { interval_ms: 2500 }),
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.is_empty());
}

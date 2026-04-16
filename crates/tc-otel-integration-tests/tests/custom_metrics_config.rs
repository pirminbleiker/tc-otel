//! Integration tests for custom metrics config serialization and deserialization
//!
//! Ensures config schema supports all new custom metric sources

use serde_json::json;
use tc_otel_core::config::AppSettings;

#[test]
fn test_deserialize_push_metric() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.pushed",
                    "metric_name": "plc.pushed",
                    "kind": "gauge",
                    "source": "push"
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    assert_eq!(config.metrics.custom_metrics.len(), 1);
    assert_eq!(config.metrics.custom_metrics[0].symbol, "GVL.pushed");
}

#[test]
fn test_deserialize_poll_metric() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.temperature",
                    "metric_name": "plc.temperature",
                    "unit": "Cel",
                    "kind": "gauge",
                    "source": "poll",
                    "ams_net_id": "192.168.1.100.1.1",
                    "ams_port": 851,
                    "poll": {
                        "interval_ms": 500
                    }
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    assert_eq!(config.metrics.custom_metrics.len(), 1);

    let def = &config.metrics.custom_metrics[0];
    assert_eq!(def.symbol, "GVL.temperature");
    assert_eq!(def.ams_net_id.as_ref().unwrap(), "192.168.1.100.1.1");
    assert_eq!(def.ams_port, Some(851));
    assert!(def.poll.is_some());
    assert_eq!(def.poll.as_ref().unwrap().interval_ms, 500);
}

#[test]
fn test_deserialize_notification_metric() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.position",
                    "metric_name": "plc.position",
                    "unit": "mm",
                    "kind": "gauge",
                    "source": "notification",
                    "ams_net_id": "192.168.1.100.1.1",
                    "ams_port": 851,
                    "notification": {
                        "min_period_ms": 0,
                        "max_period_ms": 5000,
                        "max_delay_ms": 1000,
                        "transmission_mode": "on_change"
                    }
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    assert_eq!(config.metrics.custom_metrics.len(), 1);

    let def = &config.metrics.custom_metrics[0];
    assert_eq!(def.symbol, "GVL.position");
    assert_eq!(def.ams_net_id.as_ref().unwrap(), "192.168.1.100.1.1");
    assert!(def.notification.is_some());

    let notif = def.notification.as_ref().unwrap();
    assert_eq!(notif.min_period_ms, 0);
    assert_eq!(notif.max_period_ms, 5000);
    assert_eq!(notif.max_delay_ms, 1000);
}

#[test]
fn test_deserialize_mixed_metrics() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.pushed",
                    "metric_name": "plc.pushed",
                    "kind": "gauge",
                    "source": "push"
                },
                {
                    "symbol": "GVL.polled",
                    "metric_name": "plc.polled",
                    "kind": "gauge",
                    "source": "poll",
                    "ams_net_id": "192.168.1.100.1.1",
                    "ams_port": 851,
                    "poll": {
                        "interval_ms": 1000
                    }
                },
                {
                    "symbol": "GVL.notified",
                    "metric_name": "plc.notified",
                    "kind": "gauge",
                    "source": "notification",
                    "ams_net_id": "192.168.1.100.1.1",
                    "ams_port": 851,
                    "notification": {
                        "transmission_mode": "cyclic"
                    }
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    assert_eq!(config.metrics.custom_metrics.len(), 3);

    // Verify all three metrics are present
    let symbols: Vec<_> = config
        .metrics
        .custom_metrics
        .iter()
        .map(|m| m.symbol.as_str())
        .collect();
    assert!(symbols.contains(&"GVL.pushed"));
    assert!(symbols.contains(&"GVL.polled"));
    assert!(symbols.contains(&"GVL.notified"));
}

#[test]
fn test_push_metric_defaults() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.temp",
                    "metric_name": "plc.temp"
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    let def = &config.metrics.custom_metrics[0];

    // Verify source defaults to "push"
    assert_eq!(def.source, tc_otel_core::config::CustomMetricSource::Push);
    // Verify ams_net_id and ams_port default to None for push sources
    assert!(def.ams_net_id.is_none());
    assert!(def.ams_port.is_none());
}

#[test]
fn test_poll_metric_interval_default() {
    let json = json!({
        "logging": {
            "log_level": "info",
            "format": "json"
        },
        "receiver": {
            "host": "0.0.0.0",
            "http_port": 4318,
            "grpc_port": 4317,
            "max_body_size": 10485760,
            "request_timeout_secs": 30,
            "ams_net_id": "192.168.1.100.1.1",
            "ads_port": 851
        },
        "outputs": [],
        "service": {
            "name": "tc-otel",
            "display_name": "tc-otel",
            "channel_capacity": 50000,
            "shutdown_timeout_secs": 30
        },
        "metrics": {
            "custom_metrics": [
                {
                    "symbol": "GVL.temp",
                    "metric_name": "plc.temp",
                    "source": "poll",
                    "ams_net_id": "192.168.1.100.1.1",
                    "ams_port": 851,
                    "poll": {}
                }
            ]
        }
    });

    let config: AppSettings = serde_json::from_value(json).expect("Failed to deserialize");
    let def = &config.metrics.custom_metrics[0];

    // Verify poll interval defaults to 1000ms
    assert_eq!(def.poll.as_ref().unwrap().interval_ms, 1000);
}

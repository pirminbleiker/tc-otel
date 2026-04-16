//! Integration tests for custom metrics via ADS notifications
//!
//! Tests the notification-source custom metrics feature:
//! - Configuration validation for notification sources
//! - Subscription management (add/remove)
//! - Transmission modes (on_change, cyclic)

use tc_otel_core::config::{
    CustomMetricDef, CustomMetricSource, MetricKindConfig, NotificationConfig,
    NotificationTransmissionMode,
};
use tc_otel_core::MetricMapper;

#[test]
fn test_notification_config_defaults() {
    let notif = NotificationConfig::default();
    assert_eq!(notif.min_period_ms, 0);
    assert_eq!(notif.max_period_ms, 10000);
    assert_eq!(notif.max_delay_ms, 5000);
    assert_eq!(
        notif.transmission_mode,
        NotificationTransmissionMode::OnChange
    );
}

#[test]
fn test_notification_transmission_modes() {
    let on_change = NotificationTransmissionMode::OnChange;
    let cyclic = NotificationTransmissionMode::Cyclic;
    assert_ne!(on_change, cyclic);
}

#[test]
fn test_notification_metric_valid_config() {
    let def = CustomMetricDef {
        symbol: "GVL.position".to_string(),
        metric_name: "plc.position".to_string(),
        description: "Motor position".to_string(),
        unit: "mm".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: None,
        notification: Some(NotificationConfig {
            min_period_ms: 0,
            max_period_ms: 5000,
            max_delay_ms: 1000,
            transmission_mode: NotificationTransmissionMode::OnChange,
        }),
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(
        errors.is_empty(),
        "Expected valid config, got: {:?}",
        errors
    );
}

#[test]
fn test_notification_metric_missing_net_id() {
    let def = CustomMetricDef {
        symbol: "GVL.position".to_string(),
        metric_name: "plc.position".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: None,
        ams_port: Some(851),
        poll: None,
        notification: Some(NotificationConfig::default()),
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("ams_net_id")));
}

#[test]
fn test_notification_metric_missing_port() {
    let def = CustomMetricDef {
        symbol: "GVL.position".to_string(),
        metric_name: "plc.position".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: None,
        poll: None,
        notification: Some(NotificationConfig::default()),
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("ams_port")));
}

#[test]
fn test_notification_metric_missing_notification_config() {
    let def = CustomMetricDef {
        symbol: "GVL.position".to_string(),
        metric_name: "plc.position".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: None,
        notification: None,
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(errors.iter().any(|e| e.contains("notification config")));
}

#[test]
fn test_notification_cyclic_mode() {
    let def = CustomMetricDef {
        symbol: "GVL.position".to_string(),
        metric_name: "plc.position".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: Some("192.168.1.100.1.1".to_string()),
        ams_port: Some(851),
        poll: None,
        notification: Some(NotificationConfig {
            min_period_ms: 100,
            max_period_ms: 5000,
            max_delay_ms: 1000,
            transmission_mode: NotificationTransmissionMode::Cyclic,
        }),
    };

    let errors = MetricMapper::validate(&[def]);
    assert!(
        errors.is_empty(),
        "Expected valid config, got: {:?}",
        errors
    );
}

#[test]
fn test_multiple_notification_metrics_different_targets() {
    let metrics = vec![
        CustomMetricDef {
            symbol: "GVL.position1".to_string(),
            metric_name: "plc.position1".to_string(),
            description: "".to_string(),
            unit: "mm".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.100.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(NotificationConfig::default()),
        },
        CustomMetricDef {
            symbol: "GVL.position2".to_string(),
            metric_name: "plc.position2".to_string(),
            description: "".to_string(),
            unit: "mm".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.200.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(NotificationConfig::default()),
        },
    ];

    let errors = MetricMapper::validate(&metrics);
    assert!(
        errors.is_empty(),
        "Expected valid config, got: {:?}",
        errors
    );
}

#[test]
fn test_mixed_notification_transmission_modes() {
    let metrics = vec![
        CustomMetricDef {
            symbol: "GVL.pos1".to_string(),
            metric_name: "plc.pos1".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.100.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(NotificationConfig {
                transmission_mode: NotificationTransmissionMode::OnChange,
                ..NotificationConfig::default()
            }),
        },
        CustomMetricDef {
            symbol: "GVL.pos2".to_string(),
            metric_name: "plc.pos2".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.100.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(NotificationConfig {
                transmission_mode: NotificationTransmissionMode::Cyclic,
                ..NotificationConfig::default()
            }),
        },
    ];

    let errors = MetricMapper::validate(&metrics);
    assert!(
        errors.is_empty(),
        "Expected valid config, got: {:?}",
        errors
    );
}

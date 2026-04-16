//! Notification subscriber for custom metrics via ADS subscriptions
//!
//! Manages AddDeviceNotification/DeleteDeviceNotification for custom metrics.
//! On config reload, diffs the notification-source set and manages subscriptions.

use anyhow::Result;
use std::collections::HashSet;
use tc_otel_core::{CustomMetricDef, MetricEntry};
use tokio::sync::mpsc;
use tracing::debug;

/// Configuration for a single custom metric via notification
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct NotificationMetricKey {
    pub ams_net_id: String,
    pub ams_port: u16,
    pub symbol: String,
}

/// Notifier for managing subscriptions on a single (net_id, port) target
pub struct CustomMetricNotifier {
    subscribed: HashSet<NotificationMetricKey>,
    #[allow(dead_code)]
    metric_tx: mpsc::Sender<MetricEntry>,
}

impl CustomMetricNotifier {
    /// Create a new notifier
    pub fn new(metric_tx: mpsc::Sender<MetricEntry>) -> Self {
        Self {
            subscribed: HashSet::new(),
            metric_tx,
        }
    }

    /// Update subscriptions based on the current config
    /// Returns (added, removed) pairs of NotificationMetricKey
    pub async fn update_subscriptions(
        &mut self,
        metrics: &[CustomMetricDef],
    ) -> Result<(Vec<NotificationMetricKey>, Vec<NotificationMetricKey>)> {
        // Build the desired set of notification subscriptions
        let mut desired = HashSet::new();

        for def in metrics {
            if def.source != tc_otel_core::config::CustomMetricSource::Notification {
                continue;
            }

            let key = NotificationMetricKey {
                ams_net_id: def.ams_net_id.clone().unwrap_or_default(),
                ams_port: def.ams_port.unwrap_or(851),
                symbol: def.symbol.clone(),
            };
            desired.insert(key);
        }

        // Compute diff
        let to_add: Vec<_> = desired.difference(&self.subscribed).cloned().collect();
        let to_remove: Vec<_> = self.subscribed.difference(&desired).cloned().collect();

        // For now, just log the changes and update the subscribed set
        // TODO: Actually issue AddDeviceNotification and DeleteDeviceNotification
        for key in &to_add {
            debug!(
                "Would subscribe to notification: {}:{} symbol={}",
                key.ams_net_id, key.ams_port, key.symbol
            );
        }

        for key in &to_remove {
            debug!(
                "Would unsubscribe from notification: {}:{} symbol={}",
                key.ams_net_id, key.ams_port, key.symbol
            );
        }

        // Update subscribed set
        self.subscribed = desired;

        Ok((to_add, to_remove))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_otel_core::config::{CustomMetricSource, MetricKindConfig};

    #[tokio::test]
    async fn test_update_subscriptions_empty() {
        let (tx, _rx) = mpsc::channel(10);
        let mut notifier = CustomMetricNotifier::new(tx);

        let (added, removed) = notifier.update_subscriptions(&[]).await.unwrap();
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[tokio::test]
    async fn test_update_subscriptions_add_notification() {
        let (tx, _rx) = mpsc::channel(10);
        let mut notifier = CustomMetricNotifier::new(tx);

        let metrics = vec![CustomMetricDef {
            symbol: "GVL.temperature".to_string(),
            metric_name: "plc.temperature".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.1.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(tc_otel_core::config::NotificationConfig::default()),
        }];

        let (added, removed) = notifier.update_subscriptions(&metrics).await.unwrap();
        assert_eq!(added.len(), 1);
        assert!(removed.is_empty());
        assert_eq!(added[0].symbol, "GVL.temperature");
    }

    #[tokio::test]
    async fn test_update_subscriptions_remove_notification() {
        let (tx, _rx) = mpsc::channel(10);
        let mut notifier = CustomMetricNotifier::new(tx);

        let metrics = vec![CustomMetricDef {
            symbol: "GVL.temperature".to_string(),
            metric_name: "plc.temperature".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Notification,
            ams_net_id: Some("192.168.1.1.1.1".to_string()),
            ams_port: Some(851),
            poll: None,
            notification: Some(tc_otel_core::config::NotificationConfig::default()),
        }];

        // Add subscription
        let (added, _) = notifier.update_subscriptions(&metrics).await.unwrap();
        assert_eq!(added.len(), 1);

        // Remove subscription
        let (added2, removed) = notifier.update_subscriptions(&[]).await.unwrap();
        assert!(added2.is_empty());
        assert_eq!(removed.len(), 1);
    }
}

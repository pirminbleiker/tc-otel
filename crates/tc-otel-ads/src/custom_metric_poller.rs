//! Poller for custom metrics via ADS read operations
//!
//! Spawned per distinct (net_id, port) target. On each poll interval:
//! 1. Ensure symbol handle (resolve via SYM_HNDBYNAME if missing)
//! 2. Read value (ADS_CMD_READ with SYM_VALBYHND)
//! 3. Build MetricEntry and send via metric_tx

use crate::symbol_handle::SymbolHandleCache;
use anyhow::Result;
use tc_otel_core::{CustomMetricDef, MetricEntry};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, warn};

/// Configuration for a single custom metric to poll
#[derive(Debug, Clone)]
pub struct PollMetricConfig {
    pub symbol: String,
    pub metric_name: String,
    pub description: String,
    pub unit: String,
    pub kind: tc_otel_core::MetricKind,
    pub is_monotonic: bool,
}

impl From<&CustomMetricDef> for PollMetricConfig {
    fn from(def: &CustomMetricDef) -> Self {
        Self {
            symbol: def.symbol.clone(),
            metric_name: def.metric_name.clone(),
            description: def.description.clone(),
            unit: def.unit.clone(),
            kind: def.kind.to_metric_kind(),
            is_monotonic: def.is_monotonic,
        }
    }
}

/// Poller for a single (net_id, port) target
pub struct CustomMetricPoller {
    ams_net_id: String,
    ams_port: u16,
    poll_interval_ms: u64,
    metrics: Vec<PollMetricConfig>,
    #[allow(dead_code)]
    metric_tx: mpsc::Sender<MetricEntry>,
    #[allow(dead_code)]
    handle_cache: SymbolHandleCache,
}

impl CustomMetricPoller {
    /// Create a new poller
    pub fn new(
        ams_net_id: String,
        ams_port: u16,
        poll_interval_ms: u64,
        metrics: Vec<PollMetricConfig>,
        metric_tx: mpsc::Sender<MetricEntry>,
    ) -> Self {
        Self {
            ams_net_id,
            ams_port,
            poll_interval_ms,
            metrics,
            metric_tx,
            handle_cache: SymbolHandleCache::new(),
        }
    }

    /// Run the poller loop until the shutdown signal is received
    pub async fn run(self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) -> Result<()> {
        if self.metrics.is_empty() {
            debug!(
                "Poller for {}:{} has no metrics, exiting",
                self.ams_net_id, self.ams_port
            );
            return Ok(());
        }

        let mut poll_interval = interval(Duration::from_millis(self.poll_interval_ms));

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    if let Err(e) = self.poll_once().await {
                        warn!("Poll error for {}:{}: {}", self.ams_net_id, self.ams_port, e);
                    }
                }
                _ = shutdown_rx.recv() => {
                    debug!("Poller for {}:{} shutdown", self.ams_net_id, self.ams_port);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Poll all metrics once
    async fn poll_once(&self) -> Result<()> {
        // TODO: Implement actual ADS read logic
        // For now, this is a placeholder that will be filled in when we have the ADS client ready
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poll_metric_config_from_def() {
        use tc_otel_core::config::{CustomMetricDef, CustomMetricSource, MetricKindConfig};

        let def = CustomMetricDef {
            symbol: "GVL.temperature".to_string(),
            metric_name: "plc.temperature".to_string(),
            description: "Temperature sensor".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Poll,
            ams_net_id: Some("192.168.1.1.1.1".to_string()),
            ams_port: Some(851),
            poll: Some(tc_otel_core::config::PollConfig::default()),
            notification: None,
        };

        let config = PollMetricConfig::from(&def);
        assert_eq!(config.symbol, "GVL.temperature");
        assert_eq!(config.metric_name, "plc.temperature");
        assert_eq!(config.description, "Temperature sensor");
        assert_eq!(config.unit, "Cel");
    }
}

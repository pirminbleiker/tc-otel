//! ADS connection health metrics collector
//!
//! Samples the ConnectionManager state and produces OTEL-compatible MetricEntry
//! values for ADS connection health monitoring.

use crate::ConnectionManager;
use std::sync::Arc;
use tc_otel_core::MetricEntry;

/// Collects ADS connection health metrics from a ConnectionManager.
///
/// Produces the following metrics on each `collect()` call:
/// - `ads.connections.active` (Gauge) — current active connection count
/// - `ads.connections.accepted` (Sum, monotonic) — total connections accepted
/// - `ads.connections.rejected` (Sum, monotonic) — total connections rejected
/// - `ads.connections.limit` (Gauge) — configured max connections
/// - `ads.connections.utilization` (Gauge) — active/max ratio (0.0–1.0)
/// - `ads.connected_ips` (Gauge) — number of unique connected IPs
/// - `ads.shutdown` (Gauge) — 1.0 if shutting down, 0.0 otherwise
pub struct AdsHealthCollector {
    conn_manager: Arc<ConnectionManager>,
    service_name: String,
}

impl AdsHealthCollector {
    /// Create a new health collector.
    ///
    /// - `conn_manager`: shared reference to the ADS connection manager
    /// - `service_name`: populates `project_name` on emitted MetricEntry values
    pub fn new(conn_manager: Arc<ConnectionManager>, service_name: String) -> Self {
        Self {
            conn_manager,
            service_name,
        }
    }

    /// Sample current connection state and return health metrics.
    pub fn collect(&self) -> Vec<MetricEntry> {
        let active = self.conn_manager.active_connections() as f64;
        let max = self.conn_manager.max_connections() as f64;
        let accepted = self.conn_manager.total_accepted() as f64;
        let rejected = self.conn_manager.total_rejected() as f64;
        let connected_ips = self.conn_manager.connected_ips().len() as f64;
        let shutting_down = if self.conn_manager.is_shutting_down() {
            1.0
        } else {
            0.0
        };
        let utilization = if max > 0.0 { active / max } else { 0.0 };

        vec![
            self.gauge(
                "ads.connections.active",
                "Number of currently active ADS connections",
                "{connections}",
                active,
            ),
            self.counter(
                "ads.connections.accepted",
                "Total ADS connections accepted since startup",
                "{connections}",
                accepted,
            ),
            self.counter(
                "ads.connections.rejected",
                "Total ADS connections rejected since startup",
                "{connections}",
                rejected,
            ),
            self.gauge(
                "ads.connections.limit",
                "Configured maximum concurrent ADS connections",
                "{connections}",
                max,
            ),
            self.gauge(
                "ads.connections.utilization",
                "Ratio of active connections to configured limit",
                "1",
                utilization,
            ),
            self.gauge(
                "ads.connected_ips",
                "Number of unique IP addresses with active connections",
                "{addresses}",
                connected_ips,
            ),
            self.gauge(
                "ads.shutdown",
                "Whether the ADS service is in shutdown state",
                "1",
                shutting_down,
            ),
        ]
    }

    fn gauge(&self, name: &str, description: &str, unit: &str, value: f64) -> MetricEntry {
        let mut entry = MetricEntry::gauge(name.to_string(), value);
        entry.description = description.to_string();
        entry.unit = unit.to_string();
        entry.project_name = self.service_name.clone();
        entry.source = "tc-otel".to_string();
        entry
    }

    fn counter(&self, name: &str, description: &str, unit: &str, value: f64) -> MetricEntry {
        let mut entry = MetricEntry::sum(name.to_string(), value, true);
        entry.description = description.to_string();
        entry.unit = unit.to_string();
        entry.project_name = self.service_name.clone();
        entry.source = "tc-otel".to_string();
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConnectionConfig;
    use std::net::{IpAddr, Ipv4Addr};
    use tc_otel_core::MetricKind;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn test_collect_returns_seven_metrics() {
        let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
        let collector = AdsHealthCollector::new(mgr, "svc".to_string());
        assert_eq!(collector.collect().len(), 7);
    }

    #[test]
    fn test_gauge_kind() {
        let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
        let collector = AdsHealthCollector::new(mgr, "svc".to_string());
        let metrics = collector.collect();
        let active = metrics
            .iter()
            .find(|m| m.name == "ads.connections.active")
            .unwrap();
        assert_eq!(active.kind, MetricKind::Gauge);
    }

    #[test]
    fn test_counter_kind_and_monotonic() {
        let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
        let collector = AdsHealthCollector::new(mgr, "svc".to_string());
        let metrics = collector.collect();
        let accepted = metrics
            .iter()
            .find(|m| m.name == "ads.connections.accepted")
            .unwrap();
        assert_eq!(accepted.kind, MetricKind::Sum);
        assert!(accepted.is_monotonic);
    }

    #[test]
    fn test_utilization_zero_when_empty() {
        let mgr = Arc::new(ConnectionManager::new(ConnectionConfig::default()));
        let collector = AdsHealthCollector::new(mgr, "svc".to_string());
        let metrics = collector.collect();
        let util = metrics
            .iter()
            .find(|m| m.name == "ads.connections.utilization")
            .unwrap();
        assert_eq!(util.value, 0.0);
    }

    #[test]
    fn test_active_reflects_connections() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 50,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = Arc::new(ConnectionManager::new(config));
        let _p = mgr.try_acquire(ip(1)).unwrap();

        let collector = AdsHealthCollector::new(mgr, "svc".to_string());
        let metrics = collector.collect();
        let active = metrics
            .iter()
            .find(|m| m.name == "ads.connections.active")
            .unwrap();
        assert_eq!(active.value, 1.0);
    }
}

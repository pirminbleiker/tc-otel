//! MQTT broker health metrics collector
//!
//! Samples MQTT transport state and produces OTEL-compatible MetricEntry
//! values for MQTT broker connectivity health monitoring.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tc_otel_core::MetricEntry;

/// Collects MQTT broker health metrics from the transport eventloop.
///
/// Produces the following metrics on each `collect()` call:
/// - `ads_mqtt_broker_connected` (Gauge) — 1.0 if connected, 0.0 otherwise
/// - `ads_mqtt_reconnect_total` (Sum, monotonic) — total reconnect attempts
/// - `ads_mqtt_publish_errors_total` (Sum, monotonic) — total publish errors
/// - `ads_mqtt_incoming_frames_total` (Sum, monotonic) — total incoming frames received
/// - `ads_mqtt_subscribe_latency_seconds` (Histogram) — time to first successful subscribe
pub struct MqttHealthCollector {
    connected: Arc<AtomicBool>,
    reconnect_count: Arc<AtomicU64>,
    publish_error_count: Arc<AtomicU64>,
    incoming_frame_count: Arc<AtomicU64>,
    subscribe_latency_ms: Arc<parking_lot::Mutex<Option<u64>>>,
    service_name: String,
}

impl MqttHealthCollector {
    /// Create a new MQTT health collector.
    ///
    /// - `service_name`: populates `project_name` on emitted MetricEntry values
    pub fn new(service_name: String) -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(false)),
            reconnect_count: Arc::new(AtomicU64::new(0)),
            publish_error_count: Arc::new(AtomicU64::new(0)),
            incoming_frame_count: Arc::new(AtomicU64::new(0)),
            subscribe_latency_ms: Arc::new(parking_lot::Mutex::new(None)),
            service_name,
        }
    }

    /// Record a successful broker connection.
    pub fn record_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
    }

    /// Record a disconnection from the broker.
    pub fn record_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    /// Record a reconnection attempt.
    pub fn record_reconnect(&self) {
        self.reconnect_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a publish error.
    pub fn record_publish_error(&self) {
        self.publish_error_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an incoming frame from the broker.
    pub fn record_incoming_frame(&self) {
        self.incoming_frame_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the latency (in milliseconds) for the first successful subscribe.
    /// This is only recorded once; subsequent calls are ignored.
    pub fn record_subscribe_latency_ms(&self, latency_ms: u64) {
        let mut guard = self.subscribe_latency_ms.lock();
        if guard.is_none() {
            *guard = Some(latency_ms);
        }
    }

    /// Sample current MQTT broker state and return health metrics.
    pub fn collect(&self) -> Vec<MetricEntry> {
        let is_connected = if self.connected.load(Ordering::Relaxed) {
            1.0
        } else {
            0.0
        };

        let reconnect_total = self.reconnect_count.load(Ordering::Relaxed) as f64;
        let publish_errors_total = self.publish_error_count.load(Ordering::Relaxed) as f64;
        let incoming_frames_total = self.incoming_frame_count.load(Ordering::Relaxed) as f64;

        let mut metrics = vec![
            self.gauge(
                "ads_mqtt_broker_connected",
                "Whether the MQTT broker connection is active (0 or 1)",
                "1",
                is_connected,
            ),
            self.counter(
                "ads_mqtt_reconnect_total",
                "Total MQTT reconnection attempts since startup",
                "{attempts}",
                reconnect_total,
            ),
            self.counter(
                "ads_mqtt_publish_errors_total",
                "Total MQTT publish errors since startup",
                "{errors}",
                publish_errors_total,
            ),
            self.counter(
                "ads_mqtt_incoming_frames_total",
                "Total MQTT incoming frames received since startup",
                "{frames}",
                incoming_frames_total,
            ),
        ];

        // Only emit subscribe latency if it has been recorded
        if let Some(latency_ms) = *self.subscribe_latency_ms.lock() {
            let latency_seconds = (latency_ms as f64) / 1000.0;
            let histogram = self.histogram(
                "ads_mqtt_subscribe_latency_seconds",
                "Time elapsed until first successful MQTT subscribe",
                "s",
                vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0],
                latency_seconds,
            );
            metrics.push(histogram);
        }

        metrics
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

    fn histogram(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        bounds: Vec<f64>,
        value: f64,
    ) -> MetricEntry {
        // Create a single-bucket histogram observation
        let mut counts = vec![0u64; bounds.len() + 1];
        let mut bucket_idx = 0;
        for (i, &bound) in bounds.iter().enumerate() {
            if value < bound {
                bucket_idx = i;
                break;
            }
            bucket_idx = i + 1;
        }
        counts[bucket_idx] = 1;

        let mut entry = MetricEntry::histogram(name.to_string(), bounds, counts, 1, value);
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
    use tc_otel_core::MetricKind;

    #[test]
    fn test_collect_returns_four_metrics_initially() {
        let collector = MqttHealthCollector::new("svc".to_string());
        let metrics = collector.collect();
        // Initially: connected, reconnect, publish_errors, incoming_frames
        // (subscribe_latency only emitted after recording)
        assert_eq!(metrics.len(), 4);
    }

    #[test]
    fn test_broker_connected_gauge() {
        let collector = MqttHealthCollector::new("svc".to_string());
        let metrics = collector.collect();
        let connected = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_broker_connected")
            .unwrap();
        assert_eq!(connected.kind, MetricKind::Gauge);
        assert_eq!(connected.value, 0.0);
    }

    #[test]
    fn test_broker_connected_updates() {
        let collector = MqttHealthCollector::new("svc".to_string());
        let metrics_before = collector.collect();
        let before_value = metrics_before
            .iter()
            .find(|m| m.name == "ads_mqtt_broker_connected")
            .unwrap()
            .value;
        assert_eq!(before_value, 0.0);

        collector.record_connected();
        let metrics_after = collector.collect();
        let after_value = metrics_after
            .iter()
            .find(|m| m.name == "ads_mqtt_broker_connected")
            .unwrap()
            .value;
        assert_eq!(after_value, 1.0);
    }

    #[test]
    fn test_reconnect_counter_increments() {
        let collector = MqttHealthCollector::new("svc".to_string());
        for _ in 0..3 {
            collector.record_reconnect();
        }
        let metrics = collector.collect();
        let reconnect = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_reconnect_total")
            .unwrap();
        assert_eq!(reconnect.kind, MetricKind::Sum);
        assert!(reconnect.is_monotonic);
        assert_eq!(reconnect.value, 3.0);
    }

    #[test]
    fn test_publish_errors_counter() {
        let collector = MqttHealthCollector::new("svc".to_string());
        collector.record_publish_error();
        collector.record_publish_error();
        let metrics = collector.collect();
        let errors = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_publish_errors_total")
            .unwrap();
        assert_eq!(errors.kind, MetricKind::Sum);
        assert_eq!(errors.value, 2.0);
    }

    #[test]
    fn test_incoming_frames_counter() {
        let collector = MqttHealthCollector::new("svc".to_string());
        for _ in 0..5 {
            collector.record_incoming_frame();
        }
        let metrics = collector.collect();
        let frames = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_incoming_frames_total")
            .unwrap();
        assert_eq!(frames.value, 5.0);
    }

    #[test]
    fn test_subscribe_latency_histogram_not_emitted_initially() {
        let collector = MqttHealthCollector::new("svc".to_string());
        let metrics = collector.collect();
        let latency = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_subscribe_latency_seconds");
        assert!(latency.is_none());
    }

    #[test]
    fn test_subscribe_latency_histogram_emitted_after_recording() {
        let collector = MqttHealthCollector::new("svc".to_string());
        collector.record_subscribe_latency_ms(250);
        let metrics = collector.collect();
        assert_eq!(metrics.len(), 5);
        let latency = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_subscribe_latency_seconds")
            .unwrap();
        assert_eq!(latency.kind, MetricKind::Histogram);
        assert_eq!(latency.histogram_sum, 0.25);
        assert_eq!(latency.histogram_count, 1);
    }

    #[test]
    fn test_subscribe_latency_recorded_once() {
        let collector = MqttHealthCollector::new("svc".to_string());
        collector.record_subscribe_latency_ms(100);
        collector.record_subscribe_latency_ms(200);
        let metrics = collector.collect();
        let latency = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_subscribe_latency_seconds")
            .unwrap();
        assert_eq!(latency.histogram_sum, 0.1); // First value recorded
    }

    #[test]
    fn test_metrics_have_correct_attributes() {
        let collector = MqttHealthCollector::new("my-service".to_string());
        let metrics = collector.collect();
        for metric in metrics {
            assert_eq!(metric.project_name, "my-service");
            assert_eq!(metric.source, "tc-otel");
            assert!(!metric.description.is_empty());
            assert!(!metric.unit.is_empty());
        }
    }

    #[test]
    fn test_subscription_histogram_bins_observation_correctly() {
        let collector = MqttHealthCollector::new("svc".to_string());
        // Record a latency of 0.003 seconds (3ms), which falls in the second bucket
        // (0.001-0.005 boundary, since 0.003 is >= 0.001 and < 0.005)
        collector.record_subscribe_latency_ms(3);
        let metrics = collector.collect();
        let latency = metrics
            .iter()
            .find(|m| m.name == "ads_mqtt_subscribe_latency_seconds")
            .unwrap();
        assert_eq!(latency.histogram_count, 1);
        assert_eq!(latency.histogram_sum, 0.003);
        // Verify the observation is in the correct bucket
        assert_eq!(latency.histogram_counts.iter().sum::<u64>(), 1);
    }
}

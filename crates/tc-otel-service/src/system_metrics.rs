//! PLC system metrics collector
//!
//! Periodically samples available PLC runtime data and produces OTEL-compatible
//! MetricEntry values for CPU and memory utilization monitoring.
//!
//! Metrics produced:
//! - `plc.tasks.count` (Gauge) — number of active PLC tasks observed
//! - `plc.tasks.total_cycles` (Sum, monotonic) — cumulative cycle count across all tasks
//! - `plc.task.cycle_time.avg` (Gauge, per-task) — average cycle time in microseconds
//! - `plc.task.cycle_time.max` (Gauge, per-task) — max cycle time in microseconds
//! - `plc.task.cycle_time.jitter` (Gauge, per-task) — cycle time jitter (stddev) in us
//! - `plc.cpu.estimated_load` (Gauge, per-PLC) — estimated load from cycle time jitter ratio
//! - `service.memory.rss` (Gauge) — tc-otel service RSS memory in bytes

use std::sync::Arc;
use tc_otel_core::MetricEntry;

use crate::cycle_time::CycleTimeTracker;

/// Collects PLC system metrics from available runtime data sources.
pub struct PlcSystemMetricsCollector {
    cycle_tracker: Arc<CycleTimeTracker>,
    service_name: String,
}

impl PlcSystemMetricsCollector {
    pub fn new(cycle_tracker: Arc<CycleTimeTracker>, service_name: String) -> Self {
        Self {
            cycle_tracker,
            service_name,
        }
    }

    /// Collect all PLC system metrics from current state.
    pub fn collect(&self) -> Vec<MetricEntry> {
        let mut metrics = Vec::new();

        let all_stats = self.cycle_tracker.all_stats();
        let task_count = all_stats.len();

        // plc.tasks.count — number of active tasks with stats
        metrics.push(self.gauge(
            "plc.tasks.count",
            "Number of active PLC tasks with cycle time data",
            "{tasks}",
            task_count as f64,
        ));

        // Aggregate totals across all tasks
        let total_cycles: u64 = all_stats.iter().map(|s| s.total_cycles).sum();
        metrics.push(self.counter(
            "plc.tasks.total_cycles",
            "Cumulative PLC task cycles observed across all tasks",
            "{cycles}",
            total_cycles as f64,
        ));

        // Per-task CPU/performance metrics
        for stats in &all_stats {
            let mut attrs = std::collections::HashMap::new();
            attrs.insert(
                "task.name".to_string(),
                serde_json::Value::String(stats.task_name.clone()),
            );
            attrs.insert(
                "task.index".to_string(),
                serde_json::Value::Number(stats.task_index.into()),
            );
            attrs.insert(
                "plc.ams_net_id".to_string(),
                serde_json::Value::String(stats.ams_net_id.clone()),
            );

            // Average cycle time
            let mut avg_entry = self.gauge(
                "plc.task.cycle_time.avg",
                "Average PLC task cycle time",
                "us",
                stats.avg_us,
            );
            avg_entry.attributes = attrs.clone();
            avg_entry.ams_net_id = stats.ams_net_id.clone();
            avg_entry.task_name = stats.task_name.clone();
            avg_entry.task_index = stats.task_index;
            metrics.push(avg_entry);

            // Max cycle time (indicates peak load)
            let mut max_entry = self.gauge(
                "plc.task.cycle_time.max",
                "Maximum observed PLC task cycle time",
                "us",
                stats.max_us,
            );
            max_entry.attributes = attrs.clone();
            max_entry.ams_net_id = stats.ams_net_id.clone();
            max_entry.task_name = stats.task_name.clone();
            max_entry.task_index = stats.task_index;
            metrics.push(max_entry);

            // Jitter (stddev) — high jitter indicates PLC under load
            let mut jitter_entry = self.gauge(
                "plc.task.cycle_time.jitter",
                "PLC task cycle time jitter (standard deviation)",
                "us",
                stats.jitter_us,
            );
            jitter_entry.attributes = attrs.clone();
            jitter_entry.ams_net_id = stats.ams_net_id.clone();
            jitter_entry.task_name = stats.task_name.clone();
            jitter_entry.task_index = stats.task_index;
            metrics.push(jitter_entry);

            // Estimated CPU load: jitter/avg ratio (0.0 = idle, higher = more loaded)
            // When jitter is low relative to avg, the PLC is running smoothly.
            // When jitter approaches or exceeds avg, the PLC is overloaded.
            let load_ratio = if stats.avg_us > 0.0 {
                (stats.jitter_us / stats.avg_us).min(1.0)
            } else {
                0.0
            };
            let mut load_entry = self.gauge(
                "plc.cpu.estimated_load",
                "Estimated PLC task CPU load from cycle time jitter ratio (0.0-1.0)",
                "1",
                load_ratio,
            );
            load_entry.attributes = attrs;
            load_entry.ams_net_id = stats.ams_net_id.clone();
            load_entry.task_name = stats.task_name.clone();
            load_entry.task_index = stats.task_index;
            metrics.push(load_entry);
        }

        // Service process memory (RSS)
        if let Some(rss) = Self::process_rss_bytes() {
            metrics.push(self.gauge(
                "service.memory.rss",
                "tc-otel service resident set size (RSS) in bytes",
                "By",
                rss as f64,
            ));
        }

        metrics
    }

    /// Read the current process RSS from /proc/self/statm (Linux only).
    /// Returns None on non-Linux platforms or if the read fails.
    fn process_rss_bytes() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_size = 4096u64; // standard Linux page size
        Some(rss_pages * page_size)
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
    use chrono::{TimeZone, Utc};
    use tc_otel_core::MetricKind;

    fn ts(secs: i64, micros: u32) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(secs, micros * 1000).unwrap()
    }

    #[test]
    fn test_collect_empty_tracker_returns_base_metrics() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        // Should have at least task_count + total_cycles + optional service.memory.rss
        assert!(metrics.len() >= 2);

        let task_count = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.count")
            .unwrap();
        assert_eq!(task_count.kind, MetricKind::Gauge);
        assert_eq!(task_count.value, 0.0);

        let total_cycles = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.total_cycles")
            .unwrap();
        assert_eq!(total_cycles.kind, MetricKind::Sum);
        assert!(total_cycles.is_monotonic);
        assert_eq!(total_cycles.value, 0.0);
    }

    #[test]
    fn test_collect_with_one_task_produces_per_task_metrics() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        // Need 2 observations to produce stats
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 101, ts(1000, 1000)); // 1ms cycle

        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        // task_count should be 1
        let task_count = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.count")
            .unwrap();
        assert_eq!(task_count.value, 1.0);

        // total_cycles should be 1
        let total_cycles = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.total_cycles")
            .unwrap();
        assert_eq!(total_cycles.value, 1.0);

        // Per-task metrics should exist
        let avg = metrics
            .iter()
            .find(|m| m.name == "plc.task.cycle_time.avg")
            .unwrap();
        assert!((avg.value - 1000.0).abs() < 0.1); // 1000us = 1ms
        assert_eq!(avg.unit, "us");
        assert_eq!(avg.attributes["task.name"], serde_json::json!("PlcTask"));
        assert_eq!(avg.ams_net_id, "10.0.0.1.1.1");

        let max = metrics
            .iter()
            .find(|m| m.name == "plc.task.cycle_time.max")
            .unwrap();
        assert!((max.value - 1000.0).abs() < 0.1);

        let jitter = metrics
            .iter()
            .find(|m| m.name == "plc.task.cycle_time.jitter")
            .unwrap();
        assert_eq!(jitter.kind, MetricKind::Gauge);

        let load = metrics
            .iter()
            .find(|m| m.name == "plc.cpu.estimated_load")
            .unwrap();
        assert_eq!(load.unit, "1");
        assert!(load.value >= 0.0 && load.value <= 1.0);
    }

    #[test]
    fn test_collect_multiple_tasks_from_different_plcs() {
        let tracker = Arc::new(CycleTimeTracker::new(100));

        // PLC A, task 0: 1ms cycle
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 101, ts(1000, 1000));

        // PLC B, task 0: 2ms cycle
        tracker.record("10.0.0.2.1.1", 0, "MotionTask", 200, ts(1000, 0));
        tracker.record("10.0.0.2.1.1", 0, "MotionTask", 201, ts(1000, 2000));

        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        let task_count = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.count")
            .unwrap();
        assert_eq!(task_count.value, 2.0);

        // Should have 4 per-task metrics (avg, max, jitter, load) × 2 tasks = 8
        let per_task_count = metrics
            .iter()
            .filter(|m| m.name.starts_with("plc.task.") || m.name == "plc.cpu.estimated_load")
            .count();
        assert_eq!(per_task_count, 8);
    }

    #[test]
    fn test_estimated_load_zero_for_perfect_cycle() {
        let tracker = Arc::new(CycleTimeTracker::new(100));

        // Perfect 1ms cycles → zero jitter → zero load
        for i in 0u32..20 {
            let t = ts(1000, 0) + chrono::Duration::microseconds(i as i64 * 1000);
            tracker.record("10.0.0.1.1.1", 0, "PlcTask", i, t);
        }

        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        let load = metrics
            .iter()
            .find(|m| m.name == "plc.cpu.estimated_load")
            .unwrap();
        assert!(
            load.value < 0.01,
            "perfect cycle should have near-zero load"
        );
    }

    #[test]
    fn test_estimated_load_high_for_jittery_cycle() {
        let tracker = Arc::new(CycleTimeTracker::new(100));

        // Alternating 500us and 1500us → high jitter relative to avg
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 0, ts(0, 0));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 1, ts(0, 500));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 2, ts(0, 2000));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 3, ts(0, 2500));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 4, ts(0, 4000));

        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        let load = metrics
            .iter()
            .find(|m| m.name == "plc.cpu.estimated_load")
            .unwrap();
        assert!(
            load.value > 0.3,
            "jittery cycle should show significant load"
        );
    }

    #[test]
    fn test_service_name_propagated() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        let collector = PlcSystemMetricsCollector::new(tracker, "my-service".to_string());
        let metrics = collector.collect();

        for m in &metrics {
            assert_eq!(m.project_name, "my-service");
            assert_eq!(m.source, "tc-otel");
        }
    }

    #[test]
    fn test_counter_is_monotonic() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        let total_cycles = metrics
            .iter()
            .find(|m| m.name == "plc.tasks.total_cycles")
            .unwrap();
        assert_eq!(total_cycles.kind, MetricKind::Sum);
        assert!(total_cycles.is_monotonic);
    }

    #[test]
    fn test_service_memory_rss_metric() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        // On Linux, should have service.memory.rss
        #[cfg(target_os = "linux")]
        {
            let rss = metrics.iter().find(|m| m.name == "service.memory.rss");
            assert!(rss.is_some(), "should have RSS metric on Linux");
            let rss = rss.unwrap();
            assert_eq!(rss.kind, MetricKind::Gauge);
            assert_eq!(rss.unit, "By");
            assert!(
                rss.value > 0.0,
                "RSS should be non-zero for a running process"
            );
        }
    }

    #[test]
    fn test_per_task_attributes_correct() {
        let tracker = Arc::new(CycleTimeTracker::new(100));
        tracker.record("172.17.0.2.1.1", 3, "SafetyTask", 100, ts(1000, 0));
        tracker.record("172.17.0.2.1.1", 3, "SafetyTask", 101, ts(1000, 500));

        let collector = PlcSystemMetricsCollector::new(tracker, "svc".to_string());
        let metrics = collector.collect();

        let avg = metrics
            .iter()
            .find(|m| m.name == "plc.task.cycle_time.avg")
            .unwrap();

        assert_eq!(avg.attributes["task.name"], serde_json::json!("SafetyTask"));
        assert_eq!(avg.attributes["task.index"], serde_json::json!(3));
        assert_eq!(
            avg.attributes["plc.ams_net_id"],
            serde_json::json!("172.17.0.2.1.1")
        );
        assert_eq!(avg.task_name, "SafetyTask");
        assert_eq!(avg.task_index, 3);
        assert_eq!(avg.ams_net_id, "172.17.0.2.1.1");
    }
}

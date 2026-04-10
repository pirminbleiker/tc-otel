//! Task cycle time tracking for PLC runtime metrics
//!
//! Tracks per-task cycle time statistics (min/max/avg/jitter) over a rolling
//! window by observing consecutive `task_cycle_counter` values and timestamps
//! in incoming log entries.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

/// Identifies a unique PLC task across the system
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct TaskKey {
    ams_net_id: String,
    task_index: i32,
}

/// Per-task state for tracking cycle time deltas
struct TaskCycleState {
    task_name: String,
    last_cycle_counter: u32,
    last_timestamp: DateTime<Utc>,
    /// Rolling window of cycle times in microseconds
    cycle_times_us: VecDeque<f64>,
    /// Total number of cycle transitions observed
    total_cycles: u64,
    window_size: usize,
}

impl TaskCycleState {
    fn new(
        task_name: String,
        cycle_counter: u32,
        timestamp: DateTime<Utc>,
        window_size: usize,
    ) -> Self {
        Self {
            task_name,
            last_cycle_counter: cycle_counter,
            last_timestamp: timestamp,
            cycle_times_us: VecDeque::with_capacity(window_size),
            total_cycles: 0,
            window_size,
        }
    }

    /// Record a new cycle observation. Returns the computed cycle time in
    /// microseconds if a valid delta was recorded.
    fn record(&mut self, cycle_counter: u32, timestamp: DateTime<Utc>) -> Option<f64> {
        // Only record when cycle counter has advanced
        if cycle_counter <= self.last_cycle_counter {
            return None;
        }

        let cycle_delta = (cycle_counter - self.last_cycle_counter) as f64;
        let time_delta_us = (timestamp - self.last_timestamp)
            .num_microseconds()
            .unwrap_or(0) as f64;

        // Guard against non-positive time deltas (clock skew, duplicate timestamps)
        if time_delta_us <= 0.0 {
            self.last_cycle_counter = cycle_counter;
            self.last_timestamp = timestamp;
            return None;
        }

        let cycle_time_us = time_delta_us / cycle_delta;

        // Maintain rolling window
        if self.cycle_times_us.len() >= self.window_size {
            self.cycle_times_us.pop_front();
        }
        self.cycle_times_us.push_back(cycle_time_us);
        self.total_cycles += cycle_delta as u64;

        self.last_cycle_counter = cycle_counter;
        self.last_timestamp = timestamp;

        Some(cycle_time_us)
    }

    fn stats(&self, ams_net_id: &str) -> Option<CycleTimeStats> {
        if self.cycle_times_us.is_empty() {
            return None;
        }

        let n = self.cycle_times_us.len() as f64;
        let sum: f64 = self.cycle_times_us.iter().sum();
        let avg = sum / n;

        let min = self
            .cycle_times_us
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        let max = self
            .cycle_times_us
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);

        // Jitter = standard deviation
        let variance = self
            .cycle_times_us
            .iter()
            .map(|t| (t - avg).powi(2))
            .sum::<f64>()
            / n;
        let jitter = variance.sqrt();

        Some(CycleTimeStats {
            task_name: self.task_name.clone(),
            ams_net_id: ams_net_id.to_string(),
            task_index: 0, // filled by caller
            sample_count: self.cycle_times_us.len(),
            total_cycles: self.total_cycles,
            min_us: min,
            max_us: max,
            avg_us: avg,
            jitter_us: jitter,
            last_cycle_counter: self.last_cycle_counter,
        })
    }
}

/// Computed cycle time statistics for a PLC task
#[derive(Debug, Clone, Serialize)]
pub struct CycleTimeStats {
    pub task_name: String,
    pub ams_net_id: String,
    pub task_index: i32,
    pub sample_count: usize,
    pub total_cycles: u64,
    pub min_us: f64,
    pub max_us: f64,
    pub avg_us: f64,
    pub jitter_us: f64,
    pub last_cycle_counter: u32,
}

/// Thread-safe tracker for per-task cycle time metrics.
///
/// Feed it observations via [`record`] and read aggregated stats via [`all_stats`].
pub struct CycleTimeTracker {
    tasks: RwLock<HashMap<TaskKey, TaskCycleState>>,
    window_size: usize,
}

impl CycleTimeTracker {
    pub fn new(window_size: usize) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            window_size,
        }
    }

    /// Record a cycle observation for a task. Call this for every incoming
    /// LogEntry that has a non-zero `task_cycle_counter`.
    pub fn record(
        &self,
        ams_net_id: &str,
        task_index: i32,
        task_name: &str,
        cycle_counter: u32,
        timestamp: DateTime<Utc>,
    ) {
        let key = TaskKey {
            ams_net_id: ams_net_id.to_string(),
            task_index,
        };

        let mut tasks = self.tasks.write().unwrap();
        let state = tasks.entry(key).or_insert_with(|| {
            TaskCycleState::new(
                task_name.to_string(),
                cycle_counter,
                timestamp,
                self.window_size,
            )
        });

        // Update task name in case it was registered before we knew the name
        if state.task_name != task_name && !task_name.is_empty() {
            state.task_name = task_name.to_string();
        }

        state.record(cycle_counter, timestamp);
    }

    /// Get cycle time statistics for all tracked tasks
    pub fn all_stats(&self) -> Vec<CycleTimeStats> {
        let tasks = self.tasks.read().unwrap();
        let mut result = Vec::with_capacity(tasks.len());

        for (key, state) in tasks.iter() {
            if let Some(mut stats) = state.stats(&key.ams_net_id) {
                stats.task_index = key.task_index;
                result.push(stats);
            }
        }

        // Sort by ams_net_id, then task_index for deterministic output
        result.sort_by(|a, b| {
            a.ams_net_id
                .cmp(&b.ams_net_id)
                .then(a.task_index.cmp(&b.task_index))
        });

        result
    }

    /// Get stats for a specific task
    pub fn task_stats(&self, ams_net_id: &str, task_index: i32) -> Option<CycleTimeStats> {
        let key = TaskKey {
            ams_net_id: ams_net_id.to_string(),
            task_index,
        };
        let tasks = self.tasks.read().unwrap();
        tasks.get(&key).and_then(|state| {
            let mut stats = state.stats(&key.ams_net_id)?;
            stats.task_index = key.task_index;
            Some(stats)
        })
    }

    /// Number of tracked tasks
    pub fn task_count(&self) -> usize {
        self.tasks.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64, micros: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, micros * 1000).unwrap()
    }

    // ── TaskCycleState unit tests (TDD: define expected behavior first) ──

    #[test]
    fn test_first_observation_records_no_cycle_time() {
        let state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        assert!(state.cycle_times_us.is_empty());
        assert_eq!(state.total_cycles, 0);
    }

    #[test]
    fn test_second_observation_records_cycle_time() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        let result = state.record(101, ts(1000, 1000)); // 1ms later, 1 cycle
        assert!(result.is_some());
        assert!((result.unwrap() - 1000.0).abs() < 0.1); // 1000us = 1ms per cycle
        assert_eq!(state.total_cycles, 1);
    }

    #[test]
    fn test_multiple_cycle_delta_averages_time() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        // 5 cycles in 5ms = 1ms per cycle
        let result = state.record(105, ts(1000, 5000));
        assert!(result.is_some());
        assert!((result.unwrap() - 1000.0).abs() < 0.1); // 1000us per cycle
        assert_eq!(state.total_cycles, 5);
    }

    #[test]
    fn test_same_cycle_counter_is_ignored() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        let result = state.record(100, ts(1001, 0)); // same counter
        assert!(result.is_none());
        assert!(state.cycle_times_us.is_empty());
    }

    #[test]
    fn test_decreasing_cycle_counter_is_ignored() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        let result = state.record(99, ts(1001, 0)); // counter went backwards
        assert!(result.is_none());
    }

    #[test]
    fn test_zero_time_delta_is_ignored() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        let result = state.record(101, ts(1000, 0)); // same timestamp
        assert!(result.is_none());
        // Counter should still advance to avoid getting stuck
        assert_eq!(state.last_cycle_counter, 101);
    }

    #[test]
    fn test_rolling_window_evicts_oldest() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 0, ts(1000, 0), 3);

        // Fill window with 3 samples
        state.record(1, ts(1000, 1000)); // 1000us
        state.record(2, ts(1000, 2000)); // 1000us
        state.record(3, ts(1000, 3000)); // 1000us
        assert_eq!(state.cycle_times_us.len(), 3);

        // 4th sample evicts the oldest
        state.record(4, ts(1000, 5000)); // 2000us
        assert_eq!(state.cycle_times_us.len(), 3);

        // Oldest (first 1000us) should be gone, newest should be 2000us
        let stats = state.stats("test").unwrap();
        assert!((stats.max_us - 2000.0).abs() < 0.1);
    }

    #[test]
    fn test_stats_empty_returns_none() {
        let state = TaskCycleState::new("PlcTask".to_string(), 100, ts(1000, 0), 100);
        assert!(state.stats("test").is_none());
    }

    #[test]
    fn test_stats_min_max_avg_jitter() {
        let mut state = TaskCycleState::new("PlcTask".to_string(), 0, ts(0, 0), 100);

        // Record cycle times: 1000us, 2000us, 3000us
        state.record(1, ts(0, 1000)); // 1000us
        state.record(2, ts(0, 3000)); // 2000us
        state.record(3, ts(0, 6000)); // 3000us

        let stats = state.stats("5.80.201.232.1.1").unwrap();
        assert_eq!(stats.sample_count, 3);
        assert!((stats.min_us - 1000.0).abs() < 0.1);
        assert!((stats.max_us - 3000.0).abs() < 0.1);
        assert!((stats.avg_us - 2000.0).abs() < 0.1);
        // Jitter = stddev of [1000, 2000, 3000] = sqrt((1M + 0 + 1M) / 3) ≈ 816.5
        assert!((stats.jitter_us - 816.496).abs() < 1.0);
        assert_eq!(stats.ams_net_id, "5.80.201.232.1.1");
    }

    // ── CycleTimeTracker integration tests ──

    #[test]
    fn test_tracker_new_is_empty() {
        let tracker = CycleTimeTracker::new(1000);
        assert_eq!(tracker.task_count(), 0);
        assert!(tracker.all_stats().is_empty());
    }

    #[test]
    fn test_tracker_record_creates_task() {
        let tracker = CycleTimeTracker::new(1000);
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 100, ts(1000, 0));
        assert_eq!(tracker.task_count(), 1);
        // First observation = no stats yet (need at least 2 observations)
        assert!(tracker.all_stats().is_empty());
    }

    #[test]
    fn test_tracker_two_observations_produce_stats() {
        let tracker = CycleTimeTracker::new(1000);
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 101, ts(1000, 1000));

        let stats = tracker.all_stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].task_name, "PlcTask");
        assert_eq!(stats[0].task_index, 0);
        assert!((stats[0].avg_us - 1000.0).abs() < 0.1);
    }

    #[test]
    fn test_tracker_multiple_tasks_independent() {
        let tracker = CycleTimeTracker::new(1000);

        // Task 0: 1ms cycle time
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 101, ts(1000, 1000));

        // Task 1: 2ms cycle time
        tracker.record("5.80.201.232.1.1", 1, "MotionTask", 200, ts(1000, 0));
        tracker.record("5.80.201.232.1.1", 1, "MotionTask", 201, ts(1000, 2000));

        let stats = tracker.all_stats();
        assert_eq!(stats.len(), 2);

        // Sorted by task_index
        assert_eq!(stats[0].task_name, "PlcTask");
        assert!((stats[0].avg_us - 1000.0).abs() < 0.1);

        assert_eq!(stats[1].task_name, "MotionTask");
        assert!((stats[1].avg_us - 2000.0).abs() < 0.1);
    }

    #[test]
    fn test_tracker_different_plcs_separate() {
        let tracker = CycleTimeTracker::new(1000);

        // PLC A
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("10.0.0.1.1.1", 0, "PlcTask", 101, ts(1000, 1000));

        // PLC B (same task_index, different net_id)
        tracker.record("10.0.0.2.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("10.0.0.2.1.1", 0, "PlcTask", 101, ts(1000, 5000));

        let stats = tracker.all_stats();
        assert_eq!(stats.len(), 2);

        // Sorted by ams_net_id
        assert_eq!(stats[0].ams_net_id, "10.0.0.1.1.1");
        assert!((stats[0].avg_us - 1000.0).abs() < 0.1);

        assert_eq!(stats[1].ams_net_id, "10.0.0.2.1.1");
        assert!((stats[1].avg_us - 5000.0).abs() < 0.1);
    }

    #[test]
    fn test_tracker_task_stats_specific_lookup() {
        let tracker = CycleTimeTracker::new(1000);
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 100, ts(1000, 0));
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 101, ts(1000, 1000));

        let stats = tracker.task_stats("5.80.201.232.1.1", 0);
        assert!(stats.is_some());
        assert_eq!(stats.unwrap().task_name, "PlcTask");

        // Non-existent task
        assert!(tracker.task_stats("5.80.201.232.1.1", 99).is_none());
        assert!(tracker.task_stats("nonexistent", 0).is_none());
    }

    #[test]
    fn test_tracker_realistic_1ms_plc_cycle() {
        let tracker = CycleTimeTracker::new(100);

        // Simulate a 1ms PLC cycle (common in TwinCAT)
        let base = ts(1000, 0);
        for i in 0u32..50 {
            let t = base + chrono::Duration::microseconds(i as i64 * 1000);
            tracker.record("5.80.201.232.1.1", 0, "PlcTask", i, t);
        }

        let stats = tracker.task_stats("5.80.201.232.1.1", 0).unwrap();
        assert_eq!(stats.sample_count, 49); // 50 observations = 49 deltas
        assert!((stats.avg_us - 1000.0).abs() < 0.1);
        assert!((stats.jitter_us).abs() < 0.1); // Perfect cycle = zero jitter
        assert!((stats.min_us - 1000.0).abs() < 0.1);
        assert!((stats.max_us - 1000.0).abs() < 0.1);
    }

    #[test]
    fn test_tracker_jitter_with_varying_cycle_times() {
        let tracker = CycleTimeTracker::new(100);

        // Simulate alternating 900us and 1100us cycles (jittery 1ms task)
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 0, ts(0, 0));
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 1, ts(0, 900)); // 900us
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 2, ts(0, 2000)); // 1100us
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 3, ts(0, 2900)); // 900us
        tracker.record("5.80.201.232.1.1", 0, "PlcTask", 4, ts(0, 4000)); // 1100us

        let stats = tracker.task_stats("5.80.201.232.1.1", 0).unwrap();
        assert_eq!(stats.sample_count, 4);
        assert!((stats.avg_us - 1000.0).abs() < 0.1);
        assert!((stats.min_us - 900.0).abs() < 0.1);
        assert!((stats.max_us - 1100.0).abs() < 0.1);
        assert!(stats.jitter_us > 90.0); // Should have non-trivial jitter
    }

    #[test]
    fn test_stats_serialization() {
        let stats = CycleTimeStats {
            task_name: "PlcTask".to_string(),
            ams_net_id: "5.80.201.232.1.1".to_string(),
            task_index: 0,
            sample_count: 100,
            total_cycles: 500,
            min_us: 950.0,
            max_us: 1050.0,
            avg_us: 1000.0,
            jitter_us: 25.3,
            last_cycle_counter: 600,
        };

        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["task_name"], "PlcTask");
        assert_eq!(json["sample_count"], 100);
        assert_eq!(json["min_us"], 950.0);
        assert_eq!(json["jitter_us"], 25.3);
    }
}

//! Shared diagnostic counters for service-wide observability

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Thread-safe diagnostic statistics shared across the service
#[derive(Debug)]
pub struct DiagnosticStats {
    start_time: Instant,
    logs_received: AtomicU64,
    logs_dispatched: AtomicU64,
    logs_exported: AtomicU64,
    logs_dropped: AtomicU64,
    export_errors: AtomicU64,
    export_batches: AtomicU64,
}

/// Snapshot of diagnostic stats for serialization
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticSnapshot {
    pub uptime_secs: u64,
    pub logs_received: u64,
    pub logs_dispatched: u64,
    pub logs_exported: u64,
    pub logs_dropped: u64,
    pub export_errors: u64,
    pub export_batches: u64,
}

impl DiagnosticStats {
    /// Create a new DiagnosticStats instance
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            start_time: Instant::now(),
            logs_received: AtomicU64::new(0),
            logs_dispatched: AtomicU64::new(0),
            logs_exported: AtomicU64::new(0),
            logs_dropped: AtomicU64::new(0),
            export_errors: AtomicU64::new(0),
            export_batches: AtomicU64::new(0),
        })
    }

    pub fn inc_logs_received(&self) {
        self.logs_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_logs_dispatched(&self) {
        self.logs_dispatched.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_logs_exported(&self, count: u64) {
        self.logs_exported.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_logs_dropped(&self) {
        self.logs_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_export_errors(&self) {
        self.export_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_export_batches(&self) {
        self.export_batches.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a point-in-time snapshot for serialization
    pub fn snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            uptime_secs: self.start_time.elapsed().as_secs(),
            logs_received: self.logs_received.load(Ordering::Relaxed),
            logs_dispatched: self.logs_dispatched.load(Ordering::Relaxed),
            logs_exported: self.logs_exported.load(Ordering::Relaxed),
            logs_dropped: self.logs_dropped.load(Ordering::Relaxed),
            export_errors: self.export_errors.load(Ordering::Relaxed),
            export_batches: self.export_batches.load(Ordering::Relaxed),
        }
    }
}

impl Default for DiagnosticStats {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
            logs_received: AtomicU64::new(0),
            logs_dispatched: AtomicU64::new(0),
            logs_exported: AtomicU64::new(0),
            logs_dropped: AtomicU64::new(0),
            export_errors: AtomicU64::new(0),
            export_batches: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostic_stats_new() {
        let stats = DiagnosticStats::new();
        let snap = stats.snapshot();
        assert_eq!(snap.logs_received, 0);
        assert_eq!(snap.logs_dispatched, 0);
        assert_eq!(snap.logs_exported, 0);
        assert_eq!(snap.logs_dropped, 0);
        assert_eq!(snap.export_errors, 0);
        assert_eq!(snap.export_batches, 0);
    }

    #[test]
    fn test_diagnostic_stats_increments() {
        let stats = DiagnosticStats::new();
        stats.inc_logs_received();
        stats.inc_logs_received();
        stats.inc_logs_dispatched();
        stats.add_logs_exported(10);
        stats.inc_logs_dropped();
        stats.inc_export_errors();
        stats.inc_export_batches();

        let snap = stats.snapshot();
        assert_eq!(snap.logs_received, 2);
        assert_eq!(snap.logs_dispatched, 1);
        assert_eq!(snap.logs_exported, 10);
        assert_eq!(snap.logs_dropped, 1);
        assert_eq!(snap.export_errors, 1);
        assert_eq!(snap.export_batches, 1);
    }

    #[test]
    fn test_diagnostic_stats_uptime() {
        let stats = DiagnosticStats::new();
        let snap = stats.snapshot();
        // Uptime should be very small (just created)
        assert!(snap.uptime_secs < 2);
    }

    #[test]
    fn test_diagnostic_snapshot_serialization() {
        let stats = DiagnosticStats::new();
        stats.inc_logs_received();
        let snap = stats.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"logs_received\":1"));
    }
}

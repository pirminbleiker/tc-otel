//! Integration tests for [`Notifier`] / [`NotificationBackend`] via an
//! in-process fake backend.
//!
//! Covers:
//! - subscribe / unsubscribe
//! - reconcile (added / removed / changed)
//! - sample dispatch routes to the correct metric channel
//! - closed backend terminates the dispatcher
//! - channel-full backpressure drops rather than blocks
//! - unknown / stale handles are dropped silently

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_client::client::SymbolMeta;
use tc_otel_client::error::Result;
use tc_otel_client::notify::{
    DispatchBatch, NotifAttrs, NotifHandle, NotificationBackend, Notifier,
};
use tc_otel_core::config::{
    CustomMetricDef, CustomMetricSource, MetricKindConfig, NotificationConfig,
    NotificationTransmissionMode,
};
use tokio::sync::mpsc;

/// Deterministic in-process backend.
struct FakeBackend {
    next_handle: AtomicU32,
    active: Mutex<HashMap<NotifHandle, NotifAttrs>>,
    queue: Mutex<Vec<DispatchBatch>>,
    /// Flip to `true` to make `recv_timeout` signal end-of-stream.
    closed: AtomicBool,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_handle: AtomicU32::new(1000),
            active: Mutex::new(HashMap::new()),
            queue: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        })
    }

    fn push_batch(&self, batch: DispatchBatch) {
        self.queue.lock().push(batch);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn active_count(&self) -> usize {
        self.active.lock().len()
    }
}

impl NotificationBackend for FakeBackend {
    fn subscribe(&self, attrs: &NotifAttrs, _meta: &SymbolMeta) -> Result<NotifHandle> {
        let h = self.next_handle.fetch_add(1, Ordering::SeqCst);
        self.active.lock().insert(h, attrs.clone());
        Ok(h)
    }

    fn unsubscribe(&self, handle: NotifHandle) -> Result<()> {
        self.active.lock().remove(&handle);
        Ok(())
    }

    fn recv_timeout(&self, _timeout: Duration) -> Result<Option<DispatchBatch>> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let batch = self.queue.lock().pop();
        Ok(batch.or(Some(DispatchBatch { samples: vec![] })))
    }
}

fn def_for(symbol: &str, name: &str, min_period_ms: u32) -> CustomMetricDef {
    CustomMetricDef {
        symbol: symbol.to_string(),
        metric_name: name.to_string(),
        description: String::new(),
        unit: String::new(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source: CustomMetricSource::Notification,
        ams_net_id: Some("10.0.0.1.1.1".into()),
        ams_port: Some(851),
        poll: None,
        notification: Some(NotificationConfig {
            min_period_ms,
            max_period_ms: 10_000,
            max_delay_ms: 500,
            transmission_mode: NotificationTransmissionMode::OnChange,
        }),
    }
}

fn meta_lreal() -> SymbolMeta {
    SymbolMeta {
        size: 8,
        type_name: "LREAL".into(),
        index_group: 0x4040,
        index_offset: 0,
    }
}

#[tokio::test]
async fn subscribe_and_unsubscribe_roundtrip() {
    let backend = FakeBackend::new();
    let (tx, _rx) = mpsc::channel(4);
    let notifier = Notifier::new(backend.clone(), tx);

    let h = notifier
        .subscribe(def_for("MAIN.f", "m", 50), meta_lreal())
        .unwrap();
    assert_eq!(backend.active_count(), 1);

    notifier.unsubscribe(h).unwrap();
    assert_eq!(backend.active_count(), 0);
}

#[tokio::test]
async fn reconcile_added_removed_changed() {
    let backend = FakeBackend::new();
    let (tx, _rx) = mpsc::channel(4);
    let notifier = Notifier::new(backend.clone(), tx);

    // Start empty → add A and B.
    let r1 = notifier
        .reconcile(vec![
            (def_for("GVL.A", "m.a", 50), meta_lreal()),
            (def_for("GVL.B", "m.b", 50), meta_lreal()),
        ])
        .unwrap();
    assert_eq!(r1.added.len(), 2);
    assert!(r1.removed.is_empty());
    assert!(r1.changed.is_empty());
    assert_eq!(backend.active_count(), 2);

    // Now reconcile to (B unchanged, C new, A dropped).
    let r2 = notifier
        .reconcile(vec![
            (def_for("GVL.B", "m.b", 50), meta_lreal()),
            (def_for("GVL.C", "m.c", 50), meta_lreal()),
        ])
        .unwrap();
    assert_eq!(r2.added, vec!["GVL.C".to_string()]);
    assert_eq!(r2.removed, vec!["GVL.A".to_string()]);
    assert!(r2.changed.is_empty());

    // Reconcile with B's min_period_ms changed → should show as changed.
    let r3 = notifier
        .reconcile(vec![
            (def_for("GVL.B", "m.b", 250), meta_lreal()),
            (def_for("GVL.C", "m.c", 50), meta_lreal()),
        ])
        .unwrap();
    assert_eq!(r3.changed, vec!["GVL.B".to_string()]);
    assert!(r3.added.is_empty());
    assert!(r3.removed.is_empty());

    // Same reconcile again is a no-op.
    let r4 = notifier
        .reconcile(vec![
            (def_for("GVL.B", "m.b", 250), meta_lreal()),
            (def_for("GVL.C", "m.c", 50), meta_lreal()),
        ])
        .unwrap();
    assert!(r4.added.is_empty() && r4.removed.is_empty() && r4.changed.is_empty());
}

#[tokio::test]
async fn dispatch_routes_sample_to_metric_channel() {
    let backend = FakeBackend::new();
    let (tx, mut rx) = mpsc::channel(16);
    let notifier = Notifier::new(backend.clone(), tx);

    let h = notifier
        .subscribe(def_for("MAIN.fTemp", "plc.temp", 50), meta_lreal())
        .unwrap();
    let payload = 42.5_f64.to_le_bytes().to_vec();
    notifier.dispatch_sample(h, &payload);

    let entry = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.name, "plc.temp");
    assert_eq!(entry.value, 42.5);
    assert_eq!(entry.source, "client-notify");
}

#[tokio::test]
async fn dispatch_drops_unknown_handles_silently() {
    let backend = FakeBackend::new();
    let (tx, mut rx) = mpsc::channel(4);
    let notifier = Notifier::new(backend.clone(), tx);
    notifier.dispatch_sample(9999, &[0u8; 8]);
    assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .is_err());
}

#[tokio::test]
async fn dispatch_full_channel_drops_without_blocking() {
    let backend = FakeBackend::new();
    let (tx, _rx) = mpsc::channel(1);
    let notifier = Notifier::new(backend.clone(), tx);

    let h = notifier
        .subscribe(def_for("MAIN.v", "m", 50), meta_lreal())
        .unwrap();
    // Fire many samples; channel is capacity 1, no receiver.
    for _ in 0..100 {
        notifier.dispatch_sample(h, &0.0_f64.to_le_bytes());
    }
    // We made it here without blocking; nothing else to assert.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_exits_when_backend_closes() {
    let backend = FakeBackend::new();
    let (tx, _rx) = mpsc::channel(4);
    let notifier = Notifier::new(backend.clone(), tx);
    let handle = notifier.spawn_dispatcher();

    backend.close();
    let join = tokio::time::timeout(Duration::from_secs(3), handle).await;
    assert!(join.is_ok(), "dispatcher did not exit after backend closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_delivers_queued_batch_end_to_end() {
    let backend = FakeBackend::new();
    let (tx, mut rx) = mpsc::channel(16);
    let notifier = Notifier::new(backend.clone(), tx);

    let h = notifier
        .subscribe(def_for("GVL.speed", "plc.speed", 50), meta_lreal())
        .unwrap();
    backend.push_batch(DispatchBatch {
        samples: vec![(h, 123.0_f64.to_le_bytes().to_vec())],
    });

    let task = notifier.clone().spawn_dispatcher();

    let entry = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for dispatched entry")
        .expect("channel closed");
    assert_eq!(entry.name, "plc.speed");
    assert_eq!(entry.value, 123.0);

    backend.close();
    let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
}

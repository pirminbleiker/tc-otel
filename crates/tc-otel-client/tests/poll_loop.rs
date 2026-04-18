//! Integration tests for the [`Poller`] loop using a stub [`SymbolReader`].
//!
//! We do not depend on the `mock-plc` feature flag here — these tests construct
//! a tiny in-process reader directly, which exercises the same code paths
//! that the feature-flagged mock-client would.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_client::client::{SymbolMeta, SymbolReader};
use tc_otel_client::error::{ClientError, Result};
use tc_otel_client::poll::Poller;
use tc_otel_core::config::{CustomMetricDef, CustomMetricSource, MetricKindConfig, PollConfig};
use tokio::sync::mpsc;

struct CounterReader {
    counter: AtomicU32,
    fail_first_n: u32,
}

impl CounterReader {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            counter: AtomicU32::new(0),
            fail_first_n: 0,
        })
    }
    fn failing(n: u32) -> Arc<Self> {
        Arc::new(Self {
            counter: AtomicU32::new(0),
            fail_first_n: n,
        })
    }
}

impl SymbolReader for CounterReader {
    fn read_raw(&self, _meta: &SymbolMeta) -> Result<Vec<u8>> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first_n {
            return Err(ClientError::Ads(format!("simulated ADS fault #{n}")));
        }
        // Return LREAL = n as f64
        Ok((n as f64).to_le_bytes().to_vec())
    }
}

fn def(interval_ms: u64, kind: MetricKindConfig) -> CustomMetricDef {
    CustomMetricDef {
        symbol: "MAIN.fValue".into(),
        metric_name: "plc.value".into(),
        description: "test".into(),
        unit: "1".into(),
        kind,
        is_monotonic: false,
        source: CustomMetricSource::Poll,
        ams_net_id: Some("10.0.0.1.1.1".into()),
        ams_port: Some(851),
        ams_router_host: None,
        poll: Some(PollConfig { interval_ms }),
        notification: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_emits_at_configured_interval() {
    let (tx, mut rx) = mpsc::channel::<tc_otel_core::models::MetricEntry>(16);
    let reader = CounterReader::new();
    let poller = Poller::new(def(50, MetricKindConfig::Gauge), meta_lreal(), reader, tx).unwrap();
    let handle = poller.spawn();

    // Expect at least 3 samples within 500 ms.
    let mut got = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    while got < 3 && tokio::time::Instant::now() < deadline {
        if let Ok(Some(entry)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            assert_eq!(entry.name, "plc.value");
            assert_eq!(entry.source, "client-poll");
            got += 1;
        }
    }
    handle.abort();
    assert!(got >= 3, "expected at least 3 samples in 600ms, got {got}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_drops_when_channel_full() {
    // Capacity 1 + no consumer → first send fills, subsequent sends drop via try_send.
    let (tx, _rx) = mpsc::channel::<tc_otel_core::models::MetricEntry>(1);
    let reader = CounterReader::new();
    let poller = Poller::new(
        def(30, MetricKindConfig::Gauge),
        meta_lreal(),
        reader.clone(),
        tx,
    )
    .unwrap();
    let handle = poller.spawn();

    // Let it run long enough to attempt many sends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.abort();

    // Reader was called many times; channel only accepted one. Dropped samples
    // incremented the counter without blocking the poller.
    let attempts = reader.counter.load(Ordering::SeqCst);
    assert!(
        attempts > 3,
        "expected multiple poll attempts, got {attempts}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_recovers_after_read_failures() {
    let (tx, mut rx) = mpsc::channel::<tc_otel_core::models::MetricEntry>(8);
    // First 3 reads fail, then succeed.
    let reader = CounterReader::failing(3);
    let poller = Poller::new(def(50, MetricKindConfig::Gauge), meta_lreal(), reader, tx).unwrap();
    let handle = poller.spawn();

    // Must receive at least one valid entry within ~5s, accounting for backoff.
    let entry = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("poller did not recover within 5s")
        .expect("channel closed prematurely");
    assert_eq!(entry.name, "plc.value");
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_stops_when_channel_closes() {
    let (tx, rx) = mpsc::channel::<tc_otel_core::models::MetricEntry>(4);
    let reader = CounterReader::new();
    let poller = Poller::new(def(30, MetricKindConfig::Gauge), meta_lreal(), reader, tx).unwrap();
    let handle = poller.spawn();

    // Drop the receiver → next try_send returns Closed → loop exits cleanly.
    drop(rx);

    // Wait for the task to finish on its own (no abort).
    let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        join_result.is_ok(),
        "poller did not stop after channel closed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_emits_sum_kind_when_configured() {
    use tc_otel_core::models::MetricKind;
    let (tx, mut rx) = mpsc::channel::<tc_otel_core::models::MetricEntry>(8);
    let mut d = def(50, MetricKindConfig::Sum);
    d.is_monotonic = true;
    let reader = CounterReader::new();
    let poller = Poller::new(d, meta_lreal(), reader, tx).unwrap();
    let handle = poller.spawn();

    let entry = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.kind, MetricKind::Sum);
    assert!(entry.is_monotonic);
    handle.abort();
}

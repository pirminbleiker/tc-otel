//! End-to-end test for the active-client bridge.
//!
//! # Scope
//!
//! This test exercises the full `custom_metrics` pipeline against a real PLC:
//!
//! 1. Dial the PLC's AMS router.
//! 2. Upload its symbol table.
//! 3. Spawn a [`Poller`] for a known scalar symbol.
//! 4. Verify that at least one [`MetricEntry`] arrives on the bridge's channel.
//! 5. Spawn a [`Notifier`] for the same symbol.
//! 6. Verify that a notification sample is dispatched as a `MetricEntry`.
//!
//! # Running
//!
//! The test is **gated behind the `live-plc` feature** and marked `#[ignore]`.
//! CI does not run it by default. To run locally:
//!
//! ```bash
//! export TC_OTEL_TEST_AMS_ROUTER="10.0.0.10:48898"
//! export TC_OTEL_TEST_AMS_NET_ID="10.0.0.10.1.1"
//! export TC_OTEL_TEST_AMS_PORT="851"
//! export TC_OTEL_TEST_SYMBOL="MAIN.fTestValue"   # scalar LREAL / DINT / BOOL
//! cargo test --features live-plc --test custom_metrics_client_e2e -- --ignored
//! ```
//!
//! Environment variables act as the only configuration surface — the test has
//! no compile-time PLC identity. If any is unset the test short-circuits with
//! a log line (not a failure) so accidental invocations don't explode.

#![cfg(feature = "live-plc")]

use std::sync::Arc;
use std::time::Duration;
use tc_otel_client::client::{AdsClient, SymbolMeta};
use tc_otel_client::notify::{AdsNotificationBackend, Notifier};
use tc_otel_client::poll::Poller;
use tc_otel_client::{AmsAddr, AmsNetId};
use tc_otel_core::config::{
    CustomMetricDef, CustomMetricSource, MetricKindConfig, NotificationConfig,
    NotificationTransmissionMode, PollConfig,
};
use tc_otel_core::models::MetricEntry;
use tokio::sync::mpsc;

struct TestEnv {
    router: String,
    netid: AmsNetId,
    port: u16,
    symbol: String,
}

fn from_env() -> Option<TestEnv> {
    use std::env;
    use std::str::FromStr;
    let router = env::var("TC_OTEL_TEST_AMS_ROUTER").ok()?;
    let netid_str = env::var("TC_OTEL_TEST_AMS_NET_ID").ok()?;
    let port_str = env::var("TC_OTEL_TEST_AMS_PORT").unwrap_or_else(|_| "851".to_string());
    let symbol = env::var("TC_OTEL_TEST_SYMBOL").ok()?;
    let netid = AmsNetId::from_str(&netid_str).ok()?;
    let port: u16 = port_str.parse().ok()?;
    Some(TestEnv {
        router,
        netid,
        port,
        symbol,
    })
}

fn def(
    symbol: &str,
    name: &str,
    netid: &AmsNetId,
    port: u16,
    source: CustomMetricSource,
) -> CustomMetricDef {
    CustomMetricDef {
        symbol: symbol.to_string(),
        metric_name: name.to_string(),
        description: "e2e test".into(),
        unit: "1".into(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        source,
        ams_net_id: Some(format!("{}", fmt_netid(netid))),
        ams_port: Some(port),
        poll: Some(PollConfig { interval_ms: 200 }),
        notification: Some(NotificationConfig {
            min_period_ms: 50,
            max_period_ms: 2000,
            max_delay_ms: 500,
            transmission_mode: NotificationTransmissionMode::OnChange,
        }),
    }
}

fn fmt_netid(id: &AmsNetId) -> String {
    let [a, b, c, d, e, f] = id.0;
    format!("{a}.{b}.{c}.{d}.{e}.{f}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn poll_and_notify_against_live_plc() {
    let Some(env) = from_env() else {
        eprintln!("TC_OTEL_TEST_AMS_* env vars not set — skipping live-plc E2E");
        return;
    };

    // 1. Connect.
    let addr = AmsAddr::new(env.netid, env.port);
    let router = env.router.clone();
    let client = tokio::task::spawn_blocking(move || AdsClient::connect_auto(&router, addr))
        .await
        .expect("connect join")
        .expect("ads connect");

    // 2. Upload symbols and locate our test symbol.
    let upload_client = client.clone();
    let tree = tokio::task::spawn_blocking(move || upload_client.upload_symbols())
        .await
        .expect("upload join")
        .expect("upload");
    let node = tree
        .get(&env.symbol)
        .unwrap_or_else(|| panic!("symbol '{}' not found in {} nodes", env.symbol, tree.len()));
    let meta = SymbolMeta {
        size: node.size,
        type_name: node.type_name.clone(),
        index_group: node.igroup,
        index_offset: node.ioffset,
    };

    // 3. Poll — expect at least one sample within 5s.
    {
        let (tx, mut rx) = mpsc::channel::<MetricEntry>(16);
        let d = def(
            &env.symbol,
            "e2e.poll",
            &env.netid,
            env.port,
            CustomMetricSource::Poll,
        );
        let poller =
            Poller::new(d, meta.clone(), Arc::new(client.clone()), tx).expect("poller new");
        let handle = poller.spawn();
        let entry = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("poll timed out")
            .expect("channel closed");
        assert_eq!(entry.name, "e2e.poll");
        assert_eq!(entry.source, "client-poll");
        handle.abort();
    }

    // 4. Notification — expect at least one sample within 5s. Transmission is
    // ServerOnChange with cycle_time 50ms, so the PLC will emit immediately if
    // the value is non-stable, and on the first cycle regardless for
    // `ServerOnChange` with a `max_delay_ms` kicker.
    {
        let (tx, mut rx) = mpsc::channel::<MetricEntry>(16);
        let backend = Arc::new(AdsNotificationBackend::new(client.clone()));
        let notifier = Notifier::new(backend, tx);
        let dispatcher = notifier.clone().spawn_dispatcher();
        let d = def(
            &env.symbol,
            "e2e.notify",
            &env.netid,
            env.port,
            CustomMetricSource::Notification,
        );
        notifier.subscribe(d, meta).expect("subscribe");

        let entry = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("notify timed out")
            .expect("channel closed");
        assert_eq!(entry.name, "e2e.notify");
        assert_eq!(entry.source, "client-notify");

        dispatcher.abort();
    }
}

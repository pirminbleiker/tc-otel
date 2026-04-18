//! Active polling: periodic `ADS Read` per configured custom metric.
//!
//! Each [`Poller`] owns one [`SymbolReader`] and one [`CustomMetricDef`] and
//! emits [`MetricEntry`]s into a caller-supplied channel at `interval_ms` ticks.
//! On read error the loop applies exponential backoff (capped) and continues
//! without panicking; on a full metric channel it drops + warns rather than
//! blocking the tokio runtime.

use crate::client::{PlcValue, SymbolMeta, SymbolReader};
use crate::error::{ClientError, Result};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_core::config::{CustomMetricDef, MetricKindConfig};
use tc_otel_core::models::{MetricEntry, MetricKind};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Initial backoff after a failed ADS read.
const BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Cap on the exponential backoff — above this we just keep retrying at this rate.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One active polling loop.
pub struct Poller<R: SymbolReader> {
    pub def: CustomMetricDef,
    pub meta: SymbolMeta,
    pub reader: Arc<R>,
    pub tx: mpsc::Sender<MetricEntry>,
}

impl<R: SymbolReader> Poller<R> {
    pub fn new(
        def: CustomMetricDef,
        meta: SymbolMeta,
        reader: Arc<R>,
        tx: mpsc::Sender<MetricEntry>,
    ) -> Result<Self> {
        if !matches!(def.kind, MetricKindConfig::Gauge | MetricKindConfig::Sum) {
            return Err(ClientError::Decode(
                "poll source only supports gauge/sum kinds (histogram is push-only)".into(),
            ));
        }
        Ok(Self {
            def,
            meta,
            reader,
            tx,
        })
    }

    /// Spawn the loop as a tokio task. Returns a handle that the caller can
    /// `abort()` to stop the poller (e.g. on config removal).
    pub fn spawn(self) -> JoinHandle<()> {
        let interval_ms = self
            .def
            .poll
            .as_ref()
            .map(|c| c.interval_ms.max(10))
            .unwrap_or(1000);
        tokio::spawn(run_loop(self, Duration::from_millis(interval_ms)))
    }
}

async fn run_loop<R: SymbolReader>(poller: Poller<R>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut backoff = BACKOFF_INITIAL;
    let metric_name = poller.def.metric_name.clone();

    loop {
        ticker.tick().await;

        // ads crate is blocking — drive it off the tokio worker pool.
        let reader = poller.reader.clone();
        let meta = poller.meta.clone();
        let read_result = tokio::task::spawn_blocking(move || reader.read_value(&meta)).await;

        match read_result {
            Ok(Ok(value)) => {
                backoff = BACKOFF_INITIAL;
                let entry = build_metric_entry(&poller.def, value);
                match poller.tx.try_send(entry) {
                    Ok(_) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(metric = %metric_name, "poll metric channel full — dropping sample");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        debug!(metric = %metric_name, "metric channel closed — poll loop stopping");
                        return;
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(metric = %metric_name, error = %e, backoff_ms = ?backoff.as_millis(),
                    "poll read failed — backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(join_err) => {
                // Blocking task panicked or was cancelled. Surface + keep looping.
                warn!(metric = %metric_name, error = %join_err,
                    "poll read task panicked — backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Build a [`MetricEntry`] from a [`CustomMetricDef`] + sampled [`PlcValue`].
pub fn build_metric_entry(def: &CustomMetricDef, value: PlcValue) -> MetricEntry {
    let kind = def.kind.to_metric_kind();
    let mut entry = match kind {
        MetricKind::Gauge => MetricEntry::gauge(def.metric_name.clone(), value.as_f64()),
        MetricKind::Sum => {
            MetricEntry::sum(def.metric_name.clone(), value.as_f64(), def.is_monotonic)
        }
        MetricKind::Histogram => {
            // Poll can't produce histograms — caller ensured via Poller::new.
            MetricEntry::gauge(def.metric_name.clone(), value.as_f64())
        }
    };
    entry.description = def.description.clone();
    entry.unit = def.unit.clone();
    entry.source = "client-poll".to_string();
    if let Some(id) = def.ams_net_id.as_deref() {
        entry.ams_net_id = id.to_string();
    }
    if let Some(port) = def.ams_port {
        entry.ams_source_port = port;
    }
    entry
        .attributes
        .insert("plc.symbol".into(), def.symbol.clone().into());
    entry.attributes.insert(
        "plc.datatype".into(),
        match value {
            PlcValue::Bool(_) => "bool".into(),
            PlcValue::I64(_) => "i64".into(),
            PlcValue::U64(_) => "u64".into(),
            PlcValue::F64(_) => "f64".into(),
        },
    );
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_otel_core::config::PollConfig;

    fn def_lreal(symbol: &str, name: &str) -> CustomMetricDef {
        CustomMetricDef {
            symbol: symbol.to_string(),
            metric_name: name.to_string(),
            description: "desc".into(),
            unit: "1".into(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: tc_otel_core::config::CustomMetricSource::Poll,
            ams_net_id: Some("10.0.0.1.1.1".into()),
            ams_port: Some(851),
            poll: Some(PollConfig { interval_ms: 100 }),
            notification: None,
        }
    }

    #[test]
    fn build_gauge_entry_populates_attributes() {
        let def = def_lreal("MAIN.fTemp", "plc.temp");
        let entry = build_metric_entry(&def, PlcValue::F64(42.0));
        assert_eq!(entry.name, "plc.temp");
        assert_eq!(entry.value, 42.0);
        assert_eq!(entry.kind, MetricKind::Gauge);
        assert_eq!(entry.unit, "1");
        assert_eq!(entry.source, "client-poll");
        assert_eq!(entry.ams_net_id, "10.0.0.1.1.1");
        assert_eq!(entry.ams_source_port, 851);
        assert_eq!(
            entry.attributes.get("plc.symbol").and_then(|v| v.as_str()),
            Some("MAIN.fTemp")
        );
    }

    #[test]
    fn poller_new_rejects_histogram_kind() {
        let mut def = def_lreal("X", "x");
        def.kind = MetricKindConfig::Histogram;
        let meta = SymbolMeta {
            size: 8,
            type_name: "LREAL".into(),
            index_group: 0x4040,
            index_offset: 0,
        };
        struct NopReader;
        impl SymbolReader for NopReader {
            fn read_raw(&self, _: &SymbolMeta) -> Result<Vec<u8>> {
                Ok(vec![0; 8])
            }
        }
        let (tx, _rx) = mpsc::channel(4);
        match Poller::new(def, meta, Arc::new(NopReader), tx) {
            Ok(_) => panic!("expected histogram kind to be rejected"),
            Err(e) => assert!(e.to_string().contains("histogram")),
        }
    }
}

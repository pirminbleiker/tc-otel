//! Active notifications: `AddDeviceNotification` subscriptions per configured metric.
//!
//! # Architecture
//!
//! - [`NotificationBackend`] abstracts the ADS-side subscribe/unsubscribe/stream
//!   operations so tests can swap in an in-process fake.
//! - [`Notifier`] owns the backend + a handle→subscription map. It exposes
//!   `subscribe` / `unsubscribe` / `reconcile` (for config diffs) and a
//!   `dispatch_sample` entry point used by the reader side.
//! - [`spawn_dispatcher`](Notifier::spawn_dispatcher) launches a
//!   [`tokio::task::spawn_blocking`] loop that drains the backend channel and
//!   routes samples to `dispatch_sample`.
//!
//! Reconciliation is idempotent: re-applying the same desired set is a no-op.
//! A subscription whose attributes changed is observed as remove+add.

use crate::client::{decode_scalar, AdsClient, SymbolMeta};
use crate::error::{ClientError, Result};
use crate::poll::build_metric_entry;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_core::config::{CustomMetricDef, NotificationTransmissionMode};
use tc_otel_core::models::MetricEntry;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Opaque identifier of an active subscription — matches `ads::notif::Handle` on
/// the wire (u32).
pub type NotifHandle = u32;

/// Attributes for a single subscription.
#[derive(Debug, Clone, PartialEq)]
pub struct NotifAttrs {
    pub length: u32,
    pub trans_mode: NotificationTransmissionMode,
    pub max_delay: Duration,
    pub cycle_time: Duration,
}

impl NotifAttrs {
    /// Build [`NotifAttrs`] from a [`CustomMetricDef`] + symbol size.
    pub fn from_def(def: &CustomMetricDef, size: u32) -> Result<Self> {
        let cfg = def.notification.as_ref().ok_or_else(|| {
            ClientError::Decode(format!(
                "metric '{}' has source=notification but no notification config",
                def.metric_name
            ))
        })?;
        Ok(Self {
            length: size,
            trans_mode: cfg.transmission_mode,
            // min_period_ms corresponds to the "cycle time" — how often the PLC
            // re-evaluates the change condition. max_period_ms + max_delay_ms
            // cap the delay between change and transmission.
            cycle_time: Duration::from_millis(cfg.min_period_ms.max(10) as u64),
            max_delay: Duration::from_millis(cfg.max_delay_ms as u64),
        })
    }

    /// Convert into the `ads::notif::Attributes` used by the upstream crate.
    pub fn to_ads(&self) -> ads::notif::Attributes {
        let trans_mode = match self.trans_mode {
            NotificationTransmissionMode::OnChange => ads::notif::TransmissionMode::ServerOnChange,
            NotificationTransmissionMode::Cyclic => ads::notif::TransmissionMode::ServerCycle,
        };
        ads::notif::Attributes::new(
            self.length as usize,
            trans_mode,
            self.max_delay,
            self.cycle_time,
        )
    }
}

/// Abstraction over the ADS subscription side. Real impl is
/// [`AdsNotificationBackend`]; tests substitute a fake.
pub trait NotificationBackend: Send + Sync + 'static {
    fn subscribe(&self, attrs: &NotifAttrs, meta: &SymbolMeta) -> Result<NotifHandle>;
    fn unsubscribe(&self, handle: NotifHandle) -> Result<()>;
    /// Drain one notification — returns `Ok(None)` when the backend is closed
    /// (triggers the dispatcher to exit).
    fn recv_timeout(&self, timeout: Duration) -> Result<Option<DispatchBatch>>;
}

/// One notification payload broken into (handle, bytes) pairs for dispatch.
#[derive(Debug, Clone)]
pub struct DispatchBatch {
    pub samples: Vec<(NotifHandle, Vec<u8>)>,
}

#[derive(Clone)]
struct Subscription {
    def: CustomMetricDef,
    meta: SymbolMeta,
    attrs: NotifAttrs,
}

/// Owns the subscription registry and routes dispatched samples to the
/// caller-supplied `mpsc::Sender<MetricEntry>`.
pub struct Notifier<B: NotificationBackend> {
    backend: Arc<B>,
    tx: mpsc::Sender<MetricEntry>,
    /// handle → subscription.
    subs: Mutex<HashMap<NotifHandle, Subscription>>,
    /// symbol name → handle. Used to find existing subscriptions during reconcile.
    by_symbol: Mutex<HashMap<String, NotifHandle>>,
}

impl<B: NotificationBackend> Notifier<B> {
    pub fn new(backend: Arc<B>, tx: mpsc::Sender<MetricEntry>) -> Arc<Self> {
        Arc::new(Self {
            backend,
            tx,
            subs: Mutex::new(HashMap::new()),
            by_symbol: Mutex::new(HashMap::new()),
        })
    }

    pub fn subscribe(&self, def: CustomMetricDef, meta: SymbolMeta) -> Result<NotifHandle> {
        let attrs = NotifAttrs::from_def(&def, meta.size)?;
        let handle = self.backend.subscribe(&attrs, &meta)?;
        self.subs.lock().insert(
            handle,
            Subscription {
                def: def.clone(),
                meta,
                attrs,
            },
        );
        self.by_symbol.lock().insert(def.symbol.clone(), handle);
        debug!(symbol = %def.symbol, handle, "subscription registered");
        Ok(handle)
    }

    pub fn unsubscribe(&self, handle: NotifHandle) -> Result<()> {
        let sub = self.subs.lock().remove(&handle);
        if let Some(sub) = sub.as_ref() {
            self.by_symbol.lock().remove(&sub.def.symbol);
        }
        self.backend.unsubscribe(handle)?;
        Ok(())
    }

    /// Reconcile: bring current subscriptions in line with `desired`.
    ///
    /// Comparison is by `(symbol, attrs)`. A symbol whose attributes changed is
    /// removed and re-added. Returns a report the service layer can log.
    pub fn reconcile(
        &self,
        desired: Vec<(CustomMetricDef, SymbolMeta)>,
    ) -> Result<ReconcileReport> {
        use std::collections::HashSet;

        // Build desired-by-symbol map with attrs computed up front.
        let mut desired_map: HashMap<String, (CustomMetricDef, SymbolMeta, NotifAttrs)> =
            HashMap::with_capacity(desired.len());
        for (def, meta) in desired {
            let attrs = NotifAttrs::from_def(&def, meta.size)?;
            desired_map.insert(def.symbol.clone(), (def, meta, attrs));
        }

        // Snapshot current state as (symbol → attrs) for direct comparison.
        let current: HashMap<String, (NotifHandle, NotifAttrs)> = {
            let subs = self.subs.lock();
            self.by_symbol
                .lock()
                .iter()
                .filter_map(|(sym, h)| subs.get(h).map(|s| (sym.clone(), (*h, s.attrs.clone()))))
                .collect()
        };

        let mut report = ReconcileReport::default();
        // Symbols whose removal in pass 1 is actually a "change" (attrs differ)
        // — they will be re-added in pass 2 and must be tallied as `changed`.
        let mut changed_syms: HashSet<String> = HashSet::new();

        // Pass 1: categorize each existing subscription.
        for (sym, (handle, old_attrs)) in current.iter() {
            match desired_map.get(sym) {
                None => {
                    // Not in desired → remove.
                    self.unsubscribe(*handle)?;
                    report.removed.push(sym.clone());
                }
                Some((_, _, new_attrs)) if new_attrs != old_attrs => {
                    // Attrs changed → remove now, re-add in pass 2.
                    self.unsubscribe(*handle)?;
                    changed_syms.insert(sym.clone());
                }
                Some(_) => {
                    // Unchanged — remove from desired so pass 2 skips it.
                    desired_map.remove(sym);
                }
            }
        }

        // Pass 2: add everything left in desired (either brand-new or re-adding a changed one).
        for (sym, (def, meta, _)) in desired_map {
            self.subscribe(def, meta)?;
            if changed_syms.contains(&sym) {
                report.changed.push(sym);
            } else {
                report.added.push(sym);
            }
        }

        Ok(report)
    }

    /// Dispatch a raw (handle, bytes) sample to the metric channel.
    ///
    /// Public so that the dispatcher task — and tests — can call it directly.
    pub fn dispatch_sample(&self, handle: NotifHandle, data: &[u8]) {
        let sub = match self.subs.lock().get(&handle).cloned() {
            Some(s) => s,
            None => {
                debug!(handle, "dispatch: unknown handle (likely stale) — dropping");
                return;
            }
        };
        let Some(value) = decode_scalar(&sub.meta.type_name, data) else {
            warn!(symbol = %sub.def.symbol, type_name = %sub.meta.type_name,
                bytes = data.len(), "notify: unsupported scalar — dropping sample");
            return;
        };
        let entry = build_metric_entry(&sub.def, value);
        match self
            .tx
            .try_send(metric_entry_with_source(entry, "client-notify"))
        {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(symbol = %sub.def.symbol, "notify metric channel full — dropping");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("metric channel closed — future dispatches will drop");
            }
        }
    }

    /// Spawn the dispatcher task. Runs until the backend returns `Ok(None)`.
    pub fn spawn_dispatcher(self: Arc<Self>) -> JoinHandle<()> {
        tokio::task::spawn_blocking(move || loop {
            match self.backend.recv_timeout(Duration::from_secs(1)) {
                Ok(Some(batch)) => {
                    for (h, data) in batch.samples {
                        self.dispatch_sample(h, &data);
                    }
                }
                Ok(None) => {
                    debug!("backend closed — dispatcher exiting");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "notify dispatcher recv error — continuing");
                }
            }
        })
    }
}

fn metric_entry_with_source(mut e: MetricEntry, src: &str) -> MetricEntry {
    e.source = src.to_string();
    e
}

/// Summary of a [`Notifier::reconcile`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

// -----------------------------------------------------------------------------
// Real ADS-backed implementation
// -----------------------------------------------------------------------------

/// Real backend wrapping an [`AdsClient`] plus its notification receiver.
pub struct AdsNotificationBackend {
    client: AdsClient,
    rx: crossbeam_channel::Receiver<ads::notif::Notification>,
}

impl AdsNotificationBackend {
    pub fn new(client: AdsClient) -> Self {
        let rx = client.with_client(|c, _| c.get_notification_channel());
        Self { client, rx }
    }
}

impl NotificationBackend for AdsNotificationBackend {
    fn subscribe(&self, attrs: &NotifAttrs, meta: &SymbolMeta) -> Result<NotifHandle> {
        let ads_attrs = attrs.to_ads();
        self.client.with_client(|c, target| {
            let device = c.device(target);
            device
                .add_notification(meta.index_group, meta.index_offset, &ads_attrs)
                .map_err(|e| ClientError::Ads(e.to_string()))
        })
    }

    fn unsubscribe(&self, handle: NotifHandle) -> Result<()> {
        self.client.with_client(|c, target| {
            let device = c.device(target);
            device
                .delete_notification(handle)
                .map_err(|e| ClientError::Ads(e.to_string()))
        })
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Option<DispatchBatch>> {
        match self.rx.recv_timeout(timeout) {
            Ok(notif) => {
                let samples: Vec<_> = notif
                    .samples()
                    .map(|s| (s.handle, s.data.to_vec()))
                    .collect();
                Ok(Some(DispatchBatch { samples }))
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                Ok(Some(DispatchBatch { samples: vec![] }))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

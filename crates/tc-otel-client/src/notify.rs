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

use crate::client::{decode_scalar, SymbolMeta};
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

    /// Serialize into the 16-byte `AddDeviceNotification` tail required by
    /// ADS: transmission_mode (u32) + max_delay_100ns (u32) + cycle_time_100ns
    /// (u32) + 16 reserved bytes. Returns 28 bytes (without the 12-byte
    /// ig/io/length prefix — callers splice that in).
    ///
    /// Timing values are in ADS's native 100 ns ticks (Windows FILETIME unit).
    pub fn serialize_tail(&self) -> Vec<u8> {
        let trans_mode: u32 = match self.trans_mode {
            // ADS wire values — see Beckhoff TcAdsDef.h
            NotificationTransmissionMode::OnChange => 4, // SERVERONCHA
            NotificationTransmissionMode::Cyclic => 3,   // SERVERCYCLE
        };
        let to_100ns = |d: Duration| (d.as_nanos() / 100) as u32;
        let mut out = Vec::with_capacity(28);
        out.extend_from_slice(&trans_mode.to_le_bytes());
        out.extend_from_slice(&to_100ns(self.max_delay).to_le_bytes());
        out.extend_from_slice(&to_100ns(self.cycle_time).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        out
    }
}

/// Abstraction over the ADS subscription side. Real impl is
/// [`DispatcherNotificationBackend`]; tests substitute a fake.
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
// Dispatcher-backed implementation
// -----------------------------------------------------------------------------

/// Real backend that uses the shared `tc_otel_ads::dispatcher::AmsDispatcher`
/// for subscribe/unsubscribe and receives notification stamps via a paired
/// [`NotificationSink`] attached as the dispatcher's `InboundSink`.
pub struct DispatcherNotificationBackend {
    /// Late-bound dispatcher reference. The backend must be paired with a
    /// sink *before* the dispatcher is constructed (so the sink becomes the
    /// dispatcher's `InboundSink`), but it needs the dispatcher to send
    /// subscribe/unsubscribe requests. We resolve that chicken-and-egg by
    /// filling this slot after dispatcher construction.
    dispatcher: std::sync::OnceLock<Arc<tc_otel_ads::dispatcher::AmsDispatcher>>,
    target_net_id: tc_otel_ads::ams::AmsNetId,
    target_port: u16,
    request_timeout: Duration,
    // `std::sync::mpsc::Receiver` is !Sync; wrap in a Mutex so the backend
    // can satisfy the `NotificationBackend: Sync` bound. Only one task reads
    // at a time — the dispatcher loop in `Notifier::spawn_dispatcher`.
    stamps: Mutex<std::sync::mpsc::Receiver<DispatchBatch>>,
}

impl DispatcherNotificationBackend {
    /// Build a paired (sink, backend). The backend starts without a
    /// dispatcher — call [`DispatcherNotificationBackend::set_dispatcher`]
    /// once the real dispatcher has been constructed with `sink` as its
    /// `InboundSink`.
    pub fn new_pair(
        target_net_id: tc_otel_ads::ams::AmsNetId,
        target_port: u16,
        request_timeout: Duration,
    ) -> (NotificationSink, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sink = NotificationSink { tx };
        let backend = Self {
            dispatcher: std::sync::OnceLock::new(),
            target_net_id,
            target_port,
            request_timeout,
            stamps: Mutex::new(rx),
        };
        (sink, backend)
    }

    /// Install the dispatcher. Must be called exactly once before the first
    /// `subscribe` / `unsubscribe` / `recv_timeout` call; subsequent calls
    /// are silently ignored.
    pub fn set_dispatcher(&self, dispatcher: Arc<tc_otel_ads::dispatcher::AmsDispatcher>) {
        let _ = self.dispatcher.set(dispatcher);
    }

    fn dispatcher_ref(&self) -> Result<Arc<tc_otel_ads::dispatcher::AmsDispatcher>> {
        self.dispatcher
            .get()
            .cloned()
            .ok_or_else(|| ClientError::Ads("dispatcher not yet bound to backend".into()))
    }
}

impl NotificationBackend for DispatcherNotificationBackend {
    fn subscribe(&self, attrs: &NotifAttrs, meta: &SymbolMeta) -> Result<NotifHandle> {
        let dispatcher = self.dispatcher_ref()?;
        // AddDeviceNotification request body: 12 bytes ig/io/length + 28 bytes attrs tail.
        let mut req = Vec::with_capacity(40);
        req.extend_from_slice(&meta.index_group.to_le_bytes());
        req.extend_from_slice(&meta.index_offset.to_le_bytes());
        req.extend_from_slice(&meta.size.to_le_bytes());
        req.extend_from_slice(&attrs.serialize_tail());

        let target_net_id = self.target_net_id;
        let target_port = self.target_port;
        let timeout = self.request_timeout;
        let raw = block_on_request(async move {
            dispatcher
                .send_request(
                    target_net_id,
                    target_port,
                    tc_otel_ads::ams::ADS_CMD_ADD_NOTIFICATION,
                    &req,
                    timeout,
                )
                .await
        })?;

        // Response: 4-byte result + 4-byte handle.
        if raw.len() < 8 {
            return Err(ClientError::Decode(format!(
                "ADS AddNotification response too short: {} bytes",
                raw.len()
            )));
        }
        let result = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if result != 0 {
            return Err(ClientError::Ads(format!(
                "ADS AddNotification returned error 0x{result:x}"
            )));
        }
        let handle = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        Ok(handle)
    }

    fn unsubscribe(&self, handle: NotifHandle) -> Result<()> {
        let dispatcher = self.dispatcher_ref()?;
        let req = handle.to_le_bytes().to_vec();
        let target_net_id = self.target_net_id;
        let target_port = self.target_port;
        let timeout = self.request_timeout;
        let raw = block_on_request(async move {
            dispatcher
                .send_request(
                    target_net_id,
                    target_port,
                    tc_otel_ads::ams::ADS_CMD_DEL_NOTIFICATION,
                    &req,
                    timeout,
                )
                .await
        })?;
        if raw.len() < 4 {
            return Err(ClientError::Decode(format!(
                "ADS DelNotification response too short: {} bytes",
                raw.len()
            )));
        }
        let result = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if result != 0 {
            return Err(ClientError::Ads(format!(
                "ADS DelNotification returned error 0x{result:x}"
            )));
        }
        Ok(())
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Option<DispatchBatch>> {
        let rx = self.stamps.lock();
        match rx.recv_timeout(timeout) {
            Ok(batch) => Ok(Some(batch)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Ok(Some(DispatchBatch { samples: vec![] }))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

fn block_on_request<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<
        Output = std::result::Result<T, tc_otel_ads::dispatcher::DispatcherError>,
    >,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| ClientError::Ads("no tokio runtime available".into()))?;
    tokio::task::block_in_place(|| handle.block_on(fut))
        .map_err(|e| ClientError::Ads(e.to_string()))
}

/// `InboundSink` for the dispatcher that extracts notification stamps and
/// forwards (handle, data) pairs to a paired [`DispatcherNotificationBackend`]
/// via a std mpsc channel.
pub struct NotificationSink {
    tx: std::sync::mpsc::Sender<DispatchBatch>,
}

impl tc_otel_ads::dispatcher::InboundSink for NotificationSink {
    fn deliver(&self, header: tc_otel_ads::ams::AmsHeader, payload: Vec<u8>) {
        if header.command_id != tc_otel_ads::ams::ADS_CMD_NOTIFICATION {
            return;
        }
        let samples = parse_notification_payload(&payload);
        if !samples.is_empty() {
            let _ = self.tx.send(DispatchBatch { samples });
        }
    }
}

/// Parse the body of an `AdsNotification` frame:
///
/// ```text
///   u32 length
///   u32 nstamps
///   for each stamp:
///     u64 timestamp (FILETIME, ignored here)
///     u32 nsamples
///     for each sample:
///       u32 handle
///       u32 size
///       <size bytes>
/// ```
fn parse_notification_payload(data: &[u8]) -> Vec<(NotifHandle, Vec<u8>)> {
    if data.len() < 8 {
        return Vec::new();
    }
    let _total = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let nstamps = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let mut out = Vec::new();
    let mut offset = 8usize;
    for _ in 0..nstamps {
        if data.len() < offset + 12 {
            break;
        }
        // Skip 8-byte timestamp.
        offset += 8;
        let nsamples = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        for _ in 0..nsamples {
            if data.len() < offset + 8 {
                break;
            }
            let handle = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            let sample_size = u32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]) as usize;
            offset += 8;
            if data.len() < offset + sample_size {
                break;
            }
            let bytes = data[offset..offset + sample_size].to_vec();
            offset += sample_size;
            out.push((handle, bytes));
        }
    }
    out
}

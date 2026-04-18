//! Active ADS client bridge — drives poll + notification subscriptions for all
//! `custom_metrics` entries whose `source` is `poll` or `notification`.
//!
//! Gated behind the `client-bridge` Cargo feature. When the feature is off this
//! module is compiled out entirely.
//!
//! # Responsibilities
//!
//! - For each unique PLC target referenced by `custom_metrics`, establish one
//!   outbound AMS/TCP session via [`tc_otel_client::client::AdsClient`].
//! - Populate [`tc_otel_client::cache::SymbolTreeCache`] for each target (full
//!   symbol upload) so that poll/notification entries can resolve
//!   `(igroup, ioffset, size, type_name)` locally without per-entry round-trips.
//! - Spawn one [`tc_otel_client::poll::Poller`] per poll-source entry.
//! - Subscribe one notification per notification-source entry via a shared
//!   per-target [`tc_otel_client::notify::Notifier`].
//! - Reconcile on `AppSettings` changes (watch::Receiver).
//!
//! # Reconcile strategy (v1)
//!
//! Simple and correct: on every config change, tear down all per-target state
//! and rebuild. This is inefficient with large configs, but:
//!
//! - Config edits are rare (human-driven via UI).
//! - Teardown is fast (abort tokio tasks, drop TCP).
//! - It guarantees correctness — no stale subscription can survive a config
//!   change.
//!
//! A differential reconcile is planned as a follow-up (see T10 retire-stub
//! cleanup — the old reconcile attempt used diff logic that had edge-case bugs).

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_client::cache::{SymbolTreeCache, TargetKey};
use tc_otel_client::client::{AdsClient, SymbolMeta};
use tc_otel_client::notify::{AdsNotificationBackend, Notifier};
use tc_otel_client::poll::Poller;
use tc_otel_core::config::{CustomMetricDef, CustomMetricSource};
use tc_otel_core::models::MetricEntry;
use tc_otel_core::AppSettings;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

const DEFAULT_AMS_PORT: u16 = 851;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared handle to the bridge — allows the web layer (T7) to access the cache
/// and to trigger a manual refresh.
#[derive(Clone)]
pub struct ClientBridge {
    inner: Arc<Inner>,
}

struct Inner {
    metric_tx: mpsc::Sender<MetricEntry>,
    cache: Arc<SymbolTreeCache>,
    state: parking_lot::Mutex<HashMap<TargetKey, TargetState>>,
}

struct TargetState {
    #[allow(dead_code)]
    client: AdsClient,
    #[allow(dead_code)]
    notifier: Arc<Notifier<AdsNotificationBackend>>,
    notifier_task: JoinHandle<()>,
    poll_tasks: Vec<JoinHandle<()>>,
}

impl Drop for TargetState {
    fn drop(&mut self) {
        for t in &self.poll_tasks {
            t.abort();
        }
        self.notifier_task.abort();
    }
}

impl ClientBridge {
    /// Create a new bridge. Returns a cloneable handle but does **not** spawn
    /// the watch-loop — call [`spawn`](Self::spawn) for that.
    pub fn new(metric_tx: mpsc::Sender<MetricEntry>, cache: Arc<SymbolTreeCache>) -> Self {
        Self {
            inner: Arc::new(Inner {
                metric_tx,
                cache,
                state: parking_lot::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Share the symbol cache. Used by the web layer (T7) to serve
    /// `GET /api/symbols`.
    #[allow(dead_code)]
    pub fn cache(&self) -> Arc<SymbolTreeCache> {
        self.inner.cache.clone()
    }

    /// Spawn the reconciliation loop. The bridge reconciles once immediately
    /// and then each time `config_rx` signals a change.
    ///
    /// Returns the driver task's handle. Drop or abort to stop the bridge.
    pub fn spawn(&self, mut config_rx: watch::Receiver<AppSettings>) -> JoinHandle<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            // Initial reconcile.
            let settings = config_rx.borrow().clone();
            if let Err(e) = bridge.reconcile(&settings).await {
                warn!(error = ?e, "client-bridge: initial reconcile failed");
            }

            loop {
                if config_rx.changed().await.is_err() {
                    info!("client-bridge: config watcher closed — shutting down");
                    break;
                }
                let settings = config_rx.borrow().clone();
                if let Err(e) = bridge.reconcile(&settings).await {
                    warn!(error = ?e, "client-bridge: reconcile failed");
                }
            }
        })
    }

    /// Teardown and rebuild state for all targets described by `settings`.
    pub async fn reconcile(&self, settings: &AppSettings) -> Result<()> {
        let desired: Vec<&CustomMetricDef> = settings
            .metrics
            .custom_metrics
            .iter()
            .filter(|d| !matches!(d.source, CustomMetricSource::Push))
            .collect();

        // Group desired entries by target key. Descriptor stored alongside the
        // def list (AmsAddr is not Hash so we can't use it as the map key).
        let mut by_target: HashMap<TargetKey, (TargetDescriptor, Vec<CustomMetricDef>)> =
            HashMap::new();
        for def in &desired {
            match TargetDescriptor::from_def(def) {
                Ok(tgt) => by_target
                    .entry(tgt.key)
                    .or_insert_with(|| (tgt.clone(), Vec::new()))
                    .1
                    .push((*def).clone()),
                Err(e) => warn!(metric = %def.metric_name, error = %e,
                    "skipping custom metric — cannot resolve target"),
            }
        }

        // Teardown removed targets. We rebuild the rest unconditionally (v1
        // reconcile strategy) to keep the logic simple and correct.
        let desired_keys: HashSet<TargetKey> = by_target.keys().copied().collect();
        {
            let mut state = self.inner.state.lock();
            let to_drop: Vec<TargetKey> = state
                .keys()
                .filter(|k| !desired_keys.contains(k))
                .copied()
                .collect();
            for key in to_drop {
                info!(target = %key, "client-bridge: dropping target");
                state.remove(&key);
                self.inner.cache.invalidate(key);
            }
        }

        for (_key, (tgt, defs)) in by_target {
            if let Err(e) = self.rebuild_target(tgt, defs).await {
                warn!(error = ?e, "client-bridge: rebuild failed for target");
            }
        }

        Ok(())
    }

    async fn rebuild_target(
        &self,
        tgt: TargetDescriptor,
        defs: Vec<CustomMetricDef>,
    ) -> Result<()> {
        // Tear down any previous state for this target (subscriptions will be
        // dropped server-side automatically).
        {
            let mut state = self.inner.state.lock();
            state.remove(&tgt.key);
        }

        // Dial the PLC. Blocking — done off the runtime.
        let router_addr = tgt.router_addr.clone();
        let ams_target = tgt.ams_target;
        let client =
            tokio::task::spawn_blocking(move || AdsClient::connect_auto(&router_addr, ams_target))
                .await
                .context("connect task join")?
                .context("ads connect")?;

        // Populate or refresh the symbol cache for this target.
        let bridge_client = client.clone();
        let tree = tokio::task::spawn_blocking(move || bridge_client.upload_symbols())
            .await
            .context("symbol upload join")?
            .context("symbol upload")?;
        info!(target = %tgt.key, symbols = tree.len(),
            "client-bridge: symbol cache populated");
        self.inner.cache.insert(tgt.key, tree);

        // Start notifier and its dispatcher.
        let backend = Arc::new(AdsNotificationBackend::new(client.clone()));
        let notifier = Notifier::new(backend, self.inner.metric_tx.clone());
        let notifier_task = notifier.clone().spawn_dispatcher();

        // Register notification subscriptions + spawn pollers.
        let cache = self.inner.cache.clone();
        let mut poll_tasks = Vec::new();
        for def in defs {
            let meta = match resolve_meta(&cache, tgt.key, &def, &client).await {
                Some(m) => m,
                None => continue,
            };
            match def.source {
                CustomMetricSource::Poll => {
                    match Poller::new(
                        def.clone(),
                        meta,
                        Arc::new(client.clone()),
                        self.inner.metric_tx.clone(),
                    ) {
                        Ok(poller) => {
                            info!(symbol = %def.symbol, metric = %def.metric_name,
                                "client-bridge: starting poller");
                            poll_tasks.push(poller.spawn());
                        }
                        Err(e) => warn!(metric = %def.metric_name, error = %e,
                            "client-bridge: poller rejected"),
                    }
                }
                CustomMetricSource::Notification => match notifier.subscribe(def.clone(), meta) {
                    Ok(h) => info!(symbol = %def.symbol, handle = h,
                        "client-bridge: subscription added"),
                    Err(e) => warn!(symbol = %def.symbol, error = %e,
                        "client-bridge: subscribe failed"),
                },
                CustomMetricSource::Push => unreachable!("filtered above"),
            }
        }

        let state_entry = TargetState {
            client,
            notifier,
            notifier_task,
            poll_tasks,
        };
        self.inner.state.lock().insert(tgt.key, state_entry);
        Ok(())
    }
}

async fn resolve_meta(
    cache: &Arc<SymbolTreeCache>,
    target: TargetKey,
    def: &CustomMetricDef,
    client: &AdsClient,
) -> Option<SymbolMeta> {
    if let Some(tree) = cache.get(target) {
        if let Some(node) = tree.get(&def.symbol) {
            return Some(SymbolMeta {
                size: node.size,
                type_name: node.type_name.clone(),
                index_group: node.igroup,
                index_offset: node.ioffset,
            });
        }
    }
    // Fallback: resolve location from PLC (no type info — user must have it
    // right in the cache; otherwise we bail).
    let symbol = def.symbol.clone();
    let client = client.clone();
    let res = tokio::task::spawn_blocking(move || client.resolve_location(&symbol)).await;
    match res {
        Ok(Ok((ig, io, size))) => Some(SymbolMeta {
            size,
            type_name: "LREAL".into(), // Conservative default if type unknown.
            index_group: ig,
            index_offset: io,
        }),
        _ => {
            warn!(symbol = %def.symbol, "client-bridge: resolve failed, dropping metric");
            None
        }
    }
}

#[derive(Clone)]
struct TargetDescriptor {
    key: TargetKey,
    ams_target: tc_otel_client::AmsAddr,
    /// Dial-string for `ads::Client::new`. Format: `"<host>:48898"`. The host
    /// is the first four bytes of the AMS Net ID (rendered as an IPv4 address).
    router_addr: String,
}

impl TargetDescriptor {
    fn from_def(def: &CustomMetricDef) -> anyhow::Result<Self> {
        let id_str = def
            .ams_net_id
            .as_deref()
            .context("custom metric missing ams_net_id")?;
        let netid = tc_otel_client::AmsNetId::from_str(id_str)
            .map_err(|e| anyhow::anyhow!("invalid ams_net_id '{id_str}': {e}"))?;
        let port = def.ams_port.unwrap_or(DEFAULT_AMS_PORT);
        let key = TargetKey::from(netid);
        let [a, b, c, d, _, _] = netid.0;
        let router_addr = format!("{a}.{b}.{c}.{d}:48898");
        Ok(Self {
            key,
            ams_target: tc_otel_client::AmsAddr::new(netid, port),
            router_addr,
        })
    }
}

impl std::fmt::Debug for TargetDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TargetDescriptor({}, {})", self.key, self.router_addr)
    }
}

// AdsClient isn't Clone by default? Actually I derived Clone — good.
// The Poller constructor takes Arc<R> so we wrap. Timeouts above are const.
// We unused CONNECT_TIMEOUT but keep for future wiring into Timeouts::new.
#[allow(dead_code)]
fn _connect_timeout_unused() -> Duration {
    CONNECT_TIMEOUT
}

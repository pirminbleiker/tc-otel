//! Active ADS client bridge — drives poll + notification subscriptions for
//! all `custom_metrics` entries whose `source` is `poll` or `notification`.
//!
//! Gated behind the `client-bridge` Cargo feature. When the feature is off
//! this module is compiled out entirely.
//!
//! # Architecture (post-dispatcher migration)
//!
//! The bridge now sits on top of the transport-agnostic
//! [`tc_otel_ads::dispatcher::AmsDispatcher`]: one dispatcher per target
//! PLC, each attached to the MQTT broker (for peer discovery via the
//! `/info` topic + request/response routing) and — if the config supplies a
//! `ams_router_host` — also registered as a direct-TCP peer for that
//! target. The dispatcher picks whichever transport is live, so the bridge
//! no longer cares about `StaticRoutes.xml` tweaks or TCP-only connection
//! management.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::net::ToSocketAddrs;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::ams::AmsNetId;
use tc_otel_ads::dispatcher::AmsDispatcher;
use tc_otel_client::browse;
use tc_otel_client::cache::{SymbolTreeCache, TargetKey};
use tc_otel_client::client::{DispatcherClient, SymbolMeta};
use tc_otel_client::notify::{DispatcherNotificationBackend, Notifier};
use tc_otel_client::poll::Poller;
use tc_otel_core::config::{CustomMetricDef, CustomMetricSource};
use tc_otel_core::models::MetricEntry;
use tc_otel_core::AppSettings;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

const DEFAULT_AMS_PORT: u16 = 851;
const ROUTE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared handle to the bridge — cloneable, routes/read-state observable from
/// the web layer.
#[derive(Clone)]
pub struct ClientBridge {
    inner: Arc<Inner>,
}

struct Inner {
    metric_tx: mpsc::Sender<MetricEntry>,
    cache: Arc<SymbolTreeCache>,
    state: parking_lot::Mutex<HashMap<TargetKey, TargetState>>,
    mqtt_broker_host: String,
    mqtt_broker_port: u16,
    mqtt_topic_prefix: String,
}

struct TargetState {
    #[allow(dead_code)]
    dispatcher: Arc<AmsDispatcher>,
    #[allow(dead_code)]
    notifier: Arc<Notifier<DispatcherNotificationBackend>>,
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
    /// Build a bridge with the given MQTT broker coordinates. These are the
    /// same broker tc-otel's observer path uses — the dispatcher attaches
    /// there as a second client with a distinct client-id.
    pub fn new(
        metric_tx: mpsc::Sender<MetricEntry>,
        cache: Arc<SymbolTreeCache>,
        mqtt_broker_host: String,
        mqtt_broker_port: u16,
        mqtt_topic_prefix: String,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                metric_tx,
                cache,
                state: parking_lot::Mutex::new(HashMap::new()),
                mqtt_broker_host,
                mqtt_broker_port,
                mqtt_topic_prefix,
            }),
        }
    }

    /// Share the symbol cache. Used by the web layer (T7) to serve
    /// `GET /api/client/symbols`.
    #[allow(dead_code)]
    pub fn cache(&self) -> Arc<SymbolTreeCache> {
        self.inner.cache.clone()
    }

    /// Spawn the reconciliation loop.
    pub fn spawn(&self, mut config_rx: watch::Receiver<AppSettings>) -> JoinHandle<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            let settings = config_rx.borrow().clone();
            if let Err(e) = bridge.reconcile(&settings).await {
                warn!("client-bridge: initial reconcile failed: {:#}", e);
            }

            loop {
                if config_rx.changed().await.is_err() {
                    info!("client-bridge: config watcher closed — shutting down");
                    break;
                }
                let settings = config_rx.borrow().clone();
                if let Err(e) = bridge.reconcile(&settings).await {
                    warn!("client-bridge: reconcile failed: {:#}", e);
                }
            }
        })
    }

    pub async fn reconcile(&self, settings: &AppSettings) -> Result<()> {
        let our_netid = AmsNetId::from_str(&settings.receiver.ams_net_id)
            .unwrap_or(AmsNetId([10, 10, 10, 10, 1, 1]));

        let desired: Vec<&CustomMetricDef> = settings
            .metrics
            .custom_metrics
            .iter()
            .filter(|d| !matches!(d.source, CustomMetricSource::Push))
            .collect();

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
                    "client-bridge: skipping custom metric — cannot resolve target"),
            }
        }

        // Drop targets no longer desired.
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
            if let Err(e) = self.rebuild_target(tgt, defs, our_netid).await {
                warn!("client-bridge: rebuild failed for target: {:#}", e);
            }
        }

        Ok(())
    }

    async fn rebuild_target(
        &self,
        tgt: TargetDescriptor,
        defs: Vec<CustomMetricDef>,
        our_netid: AmsNetId,
    ) -> Result<()> {
        // Tear down any previous state for this target.
        {
            let mut state = self.inner.state.lock();
            state.remove(&tgt.key);
        }

        // Build a fresh dispatcher for this target. Each target gets its
        // own dispatcher (and thus its own MQTT client-id + notification
        // sink) — isolates lifetime management and avoids cross-target
        // notification-handle collisions.
        //
        // Two-step construction because sink and dispatcher have a cyclic
        // dependency: the dispatcher needs the sink for incoming
        // notifications, the notification backend needs the dispatcher for
        // subscribe/unsubscribe requests. `new_pair` creates the backend
        // without a dispatcher; `set_dispatcher` wires it up once the
        // dispatcher (owning the paired sink) exists.
        let (sink, backend) =
            DispatcherNotificationBackend::new_pair(tgt.ams_net_id, tgt.ams_port, REQUEST_TIMEOUT);
        let mut dispatcher =
            AmsDispatcher::with_sink(our_netid, DEFAULT_SOURCE_PORT, Arc::new(sink));
        dispatcher
            .attach_mqtt(
                &self.inner.mqtt_broker_host,
                self.inner.mqtt_broker_port,
                &format!("tc-otel-dispatch-{}", tgt.ams_net_id),
                &self.inner.mqtt_topic_prefix,
            )
            .await
            .context("attach_mqtt")?;

        let dispatcher = Arc::new(dispatcher);
        let routes = dispatcher.routes();
        let target_net_id = tgt.ams_net_id;

        // MQTT discovery grace period: give the broker a moment to deliver
        // the retained `/info` for this target before we touch fallbacks.
        // If the target announces itself via MQTT, we prefer that — it's
        // bidirectional and doesn't require the PLC to have a TCP route
        // configured for us.
        let mqtt_grace = Duration::from_secs(2);
        let _ = tokio::time::timeout(mqtt_grace, async {
            loop {
                if routes.get(target_net_id).is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;

        // Still no route? Fall back to the static TCP peer (if configured).
        // `add_tcp_peer` uses `learn_if_absent`, so a concurrently-arriving
        // MQTT /info is still honored.
        if routes.get(target_net_id).is_none() {
            if let Some(host) = tgt.router_host.as_deref() {
                let addr_str = format!("{host}:48898");
                match addr_str.to_socket_addrs() {
                    Ok(mut iter) => {
                        if let Some(addr) = iter.next() {
                            dispatcher.add_tcp_peer(tgt.ams_net_id, addr).await;
                            info!(target = %tgt.key, %addr,
                                "client-bridge: MQTT discovery silent, fell back to TCP peer");
                        } else {
                            warn!(target = %tgt.key, %host,
                                "client-bridge: host resolved to no addresses");
                        }
                    }
                    Err(e) => warn!(target = %tgt.key, %host, error = %e,
                        "client-bridge: host resolution failed"),
                }
            }
        }

        // Final wait: up to `ROUTE_WAIT_TIMEOUT` for the (now possibly
        // TCP-fallback) route to stabilise before we issue the symbol
        // upload.
        let route_ready = tokio::time::timeout(ROUTE_WAIT_TIMEOUT, async {
            loop {
                if routes.get(target_net_id).is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if route_ready.is_err() {
            warn!(target = %tgt.key,
                "client-bridge: no route learned within {ROUTE_WAIT_TIMEOUT:?} — skipping");
            return Ok(());
        }
        info!(target = %tgt.key, transport = ?routes.get(target_net_id),
            "client-bridge: route resolved");

        // Upload symbol table.
        let tree = browse::upload_via_dispatcher(
            &dispatcher,
            tgt.ams_net_id,
            tgt.ams_port,
            REQUEST_TIMEOUT,
        )
        .await
        .context("symbol upload")?;
        info!(target = %tgt.key, symbols = tree.len(),
            "client-bridge: symbol cache populated");
        self.inner.cache.insert(tgt.key, tree);

        // Wire the backend to the (now fully-constructed) dispatcher.
        backend.set_dispatcher(dispatcher.clone());
        let notifier = Notifier::new(Arc::new(backend), self.inner.metric_tx.clone());
        let notifier_task = notifier.clone().spawn_dispatcher();

        // Register pollers + notifier subscriptions.
        let cache = self.inner.cache.clone();
        let mut poll_tasks = Vec::new();
        for def in defs {
            let meta = match resolve_meta(&cache, tgt.key, &def, &dispatcher).await {
                Some(m) => m,
                None => continue,
            };
            let reader = Arc::new(DispatcherClient::new(
                dispatcher.clone(),
                tgt.ams_net_id,
                tgt.ams_port,
            ));
            match def.source {
                CustomMetricSource::Poll => {
                    match Poller::new(def.clone(), meta, reader, self.inner.metric_tx.clone()) {
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

        self.inner.state.lock().insert(
            tgt.key,
            TargetState {
                dispatcher,
                notifier,
                notifier_task,
                poll_tasks,
            },
        );
        Ok(())
    }
}

/// Source port for dispatcher-originated requests. Outside TwinCAT's
/// reserved port ranges.
const DEFAULT_SOURCE_PORT: u16 = 30010;

async fn resolve_meta(
    cache: &Arc<SymbolTreeCache>,
    target: TargetKey,
    def: &CustomMetricDef,
    _dispatcher: &Arc<AmsDispatcher>,
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
    warn!(symbol = %def.symbol, "client-bridge: symbol not found in cache");
    None
}

#[derive(Clone)]
struct TargetDescriptor {
    key: TargetKey,
    ams_net_id: AmsNetId,
    ams_port: u16,
    router_host: Option<String>,
}

impl TargetDescriptor {
    fn from_def(def: &CustomMetricDef) -> anyhow::Result<Self> {
        let id_str = def
            .ams_net_id
            .as_deref()
            .context("custom metric missing ams_net_id")?;
        let netid = AmsNetId::from_str(id_str)
            .map_err(|e| anyhow::anyhow!("invalid ams_net_id '{id_str}': {e}"))?;
        let port = def.ams_port.unwrap_or(DEFAULT_AMS_PORT);
        Ok(Self {
            key: TargetKey::from(netid),
            ams_net_id: netid,
            ams_port: port,
            router_host: def
                .ams_router_host
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
        })
    }
}

impl std::fmt::Debug for TargetDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TargetDescriptor({}, port={})", self.key, self.ams_port)
    }
}

/// Our hostname as reported to TwinCAT route-add handlers. Kept short.
#[allow(dead_code)]
fn our_host_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tc-otel".to_string())
}

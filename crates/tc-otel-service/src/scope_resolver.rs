//! Resolver for OTel `InstrumentationScope.name` (logger / tracer /
//! meter name) from a PLC instance path.
//!
//! Per the plan in
//! `docs/plans/scope-name-via-ads-symbol-resolution.md`: the PLC sends
//! us a namespace string on every record (`entry.logger` for logs;
//! later `code.namespace` for traces/metrics). The instance path
//! ends in the FB_Log / FB_Tracer / FB_Metrics variable name; one
//! segment up is the parent FB whose **type** we want as the OTel
//! scope (e.g. `FB_Motor` instead of `Cell1.Axis1.Motor`). The PLC's
//! ADS symbol table is the only reliable source for that type.
//!
//! This module provides:
//!
//! - [`ScopeLookup`] trait — abstracts over the actual ADS read so
//!   the resolver can be unit-tested against a mocked symbol table.
//! - [`ScopeResolver`] — async cache-first lookup. Caches both
//!   positive (resolved type) and negative (raw fallback) outcomes
//!   per `(net_id, namespace)`. Drops the entire cache for a `net_id`
//!   on every fresh registration message — that single trigger
//!   covers Online Change, Full Activate Configuration, Cold Reset,
//!   and tc-otel restart cases (the PLC's `FB_TcOtelTask` re-emits
//!   a registration in all of them).
//!
//! Real ADS-backed `ScopeLookup` lands in a follow-up commit. For
//! now, [`NoopLookup`] always misses → records emit with the raw
//! namespace as `scope.name`. That's a strict no-regression vs the
//! current behaviour: empty `scope_name` already falls back to the
//! crate-default `tc-otel` in the encoder, and a non-empty value
//! flows through unchanged.
//!
//! Phase 1B will swap [`NoopLookup`] for an `AdsScopeLookup` that
//! pipes through `tc-otel-ads`'s symbol-table reader; the resolver
//! itself doesn't change.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tc_otel_ads::{AdsClient, AmsNetId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;

/// One resolution attempt's outcome — what we cache and what we
/// stamp onto the OTel `scope.name` field (plus optional
/// `plc.instance_path` per-record attribute).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeValue {
    /// ADS lookup found a matching FB type. `type_name` becomes
    /// `scope.name` (e.g. `FB_Motor`); `instance_path` is the
    /// concrete instance the resolver used (e.g.
    /// `PRG_TestSimpleApi.fbMotor`), shipped as
    /// `plc.instance_path` so Motor 1 vs Motor 2 stays
    /// distinguishable when both bucket under the same type.
    Resolved {
        type_name: String,
        instance_path: String,
    },
    /// ADS lookup found nothing — likely a custom logger string
    /// (e.g. `F_Log().WithLogger('MyLabel')`) or a PRG-rooted path
    /// without an enclosing FB. Use the namespace as-is so the
    /// user-supplied label still rides through. No
    /// `plc.instance_path` attribute is added.
    Raw(String),
}

/// What [`ScopeResolver::resolve`] returns to callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOutcome {
    /// Goes onto `InstrumentationScope.name`.
    pub scope_name: String,
    /// Set only when the resolver found an FB type. The dispatcher
    /// emits this as the `plc.instance_path` record attribute.
    pub instance_path: Option<String>,
}

impl ScopeValue {
    fn into_outcome(self) -> ScopeOutcome {
        match self {
            ScopeValue::Resolved {
                type_name,
                instance_path,
            } => ScopeOutcome {
                scope_name: type_name,
                instance_path: Some(instance_path),
            },
            ScopeValue::Raw(s) => ScopeOutcome {
                scope_name: s,
                instance_path: None,
            },
        }
    }
}


/// ADS-side abstraction. Implementors take the namespace the PLC
/// supplied (already an FB instance path — the PLC's FB_Log /
/// FB_Tracer / FB_Metrics FB_Init strips its own variable suffix
/// before sending) and look it up directly in the symbol table.
///
/// * **Hit** — return `(type_name, namespace)`. The type name
///   becomes `scope.name`; the namespace itself is the
///   instance path the dispatcher ships as `plc.instance_path`.
/// * **Miss** — return `Ok(None)`. Resolver caches as
///   `Raw(namespace)` so the user's literal string (custom label
///   via `F_Log().WithLogger(...)`, or a PRG-rooted path that the
///   symbol table doesn't enumerate) still rides through.
///
/// Errors propagate: a transient ADS failure is *not* cached so
/// the next record retries. Only definitive "no symbol" answers
/// produce a cached `Raw` entry.
///
/// `invalidate` lets the resolver tell the lookup to forget any
/// per-`net_id` state it might be holding (e.g. a downloaded
/// symbol table). Called whenever the resolver detects an
/// `OnlineChangeCnt` bump — symbols may have changed and the
/// lookup must re-fetch on the next miss.
/// `target_port` is the PLC runtime's ADS port (851 for the first
/// runtime, 852 for the second, …). Threaded through from the
/// log/trace/metric record's `ams_app_port` so the resolver
/// connects to the right runtime when the PLC has more than one.
#[async_trait]
pub trait ScopeLookup: Send + Sync {
    async fn resolve(
        &self,
        net_id: &str,
        namespace: &str,
        target_port: u16,
    ) -> anyhow::Result<Option<(String, String)>>;

    async fn invalidate(&self, _net_id: &str) {
        // Default: stateless lookups have nothing to invalidate.
    }
}

/// Real ADS-backed lookup. Connects to the AMS router on `addr`
/// (typically `"127.0.0.1"` for local-router setups) and downloads
/// the full PLC symbol table the first time a record arrives for
/// each `net_id`. The map is kept in memory; subsequent lookups
/// for the same `net_id` are pure hash gets — microseconds — until
/// the resolver invalidates on an `OnlineChangeCnt` bump.
///
/// Empirical numbers from the live PoC (see `scripts/scope_resolver_poc.py`):
/// 337 symbols downloaded in 6.9–10 ms over local-router on the
/// IPC. The "first record per `(net_id, namespace)` per app version
/// may be unscoped while the lookup runs" promise from the plan
/// is barely user-visible at that latency.
pub struct AdsScopeLookup {
    /// AMS-router host the resolver connects to. `"127.0.0.1"` when
    /// tc-otel runs on the same IPC as the PLC; the IP of a remote
    /// router otherwise.
    addr: String,
    /// Source NetID we use as the AMS-frame `source_net_id` on
    /// outgoing reads. Typically the local-router NetID tc-otel
    /// already learned via PortConnect during transport startup.
    source_net_id: AmsNetId,
    /// Source AMS port for outbound reads. Any unique 16-bit
    /// number; not the same as the inbound register port (16150)
    /// the local-router transport uses for receiving.
    source_port: u16,
    /// Per-net_id symbol table: `instance_path → type_name`.
    /// Lazy-filled on first miss, dropped when `invalidate` runs.
    /// Wrapped in `Arc` so concurrent resolutions share one
    /// snapshot without re-downloading.
    tables: RwLock<HashMap<String, Arc<HashMap<String, String>>>>,
}

impl AdsScopeLookup {
    /// Build a lookup that connects to the AMS router at `addr` for
    /// every new PLC `net_id` it encounters. The PLC's runtime port
    /// (851 / 852 / …) is per-call (`target_port` on `resolve`)
    /// because a single tc-otel instance may see records from
    /// multiple runtimes on the same NetID.
    pub fn new(addr: impl Into<String>, source_net_id: AmsNetId, source_port: u16) -> Self {
        Self {
            addr: addr.into(),
            source_net_id,
            source_port,
            tables: RwLock::new(HashMap::new()),
        }
    }

    /// AMS/TCP `PortConnect` handshake on a fresh socket. The local
    /// AMS router rejects outbound ADS reads with
    /// `ADSERR_DEVICE_PORTNOTCONNECTED (0x12)` when the source port
    /// hasn't been registered. PortConnect registers our port and
    /// returns the NetID + actual port the router assigned, which we
    /// then use as `source_net_id` / `source_port` in the AMS frame
    /// header so responses route back to this socket.
    ///
    /// Wire format (mirror of `LocalRouterAmsTransport::handshake`):
    /// - Request:  6 B AMS/TCP header (`cmd=0x1000`, `len=2`) + 2 B
    ///   `register_port: u16 LE`.
    /// - Response: same 6 B header + ≥8 B payload = 6 B NetID + 2 B
    ///   assigned `port: u16 LE`.
    async fn port_connect(
        stream: &mut TcpStream,
        register_port: u16,
    ) -> anyhow::Result<(AmsNetId, u16)> {
        const AMS_TCP_CMD_PORT_CONNECT: u16 = 0x1000;
        let mut req = Vec::with_capacity(8);
        req.extend_from_slice(&AMS_TCP_CMD_PORT_CONNECT.to_le_bytes());
        req.extend_from_slice(&2u32.to_le_bytes());
        req.extend_from_slice(&register_port.to_le_bytes());
        stream.write_all(&req).await?;

        let mut hdr = [0u8; 6];
        stream.read_exact(&mut hdr).await?;
        let cmd = u16::from_le_bytes([hdr[0], hdr[1]]);
        let len = u32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
        if cmd != AMS_TCP_CMD_PORT_CONNECT {
            return Err(anyhow::anyhow!(
                "PortConnect: unexpected response cmd 0x{cmd:04x}"
            ));
        }
        if len < 8 {
            return Err(anyhow::anyhow!(
                "PortConnect: short response ({len} bytes)"
            ));
        }
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let net_id = AmsNetId::from_bytes([body[0], body[1], body[2], body[3], body[4], body[5]]);
        let assigned = u16::from_le_bytes([body[6], body[7]]);
        Ok((net_id, assigned))
    }

    /// Fetch (or return cached) symbol-table map for a PLC.
    async fn table_for(
        &self,
        net_id_str: &str,
        target_port: u16,
    ) -> anyhow::Result<Arc<HashMap<String, String>>> {
        if let Some(t) = self.tables.read().await.get(net_id_str) {
            return Ok(t.clone());
        }
        // Cache miss: connect + handshake + download. Read lock
        // dropped before the network round-trip; the write happens
        // after so the contention window is short. Each phase wrapped
        // in a 10 s timeout so a misconfigured route doesn't silently
        // stall the dispatcher.
        let started = std::time::Instant::now();
        tracing::info!(
            net_id = %net_id_str,
            target_port,
            addr = %self.addr,
            "AdsScopeLookup: cache miss, downloading symbol table"
        );
        let target = AmsNetId::from_str_ref(net_id_str)
            .map_err(|e| anyhow::anyhow!("invalid net_id {net_id_str:?}: {e}"))?;

        // Open the TCP socket and do PortConnect first — without it
        // the AMS router rejects our outbound reads with
        // ADSERR_DEVICE_PORTNOTCONNECTED (0x12).
        let target_addr = format!("{}:48898", self.addr);
        let mut stream = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            TcpStream::connect(&target_addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(net_id = %net_id_str, error = %e,
                    "AdsScopeLookup: TCP connect to AMS router failed");
                return Err(anyhow::anyhow!(
                    "ADS TCP connect to {target_addr}: {e}"
                ));
            }
            Err(_) => {
                tracing::warn!(net_id = %net_id_str,
                    "AdsScopeLookup: TCP connect timed out after 10s");
                return Err(anyhow::anyhow!(
                    "ADS TCP connect to {target_addr} timed out"
                ));
            }
        };
        let _ = stream.set_nodelay(true);

        let (assigned_net_id, assigned_port) = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Self::port_connect(&mut stream, self.source_port),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(net_id = %net_id_str, error = %e,
                    "AdsScopeLookup: PortConnect handshake failed");
                return Err(e);
            }
            Err(_) => {
                tracing::warn!(net_id = %net_id_str,
                    "AdsScopeLookup: PortConnect timed out after 5s");
                return Err(anyhow::anyhow!("PortConnect timed out"));
            }
        };
        // Use the router-assigned NetID + port as source so responses
        // route back to *this* socket. `source_net_id` from the
        // config is a fallback hint; the router's view of who we are
        // is what actually matters for routing.
        let _ = self.source_net_id; // suppress unused-field lint
        let mut client = AdsClient::from_stream(
            stream,
            assigned_net_id,
            assigned_port,
            target,
            target_port,
        );
        let symbols = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.read_symbol_table(),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(net_id = %net_id_str, error = %e,
                    "AdsScopeLookup: symbol-table read failed (likely PortConnect missing — AMS router can't route response back)");
                return Err(anyhow::anyhow!(
                    "ADS symbol upload for {net_id_str}: {e}"
                ));
            }
            Err(_) => {
                tracing::warn!(net_id = %net_id_str,
                    "AdsScopeLookup: symbol-table read timed out after 10s (likely PortConnect missing)");
                return Err(anyhow::anyhow!(
                    "ADS symbol upload for {net_id_str} timed out"
                ));
            }
        };
        let n = symbols.len();
        let map: HashMap<String, String> =
            symbols.into_iter().map(|s| (s.name, s.type_name)).collect();
        tracing::info!(
            net_id = %net_id_str,
            symbols = n,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "AdsScopeLookup: symbol table downloaded"
        );
        let arc = Arc::new(map);
        let mut w = self.tables.write().await;
        let stored = w
            .entry(net_id_str.to_string())
            .or_insert_with(|| arc.clone())
            .clone();
        Ok(stored)
    }
}

#[async_trait]
impl ScopeLookup for AdsScopeLookup {
    async fn resolve(
        &self,
        net_id: &str,
        namespace: &str,
        target_port: u16,
    ) -> anyhow::Result<Option<(String, String)>> {
        if namespace.is_empty() {
            return Ok(None);
        }
        let table = self.table_for(net_id, target_port).await?;
        Ok(lookup_in_table(&table, namespace))
    }

    async fn invalidate(&self, net_id: &str) {
        self.tables.write().await.remove(net_id);
    }
}

/// Direct lookup against an already-downloaded symbol table.
/// PLC has already stripped the framework FB's own variable name
/// (see `FB_Log.FB_Init`), so the namespace IS the FB instance
/// path. Returns `(type_name, instance_path)` on hit.
fn lookup_in_table(
    table: &HashMap<String, String>,
    namespace: &str,
) -> Option<(String, String)> {
    table
        .get(namespace)
        .map(|t| (t.clone(), namespace.to_string()))
}

/// Stub used while no real symbol-table lookup is wired in.
/// Always returns `Ok(None)` — every namespace lands in the
/// negative-cache `Raw` slot, so records keep their PLC-supplied
/// namespace. Same observable behaviour as the pre-resolver world.
pub struct NoopLookup;

#[async_trait]
impl ScopeLookup for NoopLookup {
    async fn resolve(
        &self,
        _net_id: &str,
        _namespace: &str,
        _target_port: u16,
    ) -> anyhow::Result<Option<(String, String)>> {
        Ok(None)
    }
}

/// Cache-first scope resolver. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ScopeResolver {
    inner: Arc<Inner>,
}

struct Inner {
    /// `(net_id, namespace) → ScopeValue`. Both positive and negative
    /// outcomes share the cache; the consumer only sees the final
    /// scope-name string and doesn't care which branch produced it.
    cache: RwLock<HashMap<(String, String), ScopeValue>>,
    /// Per-net_id last-seen `OnlineChangeCnt`. On mismatch the cache
    /// for that net_id is dropped — covers Online Change, Full
    /// Activate Configuration, Cold Reset, and tc-otel restart in
    /// one rule (the PLC's `FB_TcOtelTask` re-registers with the
    /// updated counter in all those cases).
    occ_seen: RwLock<HashMap<String, u32>>,
    lookup: Arc<dyn ScopeLookup>,
}

impl ScopeResolver {
    /// Build a resolver around an arbitrary `ScopeLookup`. Phase 1B
    /// will pass an `AdsScopeLookup`; Phase 1A only ever calls
    /// [`ScopeResolver::noop`], hence the `dead_code` allow.
    #[allow(dead_code)]
    pub fn new(lookup: Arc<dyn ScopeLookup>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache: RwLock::new(HashMap::new()),
                occ_seen: RwLock::new(HashMap::new()),
                lookup,
            }),
        }
    }

    /// Convenience constructor — produces a resolver whose lookup
    /// always misses. Equivalent to "scope.name passes through
    /// unchanged"; used in tests and as the default while Phase 1B
    /// is unwired.
    pub fn noop() -> Self {
        Self::new(Arc::new(NoopLookup))
    }

    /// Map a namespace to a `scope.name`. Cache-first: only the
    /// first record per `(net_id, namespace)` per app version
    /// touches ADS.
    ///
    /// Returns the namespace itself (raw fallback) if the lookup
    /// errors transiently — the caller still gets a usable scope
    /// name, and the next record re-tries (errors are not cached).
    pub async fn resolve(
        &self,
        net_id: &str,
        namespace: &str,
        target_port: u16,
    ) -> ScopeOutcome {
        if namespace.is_empty() {
            return ScopeOutcome {
                scope_name: String::new(),
                instance_path: None,
            };
        }
        let key = (net_id.to_string(), namespace.to_string());
        if let Some(v) = self.inner.cache.read().await.get(&key) {
            return v.clone().into_outcome();
        }
        let value = match self
            .inner
            .lookup
            .resolve(net_id, namespace, target_port)
            .await
        {
            Ok(Some((type_name, instance_path))) => ScopeValue::Resolved {
                type_name,
                instance_path,
            },
            Ok(None) => ScopeValue::Raw(namespace.to_string()),
            Err(_) => {
                // Transient failure: don't cache, return raw so the
                // record still ships and the next call re-tries.
                return ScopeOutcome {
                    scope_name: namespace.to_string(),
                    instance_path: None,
                };
            }
        };
        self.inner.cache.write().await.insert(key, value.clone());
        value.into_outcome()
    }

    /// Drop every cached entry for the given `net_id`. Exposed for
    /// callers that have an explicit "registration arrived" signal
    /// outside the OCC channel (none today; kept for symmetry and
    /// for tests).
    #[allow(dead_code)]
    pub async fn invalidate(&self, net_id: &str) {
        self.inner
            .cache
            .write()
            .await
            .retain(|(n, _), _| n != net_id);
        self.inner.occ_seen.write().await.remove(net_id);
        self.inner.lookup.invalidate(net_id).await;
    }

    /// Observe a record's `(net_id, OnlineChangeCnt)`. If the OCC
    /// differs from the last observation for that net_id, the cache
    /// for that net_id is dropped before the next `resolve()` call —
    /// the PLC's `FB_TcOtelTask` re-emits a registration on every
    /// case that should invalidate symbols (Online Change, Full
    /// Activate, Cold Reset), and each carries the freshly-bumped
    /// OCC. tc-otel restart re-registers with *unchanged* OCC because
    /// the PLC code didn't change → no invalidation, cache stays
    /// valid (symbols are the same). One rule covers both cases.
    pub async fn observe_occ(&self, net_id: &str, occ: u32) {
        // Fast path: read-lock, compare, return on match.
        if let Some(&prev) = self.inner.occ_seen.read().await.get(net_id) {
            if prev == occ {
                return;
            }
        }
        // Mismatch (or first sighting): take write locks, double-check
        // under the lock, and either record the first sighting or
        // drop+update.
        let mut occ_map = self.inner.occ_seen.write().await;
        match occ_map.get(net_id) {
            Some(&prev) if prev == occ => {
                // A concurrent caller updated to the same value
                // between our read-check and write-lock — nothing
                // to do.
            }
            Some(_) => {
                // Genuine mismatch — drop cache and the lookup's
                // own per-net_id state (e.g. AdsScopeLookup's
                // cached symbol table) for this net_id.
                self.inner
                    .cache
                    .write()
                    .await
                    .retain(|(n, _), _| n != net_id);
                self.inner.lookup.invalidate(net_id).await;
                occ_map.insert(net_id.to_string(), occ);
            }
            None => {
                // First time we see this net_id; record without
                // touching the cache.
                occ_map.insert(net_id.to_string(), occ);
            }
        }
    }

    #[cfg(test)]
    pub async fn cache_len(&self) -> usize {
        self.inner.cache.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory symbol-table stand-in. Records every lookup so
    /// tests can assert that the cache short-circuits subsequent
    /// calls.
    struct MockLookup {
        table: HashMap<(String, String), String>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl MockLookup {
        fn new<I: IntoIterator<Item = ((&'static str, &'static str), &'static str)>>(
            it: I,
        ) -> Self {
            let table = it
                .into_iter()
                .map(|((n, p), t)| ((n.to_string(), p.to_string()), t.to_string()))
                .collect();
            Self {
                table,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ScopeLookup for MockLookup {
        async fn resolve(
            &self,
            net_id: &str,
            namespace: &str,
            _target_port: u16,
        ) -> anyhow::Result<Option<(String, String)>> {
            self.calls
                .lock()
                .unwrap()
                .push((net_id.to_string(), namespace.to_string()));
            // Mock mirrors the AdsScopeLookup direct-first +
            // framework-strip-fallback strategy so resolver tests
            // exercise the same semantics as the real lookup.
            let by_net: HashMap<String, String> = self
                .table
                .iter()
                .filter(|((n, _), _)| n == net_id)
                .map(|((_, p), t)| (p.clone(), t.clone()))
                .collect();
            Ok(lookup_in_table(&by_net, namespace))
        }
    }

#[tokio::test]
    async fn noop_lookup_returns_raw() {
        let r = ScopeResolver::noop();
        let out = r.resolve("net", "Drives.Motor.fbLog", 851).await;
        assert_eq!(out.scope_name, "Drives.Motor.fbLog");
        assert!(out.instance_path.is_none());
    }

    #[tokio::test]
    async fn empty_namespace_returns_empty() {
        let r = ScopeResolver::noop();
        let out = r.resolve("net", "", 851).await;
        assert!(out.scope_name.is_empty());
        assert!(out.instance_path.is_none());
    }

    #[tokio::test]
    async fn direct_hit_returns_type_and_instance_path() {
        // PLC has already stripped the FB_Log variable suffix in
        // FB_Init, so the namespace IS the FB instance path.
        // Direct table hit → FB type + namespace as instance path.
        let mock = Arc::new(MockLookup::new([(
            ("net", "Cell1.Axis1.Motor"),
            "FB_Motor",
        )]));
        let r = ScopeResolver::new(mock.clone());
        let out = r.resolve("net", "Cell1.Axis1.Motor", 851).await;
        assert_eq!(out.scope_name, "FB_Motor");
        assert_eq!(out.instance_path.as_deref(), Some("Cell1.Axis1.Motor"));
    }

    #[tokio::test]
    async fn prg_rooted_namespace_falls_through_to_raw() {
        // FB_Log instantiated directly in a PRG → after PLC strip,
        // namespace = PRG name. PRGs aren't enumerated as instance
        // entries in the symbol table → miss → Raw(namespace), so
        // every PRG buckets under its own scope name unchanged.
        let mock = Arc::new(MockLookup::new([] as [((&str, &str), &str); 0]));
        let r = ScopeResolver::new(mock.clone());
        let out = r.resolve("net", "PRG_TestSimpleApi", 851).await;
        assert_eq!(out.scope_name, "PRG_TestSimpleApi");
        assert!(out.instance_path.is_none());
    }

    #[tokio::test]
    async fn negative_lookup_caches_raw() {
        let mock = Arc::new(MockLookup::new([] as [((&str, &str), &str); 0]));
        let r = ScopeResolver::new(mock.clone());

        // First call: miss → ADS lookup → None → cached as Raw.
        let out1 = r.resolve("net", "Custom.Logger", 851).await;
        assert_eq!(out1.scope_name, "Custom.Logger");
        assert!(out1.instance_path.is_none());

        // Second call hits the cache; no extra lookup.
        let out2 = r.resolve("net", "Custom.Logger", 851).await;
        assert_eq!(out2.scope_name, "Custom.Logger");
        assert!(out2.instance_path.is_none());
        assert_eq!(mock.calls().len(), 1, "cache should short-circuit");
    }

    #[tokio::test]
    async fn cache_short_circuits_repeated_resolutions() {
        let mock = Arc::new(MockLookup::new([(("net", "Cell1.Motor"), "FB_Motor")]));
        let r = ScopeResolver::new(mock.clone());
        for _ in 0..5 {
            let out = r.resolve("net", "Cell1.Motor", 851).await;
            assert_eq!(out.scope_name, "FB_Motor");
            assert_eq!(out.instance_path.as_deref(), Some("Cell1.Motor"));
        }
        assert_eq!(mock.calls().len(), 1);
    }

    #[tokio::test]
    async fn invalidate_drops_only_matching_net_id() {
        let mock = Arc::new(MockLookup::new([
            (("netA", "P"), "FB_A"),
            (("netB", "P"), "FB_B"),
        ]));
        let r = ScopeResolver::new(mock.clone());
        r.resolve("netA", "P", 851).await;
        r.resolve("netB", "P", 851).await;
        assert_eq!(r.cache_len().await, 2);

        r.invalidate("netA").await;
        assert_eq!(r.cache_len().await, 1);

        // netA re-queries; netB stays cached.
        r.resolve("netA", "P", 851).await;
        r.resolve("netB", "P", 851).await;
        assert_eq!(mock.calls().len(), 3);
    }

    #[tokio::test]
    async fn observe_occ_first_sighting_does_not_clear() {
        let mock = Arc::new(MockLookup::new([(("net", "P"), "FB_X")]));
        let r = ScopeResolver::new(mock);
        r.resolve("net", "P", 851).await;
        assert_eq!(r.cache_len().await, 1);

        // First time we see this net_id's OCC: record without dropping.
        r.observe_occ("net", 7).await;
        assert_eq!(r.cache_len().await, 1);
    }

    #[tokio::test]
    async fn observe_occ_same_value_preserves_cache() {
        let mock = Arc::new(MockLookup::new([(("net", "P"), "FB_X")]));
        let r = ScopeResolver::new(mock);
        r.observe_occ("net", 5).await;
        r.resolve("net", "P", 851).await;
        assert_eq!(r.cache_len().await, 1);

        r.observe_occ("net", 5).await; // same OCC → no-op
        assert_eq!(r.cache_len().await, 1);
    }

    #[tokio::test]
    async fn observe_occ_mismatch_drops_cache_for_that_net_id() {
        let mock = Arc::new(MockLookup::new([
            (("netA", "P"), "FB_A"),
            (("netB", "P"), "FB_B"),
        ]));
        let r = ScopeResolver::new(mock);
        r.observe_occ("netA", 3).await;
        r.observe_occ("netB", 9).await;
        r.resolve("netA", "P", 851).await;
        r.resolve("netB", "P", 851).await;
        assert_eq!(r.cache_len().await, 2);

        // OCC bump on netA only.
        r.observe_occ("netA", 4).await;
        assert_eq!(
            r.cache_len().await,
            1,
            "netA's entry should be gone, netB's untouched"
        );
    }

    #[tokio::test]
    async fn transient_error_does_not_cache() {
        struct ErroringLookup;
        #[async_trait]
        impl ScopeLookup for ErroringLookup {
            async fn resolve(
                &self,
                _: &str,
                _: &str,
                _: u16,
            ) -> anyhow::Result<Option<(String, String)>> {
                Err(anyhow::anyhow!("transport down"))
            }
        }
        let r = ScopeResolver::new(Arc::new(ErroringLookup));
        // Returns raw fallback…
        let out = r.resolve("net", "X", 851).await;
        assert_eq!(out.scope_name, "X");
        assert!(out.instance_path.is_none());
        // …but doesn't cache, so next call re-tries (we can't observe
        // the retry directly without a counter; cache_len is the
        // signal that the negative-cache path didn't fire).
        assert_eq!(r.cache_len().await, 0);
    }
}

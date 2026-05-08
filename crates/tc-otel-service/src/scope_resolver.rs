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
use tokio::sync::RwLock;

/// One resolution attempt's outcome — the value we cache and stamp
/// onto the OTel `scope.name` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeValue {
    /// ADS lookup found a matching symbol; use its **type name**
    /// (e.g. `FB_Motor`) as the OTel scope. Records from any
    /// instance of this type bucket together in the encoder.
    Resolved(String),
    /// ADS lookup found nothing — likely a custom logger string
    /// (e.g. `FB_Log('Drives.Motor')`). Use the namespace as-is so
    /// the user-supplied label still rides through. Cached so we
    /// don't re-query within one PLC app version.
    Raw(String),
}

impl ScopeValue {
    /// Return the string we want as `LogRecord.scope_name` /
    /// `TraceRecord.scope_name` / `MetricRecord.scope_name`.
    pub fn into_scope_name(self) -> String {
        match self {
            ScopeValue::Resolved(s) | ScopeValue::Raw(s) => s,
        }
    }
}

/// ADS-side abstraction. Implementors return the resolved type for
/// a given `(net_id, parent_path)` pair — e.g. by looking up
/// `parent_path` in a previously-fetched symbol table for that
/// `net_id`. Returning `Ok(None)` means "no such symbol" and the
/// resolver caches that as the negative-cache `Raw` value.
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
        parent_path: &str,
        target_port: u16,
    ) -> anyhow::Result<Option<String>>;

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

    /// Fetch (or return cached) symbol-table map for a PLC.
    async fn table_for(
        &self,
        net_id_str: &str,
        target_port: u16,
    ) -> anyhow::Result<Arc<HashMap<String, String>>> {
        if let Some(t) = self.tables.read().await.get(net_id_str) {
            return Ok(t.clone());
        }
        // Cache miss: connect + download. We hold the read lock
        // already released; the write happens after the network
        // round-trip so the lock contention window is short.
        let target = AmsNetId::from_str_ref(net_id_str)
            .map_err(|e| anyhow::anyhow!("invalid net_id {net_id_str:?}: {e}"))?;
        let mut client = AdsClient::connect(
            &self.addr,
            self.source_net_id,
            self.source_port,
            target,
            target_port,
        )
        .await
        .map_err(|e| anyhow::anyhow!("ADS connect to {} for {net_id_str}: {e}", self.addr))?;
        let symbols = client
            .read_symbol_table()
            .await
            .map_err(|e| anyhow::anyhow!("ADS symbol upload for {net_id_str}: {e}"))?;
        let map: HashMap<String, String> =
            symbols.into_iter().map(|s| (s.name, s.type_name)).collect();
        let arc = Arc::new(map);
        // Race: a concurrent caller may have populated the map
        // between our read and write — accept either snapshot since
        // the symbol table is the same for one app version.
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
        parent_path: &str,
        target_port: u16,
    ) -> anyhow::Result<Option<String>> {
        if parent_path.is_empty() {
            // No parent path = the whole namespace was a single
            // segment (custom literal like `FB_Log('Drives.Motor')`).
            // Falls through to the resolver's `Raw(namespace)`
            // branch upstream.
            return Ok(None);
        }
        let table = self.table_for(net_id, target_port).await?;
        Ok(table.get(parent_path).cloned())
    }

    async fn invalidate(&self, net_id: &str) {
        self.tables.write().await.remove(net_id);
    }
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
        _parent_path: &str,
        _target_port: u16,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// Strip the trailing instance-path segment — the FB_Log /
/// FB_Tracer / FB_Metrics variable that produced the record. The
/// remainder is the parent FB's instance path, which is what we
/// look up in the symbol table to get the parent's *type*.
///
/// Examples:
/// - `Cell1.Axis1.Motor.fbLog` → `Cell1.Axis1.Motor`
/// - `Drives.Motor`            → `Drives` (single trailing segment
///   is unusual but well-defined)
/// - `fbLog`                   → empty string (no parent path)
/// - `Drives.Motor.`           → `Drives.Motor` (trailing dot
///   tolerated)
fn parent_path(namespace: &str) -> &str {
    let trimmed = namespace.trim_end_matches('.');
    match trimmed.rfind('.') {
        Some(i) => &trimmed[..i],
        None => "",
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
    pub async fn resolve(&self, net_id: &str, namespace: &str, target_port: u16) -> String {
        if namespace.is_empty() {
            return String::new();
        }
        let key = (net_id.to_string(), namespace.to_string());
        if let Some(v) = self.inner.cache.read().await.get(&key) {
            return v.clone().into_scope_name();
        }
        let parent = parent_path(namespace);
        let value = match self.inner.lookup.resolve(net_id, parent, target_port).await {
            Ok(Some(type_name)) => ScopeValue::Resolved(type_name),
            Ok(None) => ScopeValue::Raw(namespace.to_string()),
            Err(_) => {
                // Transient failure: don't cache, return raw so the
                // record still ships and the next call re-tries.
                return namespace.to_string();
            }
        };
        self.inner.cache.write().await.insert(key, value.clone());
        value.into_scope_name()
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
            parent_path: &str,
            _target_port: u16,
        ) -> anyhow::Result<Option<String>> {
            self.calls
                .lock()
                .unwrap()
                .push((net_id.to_string(), parent_path.to_string()));
            Ok(self
                .table
                .get(&(net_id.to_string(), parent_path.to_string()))
                .cloned())
        }
    }

    #[test]
    fn parent_path_strips_last_segment() {
        assert_eq!(parent_path("Cell1.Axis1.Motor.fbLog"), "Cell1.Axis1.Motor");
        assert_eq!(parent_path("Drives.Motor"), "Drives");
        assert_eq!(parent_path("fbLog"), "");
        assert_eq!(parent_path("Drives.Motor."), "Drives");
        assert_eq!(parent_path(""), "");
    }

    #[tokio::test]
    async fn noop_lookup_returns_raw() {
        let r = ScopeResolver::noop();
        assert_eq!(
            r.resolve("net", "Drives.Motor.fbLog", 851).await,
            "Drives.Motor.fbLog"
        );
    }

    #[tokio::test]
    async fn empty_namespace_returns_empty() {
        let r = ScopeResolver::noop();
        assert!(r.resolve("net", "", 851).await.is_empty());
    }

    #[tokio::test]
    async fn positive_lookup_returns_type_name() {
        let mock = Arc::new(MockLookup::new([(
            ("net", "Cell1.Axis1.Motor"),
            "FB_Motor",
        )]));
        let r = ScopeResolver::new(mock.clone());
        let scope = r.resolve("net", "Cell1.Axis1.Motor.fbLog", 851).await;
        assert_eq!(scope, "FB_Motor");
    }

    #[tokio::test]
    async fn negative_lookup_caches_raw() {
        let mock = Arc::new(MockLookup::new([] as [((&str, &str), &str); 0]));
        let r = ScopeResolver::new(mock.clone());

        // First call: miss → ADS lookup → None → cached as Raw.
        let scope1 = r.resolve("net", "Custom.Logger", 851).await;
        assert_eq!(scope1, "Custom.Logger");

        // Second call hits the cache; no extra lookup.
        let scope2 = r.resolve("net", "Custom.Logger", 851).await;
        assert_eq!(scope2, "Custom.Logger");
        assert_eq!(mock.calls().len(), 1, "cache should short-circuit");
    }

    #[tokio::test]
    async fn cache_short_circuits_repeated_resolutions() {
        let mock = Arc::new(MockLookup::new([(("net", "Cell1.Motor"), "FB_Motor")]));
        let r = ScopeResolver::new(mock.clone());
        for _ in 0..5 {
            assert_eq!(r.resolve("net", "Cell1.Motor.fbLog", 851).await, "FB_Motor");
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
        r.resolve("netA", "P.fbLog", 851).await;
        r.resolve("netB", "P.fbLog", 851).await;
        assert_eq!(r.cache_len().await, 2);

        r.invalidate("netA").await;
        assert_eq!(r.cache_len().await, 1);

        // netA re-queries; netB stays cached.
        r.resolve("netA", "P.fbLog", 851).await;
        r.resolve("netB", "P.fbLog", 851).await;
        assert_eq!(mock.calls().len(), 3);
    }

    #[tokio::test]
    async fn observe_occ_first_sighting_does_not_clear() {
        let mock = Arc::new(MockLookup::new([(("net", "P"), "FB_X")]));
        let r = ScopeResolver::new(mock);
        r.resolve("net", "P.fbLog", 851).await;
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
        r.resolve("net", "P.fbLog", 851).await;
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
        r.resolve("netA", "P.fbLog", 851).await;
        r.resolve("netB", "P.fbLog", 851).await;
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
            async fn resolve(&self, _: &str, _: &str, _: u16) -> anyhow::Result<Option<String>> {
                Err(anyhow::anyhow!("transport down"))
            }
        }
        let r = ScopeResolver::new(Arc::new(ErroringLookup));
        // Returns raw fallback…
        assert_eq!(r.resolve("net", "X.fbLog", 851).await, "X.fbLog");
        // …but doesn't cache, so next call re-tries (we can't observe
        // the retry directly without a counter; cache_len is the
        // signal that the negative-cache path didn't fire).
        assert_eq!(r.cache_len().await, 0);
    }
}

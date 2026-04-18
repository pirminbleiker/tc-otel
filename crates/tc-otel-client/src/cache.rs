//! Per-target cache for [`SymbolTree`] instances.
//!
//! The ADS symbol table is large (10k+ entries is normal on real projects) and
//! rarely changes. The cache holds one tree per PLC target keyed by `AmsNetId`
//! and only refreshes on explicit invalidation — the UI provides a refresh
//! button, and the service layer invalidates on observed PLC state transitions
//! (e.g. `Reconfig → Run`).

use crate::browse::SymbolTree;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Opaque key for a target PLC — the 6-byte AMS Net ID.
///
/// Re-declared here (rather than importing from `ads::AmsNetId`) because the
/// upstream type does not derive `Hash`. Interconversion is trivial:
/// `TargetKey::from(ads_netid.0)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TargetKey(pub [u8; 6]);

impl From<[u8; 6]> for TargetKey {
    fn from(raw: [u8; 6]) -> Self {
        Self(raw)
    }
}

impl From<ads::AmsNetId> for TargetKey {
    fn from(id: ads::AmsNetId) -> Self {
        Self(id.0)
    }
}

impl std::fmt::Display for TargetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a}.{b}.{c}.{d}.{e}.{g}")
    }
}

#[derive(Clone)]
struct CacheEntry {
    tree: Arc<SymbolTree>,
    fetched_at: DateTime<Utc>,
}

/// Thread-safe cache of symbol trees.
///
/// Reads take a read-lock and return an `Arc<SymbolTree>` so the caller can
/// release the lock immediately and keep browsing.
#[derive(Clone, Default)]
pub struct SymbolTreeCache {
    inner: Arc<RwLock<HashMap<TargetKey, CacheEntry>>>,
}

impl SymbolTreeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached tree for `target`, if any.
    pub fn get(&self, target: TargetKey) -> Option<Arc<SymbolTree>> {
        self.inner.read().get(&target).map(|e| e.tree.clone())
    }

    /// Return when the cache was last populated for `target`.
    pub fn fetched_at(&self, target: TargetKey) -> Option<DateTime<Utc>> {
        self.inner.read().get(&target).map(|e| e.fetched_at)
    }

    /// Insert (or replace) the tree for `target`. Sets `fetched_at = now`.
    pub fn insert(&self, target: TargetKey, tree: SymbolTree) {
        self.insert_with_time(target, tree, Utc::now());
    }

    /// Variant of [`insert`] that accepts an explicit timestamp — useful for
    /// deterministic tests.
    pub fn insert_with_time(&self, target: TargetKey, tree: SymbolTree, fetched_at: DateTime<Utc>) {
        let mut guard = self.inner.write();
        guard.insert(
            target,
            CacheEntry {
                tree: Arc::new(tree),
                fetched_at,
            },
        );
    }

    /// Drop the cached tree for `target` (if present).
    pub fn invalidate(&self, target: TargetKey) {
        self.inner.write().remove(&target);
    }

    /// Drop all cached trees.
    pub fn invalidate_all(&self) {
        self.inner.write().clear();
    }

    /// Count of targets currently cached. Useful for diagnostics only.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// List all targets currently cached (snapshot — not synchronized with
    /// subsequent mutations).
    pub fn targets(&self) -> Vec<TargetKey> {
        self.inner.read().keys().copied().collect()
    }
}

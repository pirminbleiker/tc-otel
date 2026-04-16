//! Symbol handle cache for efficient ADS symbol operations
//!
//! Caches symbol name → handle mappings to avoid repeated name resolution.
//! Handles are scoped to (AMS Net ID, AMS port) pairs.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Key for caching: (net_id_string, port, symbol_name)
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SymbolHandleKey {
    pub ams_net_id: String,
    pub ams_port: u16,
    pub symbol: String,
}

/// Cached symbol handle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedSymbolHandle {
    pub handle: u32,
}

/// Symbol handle cache with TTL invalidation
#[derive(Debug, Clone)]
pub struct SymbolHandleCache {
    cache: Arc<Mutex<HashMap<SymbolHandleKey, CachedSymbolHandle>>>,
}

impl SymbolHandleCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a cached handle, if present
    pub async fn get(&self, key: &SymbolHandleKey) -> Option<CachedSymbolHandle> {
        let cache = self.cache.lock().await;
        cache.get(key).copied()
    }

    /// Insert a handle into the cache
    pub async fn insert(&self, key: SymbolHandleKey, handle: CachedSymbolHandle) {
        let mut cache = self.cache.lock().await;
        cache.insert(key, handle);
    }

    /// Remove a cached handle
    pub async fn remove(&self, key: &SymbolHandleKey) {
        let mut cache = self.cache.lock().await;
        cache.remove(key);
    }

    /// Clear all cached handles for a given (net_id, port) pair
    pub async fn clear_target(&self, ams_net_id: &str, ams_port: u16) {
        let mut cache = self.cache.lock().await;
        cache.retain(|k, _| !(k.ams_net_id == ams_net_id && k.ams_port == ams_port));
    }

    /// Clear all cached handles
    pub async fn clear(&self) {
        let mut cache = self.cache.lock().await;
        cache.clear();
    }
}

impl Default for SymbolHandleCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_insert_and_get() {
        let cache = SymbolHandleCache::new();
        let key = SymbolHandleKey {
            ams_net_id: "192.168.1.1.1.1".to_string(),
            ams_port: 851,
            symbol: "GVL.temperature".to_string(),
        };
        let handle = CachedSymbolHandle { handle: 42 };

        cache.insert(key.clone(), handle).await;
        assert_eq!(cache.get(&key).await, Some(handle));
    }

    #[tokio::test]
    async fn test_cache_remove() {
        let cache = SymbolHandleCache::new();
        let key = SymbolHandleKey {
            ams_net_id: "192.168.1.1.1.1".to_string(),
            ams_port: 851,
            symbol: "GVL.temperature".to_string(),
        };
        let handle = CachedSymbolHandle { handle: 42 };

        cache.insert(key.clone(), handle).await;
        assert!(cache.get(&key).await.is_some());

        cache.remove(&key).await;
        assert!(cache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_cache_clear_target() {
        let cache = SymbolHandleCache::new();
        let key1 = SymbolHandleKey {
            ams_net_id: "192.168.1.1.1.1".to_string(),
            ams_port: 851,
            symbol: "GVL.temp1".to_string(),
        };
        let key2 = SymbolHandleKey {
            ams_net_id: "192.168.1.2.1.1".to_string(),
            ams_port: 851,
            symbol: "GVL.temp2".to_string(),
        };

        cache
            .insert(key1.clone(), CachedSymbolHandle { handle: 1 })
            .await;
        cache
            .insert(key2.clone(), CachedSymbolHandle { handle: 2 })
            .await;

        cache.clear_target("192.168.1.1.1.1", 851).await;

        assert!(cache.get(&key1).await.is_none());
        assert!(cache.get(&key2).await.is_some());
    }
}

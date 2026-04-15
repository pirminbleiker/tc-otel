//! Backward compatibility wrapper for AMS/TCP server
//!
//! This module re-exports the TCP transport implementation and provides
//! a type alias for backward compatibility with code that uses AmsTcpServer.

use crate::router::AdsRouter;
pub use crate::transport::{AmsTransport, TcpAmsTransport};
use std::sync::Arc;
use tc_otel_core::LogEntry;

/// Backward-compatible wrapper for AmsTcpServer
/// Provides the old `start()` method that delegates to the AmsTransport `run()` method
pub struct AmsTcpServer {
    inner: TcpAmsTransport,
}

impl std::fmt::Debug for AmsTcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmsTcpServer").finish()
    }
}

impl AmsTcpServer {
    pub fn new(
        host: String,
        net_id: crate::ams::AmsNetId,
        ads_port: u16,
        log_tx: tokio::sync::mpsc::Sender<LogEntry>,
    ) -> Self {
        // Create a local registry and router for backward compatibility
        let registry = Arc::new(crate::registry::TaskRegistry::new());
        let router = Arc::new(AdsRouter::new(ads_port, log_tx, None, registry));
        Self {
            inner: TcpAmsTransport::new(host, net_id, router),
        }
    }

    pub fn with_connection_config(
        mut self,
        config: crate::connection_manager::ConnectionConfig,
    ) -> Self {
        self.inner = self.inner.with_connection_config(config);
        self
    }

    /// Get a reference to the connection manager
    pub fn connection_manager(&self) -> &Arc<crate::connection_manager::ConnectionManager> {
        self.inner.connection_manager()
    }

    /// Get a reference to the task registry
    pub fn task_registry(&self) -> &Arc<crate::registry::TaskRegistry> {
        self.inner.task_registry()
    }

    /// Start the TCP server (backward compatible method)
    /// This calls the underlying AmsTransport::run() method
    pub async fn start(&self) -> crate::Result<()> {
        let transport = Arc::new(self.inner.clone());
        AmsTransport::run(transport).await
    }
}

impl Clone for AmsTcpServer {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

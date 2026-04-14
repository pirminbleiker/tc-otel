//! Backward compatibility wrapper for AMS/TCP server
//!
//! This module re-exports the TCP transport implementation and provides
//! a type alias for backward compatibility with code that uses AmsTcpServer.

pub use crate::transport::{AmsTransport, TcpAmsTransport};
use std::sync::Arc;

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
        log_tx: tokio::sync::mpsc::Sender<tc_otel_core::LogEntry>,
    ) -> Self {
        Self {
            inner: TcpAmsTransport::new(host, net_id, ads_port, log_tx),
        }
    }

    /// Enable metrics forwarding by providing a channel sender
    pub fn with_metric_sender(
        mut self,
        metric_tx: tokio::sync::mpsc::Sender<tc_otel_core::MetricEntry>,
    ) -> Self {
        self.inner = self.inner.with_metric_sender(metric_tx);
        self
    }

    pub fn with_registry(mut self, registry: Arc<crate::registry::TaskRegistry>) -> Self {
        self.inner = self.inner.with_registry(registry);
        self
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

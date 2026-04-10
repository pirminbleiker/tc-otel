//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::{AmsNetId, AmsTcpServer, ConnectionConfig, ConnectionManager, TaskRegistry};
use tc_otel_core::{AppSettings, DiagnosticStats, SubscriptionManager};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::dispatcher::LogDispatcher;
use crate::web::{self, WebState};

/// Main Log4TC Service
pub struct Log4TcService {
    settings: AppSettings,
    log_dispatcher: LogDispatcher,
    stats: Arc<DiagnosticStats>,
}

impl Log4TcService {
    pub async fn new(settings: AppSettings) -> Result<Self> {
        let stats = DiagnosticStats::new();
        let dispatcher = LogDispatcher::new(&settings, stats.clone()).await?;
        Ok(Self {
            settings,
            log_dispatcher: dispatcher,
            stats,
        })
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Log4TC Service starting");

        let (log_tx, mut log_rx) = mpsc::channel(self.settings.service.channel_capacity);
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);

        // Shared state for connection management and task registry
        let conn_config = ConnectionConfig {
            max_connections: self.settings.receiver.max_connections,
            idle_timeout_secs: self.settings.receiver.idle_timeout_secs,
            max_connections_per_ip: self.settings.receiver.max_connections_per_ip,
            rate_limit_per_sec_per_ip: self.settings.receiver.rate_limit_per_sec_per_ip,
            keepalive_interval_secs: self.settings.receiver.keepalive_interval_secs,
            send_buffer_size: self.settings.receiver.send_buffer_size,
            shutdown_timeout_secs: self.settings.service.shutdown_timeout_secs,
        };
        let conn_manager = Arc::new(ConnectionManager::new(conn_config));
        let registry = Arc::new(TaskRegistry::new());

        // Start AMS/TCP server (receives ADS from PLC via AMS routing)
        let net_id = AmsNetId::from_str(&self.settings.receiver.ams_net_id)
            .map_err(|e| anyhow::anyhow!("Invalid AMS Net ID: {}", e))?;

        let ams_server = AmsTcpServer::new(
            self.settings.receiver.host.clone(),
            net_id,
            self.settings.receiver.ads_port,
            log_tx.clone(),
        )
        .with_connection_manager(conn_manager.clone())
        .with_registry(registry.clone());

        let mut shutdown_rx_ams = shutdown_tx.subscribe();
        let ams_handle = tokio::spawn(async move {
            tokio::select! {
                result = ams_server.start() => {
                    if let Err(e) = result {
                        tracing::error!("AMS/TCP server error: {}", e);
                    }
                }
                _ = shutdown_rx_ams.recv() => {
                    tracing::info!("AMS/TCP server shutdown");
                }
            }
        });

        // Start web UI server (if enabled)
        let web_handle = if self.settings.web.enabled {
            let subscriptions = Arc::new(SubscriptionManager::new(
                self.settings.web.max_subscriptions,
            ));
            let web_state = Arc::new(WebState {
                stats: self.stats.clone(),
                settings: self.settings.clone(),
                conn_manager: conn_manager.clone(),
                registry: registry.clone(),
                subscriptions,
            });

            let host = self.settings.web.host.clone();
            let port = self.settings.web.port;
            let mut shutdown_rx_web = shutdown_tx.subscribe();

            Some(tokio::spawn(async move {
                tokio::select! {
                    result = web::start_web_server(&host, port, web_state) => {
                        if let Err(e) = result {
                            tracing::error!("Web UI server error: {}", e);
                        }
                    }
                    _ = shutdown_rx_web.recv() => {
                        tracing::info!("Web UI server shutdown");
                    }
                }
            }))
        } else {
            tracing::info!("Web UI disabled");
            None
        };

        // Start dispatcher
        let dispatcher = self.log_dispatcher.clone();
        let stats = self.stats.clone();
        let dispatcher_handle = tokio::spawn(async move {
            let mut processed = 0u64;
            loop {
                tokio::select! {
                    Some(entry) = log_rx.recv() => {
                        stats.inc_logs_received();
                        if let Err(e) = dispatcher.dispatch(entry).await {
                            tracing::error!("Dispatch error: {}", e);
                        } else {
                            processed += 1;
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Dispatcher stopped. Processed: {}", processed);
                        break;
                    }
                }
            }
        });

        // Wait for Ctrl+C / SIGTERM
        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutdown signal received");

        let _ = shutdown_tx.send(());

        let shutdown_timeout = Duration::from_secs(self.settings.service.shutdown_timeout_secs);
        let _ = timeout(shutdown_timeout, async {
            let _ = tokio::join!(ams_handle, dispatcher_handle);
            if let Some(h) = web_handle {
                let _ = h.await;
            }
        })
        .await;

        tracing::info!("Log4TC Service stopped");
        Ok(())
    }
}

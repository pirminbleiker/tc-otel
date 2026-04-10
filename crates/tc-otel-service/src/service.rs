//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tc_otel_ads::{AmsNetId, AmsTcpServer, ConnectionConfig};
use tc_otel_core::AppSettings;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::config_watcher::ConfigWatcher;
use crate::dispatcher::LogDispatcher;

/// Main Log4TC Service
pub struct Log4TcService {
    settings: AppSettings,
    config_path: Option<PathBuf>,
    log_dispatcher: LogDispatcher,
}

impl Log4TcService {
    pub async fn new(settings: AppSettings) -> Result<Self> {
        let dispatcher = LogDispatcher::new(&settings).await?;
        Ok(Self {
            settings,
            config_path: None,
            log_dispatcher: dispatcher,
        })
    }

    /// Enable hot-reload by watching the given config file for changes
    pub fn with_config_watch(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Log4TC Service starting");

        let (log_tx, mut log_rx) = mpsc::channel(self.settings.service.channel_capacity);
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);

        // Start config watcher if a config path was provided
        let config_watcher_handle = if let Some(ref config_path) = self.config_path {
            let (watcher, _rx) = ConfigWatcher::new(
                config_path.clone(),
                self.settings.clone(),
                Duration::from_secs(2),
            );
            let mut shutdown_rx_watcher = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                tokio::select! {
                    _ = watcher.run() => {}
                    _ = shutdown_rx_watcher.recv() => {
                        tracing::info!("Config watcher shutdown");
                    }
                }
            });
            tracing::info!(
                "Config hot-reload enabled, watching {}",
                config_path.display()
            );
            Some(handle)
        } else {
            None
        };

        // Start AMS/TCP server (receives ADS from PLC via AMS routing)
        let net_id = AmsNetId::from_str(&self.settings.receiver.ams_net_id)
            .map_err(|e| anyhow::anyhow!("Invalid AMS Net ID: {}", e))?;

        let conn_config = ConnectionConfig {
            max_connections: self.settings.receiver.max_connections,
            idle_timeout_secs: self.settings.receiver.idle_timeout_secs,
            max_connections_per_ip: self.settings.receiver.max_connections_per_ip,
            rate_limit_per_sec_per_ip: self.settings.receiver.rate_limit_per_sec_per_ip,
            keepalive_interval_secs: self.settings.receiver.keepalive_interval_secs,
            send_buffer_size: self.settings.receiver.send_buffer_size,
            shutdown_timeout_secs: self.settings.service.shutdown_timeout_secs,
        };

        let ams_server = AmsTcpServer::new(
            self.settings.receiver.host.clone(),
            net_id,
            self.settings.receiver.ads_port,
            log_tx.clone(),
        )
        .with_connection_config(conn_config);

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

        // Start dispatcher
        let dispatcher = self.log_dispatcher.clone();
        let dispatcher_handle = tokio::spawn(async move {
            let mut processed = 0u64;
            loop {
                tokio::select! {
                    Some(entry) = log_rx.recv() => {
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
            if let Some(handle) = config_watcher_handle {
                let _ = handle.await;
            }
        })
        .await;

        tracing::info!("Log4TC Service stopped");
        Ok(())
    }
}

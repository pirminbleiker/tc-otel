//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::{AdsClient, AdsClientConfig, AmsNetId, AmsTcpServer, ConnectionConfig};
use tc_otel_core::AppSettings;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::api::{self, ApiState};
use crate::dispatcher::LogDispatcher;

/// Main Log4TC Service
pub struct Log4TcService {
    settings: AppSettings,
    log_dispatcher: LogDispatcher,
}

impl Log4TcService {
    pub async fn new(settings: AppSettings) -> Result<Self> {
        let dispatcher = LogDispatcher::new(&settings).await?;
        Ok(Self {
            settings,
            log_dispatcher: dispatcher,
        })
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Log4TC Service starting");

        let (log_tx, mut log_rx) = mpsc::channel(self.settings.service.channel_capacity);
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);

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

        // Start REST API server for symbol browsing
        // ADS client is optional — only created if target PLC is configured
        let ads_client = self.create_ads_client();
        let api_state = ApiState::new(ads_client);
        let api_router = api::symbol_router(api_state);

        let api_addr: SocketAddr = format!("{}:{}", self.settings.receiver.host, self.settings.receiver.http_port)
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid API listen address: {}", e))?;

        let mut shutdown_rx_api = shutdown_tx.subscribe();
        let api_listener = tokio::net::TcpListener::bind(api_addr).await?;
        tracing::info!("REST API listening on {}", api_addr);

        let api_handle = tokio::spawn(async move {
            axum::serve(api_listener, api_router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx_api.recv().await;
                })
                .await
                .ok();
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
            let _ = tokio::join!(ams_handle, dispatcher_handle, api_handle);
        })
        .await;

        tracing::info!("Log4TC Service stopped");
        Ok(())
    }

    /// Create an ADS client for symbol browsing if a target PLC is configured.
    ///
    /// Returns None if the AMS Net ID is the default wildcard (0.0.0.0.1.1),
    /// meaning no specific PLC target is configured.
    fn create_ads_client(&self) -> Option<Arc<AdsClient>> {
        let target_net_id_str = &self.settings.receiver.ams_net_id;

        // Default/wildcard means "listen only, don't connect out"
        if target_net_id_str == "0.0.0.0.1.1" {
            tracing::info!("No PLC target configured — symbol browsing disabled");
            return None;
        }

        let target_net_id = match AmsNetId::from_str(target_net_id_str) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Invalid target AMS Net ID '{}': {}", target_net_id_str, e);
                return None;
            }
        };

        // Derive TCP address from AMS Net ID (first 4 octets = IP)
        let net_id_bytes = target_net_id.bytes();
        let target_ip = format!(
            "{}.{}.{}.{}",
            net_id_bytes[0], net_id_bytes[1], net_id_bytes[2], net_id_bytes[3]
        );
        let target_addr: SocketAddr = format!("{}:{}", target_ip, self.settings.receiver.ams_tcp_port)
            .parse()
            .ok()?;

        // Source Net ID for the client (use the server's Net ID)
        let source_net_id = match AmsNetId::from_str(target_net_id_str) {
            Ok(id) => id,
            Err(_) => return None,
        };

        let config = AdsClientConfig::new(
            target_addr,
            source_net_id,
            target_net_id,
            self.settings.receiver.ads_port,
        );

        tracing::info!(
            target_addr = %target_addr,
            target_net_id = %target_net_id,
            "ADS client configured for symbol browsing"
        );

        Some(Arc::new(AdsClient::new(config)))
    }
}

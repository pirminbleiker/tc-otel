//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::{AmsNetId, AmsTcpServer, ConnectionConfig};
use tc_otel_core::AppSettings;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::config_watcher::ConfigWatcher;
use crate::cycle_time::CycleTimeTracker;
use crate::dispatcher::LogDispatcher;
use crate::web::{self, DiagnosticStats, SubscriptionManager, SymbolStore, WebState};

/// Main Log4TC Service
pub struct Log4TcService {
    settings: AppSettings,
    config_path: Option<PathBuf>,
}

impl Log4TcService {
    pub async fn new(settings: AppSettings) -> Result<Self> {
        Ok(Self {
            settings,
            config_path: None,
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

        // Start config watcher if a config path was provided, and get the
        // receiver for hot-reloading downstream components.
        let (config_watcher_handle, config_rx) = if let Some(ref config_path) = self.config_path {
            let (watcher, rx) = ConfigWatcher::new(
                config_path.clone(),
                self.settings.clone(),
                Duration::from_secs(2),
            );

            // Clone receiver for logging reload
            let logging_rx = rx.clone();

            let mut shutdown_rx_watcher = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                tokio::select! {
                    _ = watcher.run() => {}
                    _ = shutdown_rx_watcher.recv() => {
                        tracing::info!("Config watcher shutdown");
                    }
                }
            });

            // Spawn logging reload task
            let shutdown_rx_logging = shutdown_tx.subscribe();
            tokio::spawn(Self::logging_reload_task(logging_rx, shutdown_rx_logging));

            tracing::info!(
                "Config hot-reload enabled, watching {}",
                config_path.display()
            );
            (Some(handle), Some(rx))
        } else {
            (None, None)
        };

        // Create dispatcher with config watch receiver for hot-reload
        let log_dispatcher = LogDispatcher::new(&self.settings, config_rx).await?;

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

        // Extract shared state for the web UI before spawning AMS server
        let conn_manager = ams_server.connection_manager().clone();
        let task_registry = ams_server.task_registry().clone();
        let diagnostic_stats = Arc::new(DiagnosticStats::new());

        // Create cycle time tracker (shared between dispatcher and web UI)
        let cycle_tracker = Arc::new(CycleTimeTracker::new(
            self.settings.metrics.cycle_time_window,
        ));
        let cycle_time_enabled = self.settings.metrics.cycle_time_enabled;

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

        // Start dispatcher with diagnostic stats tracking
        let dispatcher = log_dispatcher.clone();
        let stats_for_dispatcher = diagnostic_stats.clone();
        let tracker_for_dispatcher = cycle_tracker.clone();
        let dispatcher_handle = tokio::spawn(async move {
            let mut processed = 0u64;
            loop {
                tokio::select! {
                    Some(entry) = log_rx.recv() => {
                        stats_for_dispatcher.logs_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        // Feed cycle time tracker before dispatch
                        if cycle_time_enabled && entry.task_cycle_counter > 0 {
                            tracker_for_dispatcher.record(
                                &entry.ams_net_id,
                                entry.task_index,
                                &entry.task_name,
                                entry.task_cycle_counter,
                                entry.plc_timestamp,
                            );
                        }

                        if let Err(e) = dispatcher.dispatch(entry).await {
                            tracing::error!("Dispatch error: {}", e);
                            stats_for_dispatcher.logs_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            processed += 1;
                            stats_for_dispatcher.logs_dispatched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Dispatcher stopped. Processed: {}", processed);
                        break;
                    }
                }
            }
        });

        // Start web UI server if enabled
        let web_handle = if self.settings.web.enabled {
            let web_state = WebState {
                stats: diagnostic_stats,
                conn_manager,
                task_registry,
                subscriptions: Arc::new(SubscriptionManager::new(
                    self.settings.web.max_subscriptions,
                )),
                symbols: Arc::new(SymbolStore::new()),
                cycle_tracker,
                service_name: self.settings.service.name.clone(),
            };
            let web_config = self.settings.web.clone();
            let shutdown_rx_web = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                if let Err(e) = web::start_web_server(&web_config, web_state, shutdown_rx_web).await
                {
                    tracing::error!("Web UI server error: {}", e);
                }
            });
            Some(handle)
        } else {
            tracing::info!("Web UI disabled");
            None
        };

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
            if let Some(handle) = web_handle {
                let _ = handle.await;
            }
        })
        .await;

        tracing::info!("Log4TC Service stopped");
        Ok(())
    }

    /// Task that watches for logging configuration changes and applies them.
    /// Updates the tracing EnvFilter when log_level changes.
    async fn logging_reload_task(
        mut config_rx: tokio::sync::watch::Receiver<AppSettings>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        let mut current_level = config_rx.borrow().logging.log_level.clone();

        loop {
            tokio::select! {
                result = config_rx.changed() => {
                    if result.is_err() {
                        break; // Channel closed
                    }
                    let new_settings = config_rx.borrow().clone();
                    if new_settings.logging.log_level != current_level {
                        tracing::info!(
                            "Hot-reload: log level changed from {} to {}",
                            current_level,
                            new_settings.logging.log_level,
                        );
                        current_level = new_settings.logging.log_level;
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("Logging reload task shutdown");
                    break;
                }
            }
        }
    }
}

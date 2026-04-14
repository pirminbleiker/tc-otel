//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::transport::{MqttAmsTransport, MqttTransportConfig};
use tc_otel_ads::{AmsNetId, AmsTransport, ConnectionConfig, ConnectionManager, TcpAmsTransport};
use tc_otel_core::config::TransportConfig;
use tc_otel_core::{AppSettings, MetricEntry};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::config_watcher::ConfigWatcher;
use crate::cycle_time::CycleTimeTracker;
use crate::dispatcher::{LogDispatcher, MetricDispatcher};
use crate::system_metrics::PlcSystemMetricsCollector;
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

        // Create log dispatcher with config watch receiver for hot-reload
        let log_dispatcher = LogDispatcher::new(&self.settings, config_rx.clone()).await?;

        // Create metric dispatcher and channel (if metrics export is enabled)
        let metrics_export_enabled = self.settings.metrics.export_enabled;
        let (metric_tx, metric_rx) = if metrics_export_enabled {
            let (tx, rx) = mpsc::channel::<MetricEntry>(self.settings.service.channel_capacity);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let metric_dispatcher = if metrics_export_enabled {
            let dispatcher = MetricDispatcher::new(&self.settings, config_rx).await?;
            Some(dispatcher)
        } else {
            tracing::info!("Metrics export disabled");
            None
        };

        // Start AMS transport (TCP or MQTT based on configuration)
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

        // Parse broker address (format: "host:port" or "host", default port 1883)
        let parse_broker_addr = |broker: &str| -> (String, u16) {
            let parts: Vec<&str> = broker.split(':').collect();
            let host = parts[0].to_string();
            let port = parts
                .get(1)
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(1883);
            (host, port)
        };

        // Dispatch based on transport configuration
        enum TransportVariant {
            Tcp(Arc<TcpAmsTransport>),
            Mqtt(Arc<MqttAmsTransport>),
        }

        let transport_variant = match &self.settings.receiver.transport {
            TransportConfig::Tcp(tcp_cfg) => {
                let mut tcp_transport = TcpAmsTransport::new(
                    tcp_cfg.host.clone(),
                    net_id,
                    self.settings.receiver.ads_port,
                    log_tx.clone(),
                )
                .with_connection_config(conn_config.clone());

                if let Some(ref m_tx) = metric_tx {
                    tcp_transport = tcp_transport.with_metric_sender(m_tx.clone());
                }

                tracing::info!("Using TCP transport on {}:{}", tcp_cfg.host, tcp_cfg.port);
                TransportVariant::Tcp(Arc::new(tcp_transport))
            }
            TransportConfig::Mqtt(mqtt_cfg) => {
                let (broker_host, broker_port) = parse_broker_addr(&mqtt_cfg.broker);

                let mqtt_transport_config = MqttTransportConfig {
                    broker_host: broker_host.clone(),
                    broker_port,
                    client_id: mqtt_cfg.client_id.clone(),
                    topic_prefix: mqtt_cfg.topic_prefix.clone(),
                    local_net_id: net_id,
                    ads_port: self.settings.receiver.ads_port,
                    username: mqtt_cfg.username.clone(),
                    password: mqtt_cfg.password.clone(),
                    tls: mqtt_cfg.tls.clone(),
                };

                let mut mqtt_transport =
                    MqttAmsTransport::new(mqtt_transport_config, log_tx.clone());

                if let Some(ref m_tx) = metric_tx {
                    mqtt_transport = mqtt_transport.with_metric_sender(m_tx.clone());
                }

                tracing::info!(
                    "Using MQTT transport: broker={}:{}, client_id={}, topic_prefix={}",
                    broker_host,
                    broker_port,
                    mqtt_cfg.client_id,
                    mqtt_cfg.topic_prefix
                );
                TransportVariant::Mqtt(Arc::new(mqtt_transport))
            }
        };

        // Extract connection manager and task registry based on transport type
        let (conn_manager, task_registry) = match &transport_variant {
            TransportVariant::Tcp(tcp) => (
                tcp.connection_manager().clone(),
                tcp.task_registry().clone(),
            ),
            TransportVariant::Mqtt(mqtt) => (
                Arc::new(ConnectionManager::new(conn_config.clone())),
                mqtt.task_registry().clone(),
            ),
        };

        let diagnostic_stats = Arc::new(DiagnosticStats::new());

        // Create cycle time tracker (shared between dispatcher and web UI)
        let cycle_tracker = Arc::new(CycleTimeTracker::new(
            self.settings.metrics.cycle_time_window,
        ));
        let cycle_time_enabled = self.settings.metrics.cycle_time_enabled;

        let mut shutdown_rx_ams = shutdown_tx.subscribe();
        let ams_handle = match transport_variant {
            TransportVariant::Tcp(tcp) => tokio::spawn(async move {
                tokio::select! {
                    result = {
                        let transport: Arc<dyn AmsTransport> = tcp.clone();
                        AmsTransport::run(transport.clone())
                    } => {
                        if let Err(e) = result {
                            tracing::error!("AMS/TCP transport error: {}", e);
                        }
                    }
                    _ = shutdown_rx_ams.recv() => {
                        tracing::info!("AMS transport shutdown");
                    }
                }
            }),
            TransportVariant::Mqtt(mqtt) => tokio::spawn(async move {
                tokio::select! {
                    result = {
                        let transport: Arc<dyn AmsTransport> = mqtt.clone();
                        AmsTransport::run(transport.clone())
                    } => {
                        if let Err(e) = result {
                            tracing::error!("AMS/MQTT transport error: {}", e);
                        }
                    }
                    _ = shutdown_rx_ams.recv() => {
                        tracing::info!("AMS transport shutdown");
                    }
                }
            }),
        };

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

        // Start metric dispatcher loop (if enabled)
        let metric_dispatcher_handle = if let (Some(mut m_rx), Some(m_dispatcher)) =
            (metric_rx, metric_dispatcher)
        {
            let mut shutdown_rx_metrics = shutdown_tx.subscribe();
            let stats_for_metrics = diagnostic_stats.clone();
            Some(tokio::spawn(async move {
                let mut processed = 0u64;
                loop {
                    tokio::select! {
                        Some(entry) = m_rx.recv() => {
                            stats_for_metrics.logs_received.fetch_add(0, std::sync::atomic::Ordering::Relaxed);
                            if let Err(e) = m_dispatcher.dispatch(entry).await {
                                tracing::error!("Metric dispatch error: {}", e);
                            } else {
                                processed += 1;
                            }
                        }
                        _ = shutdown_rx_metrics.recv() => {
                            tracing::info!("Metric dispatcher stopped. Processed: {}", processed);
                            break;
                        }
                    }
                }
            }))
        } else {
            None
        };

        // Start periodic PLC system metrics collector (if metrics export enabled)
        let system_metrics_handle = if metrics_export_enabled {
            if let Some(ref m_tx) = metric_tx {
                let collector = PlcSystemMetricsCollector::new(
                    cycle_tracker.clone(),
                    self.settings.service.name.clone(),
                );
                let m_tx = m_tx.clone();
                let mut shutdown_rx_sys = shutdown_tx.subscribe();
                let collection_interval =
                    Duration::from_millis(self.settings.metrics.export_flush_interval_ms);
                Some(tokio::spawn(async move {
                    let mut interval = tokio::time::interval(collection_interval);
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                for entry in collector.collect() {
                                    let _ = m_tx.send(entry).await;
                                }
                            }
                            _ = shutdown_rx_sys.recv() => {
                                tracing::info!("System metrics collector stopped");
                                break;
                            }
                        }
                    }
                }))
            } else {
                None
            }
        } else {
            None
        };

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
            if let Some(handle) = metric_dispatcher_handle {
                let _ = handle.await;
            }
            if let Some(handle) = system_metrics_handle {
                let _ = handle.await;
            }
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

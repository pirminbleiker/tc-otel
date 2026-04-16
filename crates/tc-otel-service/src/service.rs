//! Main service orchestration with graceful shutdown and backpressure handling

use anyhow::Result;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::router::AdsRouter;
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

        // Create the AdsRouter with channels and registry
        let task_registry = Arc::new(tc_otel_ads::registry::TaskRegistry::new());
        let (push_tx, mut push_rx) =
            mpsc::channel::<(tc_otel_ads::AmsNetId, tc_otel_ads::diagnostics::DiagEvent)>(256);
        let ads_router = Arc::new(
            AdsRouter::new(
                self.settings.receiver.ads_port,
                log_tx.clone(),
                metric_tx.clone(),
                task_registry.clone(),
            )
            .with_push_sender(push_tx),
        );

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
                let tcp_transport =
                    TcpAmsTransport::new(tcp_cfg.host.clone(), net_id, ads_router.clone())
                        .with_port(tcp_cfg.port)
                        .with_connection_config(conn_config.clone());

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
                    username: mqtt_cfg.username.clone(),
                    password: mqtt_cfg.password.clone(),
                    tls: mqtt_cfg.tls.clone(),
                };

                let mqtt_transport =
                    MqttAmsTransport::new(mqtt_transport_config, ads_router.clone());

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

        // Extract connection manager based on transport type
        let conn_manager = match &transport_variant {
            TransportVariant::Tcp(tcp) => tcp.connection_manager().clone(),
            TransportVariant::Mqtt(_mqtt) => Arc::new(ConnectionManager::new(conn_config.clone())),
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

        // Spawn push-diagnostic drain task
        let mut shutdown_rx_push = shutdown_tx.subscribe();
        let push_drain_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((_net_id, ev)) = push_rx.recv() => {
                        tracing::debug!("push-diagnostic event: {:?}", ev);
                        // Unit 4 will replace this body with actual metric dispatch logic
                    }
                    _ = shutdown_rx_push.recv() => {
                        tracing::debug!("Push-diagnostic drain shutdown");
                        break;
                    }
                }
            }
        });

        // Optional self-polling diagnostics collector — only runs for MQTT
        // transport (needs the same broker) and when explicitly enabled.
        // Metrics bridge is future work; for now events are drained and logged.
        if self.settings.diagnostics.enabled && !self.settings.diagnostics.targets.is_empty() {
            if let TransportConfig::Mqtt(ref mqtt_cfg) = self.settings.receiver.transport {
                let (broker_host, broker_port) = parse_broker_addr(&mqtt_cfg.broker);
                let poller_config = build_poller_config(
                    broker_host,
                    broker_port,
                    mqtt_cfg,
                    net_id,
                    &self.settings.diagnostics.targets,
                );
                match poller_config {
                    Ok(cfg) => {
                        let (diag_tx, mut diag_rx) = mpsc::channel::<(
                            tc_otel_ads::AmsNetId,
                            tc_otel_ads::diagnostics::DiagEvent,
                        )>(256);
                        let poller = Arc::new(
                            tc_otel_ads::diagnostics_poller::DiagnosticsPoller::new(cfg, diag_tx),
                        );
                        let task_names = poller.task_names();
                        let push_seen_map = poller.push_seen();
                        let mut shutdown_rx_poller = shutdown_tx.subscribe();
                        tokio::spawn(async move {
                            tokio::select! {
                                result = poller.clone().run() => {
                                    if let Err(e) = result {
                                        tracing::error!("Diagnostics poller error: {}", e);
                                    }
                                }
                                _ = shutdown_rx_poller.recv() => {
                                    tracing::info!("Diagnostics poller shutdown");
                                }
                            }
                        });
                        // Bridge to the existing metric pipeline — each
                        // DiagEvent fans out to one or more MetricEntry items.
                        let bridge_metric_tx = metric_tx.clone();
                        tokio::spawn(async move {
                            while let Some((net_id, ev)) = diag_rx.recv().await {
                                push_seen_map
                                    .write()
                                    .await
                                    .insert(net_id, std::time::Instant::now());
                                let names = task_names.read().await.clone();
                                let metrics = crate::diagnostics_bridge::diag_event_to_metrics(
                                    net_id, ev, &names,
                                );
                                if let Some(ref tx) = bridge_metric_tx {
                                    for m in metrics {
                                        if tx.try_send(m).is_err() {
                                            tracing::debug!(
                                                "diagnostics bridge: metric channel full, dropping"
                                            );
                                        }
                                    }
                                }
                            }
                        });
                        tracing::info!(
                            "Diagnostics self-poller started ({} targets)",
                            self.settings.diagnostics.targets.len()
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Diagnostics self-poller disabled: {}", e);
                    }
                }
            } else {
                tracing::warn!("Diagnostics self-poller requires MQTT transport; skipping");
            }
        }

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
            let _ = tokio::join!(ams_handle, dispatcher_handle, push_drain_handle);
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

/// Translate `DiagnosticsTargetConfig` (strings, u64 ms) into the
/// `diagnostics_poller` runtime types (parsed net IDs, `Duration`).
fn build_poller_config(
    broker_host: String,
    broker_port: u16,
    mqtt_cfg: &tc_otel_core::config::MqttTransportConfig,
    local_net_id: AmsNetId,
    targets: &[tc_otel_core::DiagnosticsTargetConfig],
) -> Result<tc_otel_ads::diagnostics_poller::PollerConfig> {
    use tc_otel_ads::diagnostics_poller::{PollerConfig, TargetConfig};

    let mut parsed_targets = Vec::with_capacity(targets.len());
    for t in targets {
        let net_id = AmsNetId::from_str(&t.ams_net_id)
            .map_err(|e| anyhow::anyhow!("invalid ams_net_id '{}': {}", t.ams_net_id, e))?;
        // Parse the string-keyed `task_names` map from config into
        // `HashMap<u16, String>`, silently skipping entries with a
        // non-numeric port.
        let mut task_names = std::collections::HashMap::new();
        for (port_str, name) in &t.task_names {
            if let Ok(port) = port_str.parse::<u16>() {
                task_names.insert(port, name.clone());
            } else {
                tracing::warn!(
                    "diagnostics: ignoring task_names entry with non-numeric port '{}'",
                    port_str
                );
            }
        }
        parsed_targets.push(TargetConfig {
            net_id,
            poll_interval: Duration::from_millis(t.poll_interval_ms),
            exceed_counter: t.exceed_counter,
            rt_usage: t.rt_usage,
            task_ports: t.task_ports.clone(),
            rt_port: t.rt_port,
            task_names,
        });
    }

    Ok(PollerConfig {
        broker_host,
        broker_port,
        // Use a distinct client-id so we can run alongside the main MQTT
        // transport without session conflicts on the broker.
        client_id: format!("{}-diag", mqtt_cfg.client_id),
        topic_prefix: mqtt_cfg.topic_prefix.clone(),
        local_net_id,
        targets: parsed_targets,
    })
}

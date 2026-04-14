//! MQTT transport for AMS frames
//!
//! Implements ADS-over-MQTT transport, receiving AMS frames from MQTT topics
//! and publishing responses back to the broker.
//!
//! Topic structure:
//! - Subscribe: `{prefix}/{local_net_id}/ams` — receive AMS command frames
//! - Publish: `{prefix}/{dest_net_id}/ams/res` — send AMS response frames

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, TlsConfiguration, Transport};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::AmsTransport;
use crate::ams::{AmsNetId, ADS_CMD_WRITE};
use crate::parser::AdsParser;
use crate::protocol::RegistrationKey;
use crate::registry::TaskRegistry;
use crate::{AdsError, AmsHeader};
use tc_otel_core::config::MqttTlsConfig;
use tc_otel_core::{LogEntry, MetricEntry};

/// Error type for certificate loading failures
#[derive(Debug, Error)]
pub enum CertificateError {
    #[error("Failed to read certificate file {path}: {source}")]
    ReadError {
        path: String,
        source: std::io::Error,
    },
    #[error("Invalid certificate format: {0}")]
    InvalidFormat(String),
}

/// MQTT AMS transport configuration (internal wrapper)
#[derive(Debug, Clone)]
pub struct MqttTransportConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    pub topic_prefix: String,
    pub local_net_id: AmsNetId,
    pub ads_port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls: Option<MqttTlsConfig>,
}

/// Helper function to load certificate bytes from file
fn load_cert_bytes(path: &PathBuf) -> Result<Vec<u8>, CertificateError> {
    fs::read(path).map_err(|e| CertificateError::ReadError {
        path: path.to_string_lossy().into_owned(),
        source: e,
    })
}

/// Configure TLS transport for MQTT options
fn apply_tls_config(
    mqtt_options: &mut MqttOptions,
    tls_config: &MqttTlsConfig,
) -> Result<(), CertificateError> {
    let ca_cert = load_cert_bytes(&tls_config.ca_cert_path)?;

    // Build TLS configuration based on whether client certs are provided
    let tls = if let (Some(cert_path), Some(key_path)) =
        (&tls_config.client_cert_path, &tls_config.client_key_path)
    {
        let client_cert = load_cert_bytes(cert_path)?;
        let client_key = load_cert_bytes(key_path)?;

        TlsConfiguration::Simple {
            ca: ca_cert,
            alpn: None,
            client_auth: Some((client_cert, client_key)),
        }
    } else {
        TlsConfiguration::Simple {
            ca: ca_cert,
            alpn: None,
            client_auth: None,
        }
    };

    mqtt_options.set_transport(Transport::Tls(tls));
    Ok(())
}

/// MQTT AMS transport
pub struct MqttAmsTransport {
    config: MqttTransportConfig,
    log_tx: mpsc::Sender<LogEntry>,
    metric_tx: Option<mpsc::Sender<MetricEntry>>,
    registry: Arc<TaskRegistry>,
}

impl MqttAmsTransport {
    pub fn new(config: MqttTransportConfig, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self {
            config,
            log_tx,
            metric_tx: None,
            registry: Arc::new(TaskRegistry::new()),
        }
    }

    pub fn with_metric_sender(mut self, metric_tx: mpsc::Sender<MetricEntry>) -> Self {
        self.metric_tx = Some(metric_tx);
        self
    }

    pub fn with_registry(mut self, registry: Arc<TaskRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Get a reference to the task registry
    pub fn task_registry(&self) -> &Arc<TaskRegistry> {
        &self.registry
    }

    async fn handle_frame(
        data: &[u8],
        ads_port: u16,
        log_tx: &mpsc::Sender<LogEntry>,
        metric_tx: &Option<mpsc::Sender<MetricEntry>>,
        registry: &Arc<TaskRegistry>,
    ) -> crate::Result<()> {
        if data.len() < 32 {
            return Err(AdsError::ParseError("AMS header too short".into()));
        }

        let header = AmsHeader::parse(data)?;
        let source_net_id = header.source_net_id.to_string();
        let source_port = header.source_port;

        tracing::trace!(
            "AMS frame from MQTT: cmd={} src={}:{} -> dst={}:{}",
            header.command_id,
            source_net_id,
            source_port,
            header.target_net_id.to_string(),
            header.target_port,
        );

        // Only process WRITE commands containing log/metric data
        if header.command_id != ADS_CMD_WRITE {
            return Ok(());
        }

        let payload = &data[32..];

        // Parse ADS Write request: IndexGroup(4) + IndexOffset(4) + WriteLength(4) + Data
        if payload.len() < 12 {
            return Err(AdsError::ParseError("Write request too short".into()));
        }

        let write_length =
            u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
        if payload.len() < 12 + write_length {
            return Err(AdsError::ParseError("Write data incomplete".to_string()));
        }

        let write_data = &payload[12..12 + write_length];

        // Only parse if targeting our ADS port
        if header.target_port == ads_port {
            match AdsParser::parse_all(write_data) {
                Ok(parse_result) => {
                    // Process registrations first
                    for registration in parse_result.registrations {
                        let reg_key = RegistrationKey {
                            ams_net_id: source_net_id.clone(),
                            ams_source_port: source_port,
                            task_index: registration.task_index,
                        };
                        let metadata = crate::protocol::TaskMetadata {
                            task_name: registration.task_name.clone(),
                            app_name: registration.app_name,
                            project_name: registration.project_name,
                            online_change_count: registration.online_change_count,
                        };
                        tracing::debug!(
                            "Registered task {}: {}",
                            registration.task_index,
                            metadata.task_name
                        );
                        registry.register(reg_key, metadata);
                    }

                    // Process log entries
                    for mut ads_entry in parse_result.entries {
                        // Enrich v2 entries with registry metadata
                        if ads_entry.version == crate::protocol::AdsProtocolVersion::V2 {
                            let reg_key = RegistrationKey {
                                ams_net_id: source_net_id.clone(),
                                ams_source_port: source_port,
                                task_index: ads_entry.task_index as u8,
                            };
                            if let Some(metadata) = registry.lookup(&reg_key) {
                                ads_entry.task_name = metadata.task_name;
                                ads_entry.app_name = metadata.app_name;
                                ads_entry.project_name = metadata.project_name;
                                ads_entry.online_change_count = metadata.online_change_count;
                            }
                        }

                        let source = "mqtt".to_string();
                        let hostname = format!("plc-{}", source_net_id);

                        let mut log_entry = LogEntry::new(
                            source,
                            hostname,
                            ads_entry.message,
                            ads_entry.logger,
                            ads_entry.level,
                        );

                        log_entry.plc_timestamp = ads_entry.plc_timestamp;
                        log_entry.clock_timestamp = ads_entry.clock_timestamp;
                        log_entry.task_index = ads_entry.task_index;
                        log_entry.task_name = ads_entry.task_name;
                        log_entry.task_cycle_counter = ads_entry.task_cycle_counter;
                        log_entry.app_name = ads_entry.app_name;
                        log_entry.project_name = ads_entry.project_name;
                        log_entry.online_change_count = ads_entry.online_change_count;
                        log_entry.arguments = ads_entry.arguments;
                        log_entry.context = ads_entry.context;
                        log_entry.ams_net_id = source_net_id.clone();
                        log_entry.ams_source_port = source_port;
                        log_entry.trace_id = ads_entry.trace_id;
                        log_entry.span_id = ads_entry.span_id;

                        let _ = log_tx.send(log_entry).await;
                    }

                    // Process metrics (if a metric channel is configured)
                    if let Some(m_tx) = metric_tx {
                        for ads_metric in parse_result.metrics {
                            let source = "mqtt".to_string();
                            let hostname = format!("plc-{}", source_net_id);

                            // Enrich with registry metadata
                            let reg_key = RegistrationKey {
                                ams_net_id: source_net_id.clone(),
                                ams_source_port: source_port,
                                task_index: ads_metric.task_index as u8,
                            };
                            let (task_name, app_name, project_name) =
                                if let Some(metadata) = registry.lookup(&reg_key) {
                                    (metadata.task_name, metadata.app_name, metadata.project_name)
                                } else {
                                    (String::new(), String::new(), String::new())
                                };

                            let metric_entry = MetricEntry {
                                name: ads_metric.name,
                                description: ads_metric.description,
                                unit: ads_metric.unit,
                                kind: ads_metric.kind,
                                value: ads_metric.value,
                                timestamp: ads_metric.timestamp,
                                source,
                                hostname,
                                ams_net_id: source_net_id.clone(),
                                ams_source_port: source_port,
                                task_index: ads_metric.task_index,
                                task_name,
                                task_cycle_counter: ads_metric.task_cycle_counter,
                                app_name,
                                project_name,
                                attributes: ads_metric.attributes,
                                histogram_bounds: ads_metric.histogram_bounds,
                                histogram_counts: ads_metric.histogram_counts,
                                histogram_count: ads_metric.histogram_count,
                                histogram_sum: ads_metric.histogram_sum,
                                is_monotonic: ads_metric.is_monotonic,
                            };

                            let _ = m_tx.send(metric_entry).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse log entries from MQTT: {} (raw {} bytes)",
                        e,
                        write_data.len()
                    );
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl AmsTransport for MqttAmsTransport {
    async fn run(self: Arc<Self>) -> crate::Result<()> {
        let mut mqtt_options = MqttOptions::new(
            &self.config.client_id,
            &self.config.broker_host,
            self.config.broker_port,
        );

        // Set keep-alive interval
        mqtt_options.set_keep_alive(Duration::from_secs(60));

        // Set credentials if provided
        if let (Some(username), Some(password)) = (&self.config.username, &self.config.password) {
            mqtt_options.set_credentials(username.clone(), password.clone());
        }

        // Apply TLS configuration if provided
        if let Some(tls_config) = &self.config.tls {
            apply_tls_config(&mut mqtt_options, tls_config)
                .map_err(|e| AdsError::BufferError(format!("TLS configuration error: {}", e)))?;
        }

        let (client, mut event_loop) = AsyncClient::new(mqtt_options, 10);

        let subscribe_topic = format!(
            "{}/{}/ams",
            self.config.topic_prefix, self.config.local_net_id
        );

        tracing::info!(
            "Connecting to MQTT broker at {}:{}",
            self.config.broker_host,
            self.config.broker_port
        );

        // Subscribe to the AMS topic
        client
            .subscribe(&subscribe_topic, QoS::AtMostOnce)
            .await
            .map_err(|e| AdsError::BufferError(format!("Failed to subscribe to MQTT: {}", e)))?;

        tracing::info!(
            "MQTT: subscribed to topic '{}' (local Net ID: {})",
            subscribe_topic,
            self.config.local_net_id
        );

        // Event loop
        loop {
            let notification = event_loop.poll().await;

            match notification {
                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                    let payload = publish.payload.to_vec();
                    tracing::trace!(
                        "MQTT received {} bytes on topic '{}'",
                        payload.len(),
                        publish.topic
                    );

                    // Parse and process the AMS frame
                    if let Err(e) = Self::handle_frame(
                        &payload,
                        self.config.ads_port,
                        &self.log_tx,
                        &self.metric_tx,
                        &self.registry,
                    )
                    .await
                    {
                        tracing::warn!("Error processing MQTT AMS frame: {}", e);
                    }
                }
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    tracing::info!("MQTT connected and ready");
                }
                Ok(Event::Incoming(Incoming::SubAck(_))) => {
                    tracing::debug!("MQTT subscription acknowledged");
                }
                Ok(Event::Outgoing(_)) => {
                    // Outgoing events - ignore
                }
                Ok(Event::Incoming(incoming)) => {
                    tracing::trace!("MQTT event: {:?}", incoming);
                }
                Err(e) => {
                    tracing::error!("MQTT error: {}", e);
                    return Err(AdsError::BufferError(format!("MQTT error: {}", e)));
                }
            }
        }
    }

    async fn send(&self, dest: AmsNetId, frame: Vec<u8>) -> crate::Result<()> {
        // MQTT implementation would publish to the response topic
        let response_topic = format!("{}/{}/ams/res", self.config.topic_prefix, dest);

        // For now, we just log that we would send it
        // A full implementation would need the AsyncClient to be available here
        tracing::trace!(
            "MQTT: would send {} bytes to topic '{}'",
            frame.len(),
            response_topic
        );

        Ok(())
    }

    fn local_net_id(&self) -> AmsNetId {
        self.config.local_net_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mqtt_fixture_parse() {
        let fixture_data = include_bytes!("../../tests/fixtures/mqtt_ams_frame.bin");
        assert!(fixture_data.len() >= 32, "Fixture too small");
        let header = AmsHeader::parse(fixture_data).expect("Failed to parse AMS header");
        assert_eq!(
            header.target_net_id.to_string(),
            "15.15.15.15.1.1",
            "target_net_id mismatch"
        );
        assert_eq!(
            header.source_net_id.to_string(),
            "172.18.164.255.1.1",
            "source_net_id mismatch"
        );
        assert_eq!(header.command_id, 2, "command_id should be READ (2)");
        assert_eq!(header.data_length, 12, "data_length should be 12");
    }

    #[test]
    fn test_mqtt_transport_config_default() {
        let config = MqttTransportConfig {
            broker_host: "localhost".to_string(),
            broker_port: 1883,
            client_id: "tc-otel-test".to_string(),
            topic_prefix: "AdsOverMqtt".to_string(),
            local_net_id: "1.1.1.1.1.1".parse().unwrap(),
            ads_port: 16150,
            username: None,
            password: None,
            tls: None,
        };
        assert_eq!(config.broker_host, "localhost");
        assert_eq!(config.broker_port, 1883);
        assert!(config.username.is_none());
        assert!(config.tls.is_none());
    }

    #[test]
    fn test_mqtt_transport_config_with_credentials() {
        let config = MqttTransportConfig {
            broker_host: "broker.example.com".to_string(),
            broker_port: 8883,
            client_id: "tc-otel-prod".to_string(),
            topic_prefix: "AdsOverMqtt".to_string(),
            local_net_id: "192.168.1.100.1.1".parse().unwrap(),
            ads_port: 16150,
            username: Some("mqtt_user".to_string()),
            password: Some("mqtt_pass".to_string()),
            tls: None,
        };
        assert_eq!(config.username, Some("mqtt_user".to_string()));
        assert_eq!(config.password, Some("mqtt_pass".to_string()));
        assert!(config.tls.is_none());
    }

    #[test]
    fn test_mqtt_tls_config_ca_only() {
        let tls_config = MqttTlsConfig {
            ca_cert_path: PathBuf::from("/etc/ssl/certs/ca.pem"),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        assert_eq!(
            tls_config.ca_cert_path,
            PathBuf::from("/etc/ssl/certs/ca.pem")
        );
        assert!(tls_config.client_cert_path.is_none());
        assert!(!tls_config.insecure_skip_verify);
    }

    #[test]
    fn test_mqtt_tls_config_with_client_certs() {
        let tls_config = MqttTlsConfig {
            ca_cert_path: PathBuf::from("/etc/ssl/certs/ca.pem"),
            client_cert_path: Some(PathBuf::from("/etc/ssl/certs/client.pem")),
            client_key_path: Some(PathBuf::from("/etc/ssl/private/client.key")),
            insecure_skip_verify: false,
        };
        assert!(tls_config.client_cert_path.is_some());
        assert!(tls_config.client_key_path.is_some());
    }

    #[test]
    fn test_mqtt_transport_config_with_tls() {
        let tls_config = MqttTlsConfig {
            ca_cert_path: PathBuf::from("/etc/ssl/certs/ca.pem"),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let config = MqttTransportConfig {
            broker_host: "broker.example.com".to_string(),
            broker_port: 8883,
            client_id: "tc-otel-tls".to_string(),
            topic_prefix: "AdsOverMqtt".to_string(),
            local_net_id: "192.168.1.100.1.1".parse().unwrap(),
            ads_port: 16150,
            username: Some("mqtt_user".to_string()),
            password: Some("mqtt_pass".to_string()),
            tls: Some(tls_config),
        };
        assert!(config.tls.is_some());
        assert_eq!(config.broker_port, 8883);
    }

    #[test]
    fn test_mqtt_options_with_credentials() {
        let mut options = MqttOptions::new("test-client", "localhost", 1883);
        options.set_credentials("testuser", "testpass");
        assert_eq!(options.client_id(), "test-client");
    }

    #[test]
    fn test_mqtt_options_with_tls_config_error_handling() {
        let mut options = MqttOptions::new("test-client", "localhost", 8883);
        let nonexistent_tls = MqttTlsConfig {
            ca_cert_path: PathBuf::from("/nonexistent/ca.pem"),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = apply_tls_config(&mut options, &nonexistent_tls);
        assert!(result.is_err());
        match result {
            Err(CertificateError::ReadError { path, .. }) => {
                assert!(path.contains("nonexistent"));
            }
            _ => panic!("Expected ReadError"),
        }
    }

    #[test]
    fn test_certificate_error_display() {
        let err = CertificateError::ReadError {
            path: "/etc/ssl/certs/ca.pem".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("Failed to read certificate file"));
        assert!(msg.contains("/etc/ssl/certs/ca.pem"));
    }
}

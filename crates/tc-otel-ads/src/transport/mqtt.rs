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
use tokio::time::Duration;

use super::AmsTransport;
use crate::ams::AmsNetId;
use crate::router::AdsRouter;
use crate::AdsError;
use tc_otel_core::config::MqttTlsConfig;

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
    router: Arc<AdsRouter>,
}

impl MqttAmsTransport {
    pub fn new(config: MqttTransportConfig, router: Arc<AdsRouter>) -> Self {
        Self { config, router }
    }

    /// Get a reference to the task registry (via router)
    pub fn task_registry(&self) -> &Arc<crate::registry::TaskRegistry> {
        self.router.registry()
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

        // Raise packet-size limit — AMS WRITE frames with batched log entries
        // can exceed rumqttc's 10KB default.
        mqtt_options.set_max_packet_size(16 * 1024 * 1024, 16 * 1024 * 1024);

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

                    // Dispatch through router
                    match self.router.dispatch(&payload).await {
                        Ok(Some(response)) => {
                            // Extract source Net ID from request to build response topic
                            if payload.len() >= 14 {
                                let src_net_id = AmsNetId::from_bytes([
                                    payload[8],
                                    payload[9],
                                    payload[10],
                                    payload[11],
                                    payload[12],
                                    payload[13],
                                ]);
                                let response_topic =
                                    format!("{}/{}/ams/res", self.config.topic_prefix, src_net_id);
                                if let Err(e) = client
                                    .publish(&response_topic, QoS::AtMostOnce, false, response)
                                    .await
                                {
                                    tracing::warn!(
                                        "Failed to publish response to {}: {}",
                                        response_topic,
                                        e
                                    );
                                }
                            }
                        }
                        Ok(None) => {
                            // No response needed
                        }
                        Err(e) => {
                            tracing::warn!("Error dispatching MQTT AMS frame: {}", e);
                        }
                    }
                }
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    tracing::info!("MQTT connected and ready");
                    // Publish info message on ConnAck
                    // MQTT forbids wildcards in publish topics — use own NetId.
                    let info_topic = format!(
                        "{}/{}/info",
                        self.config.topic_prefix, self.config.local_net_id
                    );
                    // Beckhoff ADS-over-MQTT /info XML: TwinCAT routers parse
                    // this to populate their route table. Plain-text payload
                    // is ignored → remote NetId never routable → adsErrId=6.
                    let info_msg = "<info><online name=\"tc-otel\" \
                        unidirectional=\"true\" osVersion=\"0.0.0\" \
                        osPlatform=\"0\">true</online></info>"
                        .to_string();
                    let _ = client
                        .publish(&info_topic, QoS::AtMostOnce, true, info_msg)
                        .await;
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

    async fn send(&self, _dest: AmsNetId, _frame: Vec<u8>) -> crate::Result<()> {
        // Responses are sent inline during event processing
        // This method is for completeness of the AmsTransport trait
        Ok(())
    }

    fn local_net_id(&self) -> AmsNetId {
        self.config.local_net_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AmsHeader;

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

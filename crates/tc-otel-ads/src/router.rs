//! ADS Router - transport-agnostic ADS protocol dispatch
//!
//! Handles ADS command dispatch and response building, independent of the
//! underlying transport (TCP, MQTT, etc).

use crate::ams::{
    AdsWriteRequest, AmsHeader, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO,
    ADS_CMD_READ_STATE, ADS_CMD_WRITE,
};
use crate::parser::AdsParser;
use crate::protocol::RegistrationKey;
use crate::registry::TaskRegistry;
use std::sync::Arc;
use tc_otel_core::{LogEntry, MetricEntry};
use tokio::sync::mpsc;

/// ADS Router - handles command dispatch and response building
pub struct AdsRouter {
    ads_port: u16,
    log_tx: mpsc::Sender<LogEntry>,
    metric_tx: Option<mpsc::Sender<MetricEntry>>,
    registry: Arc<TaskRegistry>,
}

impl AdsRouter {
    pub fn new(
        ads_port: u16,
        log_tx: mpsc::Sender<LogEntry>,
        metric_tx: Option<mpsc::Sender<MetricEntry>>,
        registry: Arc<TaskRegistry>,
    ) -> Self {
        Self {
            ads_port,
            log_tx,
            metric_tx,
            registry,
        }
    }

    /// Process an inbound AMS frame (32-byte header + ADS payload).
    pub async fn dispatch(&self, frame: &[u8]) -> crate::Result<Option<Vec<u8>>> {
        if frame.len() < 32 {
            return Err(crate::AdsError::ParseError("AMS frame too short".into()));
        }

        let header = AmsHeader::parse(frame)?;
        let source_net_id = header.source_net_id.to_string();
        let source_port = header.source_port;

        let response = match header.command_id {
            ADS_CMD_READ_STATE => {
                let payload = vec![0, 0, 0, 0, 5, 0, 0, 0];
                Some(Self::build_response(&header, payload))
            }
            ADS_CMD_READ_DEVICE_INFO => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.push(0);
                payload.push(1);
                payload.extend_from_slice(&1u16.to_le_bytes());
                let mut name = [0u8; 16];
                let src = b"log4tc";
                name[..src.len()].copy_from_slice(src);
                payload.extend_from_slice(&name);
                Some(Self::build_response(&header, payload))
            }
            ADS_CMD_READ => {
                let read_len = if frame.len() >= 44 {
                    u32::from_le_bytes([frame[40], frame[41], frame[42], frame[43]])
                } else {
                    0
                };
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.extend_from_slice(&read_len.to_le_bytes());
                payload.resize(payload.len() + read_len as usize, 0);
                Some(Self::build_response(&header, payload))
            }
            ADS_CMD_WRITE => {
                let payload = &frame[32..];
                if let Ok(write_req) = AdsWriteRequest::parse(payload) {
                    if header.target_port == self.ads_port {
                        if let Err(e) = self
                            .dispatch_write(&source_net_id, source_port, &write_req.data)
                            .await
                        {
                            tracing::warn!("Failed to dispatch write: {}", e);
                        }
                    }
                }
                let payload = vec![0, 0, 0, 0];
                Some(Self::build_response(&header, payload))
            }
            _ => {
                tracing::debug!(
                    "Unknown ADS cmd={} from {}:{}",
                    header.command_id,
                    source_net_id,
                    source_port
                );
                let payload = vec![0, 0, 0, 0];
                Some(Self::build_response(&header, payload))
            }
        };

        Ok(response)
    }

    fn build_response(header: &AmsHeader, payload: Vec<u8>) -> Vec<u8> {
        let mut response_header = header.make_response(0);
        response_header.data_length = payload.len() as u32;
        response_header.state_flags = 0x0005;
        let mut response = response_header.serialize();
        response.extend_from_slice(&payload);
        response
    }

    async fn dispatch_write(
        &self,
        source_net_id: &str,
        source_port: u16,
        write_data: &[u8],
    ) -> crate::Result<()> {
        match AdsParser::parse_all(write_data) {
            Ok(parse_result) => {
                for registration in parse_result.registrations {
                    let reg_key = RegistrationKey {
                        ams_net_id: source_net_id.to_string(),
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
                    self.registry.register(reg_key, metadata);
                }

                for mut ads_entry in parse_result.entries {
                    if ads_entry.version == crate::protocol::AdsProtocolVersion::V2 {
                        let reg_key = RegistrationKey {
                            ams_net_id: source_net_id.to_string(),
                            ams_source_port: source_port,
                            task_index: ads_entry.task_index as u8,
                        };
                        if let Some(metadata) = self.registry.lookup(&reg_key) {
                            ads_entry.task_name = metadata.task_name;
                            ads_entry.app_name = metadata.app_name;
                            ads_entry.project_name = metadata.project_name;
                            ads_entry.online_change_count = metadata.online_change_count;
                        }
                    }

                    let mut log_entry = LogEntry::new(
                        source_net_id.to_string(),
                        format!("plc-{}", source_net_id),
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
                    log_entry.ams_net_id = source_net_id.to_string();
                    log_entry.ams_source_port = source_port;
                    log_entry.trace_id = ads_entry.trace_id;
                    log_entry.span_id = ads_entry.span_id;

                    let _ = self.log_tx.send(log_entry).await;
                }

                if let Some(ref m_tx) = self.metric_tx {
                    for ads_metric in parse_result.metrics {
                        let hostname = format!("plc-{}", source_net_id);
                        let reg_key = RegistrationKey {
                            ams_net_id: source_net_id.to_string(),
                            ams_source_port: source_port,
                            task_index: ads_metric.task_index as u8,
                        };
                        let (task_name, app_name, project_name) =
                            if let Some(metadata) = self.registry.lookup(&reg_key) {
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
                            source: source_net_id.to_string(),
                            hostname,
                            ams_net_id: source_net_id.to_string(),
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

                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to parse log entries: {} (raw {} bytes)",
                    e,
                    write_data.len()
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dispatch_read_state() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let mut frame = vec![0u8; 32];
        frame[16] = (ADS_CMD_READ_STATE & 0xFF) as u8;
        frame[17] = ((ADS_CMD_READ_STATE >> 8) & 0xFF) as u8;
        let response = router.dispatch(&frame).await.expect("dispatch failed");
        assert!(response.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_read_device_info() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let mut frame = vec![0u8; 32];
        frame[16] = (ADS_CMD_READ_DEVICE_INFO & 0xFF) as u8;
        frame[17] = ((ADS_CMD_READ_DEVICE_INFO >> 8) & 0xFF) as u8;
        let response = router.dispatch(&frame).await.expect("dispatch failed");
        assert!(response.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_read() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let mut frame = vec![0u8; 44];
        frame[16] = (ADS_CMD_READ & 0xFF) as u8;
        frame[17] = ((ADS_CMD_READ >> 8) & 0xFF) as u8;
        let response = router.dispatch(&frame).await.expect("dispatch failed");
        assert!(response.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_write_empty() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let mut frame = vec![0u8; 44];
        frame[16] = (ADS_CMD_WRITE & 0xFF) as u8;
        frame[17] = ((ADS_CMD_WRITE >> 8) & 0xFF) as u8;
        let response = router.dispatch(&frame).await.expect("dispatch failed");
        assert!(response.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_unknown_command() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let mut frame = vec![0u8; 32];
        frame[16] = 99;
        let response = router.dispatch(&frame).await.expect("dispatch failed");
        assert!(response.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_frame_too_short() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        let frame = vec![0u8; 20];
        let result = router.dispatch(&frame).await;
        assert!(result.is_err());
    }
}

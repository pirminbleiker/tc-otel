//! ADS Router - transport-agnostic ADS protocol dispatch
use crate::ams::{
    AdsWriteRequest, AmsHeader, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO, ADS_CMD_READ_STATE,
    ADS_CMD_WRITE,
};
use crate::parser::AdsParser;
use crate::protocol::RegistrationKey;
use crate::registry::TaskRegistry;
use std::sync::Arc;
use tc_otel_core::{LogEntry, MetricEntry};
use tokio::sync::mpsc;

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

    pub fn registry(&self) -> &Arc<TaskRegistry> {
        &self.registry
    }

    pub async fn dispatch(&self, frame: &[u8]) -> crate::Result<Option<Vec<u8>>> {
        if frame.len() < 32 {
            return Err(crate::AdsError::ParseError("AMS frame too short".into()));
        }
        let header = AmsHeader::parse(frame)?;
        let source_net_id = header.source_net_id.to_string();
        let source_port = header.source_port;
        let response = match header.command_id {
            ADS_CMD_READ_STATE => Some(Self::build_response(&header, vec![0, 0, 0, 0, 5, 0, 0, 0])),
            ADS_CMD_READ_DEVICE_INFO => {
                let mut p = Vec::new();
                p.extend_from_slice(&0u32.to_le_bytes());
                p.push(0);
                p.push(1);
                p.extend_from_slice(&1u16.to_le_bytes());
                let mut name = [0u8; 16];
                name[..6].copy_from_slice(b"log4tc");
                p.extend_from_slice(&name);
                Some(Self::build_response(&header, p))
            }
            ADS_CMD_READ => {
                let read_len = if frame.len() >= 44 {
                    u32::from_le_bytes([frame[40], frame[41], frame[42], frame[43]])
                } else {
                    0
                };
                let mut p = Vec::new();
                p.extend_from_slice(&0u32.to_le_bytes());
                p.extend_from_slice(&read_len.to_le_bytes());
                p.resize(p.len() + read_len as usize, 0);
                Some(Self::build_response(&header, p))
            }
            ADS_CMD_WRITE => {
                let payload = &frame[32..];
                if let Ok(wr) = AdsWriteRequest::parse(payload) {
                    if header.target_port == self.ads_port {
                        let _ = self
                            .dispatch_write(&source_net_id, source_port, &wr.data)
                            .await;
                    }
                }
                Some(Self::build_response(&header, vec![0, 0, 0, 0]))
            }
            _ => Some(Self::build_response(&header, vec![0, 0, 0, 0])),
        };
        Ok(response)
    }

    fn build_response(header: &AmsHeader, payload: Vec<u8>) -> Vec<u8> {
        let mut rh = header.make_response(0);
        rh.data_length = payload.len() as u32;
        rh.state_flags = 0x0005;
        let mut r = rh.serialize();
        r.extend_from_slice(&payload);
        r
    }

    async fn dispatch_write(
        &self,
        source_net_id: &str,
        source_port: u16,
        write_data: &[u8],
    ) -> crate::Result<()> {
        if let Ok(pr) = AdsParser::parse_all(write_data) {
            for reg in pr.registrations {
                let k = RegistrationKey {
                    ams_net_id: source_net_id.to_string(),
                    ams_source_port: source_port,
                    task_index: reg.task_index,
                };
                let m = crate::protocol::TaskMetadata {
                    task_name: reg.task_name.clone(),
                    app_name: reg.app_name,
                    project_name: reg.project_name,
                    online_change_count: reg.online_change_count,
                };
                self.registry.register(k, m);
            }
            for mut e in pr.entries {
                if e.version == crate::protocol::AdsProtocolVersion::V2 {
                    let k = RegistrationKey {
                        ams_net_id: source_net_id.to_string(),
                        ams_source_port: source_port,
                        task_index: e.task_index as u8,
                    };
                    if let Some(m) = self.registry.lookup(&k) {
                        e.task_name = m.task_name;
                        e.app_name = m.app_name;
                        e.project_name = m.project_name;
                        e.online_change_count = m.online_change_count;
                    }
                }
                let mut le = LogEntry::new(
                    source_net_id.to_string(),
                    format!("plc-{}", source_net_id),
                    e.message,
                    e.logger,
                    e.level,
                );
                le.plc_timestamp = e.plc_timestamp;
                le.clock_timestamp = e.clock_timestamp;
                le.task_index = e.task_index;
                le.task_name = e.task_name;
                le.task_cycle_counter = e.task_cycle_counter;
                le.app_name = e.app_name;
                le.project_name = e.project_name;
                le.online_change_count = e.online_change_count;
                le.arguments = e.arguments;
                le.context = e.context;
                le.ams_net_id = source_net_id.to_string();
                le.ams_source_port = source_port;
                le.trace_id = e.trace_id;
                le.span_id = e.span_id;
                let _ = self.log_tx.send(le).await;
            }
            if let Some(ref m_tx) = self.metric_tx {
                for me in pr.metrics {
                    let hn = format!("plc-{}", source_net_id);
                    let k = RegistrationKey {
                        ams_net_id: source_net_id.to_string(),
                        ams_source_port: source_port,
                        task_index: me.task_index as u8,
                    };
                    let (tn, an, pn) = if let Some(m) = self.registry.lookup(&k) {
                        (m.task_name, m.app_name, m.project_name)
                    } else {
                        (String::new(), String::new(), String::new())
                    };
                    let met = MetricEntry {
                        name: me.name,
                        description: me.description,
                        unit: me.unit,
                        kind: me.kind,
                        value: me.value,
                        timestamp: me.timestamp,
                        source: source_net_id.to_string(),
                        hostname: hn,
                        ams_net_id: source_net_id.to_string(),
                        ams_source_port: source_port,
                        task_index: me.task_index,
                        task_name: tn,
                        task_cycle_counter: me.task_cycle_counter,
                        app_name: an,
                        project_name: pn,
                        attributes: me.attributes,
                        histogram_bounds: me.histogram_bounds,
                        histogram_counts: me.histogram_counts,
                        histogram_count: me.histogram_count,
                        histogram_sum: me.histogram_sum,
                        is_monotonic: me.is_monotonic,
                    };
                    let _ = m_tx.send(met).await;
                }
            }
        }
        Ok(())
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
        frame[16] = 4;
        frame[17] = 0;
        assert!(router.dispatch(&frame).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_dispatch_frame_too_short() {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        let router = AdsRouter::new(16150, log_tx, None, registry);
        assert!(router.dispatch(&[0u8; 20]).await.is_err());
    }
}

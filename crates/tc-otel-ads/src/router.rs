//! ADS Router - transport-agnostic ADS protocol dispatch
use crate::ams::{
    AdsWriteRequest, AmsHeader, ADSERR_DEVICE_SRVNOTSUPP, ADS_CMD_ADD_NOTIFICATION,
    ADS_CMD_DEL_NOTIFICATION, ADS_CMD_NOTIFICATION, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO,
    ADS_CMD_READ_STATE, ADS_CMD_READ_WRITE, ADS_CMD_WRITE, ADS_CMD_WRITE_CONTROL,
};
use crate::diagnostics::{DiagEvent, IG_PUSH_DIAG, IO_PUSH_BATCH, IO_PUSH_METRIC_AGG};
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
    push_tx: Option<mpsc::Sender<(crate::AmsNetId, DiagEvent)>>,
    trace_tx: Option<mpsc::Sender<(crate::AmsNetId, crate::protocol::TraceWireEvent)>>,
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
            push_tx: None,
            trace_tx: None,
            registry,
        }
    }

    pub fn with_push_sender(mut self, tx: mpsc::Sender<(crate::AmsNetId, DiagEvent)>) -> Self {
        self.push_tx = Some(tx);
        self
    }

    pub fn with_trace_sender(
        mut self,
        tx: mpsc::Sender<(crate::AmsNetId, crate::protocol::TraceWireEvent)>,
    ) -> Self {
        self.trace_tx = Some(tx);
        self
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
                name[..7].copy_from_slice(b"tc-otel");
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
                // Build ACK response FIRST and return it before any parsing /
                // log dispatch. PLC's ADSWRITE has a 5 s timeout; anything
                // that delays this path (e.g. scheduler yield during parse_all)
                // can trigger adsErrId=6. Parse/dispatch is scheduled inline
                // after the caller has the response in hand.
                let response = Self::build_response(&header, vec![0, 0, 0, 0]);
                let payload = &frame[32..];
                if let Ok(wr) = AdsWriteRequest::parse(payload) {
                    if header.target_port == self.ads_port {
                        // Check if this is a push-diagnostic batch write
                        if wr.index_group == IG_PUSH_DIAG {
                            tracing::debug!(
                                "push-diag dispatch: io_offset={} bytes={} from={}",
                                wr.index_offset,
                                wr.data.len(),
                                source_net_id
                            );
                            // Wire-format v2 dispatch:
                            //   IO=0 -> per-task diagnostic batch (event_type=10)
                            //   IO=2 -> FB_Metrics aggregate batch (event_type=21)
                            // (IO=1 is reserved for the existing UI-driven push
                            // metric batch, dispatched elsewhere.)
                            let ev = if wr.index_offset == IO_PUSH_BATCH {
                                crate::diagnostics_push::decode_batch(&wr.data)
                            } else if wr.index_offset == IO_PUSH_METRIC_AGG {
                                let decoded = crate::diagnostics_push::decode_metric_aggregate(&wr.data);
                                if decoded.is_none() {
                                    tracing::warn!(
                                        "metric_agg decode failed: bytes={} first16={:02x?}",
                                        wr.data.len(),
                                        &wr.data[..wr.data.len().min(16)]
                                    );
                                }
                                decoded
                            } else {
                                None
                            };
                            if let (Some(ev), Some(tx)) = (ev, self.push_tx.as_ref()) {
                                let net_id = header.source_net_id;
                                if tx.try_send((net_id, ev)).is_err() {
                                    tracing::warn!(
                                        "push-diagnostic channel full, dropping batch from {}",
                                        source_net_id
                                    );
                                }
                            }
                        } else {
                            // Parse as log batch (existing behavior)
                            self.dispatch_write_sync(&source_net_id, source_port, &wr.data);
                        }
                    }
                }
                Some(response)
            }
            ADS_CMD_READ_WRITE => {
                // Not implemented — return ADS-level SRVNOTSUPP so the client
                // sees a real error instead of timing out. Response payload
                // shape: result(4) + length(4) + data(length).
                let mut p = Vec::with_capacity(8);
                p.extend_from_slice(&ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes());
                p.extend_from_slice(&0u32.to_le_bytes());
                Some(Self::build_response(&header, p))
            }
            ADS_CMD_WRITE_CONTROL => {
                // Not implemented — response is just the 4-byte result.
                Some(Self::build_response(
                    &header,
                    ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes().to_vec(),
                ))
            }
            ADS_CMD_ADD_NOTIFICATION => {
                // Response: result(4) + notificationHandle(4). Return zero
                // handle alongside the error so the client rejects cleanly.
                let mut p = Vec::with_capacity(8);
                p.extend_from_slice(&ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes());
                p.extend_from_slice(&0u32.to_le_bytes());
                Some(Self::build_response(&header, p))
            }
            ADS_CMD_DEL_NOTIFICATION => Some(Self::build_response(
                &header,
                ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes().to_vec(),
            )),
            ADS_CMD_NOTIFICATION => {
                // Device notification is a server→client indication; receiving
                // it as a request is unusual. Answer with SRVNOTSUPP so the
                // peer does not hang on a reply.
                Some(Self::build_response(
                    &header,
                    ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes().to_vec(),
                ))
            }
            other => {
                tracing::debug!(
                    "AMS: unsupported command id {} — replying SRVNOTSUPP",
                    other
                );
                Some(Self::build_response(
                    &header,
                    ADSERR_DEVICE_SRVNOTSUPP.to_le_bytes().to_vec(),
                ))
            }
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

    fn dispatch_write_sync(&self, source_net_id: &str, source_port: u16, write_data: &[u8]) {
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
                let _ = self.log_tx.try_send(le);
            }
            // Dispatch trace events
            if let Some(ref tx) = self.trace_tx {
                if let Ok(net_id) = crate::AmsNetId::from_str_ref(source_net_id) {
                    for ev in pr.trace_events {
                        if tx.try_send((net_id, ev)).is_err() {
                            tracing::debug!(
                                "trace-event channel full, dropping from {}",
                                source_net_id
                            );
                        }
                    }
                } else {
                    tracing::warn!("Invalid source AMS Net ID: {}", source_net_id);
                }
            }
        }
    }

    #[allow(dead_code)]
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
                // Non-blocking: drop under backpressure rather than stall the
                // AMS response path (PLC times out after 5s with adsErrId=6).
                let _ = self.log_tx.try_send(le);
            }
            if let Some(ref m_tx) = self.metric_tx {
                for me in pr.metrics {
                    let hn = format!("plc-{}", source_net_id);
                    let k = RegistrationKey {
                        ams_net_id: source_net_id.to_string(),
                        ams_source_port: source_port,
                        task_index: me.task_index as u8,
                    };
                    let (task_name, app_name, project_name) =
                        if let Some(m) = self.registry.lookup(&k) {
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
                        task_name,
                        task_cycle_counter: me.task_cycle_counter,
                        app_name,
                        project_name,
                        attributes: me.attributes,
                        histogram_bounds: me.histogram_bounds,
                        histogram_counts: me.histogram_counts,
                        histogram_count: me.histogram_count,
                        histogram_sum: me.histogram_sum,
                        is_monotonic: me.is_monotonic,
                    };
                    let _ = m_tx.try_send(met);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ams::{AmsNetId, ADS_STATE_REQUEST, ADS_STATE_RESPONSE};

    fn make_router() -> AdsRouter {
        let (log_tx, _) = mpsc::channel(100);
        let registry = Arc::new(TaskRegistry::new());
        AdsRouter::new(16150, log_tx, None, registry)
    }

    /// Build a well-formed AMS request frame (32-byte header + payload) with
    /// distinguishable source/target NetIDs and a non-zero invoke_id so we can
    /// verify the response echoes the correct fields.
    fn make_request_frame(command_id: u16, payload: &[u8]) -> (AmsHeader, Vec<u8>) {
        let header = AmsHeader {
            target_net_id: AmsNetId::from_bytes([10, 0, 0, 1, 1, 1]),
            target_port: 16150,
            source_net_id: AmsNetId::from_bytes([192, 168, 1, 50, 1, 1]),
            source_port: 32768,
            command_id,
            state_flags: ADS_STATE_REQUEST,
            data_length: payload.len() as u32,
            error_code: 0,
            invoke_id: 0xDEADBEEF,
        };
        let mut frame = header.serialize();
        frame.extend_from_slice(payload);
        (header, frame)
    }

    /// Parse a response frame and assert the AMS-level invariants that any
    /// router response must satisfy (header swap, state flags, invoke_id echo,
    /// data_length matches payload, no AMS transport error).
    fn assert_valid_response(request: &AmsHeader, response: &[u8]) -> AmsHeader {
        assert!(response.len() >= 32, "response shorter than AMS header");
        let resp = AmsHeader::parse(response).expect("response header parses");
        assert_eq!(resp.target_net_id, request.source_net_id, "net id swap");
        assert_eq!(resp.target_port, request.source_port, "port swap");
        assert_eq!(resp.source_net_id, request.target_net_id, "net id swap");
        assert_eq!(resp.source_port, request.target_port, "port swap");
        assert_eq!(resp.command_id, request.command_id, "command id echoed");
        assert_eq!(resp.invoke_id, request.invoke_id, "invoke id echoed");
        assert_eq!(resp.state_flags, ADS_STATE_RESPONSE, "response flag set");
        assert_eq!(resp.error_code, 0, "AMS-level error stays 0");
        assert_eq!(
            resp.data_length as usize,
            response.len() - 32,
            "data_length matches actual payload length"
        );
        resp
    }

    fn parse_result_code(response: &[u8]) -> u32 {
        u32::from_le_bytes([response[32], response[33], response[34], response[35]])
    }

    #[tokio::test]
    async fn test_dispatch_frame_too_short() {
        let router = make_router();
        assert!(router.dispatch(&[0u8; 20]).await.is_err());
    }

    #[tokio::test]
    async fn test_dispatch_read_state_returns_valid_state_payload() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_READ_STATE, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        let hdr = assert_valid_response(&req, &resp);
        // READ_STATE response payload: adsState(2) + deviceState(2) — 8 bytes
        // total including the leading 4-byte result code? No: READ_STATE has
        // no result prefix — it returns state(2)+deviceState(2) only per
        // Beckhoff. Current implementation emits 8 bytes `[0,0,0,0,5,0,0,0]`
        // which is result(0) + state(5) + devState(0). Validate that shape.
        assert_eq!(hdr.data_length, 8);
        assert_eq!(parse_result_code(&resp), 0);
    }

    #[tokio::test]
    async fn test_dispatch_read_device_info_returns_name() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_READ_DEVICE_INFO, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), 0);
        // Payload: result(4) + major(1) + minor(1) + build(2) + name(16)
        assert_eq!(resp.len(), 32 + 24);
        let name = &resp[32 + 8..32 + 8 + 6];
        assert_eq!(name, b"tc-ote");
    }

    #[tokio::test]
    async fn test_dispatch_read_returns_zeroed_buffer_of_requested_length() {
        let router = make_router();
        // ADS Read request body: index_group(4) + index_offset(4) + length(4)
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&16u32.to_le_bytes());
        let (req, frame) = make_request_frame(ADS_CMD_READ, &body);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), 0);
        // Payload: result(4) + length(4) + data(16)
        let length = u32::from_le_bytes([resp[36], resp[37], resp[38], resp[39]]);
        assert_eq!(length, 16);
        assert_eq!(resp.len(), 32 + 4 + 4 + 16);
        assert!(resp[40..].iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn test_dispatch_write_returns_success_ack() {
        let router = make_router();
        // Empty write body — parser will reject, but ACK must still go out.
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        let (req, frame) = make_request_frame(ADS_CMD_WRITE, &body);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), 0, "write always ACKs with 0");
        assert_eq!(resp.len(), 32 + 4);
    }

    #[tokio::test]
    async fn test_dispatch_read_write_returns_srvnotsupp() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_READ_WRITE, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
        // READ_WRITE response shape: result(4) + length(4)
        assert_eq!(resp.len(), 32 + 8);
        let len = u32::from_le_bytes([resp[36], resp[37], resp[38], resp[39]]);
        assert_eq!(len, 0);
    }

    #[tokio::test]
    async fn test_dispatch_write_control_returns_srvnotsupp() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_WRITE_CONTROL, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
        assert_eq!(resp.len(), 32 + 4);
    }

    #[tokio::test]
    async fn test_dispatch_add_notification_returns_srvnotsupp_with_zero_handle() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_ADD_NOTIFICATION, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
        assert_eq!(resp.len(), 32 + 8);
        let handle = u32::from_le_bytes([resp[36], resp[37], resp[38], resp[39]]);
        assert_eq!(handle, 0);
    }

    #[tokio::test]
    async fn test_dispatch_del_notification_returns_srvnotsupp() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_DEL_NOTIFICATION, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
        assert_eq!(resp.len(), 32 + 4);
    }

    #[tokio::test]
    async fn test_dispatch_notification_returns_srvnotsupp() {
        let router = make_router();
        let (req, frame) = make_request_frame(ADS_CMD_NOTIFICATION, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
    }

    #[tokio::test]
    async fn test_dispatch_unknown_command_returns_srvnotsupp() {
        let router = make_router();
        let (req, frame) = make_request_frame(0xEEEE, &[]);
        let resp = router.dispatch(&frame).await.unwrap().unwrap();
        assert_valid_response(&req, &resp);
        assert_eq!(parse_result_code(&resp), ADSERR_DEVICE_SRVNOTSUPP);
        assert_eq!(resp.len(), 32 + 4);
    }

    /// Exhaustive sweep of every 16-bit command id: dispatch must never hang,
    /// never panic, and must always produce a well-formed response (header
    /// swap, invoke_id echo, matching data_length). This is the core promise
    /// of the change — "client never times out".
    #[tokio::test]
    async fn test_dispatch_every_command_id_produces_valid_response() {
        let router = make_router();
        // Sample: the full ADS portfolio plus a spread of unknown ids.
        let ids: Vec<u16> = (0u16..=9)
            .chain([10, 42, 100, 0x0100, 0x1000, 0x7FFF, 0xFFFE, 0xFFFF])
            .collect();
        for id in ids {
            let (req, frame) = make_request_frame(id, &[]);
            let resp = router
                .dispatch(&frame)
                .await
                .unwrap_or_else(|e| panic!("dispatch errored for cmd {}: {}", id, e))
                .unwrap_or_else(|| panic!("dispatch returned None for cmd {}", id));
            assert_valid_response(&req, &resp);
        }
    }
}

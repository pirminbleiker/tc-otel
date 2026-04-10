//! AMS/TCP Server for receiving ADS commands from TwinCAT PLCs
//!
//! This server listens on TCP port 48898 (AMS/TCP) and UDP port 48899 (route discovery).
//! It handles AMS/TCP frames containing ADS commands and responds appropriately.

use crate::ams::{
    AdsWriteRequest, AmsHeader, AmsNetId, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO,
    ADS_CMD_READ_STATE, ADS_CMD_WRITE,
};
use crate::parser::AdsParser;
use crate::protocol::RegistrationKey;
use crate::registry::TaskRegistry;
use tc_otel_core::LogEntry;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

/// AMS/TCP Server on port 48898 + UDP discovery on 48899
pub struct AmsTcpServer {
    host: String,
    net_id: AmsNetId,
    port: u16,
    ads_port: u16,
    log_tx: mpsc::Sender<LogEntry>,
    registry: Arc<TaskRegistry>,
}

impl AmsTcpServer {
    pub fn new(
        host: String,
        net_id: AmsNetId,
        ads_port: u16,
        log_tx: mpsc::Sender<LogEntry>,
    ) -> Self {
        Self {
            host,
            net_id,
            port: 48898,
            ads_port,
            log_tx,
            registry: Arc::new(TaskRegistry::new()),
        }
    }

    pub fn with_registry(mut self, registry: Arc<TaskRegistry>) -> Self {
        self.registry = registry;
        self
    }

    pub async fn start(&self) -> crate::Result<()> {
        let tcp_addr = format!("{}:{}", self.host, self.port);
        let _udp_addr = format!("{}:{}", self.host, self.port + 1); // 48899

        let tcp_listener = TcpListener::bind(&tcp_addr)
            .await
            .map_err(|e| crate::AdsError::BufferError(format!("Failed to bind TCP {}: {}", tcp_addr, e)))?;

        tracing::info!(
            "AMS/TCP server listening on {} with Net ID {}",
            tcp_addr,
            self.net_id.to_string()
        );

        // Start UDP route discovery listener
        let net_id = self.net_id;
        let udp_host = self.host.clone();
        tokio::spawn(async move {
            if let Err(e) = Self::udp_discovery_listener(&udp_host, net_id).await {
                tracing::warn!("UDP discovery listener error: {}", e);
            }
        });

        // Accept TCP connections
        loop {
            let (stream, peer_addr) = tcp_listener
                .accept()
                .await
                .map_err(|e| crate::AdsError::BufferError(format!("Accept error: {}", e)))?;
            tracing::trace!("AMS/TCP connection from {}", peer_addr);

            let net_id = self.net_id;
            let ads_port = self.ads_port;
            let log_tx = self.log_tx.clone();
            let registry = self.registry.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::handle_connection(stream, peer_addr, net_id, ads_port, log_tx, registry).await
                {
                    tracing::warn!("AMS/TCP connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }

    /// UDP listener on port 48899 for ADS route discovery
    async fn udp_discovery_listener(host: &str, net_id: AmsNetId) -> crate::Result<()> {
        let addr = format!("{}:48899", host);
        let socket = match UdpSocket::bind(&addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Could not bind UDP discovery on {}: {}", addr, e);
                return Ok(());
            }
        };

        tracing::info!("ADS UDP discovery listening on {}", addr);

        let mut buf = [0u8; 2048];
        loop {
            let (len, src) = match socket.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("UDP recv error: {}", e);
                    continue;
                }
            };

            tracing::debug!("UDP discovery packet from {} ({} bytes)", src, len);

            // Build a minimal route discovery response
            // The response tells the PLC: "I am here, my Net ID is X"
            let response = Self::build_udp_discovery_response(&net_id);
            if let Err(e) = socket.send_to(&response, src).await {
                tracing::debug!("UDP response error: {}", e);
            }
        }
    }

    /// Build UDP route discovery response packet
    fn build_udp_discovery_response(net_id: &AmsNetId) -> Vec<u8> {
        let mut resp = Vec::with_capacity(64);

        // ADS discovery response header
        // Based on Beckhoff ADS protocol: response type + Net ID + name
        resp.extend_from_slice(&[0x03, 0x66, 0x14, 0x71]); // Discovery response magic
        resp.extend_from_slice(&24u32.to_le_bytes()); // Data length

        // Our AMS Net ID (6 bytes)
        let id = net_id.bytes();
        resp.extend_from_slice(id);

        // AMS TCP port (2 bytes)
        resp.extend_from_slice(&48898u16.to_le_bytes());

        // Device name (null-terminated, padded to 16 bytes)
        let mut name = [0u8; 16];
        let src = b"tc-otel-rust";
        name[..src.len()].copy_from_slice(src);
        resp.extend_from_slice(&name);

        resp
    }

    async fn handle_connection(
        mut stream: TcpStream,
        peer_addr: SocketAddr,
        _net_id: AmsNetId,
        ads_port: u16,
        log_tx: mpsc::Sender<LogEntry>,
        registry: Arc<TaskRegistry>,
    ) -> crate::Result<()> {
        let _ = stream.set_nodelay(true);

        // Pre-allocated buffers to avoid per-frame allocations
        let mut read_buf = vec![0u8; 16384]; // 16KB reusable read buffer
        let mut resp_buf = vec![0u8; 256];   // reusable response buffer

        loop {
            // Read AMS/TCP header (6 bytes)
            match stream.read_exact(&mut read_buf[..6]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(crate::AdsError::IoError(e)),
            }

            let reserved = u16::from_le_bytes([read_buf[0], read_buf[1]]);
            let data_len = u32::from_le_bytes([read_buf[2], read_buf[3], read_buf[4], read_buf[5]]) as usize;

            if reserved != 0 || data_len == 0 || data_len > 1_048_576 {
                break;
            }

            // Grow read buffer if needed (rare - only for large writes)
            if data_len > read_buf.len() {
                read_buf.resize(data_len, 0);
            }

            // Read AMS data into reusable buffer
            match stream.read_exact(&mut read_buf[..data_len]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(crate::AdsError::IoError(e)),
            }

            let data = &read_buf[..data_len];

            // Fast path: check command ID without full parse (byte 16-17 in AMS header)
            if data.len() < 32 { continue; }
            let cmd = u16::from_le_bytes([data[16], data[17]]);

            if cmd == ADS_CMD_READ_STATE || cmd == ADS_CMD_READ || cmd == ADS_CMD_READ_DEVICE_INFO {
                // Heartbeat/system query - build response in-place, zero allocation
                let resp_len = Self::build_fast_response(data, cmd, &mut resp_buf);
                if resp_len > 0 {
                    if stream.write_all(&resp_buf[..resp_len]).await.is_err() {
                        break;
                    }
                }
            } else if cmd == ADS_CMD_WRITE {
                // Log data - full processing path
                match Self::handle_frame(data, ads_port, peer_addr, &log_tx, &registry).await {
                    Ok(ams_response) => {
                        // Wrap in AMS/TCP header (6 bytes: reserved + length)
                        let mut full_response = Vec::with_capacity(6 + ams_response.len());
                        full_response.extend_from_slice(&0u16.to_le_bytes());
                        full_response.extend_from_slice(&(ams_response.len() as u32).to_le_bytes());
                        full_response.extend_from_slice(&ams_response);
                        if stream.write_all(&full_response).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {}
                }
            } else {
                // Unknown command - success response
                let resp_len = Self::build_fast_response(data, cmd, &mut resp_buf);
                if resp_len > 0 {
                    let _ = stream.write_all(&resp_buf[..resp_len]).await;
                }
            }
        }

        Ok(())
    }

    /// Build a response in a pre-allocated buffer - zero heap allocation
    /// Returns total bytes written (including AMS/TCP header)
    fn build_fast_response(request: &[u8], cmd: u16, buf: &mut Vec<u8>) -> usize {
        // AMS header is first 32 bytes of request
        // Response: swap source/target (bytes 0-7 ↔ 8-15), set response flag, set payload

        let (payload_len, payload) = match cmd {
            ADS_CMD_READ_STATE => {
                // Result(4) + AdsState(2) + DeviceState(2) = 8 bytes
                (8u32, &[0,0,0,0, 5,0, 0,0][..])
            }
            ADS_CMD_READ_DEVICE_INFO => {
                // Result(4) + Major(1) + Minor(1) + Build(2) + Name(16) = 24 bytes
                static DEVICE_INFO: [u8; 24] = [
                    0,0,0,0,  // result: success
                    0, 1,     // version 0.1
                    1, 0,     // build 1
                    b'l',b'o',b'g',b'4',b't',b'c',0,0,0,0,0,0,0,0,0,0, // name
                ];
                (24u32, &DEVICE_INFO[..])
            }
            ADS_CMD_READ => {
                // Parse read_length from request payload (offset 40 = 32 header + 8 ig/io)
                let read_len = if request.len() >= 44 {
                    u32::from_le_bytes([request[40], request[41], request[42], request[43]])
                } else {
                    0
                };
                // Result(4) + DataLength(4) + Data(N)
                // For large reads, fall back to allocated response
                if read_len > 128 {
                    return 0; // signal: use slow path
                }
                let total = 8 + read_len as usize;
                // Build inline: result=0, datalen=read_len, zeros
                buf.clear();
                buf.extend_from_slice(&0u16.to_le_bytes()); // TCP reserved
                buf.extend_from_slice(&(32 + total as u32).to_le_bytes()); // TCP data len
                // Swap source/target in AMS header
                buf.extend_from_slice(&request[8..14]);   // target = request.source netid
                buf.extend_from_slice(&request[14..16]);  // target port = request.source port
                buf.extend_from_slice(&request[0..6]);    // source = request.target netid
                buf.extend_from_slice(&request[6..8]);    // source port = request.target port
                buf.extend_from_slice(&request[16..18]);  // command id
                buf.extend_from_slice(&5u16.to_le_bytes()); // state flags = response
                buf.extend_from_slice(&(total as u32).to_le_bytes()); // data length
                buf.extend_from_slice(&0u32.to_le_bytes()); // error code
                buf.extend_from_slice(&request[28..32]);  // invoke id
                buf.extend_from_slice(&0u32.to_le_bytes()); // result: success
                buf.extend_from_slice(&read_len.to_le_bytes()); // data length
                buf.resize(buf.len() + read_len as usize, 0); // zero data
                return buf.len();
            }
            _ => {
                // Generic success: Result(4) = 4 bytes
                (4u32, &[0,0,0,0][..])
            }
        };

        let ams_data_len = 32 + payload_len;

        buf.clear();
        // AMS/TCP header (6 bytes)
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&ams_data_len.to_le_bytes());
        // AMS header (32 bytes) - swap source/target
        buf.extend_from_slice(&request[8..14]);   // target netid = source netid from request
        buf.extend_from_slice(&request[14..16]);  // target port
        buf.extend_from_slice(&request[0..6]);    // source netid = target netid from request
        buf.extend_from_slice(&request[6..8]);    // source port
        buf.extend_from_slice(&request[16..18]);  // command id (same)
        buf.extend_from_slice(&5u16.to_le_bytes()); // state flags = response (0x0005)
        buf.extend_from_slice(&payload_len.to_le_bytes()); // data length
        buf.extend_from_slice(&0u32.to_le_bytes()); // error code = 0
        buf.extend_from_slice(&request[28..32]);  // invoke id (echo back)
        // Payload
        buf.extend_from_slice(payload);

        buf.len()
    }

    async fn handle_frame(
        data: &[u8],
        ads_port: u16,
        peer_addr: SocketAddr,
        log_tx: &mpsc::Sender<LogEntry>,
        registry: &Arc<TaskRegistry>,
    ) -> crate::Result<Vec<u8>> {
        if data.len() < 32 {
            return Err(crate::AdsError::ParseError("AMS header too short".into()));
        }

        let header = AmsHeader::parse(data)?;
        let source_net_id = header.source_net_id.to_string();
        let source_port = header.source_port;

        tracing::trace!(
            "AMS frame: cmd={} src={}:{} -> dst={}:{}",
            header.command_id,
            source_net_id,
            source_port,
            header.target_net_id.to_string(),
            header.target_port,
        );

        match header.command_id {
            ADS_CMD_READ_DEVICE_INFO => {
                tracing::trace!("DeviceInfo request from {}", peer_addr);
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes()); // Result: success
                payload.push(0); // Major version
                payload.push(1); // Minor version
                payload.extend_from_slice(&1u16.to_le_bytes()); // Build number
                let mut name = [0u8; 16];
                let src = b"tc-otel-rust";
                name[..src.len()].copy_from_slice(src);
                payload.extend_from_slice(&name);

                let mut rh = header.make_response(0);
                rh.data_length = payload.len() as u32;
                let mut response = rh.serialize();
                response.extend_from_slice(&payload);
                Ok(response)
            }

            ADS_CMD_READ => {
                let payload_data = &data[32..];

                // Parse Read request: IndexGroup(4) + IndexOffset(4) + ReadLength(4)
                if payload_data.len() < 12 {
                    return Err(crate::AdsError::ParseError("Read request too short".into()));
                }

                let index_group = u32::from_le_bytes([payload_data[0], payload_data[1], payload_data[2], payload_data[3]]);
                let index_offset = u32::from_le_bytes([payload_data[4], payload_data[5], payload_data[6], payload_data[7]]);
                let read_length = u32::from_le_bytes([payload_data[8], payload_data[9], payload_data[10], payload_data[11]]);

                tracing::trace!("Read from {} ig={:#x} io={:#x}", peer_addr, index_group, index_offset);

                // Build response: Result(4) + DataLength(4) + Data(N)
                // Return requested amount of zero-filled data with correct DataLength
                let actual_data = vec![0u8; read_length as usize];
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes()); // Result: success
                payload.extend_from_slice(&(actual_data.len() as u32).to_le_bytes()); // DataLength = actual size
                payload.extend_from_slice(&actual_data);

                let mut rh = header.make_response(0);
                rh.data_length = payload.len() as u32;
                let mut response = rh.serialize();
                response.extend_from_slice(&payload);
                Ok(response)
            }

            ADS_CMD_READ_STATE => {
                tracing::trace!("ReadState from {}", peer_addr);
                // Result(4) + AdsState(2) + DeviceState(2)
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes()); // Result: success
                payload.extend_from_slice(&5u16.to_le_bytes()); // ADS State: RUN
                payload.extend_from_slice(&0u16.to_le_bytes()); // Device State: 0

                let mut rh = header.make_response(0);
                rh.data_length = payload.len() as u32;
                let mut response = rh.serialize();
                response.extend_from_slice(&payload);
                Ok(response)
            }

            ADS_CMD_WRITE => {
                let payload = &data[32..];
                let write_req = AdsWriteRequest::parse(payload)?;

                tracing::debug!("ADS Write: {} bytes from {}", write_req.data.len(), peer_addr);

                // Only parse as log entry if targeting our ADS port
                // Buffer can contain MULTIPLE log entries + registrations
                if header.target_port == ads_port {
                    match AdsParser::parse_all(&write_req.data) {
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
                                tracing::debug!("Registered task {}: {}", registration.task_index, metadata.task_name);
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

                                let source = peer_addr.ip().to_string();
                                let hostname = format!("plc-{}", peer_addr.port());

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

                                let _ = log_tx.send(log_entry).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse log entries: {} (raw {} bytes)", e, write_req.data.len());
                        }
                    }
                }

                // Write response: Result(4) only
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes()); // Result: success

                let mut rh = header.make_response(0);
                rh.data_length = payload.len() as u32;
                let mut response = rh.serialize();
                response.extend_from_slice(&payload);
                Ok(response)
            }

            _ => {
                tracing::debug!("Unknown AMS cmd={} from {} port={}", header.command_id, peer_addr, header.target_port);
                // Respond with success to avoid blocking the PLC
                let mut payload = Vec::new();
                payload.extend_from_slice(&0u32.to_le_bytes()); // Result: success

                let mut rh = header.make_response(0);
                rh.data_length = payload.len() as u32;
                let mut response = rh.serialize();
                response.extend_from_slice(&payload);
                Ok(response)
            }
        }
    }
}

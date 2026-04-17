//! TCP transport for AMS frames
//!
//! Implements the AMS/TCP protocol as defined by Beckhoff, listening on port 48898 for
//! incoming AMS commands and UDP port 48899 for route discovery.

use super::AmsTransport;
use crate::ams::{
    AmsNetId, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO, ADS_CMD_READ_STATE, ADS_CMD_WRITE,
};
use crate::connection_manager::{ConnectionConfig, ConnectionManager};
use crate::registry::TaskRegistry;
use crate::router::AdsRouter;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// TCP AMS transport (AMS/TCP on port 48898 + UDP discovery on 48899)
#[derive(Clone)]
pub struct TcpAmsTransport {
    host: String,
    net_id: AmsNetId,
    port: u16,
    router: Arc<AdsRouter>,
    conn_manager: Arc<ConnectionManager>,
}

impl TcpAmsTransport {
    pub fn new(host: String, net_id: AmsNetId, router: Arc<AdsRouter>) -> Self {
        Self {
            host,
            net_id,
            port: 48898,
            router,
            conn_manager: Arc::new(ConnectionManager::new(ConnectionConfig::default())),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_connection_config(mut self, config: ConnectionConfig) -> Self {
        self.conn_manager = Arc::new(ConnectionManager::new(config));
        self
    }

    /// Get a reference to the connection manager
    pub fn connection_manager(&self) -> &Arc<ConnectionManager> {
        &self.conn_manager
    }

    /// Get a reference to the task registry (via router)
    pub fn task_registry(&self) -> &Arc<TaskRegistry> {
        self.router.registry()
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
        _peer_addr: SocketAddr,
        router: Arc<AdsRouter>,
    ) -> crate::Result<()> {
        let _ = stream.set_nodelay(true);

        // Pre-allocated buffers to avoid per-frame allocations
        let mut read_buf = vec![0u8; 16384]; // 16KB reusable read buffer

        loop {
            // Read AMS/TCP header (6 bytes)
            match stream.read_exact(&mut read_buf[..6]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::debug!("AMS/TCP: peer closed ({})", _peer_addr);
                    break;
                }
                Err(e) => return Err(crate::AdsError::IoError(e)),
            }

            let reserved = u16::from_le_bytes([read_buf[0], read_buf[1]]);
            let data_len =
                u32::from_le_bytes([read_buf[2], read_buf[3], read_buf[4], read_buf[5]]) as usize;

            // 16 MB matches MQTT transport limit. Batched log WRITE frames
            // from tc-otel can exceed 1 MB; a hard break here caused the PLC
            // to time out every ~5 s with adsErrId=6 and reconnect.
            if reserved != 0 || data_len == 0 || data_len > 16 * 1_048_576 {
                tracing::warn!(
                    "AMS/TCP: dropping connection from {} — bad header (reserved={}, data_len={})",
                    _peer_addr,
                    reserved,
                    data_len
                );
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

            // Skip malformed short frames without dropping the connection.
            if data.len() < 32 {
                continue;
            }
            let cmd = u16::from_le_bytes([data[16], data[17]]);
            let tgt_port = u16::from_le_bytes([data[6], data[7]]);
            tracing::trace!(
                "AMS in: cmd={} tgt_port={} dlen={} first16={:02x?}",
                cmd,
                tgt_port,
                data.len(),
                &data[..16.min(data.len())]
            );

            // Fast path for heartbeat/system queries — inline synchronous
            // response (same as pre-refactor bf10d124, which had no adsErrId=6).
            if cmd == ADS_CMD_READ_STATE
                || cmd == ADS_CMD_READ
                || cmd == ADS_CMD_READ_DEVICE_INFO
                || cmd != ADS_CMD_WRITE
            {
                match router.dispatch(data).await {
                    Ok(Some(response)) => {
                        let mut full = Vec::with_capacity(6 + response.len());
                        full.extend_from_slice(&0u16.to_le_bytes());
                        full.extend_from_slice(&(response.len() as u32).to_le_bytes());
                        full.extend_from_slice(&response);
                        if stream.write_all(&full).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!("Router dispatch error (skipping frame): {}", e);
                    }
                }
                continue;
            }

            // ADS_CMD_WRITE: full log-processing path
            match router.dispatch(data).await {
                Ok(Some(response)) => {
                    let mut full_response = Vec::with_capacity(6 + response.len());
                    full_response.extend_from_slice(&0u16.to_le_bytes());
                    full_response.extend_from_slice(&(response.len() as u32).to_le_bytes());
                    full_response.extend_from_slice(&response);
                    tracing::trace!(
                        "WRITE ACK ({}B): {:02x?}",
                        full_response.len(),
                        &full_response[..full_response.len().min(48)]
                    );
                    if stream.write_all(&full_response).await.is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!("Router dispatch error (skipping frame): {}", e);
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl AmsTransport for TcpAmsTransport {
    async fn run(self: Arc<Self>) -> crate::Result<()> {
        let tcp_addr = format!("{}:{}", self.host, self.port);

        let tcp_listener = TcpListener::bind(&tcp_addr).await.map_err(|e| {
            crate::AdsError::BufferError(format!("Failed to bind TCP {}: {}", tcp_addr, e))
        })?;

        tracing::info!(
            "AMS/TCP server listening on {} with Net ID {} (max {} connections)",
            tcp_addr,
            self.net_id.to_string(),
            self.conn_manager.max_connections()
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

            // Enforce connection limits
            let permit = match self.conn_manager.try_acquire(peer_addr.ip()) {
                Ok(permit) => permit,
                Err(rejection) => {
                    tracing::warn!(
                        "AMS/TCP connection rejected from {}: {}",
                        peer_addr,
                        rejection
                    );
                    drop(stream);
                    continue;
                }
            };

            tracing::info!("AMS/TCP connection from {}", peer_addr);

            let router = self.router.clone();

            tokio::spawn(async move {
                let _permit = permit; // Hold permit until connection ends
                if let Err(e) = Self::handle_connection(stream, peer_addr, router).await {
                    tracing::warn!("AMS/TCP connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }

    async fn send(&self, _dest: AmsNetId, _frame: Vec<u8>) -> crate::Result<()> {
        // TCP transport doesn't implement send since responses are sent
        // on the same connection that the request came from.
        // This is included for interface completeness.
        Ok(())
    }

    fn local_net_id(&self) -> AmsNetId {
        self.net_id
    }
}

//! Local TwinCAT AMS router client transport.
//!
//! When TwinCAT runs on the same machine as tc-otel, it owns TCP/48898 — so
//! tc-otel cannot bind that port. Instead, tc-otel acts as a *client* of the
//! local router:
//!
//! 1. Open outbound TCP to `127.0.0.1:48898`.
//! 2. Send AMS/TCP `PortConnect` (cmd `0x1000`) with the desired AMS port
//!    (e.g. `16150` = ADS_LOG_PORT) as the 2-byte LE payload.
//! 3. Router replies with the assigned port + the local AmsNetId (8 bytes
//!    total).
//! 4. From then on, every frame the PLC writes to
//!    `<localNetId>:<assigned_port>` is delivered over the same socket as a
//!    `cmd=0x0000` (ADS) frame.
//!
//! This means tc-otel piggybacks on the running TwinCAT router and shares
//! its NetID — no separate AMS NetId, no `StaticRoutes.xml` edit, no
//! port-48898 conflict.
//!
//! References:
//! - Beckhoff AMS/TCP packet spec: <https://infosys.beckhoff.com/content/1033/tcadscommon/12440280843.html>
//! - jisotalo/ads-server `AMS_HEADER_FLAG`: <https://github.com/jisotalo/ads-server/blob/master/src/ads-commons.ts>

use super::AmsTransport;
use crate::ams::AmsNetId;
use crate::router::AdsRouter;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// AMS/TCP control commands. The 2-byte command id sits at the very start
/// of every AMS/TCP packet (replacing the "reserved zero" used by regular
/// ADS frames). Beckhoff's spec only documents `0x0000` for ADS commands;
/// the control IDs below come from the open-source ads-server reference
/// and from observation of the live router.
#[allow(dead_code)]
const AMS_TCP_CMD_ADS: u16 = 0x0000;
const AMS_TCP_CMD_PORT_CLOSE: u16 = 0x0001;
const AMS_TCP_CMD_PORT_CONNECT: u16 = 0x1000;
#[allow(dead_code)]
const AMS_TCP_CMD_ROUTER_NOTE: u16 = 0x1001;
#[allow(dead_code)]
const AMS_TCP_CMD_GET_LOCAL_NETID: u16 = 0x1002;

/// AMS port to register at the local router. Currently fixed to the
/// well-known tc-otel log port. The PLC's `FB_TcOtelTask` writes to this
/// port (see `nAdsPort := 16150` in the PLC library).
pub const TC_OTEL_REGISTER_PORT: u16 = 16150;

/// Detect the IPC's primary outbound IPv4 (the interface the kernel
/// would pick for traffic to the public internet). Used to populate
/// OTel sem-conv `source.address` for frames that arrived over the
/// local AMS router — TwinCAT runs on the same host, so the
/// network-level peer IS this IPC. Reporting the real interface IP
/// (e.g. `172.18.129.178`) instead of `127.0.0.1` lets dashboards
/// correlate logs/metrics with the host they were collected from
/// without an extra `host.ip` lookup.
///
/// Uses the standard UDP-connect trick: `UdpSocket::connect()` only
/// sets the destination on the socket — no packets are sent — but it
/// forces the kernel to resolve the outbound interface so
/// `local_addr()` returns the real IP. Works offline; returns `None`
/// only on hosts without any usable interface.
fn detect_primary_local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

/// Local AMS router client transport.
pub struct LocalRouterAmsTransport {
    router_host: String,
    router_port: u16,
    register_port: u16,
    /// The local NetId reported back by the router during `PortConnect`.
    /// Populated on first successful `run()` call; stable thereafter.
    local_net_id: Arc<RwLock<AmsNetId>>,
    router: Arc<AdsRouter>,
    /// IPC's primary outbound IPv4, captured once at construction.
    /// `None` only on networkless hosts; the dispatch path falls back
    /// to leaving `source.address` empty in that case (better than a
    /// misleading `127.0.0.1`).
    local_ip: Option<String>,
}

impl LocalRouterAmsTransport {
    pub fn new(router_host: String, router_port: u16, router: Arc<AdsRouter>) -> Self {
        Self {
            router_host,
            router_port,
            register_port: TC_OTEL_REGISTER_PORT,
            // Placeholder until the first PortConnect handshake completes.
            // run() overwrites this before the first ADS frame is dispatched.
            local_net_id: Arc::new(RwLock::new(AmsNetId::from_bytes([0, 0, 0, 0, 0, 0]))),
            router,
            local_ip: detect_primary_local_ip(),
        }
    }

    pub fn with_register_port(mut self, port: u16) -> Self {
        self.register_port = port;
        self
    }

    /// Build a 6-byte AMS/TCP framing header.
    fn make_amstcp_header(cmd: u16, len: u32) -> [u8; 6] {
        let mut h = [0u8; 6];
        h[0..2].copy_from_slice(&cmd.to_le_bytes());
        h[2..6].copy_from_slice(&len.to_le_bytes());
        h
    }

    /// Send `PortConnect` and return `(local_net_id, assigned_port)`.
    async fn handshake(
        stream: &mut TcpStream,
        register_port: u16,
    ) -> crate::Result<(AmsNetId, u16)> {
        let payload = register_port.to_le_bytes();
        let mut req = Vec::with_capacity(8);
        req.extend_from_slice(&Self::make_amstcp_header(
            AMS_TCP_CMD_PORT_CONNECT,
            payload.len() as u32,
        ));
        req.extend_from_slice(&payload);
        stream
            .write_all(&req)
            .await
            .map_err(crate::AdsError::IoError)?;

        let mut hdr = [0u8; 6];
        stream
            .read_exact(&mut hdr)
            .await
            .map_err(crate::AdsError::IoError)?;
        let cmd = u16::from_le_bytes([hdr[0], hdr[1]]);
        let len = u32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
        if cmd != AMS_TCP_CMD_PORT_CONNECT {
            return Err(crate::AdsError::BufferError(format!(
                "PortConnect: unexpected response cmd 0x{cmd:04x}"
            )));
        }
        if len < 8 {
            return Err(crate::AdsError::BufferError(format!(
                "PortConnect: short response ({len} bytes)"
            )));
        }
        let mut body = vec![0u8; len];
        stream
            .read_exact(&mut body)
            .await
            .map_err(crate::AdsError::IoError)?;

        let net_id = AmsNetId::from_bytes([body[0], body[1], body[2], body[3], body[4], body[5]]);
        let assigned = u16::from_le_bytes([body[6], body[7]]);
        Ok((net_id, assigned))
    }

    /// Frame loop: read AMS/TCP packets, dispatch ADS frames, write
    /// responses back on the same socket. Returns when the router
    /// closes the connection or an unrecoverable error occurs.
    async fn frame_loop(
        stream: &mut TcpStream,
        router: Arc<AdsRouter>,
        local_ip: Option<&str>,
    ) -> crate::Result<()> {
        let mut buf = vec![0u8; 16384];
        loop {
            // Read 6-byte AMS/TCP header.
            match stream.read_exact(&mut buf[..6]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::info!("local AMS router closed the connection");
                    return Ok(());
                }
                Err(e) => return Err(crate::AdsError::IoError(e)),
            }

            let cmd = u16::from_le_bytes([buf[0], buf[1]]);
            let data_len = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;

            // Match the TCP transport's 16 MiB ceiling — batched log writes
            // routinely exceed 1 MB.
            if data_len > 16 * 1_048_576 {
                tracing::warn!("local-router: oversized frame ({data_len}B), dropping connection");
                return Ok(());
            }
            if data_len > buf.len() {
                buf.resize(data_len, 0);
            }
            // PortConnect/ack control frames carry no body — skip the read.
            if data_len > 0 {
                stream
                    .read_exact(&mut buf[..data_len])
                    .await
                    .map_err(crate::AdsError::IoError)?;
            }

            match cmd {
                AMS_TCP_CMD_ADS => {
                    let frame = &buf[..data_len];
                    if frame.len() < 32 {
                        // Malformed AMS header; the router shouldn't send this.
                        // Skip without dropping the registration.
                        continue;
                    }
                    // Local-router transport: frames come from the Windows
                    // AMS router (TwinCAT runtime on the same IPC) over a
                    // loopback pipe. Report the IPC's primary interface
                    // IP (not `127.0.0.1`) so OTel dashboards can correlate
                    // logs/metrics with the host they came from.
                    match router.dispatch(frame, local_ip).await {
                        Ok(Some(response)) => {
                            let mut full = Vec::with_capacity(6 + response.len());
                            full.extend_from_slice(&Self::make_amstcp_header(
                                AMS_TCP_CMD_ADS,
                                response.len() as u32,
                            ));
                            full.extend_from_slice(&response);
                            if stream.write_all(&full).await.is_err() {
                                return Ok(());
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!("router dispatch error: {e}");
                        }
                    }
                }
                AMS_TCP_CMD_ROUTER_NOTE => {
                    // Router-state notifications. Useful for diagnostics but
                    // not actionable for tc-otel; log and continue.
                    tracing::debug!(
                        "AMS router notification ({}B): {:02x?}",
                        data_len,
                        &buf[..data_len.min(16)]
                    );
                }
                other => {
                    tracing::debug!("unexpected AMS/TCP cmd 0x{other:04x} ({data_len}B)");
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl AmsTransport for LocalRouterAmsTransport {
    async fn run(self: Arc<Self>) -> crate::Result<()> {
        let addr = format!("{}:{}", self.router_host, self.router_port);
        loop {
            tracing::info!(
                "connecting to local AMS router at {} (register port {})",
                addr,
                self.register_port
            );

            let mut stream = match TcpStream::connect(&addr).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("local-router connect failed: {e} — retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);

            let (net_id, assigned) = match Self::handshake(&mut stream, self.register_port).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("local-router PortConnect failed: {e} — retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            *self.local_net_id.write() = net_id;
            tracing::info!(
                "registered with local AMS router: netId={} port={}",
                net_id,
                assigned
            );
            if assigned != self.register_port {
                tracing::warn!(
                    "router assigned port {} (we requested {}); PLC must be configured \
                     to write to this port",
                    assigned,
                    self.register_port
                );
            }

            // Hand off to the read loop. On clean disconnect we reconnect.
            if let Err(e) = Self::frame_loop(
                &mut stream,
                self.router.clone(),
                self.local_ip.as_deref(),
            )
            .await
            {
                tracing::warn!("local-router frame loop error: {e}");
            }

            // Best-effort: ask the router to release our port before we drop
            // the socket. Failures here are not fatal.
            let close = Self::make_amstcp_header(AMS_TCP_CMD_PORT_CLOSE, 0);
            let _ = stream.write_all(&close).await;
            let _ = stream.shutdown().await;

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    async fn send(&self, _dest: AmsNetId, _frame: Vec<u8>) -> crate::Result<()> {
        // Outbound responses are written inline in `frame_loop` on the same
        // socket the request arrived on. We don't have a separate send path.
        Ok(())
    }

    fn local_net_id(&self) -> AmsNetId {
        // Sync RwLock — written once at handshake, occasionally on reconnect.
        // `local_net_id()` is called from sync contexts, so no async lock.
        *self.local_net_id.read()
    }
}

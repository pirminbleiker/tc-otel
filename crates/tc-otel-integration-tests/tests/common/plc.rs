//! Fake PLC: a minimal AMS/TCP client for driving the service over a socket.
//!
//! Uses only public `tc_otel_ads::ams` items. Every read is wrapped in a
//! timeout so a misbehaving service fails a test instead of hanging it.

use std::net::SocketAddr;
use std::time::Duration;

use tc_otel_ads::ams::{AmsHeader, AmsNetId, ADS_CMD_READ, ADS_CMD_WRITE};
use tc_otel_ads::ams::{ADS_STATE_REQUEST, ADS_STATE_RESPONSE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Default per-read timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Default receiver ADS port the service dispatches log and span writes on.
pub const DEFAULT_ADS_PORT: u16 = 16150;

/// Index group used for log WRITEs: any value other than `IG_PUSH_DIAG`.
pub const LOG_INDEX_GROUP: u32 = 1;

/// What `expect_closed` saw on the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// The peer closed the connection (EOF or reset).
    Closed,
    /// The connection stayed open (or only produced data) until the timeout.
    StillOpen,
}

/// A response frame: the AMS header plus the ADS payload.
#[derive(Debug, Clone)]
pub struct Response {
    pub header: AmsHeader,
    pub payload: Vec<u8>,
}

pub struct FakePlc {
    stream: TcpStream,
    source_net_id: AmsNetId,
    source_port: u16,
    target_net_id: AmsNetId,
    target_port: u16,
    next_invoke_id: u32,
}

impl FakePlc {
    /// Connect to the service's AMS/TCP listener.
    ///
    /// `target` is the service's `(net_id, ads_port)`; use
    /// [`DEFAULT_ADS_PORT`] for the receiver default.
    pub async fn connect(
        addr: SocketAddr,
        source_net_id: &str,
        source_port: u16,
        target: (&str, u16),
    ) -> std::io::Result<Self> {
        let parse = |s: &str| {
            s.parse::<AmsNetId>()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
        };
        let source_net_id = parse(source_net_id)?;
        let target_net_id = parse(target.0)?;
        let stream = timeout(IO_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")
            })??;
        Ok(Self {
            stream,
            source_net_id,
            source_port,
            target_net_id,
            target_port: target.1,
            next_invoke_id: 1,
        })
    }

    fn frame(&mut self, command_id: u16, payload: &[u8]) -> (Vec<u8>, u32) {
        let invoke_id = self.next_invoke_id;
        self.next_invoke_id = self.next_invoke_id.wrapping_add(1);
        let header = AmsHeader {
            target_net_id: self.target_net_id,
            target_port: self.target_port,
            source_net_id: self.source_net_id,
            source_port: self.source_port,
            command_id,
            state_flags: ADS_STATE_REQUEST,
            data_length: payload.len() as u32,
            error_code: 0,
            invoke_id,
        };
        let ams = header.serialize();
        let mut out = Vec::with_capacity(6 + ams.len() + payload.len());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&((ams.len() + payload.len()) as u32).to_le_bytes());
        out.extend_from_slice(&ams);
        out.extend_from_slice(payload);
        (out, invoke_id)
    }

    /// Send raw bytes, bypassing framing (for malformed-frame scenarios).
    pub async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await
    }

    async fn read_response(&mut self) -> std::io::Result<Response> {
        let mut prefix = [0u8; 6];
        timeout(IO_TIMEOUT, self.stream.read_exact(&mut prefix))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out"))??;
        let len = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
        if len < 32 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("response shorter than an AMS header: {len} bytes"),
            ));
        }
        let mut body = vec![0u8; len];
        timeout(IO_TIMEOUT, self.stream.read_exact(&mut body))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out"))??;
        let header = AmsHeader::parse(&body[..32])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Response {
            header,
            payload: body[32..].to_vec(),
        })
    }

    /// Send a request and read its response, checking command echo,
    /// response flag and invoke id.
    async fn request(&mut self, command_id: u16, payload: &[u8]) -> std::io::Result<Response> {
        let (bytes, invoke_id) = self.frame(command_id, payload);
        self.send_raw(&bytes).await?;
        let resp = self.read_response().await?;
        let bad = |what: &str| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad response: {what}"),
            )
        };
        if resp.header.command_id != command_id {
            return Err(bad("command id not echoed"));
        }
        if resp.header.state_flags & ADS_STATE_RESPONSE != ADS_STATE_RESPONSE {
            return Err(bad("response flag not set"));
        }
        if resp.header.invoke_id != invoke_id {
            return Err(bad("invoke id mismatch"));
        }
        Ok(resp)
    }

    /// ADS WRITE; returns the ADS result code from the ACK.
    pub async fn write(
        &mut self,
        index_group: u32,
        index_offset: u32,
        data: &[u8],
    ) -> std::io::Result<u32> {
        let mut payload = Vec::with_capacity(12 + data.len());
        payload.extend_from_slice(&index_group.to_le_bytes());
        payload.extend_from_slice(&index_offset.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
        payload.extend_from_slice(data);
        let resp = self.request(ADS_CMD_WRITE, &payload).await?;
        result_code(&resp.payload)
    }

    /// ADS WRITE of PLC log/span data on [`LOG_INDEX_GROUP`].
    pub async fn write_log_data(&mut self, data: &[u8]) -> std::io::Result<u32> {
        self.write(LOG_INDEX_GROUP, 0, data).await
    }

    /// ADS READ; returns the full response payload (`result(4) len(4) data`).
    pub async fn read(
        &mut self,
        index_group: u32,
        index_offset: u32,
        len: u32,
    ) -> std::io::Result<Vec<u8>> {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&index_group.to_le_bytes());
        payload.extend_from_slice(&index_offset.to_le_bytes());
        payload.extend_from_slice(&len.to_le_bytes());
        Ok(self.request(ADS_CMD_READ, &payload).await?.payload)
    }

    /// Read until EOF/reset or `wait` elapses, discarding any data.
    ///
    /// Records behaviour only; callers should not assert on it for
    /// malformed input.
    pub async fn expect_closed(&mut self, wait: Duration) -> Observed {
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.stream.read(&mut buf)).await {
                Err(_) => return Observed::StillOpen,
                Ok(Ok(0)) | Ok(Err(_)) => return Observed::Closed,
                Ok(Ok(_)) => continue,
            }
        }
    }
}

fn result_code(payload: &[u8]) -> std::io::Result<u32> {
    payload
        .get(..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response has no result code",
            )
        })
}

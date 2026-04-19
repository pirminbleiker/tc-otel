//! Dispatcher-backed client helpers for custom-metrics polls/notifications.
//!
//! The dispatcher lives in `tc-otel-ads`; this module only exposes the thin
//! glue that presents it as a [`SymbolReader`] trait (so the [`crate::poll`]
//! loop doesn't care whether reads go out over MQTT or TCP) and re-exports
//! the primitive scalar-decoder used by both the poll and notify paths.

use crate::error::{ClientError, Result};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::ams::AmsNetId;
use tc_otel_ads::dispatcher::{AmsDispatcher, DispatcherError};

/// Default ADS "read by handle" index group — only used when the caller
/// hasn't looked up the symbol's actual location yet. `SYM_UPLOAD` and
/// related IGs live in [`crate::browse`].
pub use tc_otel_ads::ams::ADS_CMD_READ;

/// Decoded PLC primitive suitable for promotion into a metric `f64`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlcValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
}

impl PlcValue {
    /// Lossy cast into `f64` for metric emission.
    pub fn as_f64(self) -> f64 {
        match self {
            PlcValue::Bool(b) => {
                if b {
                    1.0
                } else {
                    0.0
                }
            }
            PlcValue::I64(v) => v as f64,
            PlcValue::U64(v) => v as f64,
            PlcValue::F64(v) => v,
        }
    }
}

/// Metadata captured once at poller startup and reused on every read tick.
///
/// Service layer typically populates this from the cached
/// [`crate::browse::SymbolTree`].
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    pub size: u32,
    pub type_name: String,
    pub index_group: u32,
    pub index_offset: u32,
}

/// Read-only projection of the ADS client API. Real impl is [`DispatcherClient`];
/// tests plug in their own via `impl SymbolReader for FakeReader`.
pub trait SymbolReader: Send + Sync + 'static {
    /// Return the raw bytes for a symbol described by `meta`.
    fn read_raw(&self, meta: &SymbolMeta) -> Result<Vec<u8>>;

    /// Combined read + decode into a scalar.
    fn read_value(&self, meta: &SymbolMeta) -> Result<PlcValue> {
        let bytes = self.read_raw(meta)?;
        decode_scalar(&meta.type_name, &bytes).ok_or_else(|| {
            ClientError::Decode(format!(
                "unsupported scalar type '{}' (size {})",
                meta.type_name, meta.size
            ))
        })
    }
}

/// Dispatcher-backed symbol reader. Each `read_raw` call issues one
/// `ADS_CMD_READ` request via the shared [`AmsDispatcher`] and blocks on the
/// poll-worker thread until the invoke-id-matched response arrives.
///
/// The dispatcher chooses the transport (MQTT or TCP) based on its live
/// route table — clients don't care which one.
#[derive(Clone)]
pub struct DispatcherClient {
    dispatcher: Arc<AmsDispatcher>,
    target_net_id: AmsNetId,
    target_port: u16,
    request_timeout: Duration,
}

impl DispatcherClient {
    pub fn new(dispatcher: Arc<AmsDispatcher>, target_net_id: AmsNetId, target_port: u16) -> Self {
        Self {
            dispatcher,
            target_net_id,
            target_port,
            request_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn dispatcher(&self) -> Arc<AmsDispatcher> {
        self.dispatcher.clone()
    }

    pub fn target_net_id(&self) -> AmsNetId {
        self.target_net_id
    }

    pub fn target_port(&self) -> u16 {
        self.target_port
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Run an async `send_request` from a synchronous context (the
    /// [`SymbolReader`] trait, which is sync on the poll worker thread).
    /// If no Tokio runtime is available (tests without one), returns an
    /// error rather than panicking.
    fn block_on_send(
        &self,
        cmd: u16,
        payload: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, DispatcherError> {
        let dispatcher = self.dispatcher.clone();
        let target_net_id = self.target_net_id;
        let target_port = self.target_port;
        let timeout = self.request_timeout;
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            DispatcherError::Mqtt("no tokio runtime available for dispatcher call".into())
        })?;
        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                dispatcher
                    .send_request(target_net_id, target_port, cmd, &payload, timeout)
                    .await
            })
        })
    }
}

impl SymbolReader for DispatcherClient {
    fn read_raw(&self, meta: &SymbolMeta) -> Result<Vec<u8>> {
        // ADS Read request body: 4-byte index_group + 4-byte index_offset + 4-byte length.
        let mut req = Vec::with_capacity(12);
        req.extend_from_slice(&meta.index_group.to_le_bytes());
        req.extend_from_slice(&meta.index_offset.to_le_bytes());
        req.extend_from_slice(&meta.size.to_le_bytes());

        let raw = self
            .block_on_send(ADS_CMD_READ, req)
            .map_err(|e| ClientError::Ads(e.to_string()))?;

        // ADS Read response body: 4-byte result (ADS error) + 4-byte length + data.
        if raw.len() < 8 {
            return Err(ClientError::Decode(format!(
                "ADS read response too short: {} bytes",
                raw.len()
            )));
        }
        let result_code = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if result_code != 0 {
            return Err(ClientError::Ads(format!(
                "ADS read returned error 0x{result_code:x}"
            )));
        }
        let declared_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        let data = &raw[8..];
        if data.len() < declared_len {
            return Err(ClientError::Decode(format!(
                "ADS read payload truncated: declared {} bytes, got {}",
                declared_len,
                data.len()
            )));
        }
        Ok(data[..declared_len].to_vec())
    }
}

/// Decode a PLC scalar by declared IEC type name.
///
/// Returns `None` for unsupported types (STRING, arrays, structs) — the caller
/// should surface that as a configuration error, not panic.
pub fn decode_scalar(type_name: &str, bytes: &[u8]) -> Option<PlcValue> {
    let t = type_name.trim().to_ascii_uppercase();
    Some(match t.as_str() {
        "BOOL" if !bytes.is_empty() => PlcValue::Bool(bytes[0] != 0),
        "SINT" if !bytes.is_empty() => PlcValue::I64(bytes[0] as i8 as i64),
        "USINT" | "BYTE" if !bytes.is_empty() => PlcValue::U64(bytes[0] as u64),
        "INT" if bytes.len() >= 2 => PlcValue::I64(i16::from_le_bytes([bytes[0], bytes[1]]) as i64),
        "UINT" | "WORD" if bytes.len() >= 2 => {
            PlcValue::U64(u16::from_le_bytes([bytes[0], bytes[1]]) as u64)
        }
        "DINT" if bytes.len() >= 4 => {
            PlcValue::I64(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64)
        }
        "UDINT" | "DWORD" if bytes.len() >= 4 => {
            PlcValue::U64(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64)
        }
        "LINT" if bytes.len() >= 8 => PlcValue::I64(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        "ULINT" | "LWORD" if bytes.len() >= 8 => PlcValue::U64(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        "REAL" if bytes.len() >= 4 => {
            PlcValue::F64(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        }
        "LREAL" if bytes.len() >= 8 => PlcValue::F64(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_bool() {
        assert_eq!(decode_scalar("BOOL", &[0]).unwrap(), PlcValue::Bool(false));
        assert_eq!(decode_scalar("BOOL", &[1]).unwrap(), PlcValue::Bool(true));
    }

    #[test]
    fn decode_lreal() {
        let x: f64 = 1234.56789;
        let bytes = x.to_le_bytes();
        match decode_scalar("LREAL", &bytes).unwrap() {
            PlcValue::F64(v) => assert!((v - x).abs() < 1e-9),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn decode_real() {
        let x: f32 = 2.5;
        let bytes = x.to_le_bytes();
        match decode_scalar("REAL", &bytes).unwrap() {
            PlcValue::F64(v) => assert!((v - 2.5_f64).abs() < 1e-6),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn decode_dint_negative() {
        let bytes = (-42_i32).to_le_bytes();
        assert_eq!(decode_scalar("DINT", &bytes).unwrap(), PlcValue::I64(-42));
    }

    #[test]
    fn decode_uint_word_aliases() {
        let bytes = 0x1234_u16.to_le_bytes();
        assert_eq!(
            decode_scalar("UINT", &bytes).unwrap(),
            PlcValue::U64(0x1234)
        );
        assert_eq!(
            decode_scalar("WORD", &bytes).unwrap(),
            PlcValue::U64(0x1234)
        );
    }

    #[test]
    fn decode_unknown_type_returns_none() {
        assert!(decode_scalar("STRING", &[0; 80]).is_none());
        assert!(decode_scalar("ST_MyStruct", &[0; 4]).is_none());
    }

    #[test]
    fn decode_short_buffer_returns_none() {
        assert!(decode_scalar("LREAL", &[0; 4]).is_none());
        assert!(decode_scalar("DINT", &[0; 2]).is_none());
    }

    #[test]
    fn as_f64_bridges_all_variants() {
        assert_eq!(PlcValue::Bool(true).as_f64(), 1.0);
        assert_eq!(PlcValue::Bool(false).as_f64(), 0.0);
        assert_eq!(PlcValue::I64(-7).as_f64(), -7.0);
        assert_eq!(PlcValue::U64(42).as_f64(), 42.0);
        assert_eq!(PlcValue::F64(1.5).as_f64(), 1.5);
    }
}

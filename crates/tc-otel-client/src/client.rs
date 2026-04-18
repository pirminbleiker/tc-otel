//! Wrapper around `ads::Client` / `ads::Device` for active client sessions.
//!
//! The upstream `ads` crate is **blocking** and the `Client` type is `Send`
//! but not `Sync` (it uses interior mutability for the invoke-id counter and
//! notification-handle bookkeeping). To share a single TCP connection across
//! multiple pollers/notifiers driving the same target, we wrap it in a
//! `parking_lot::Mutex`. Each ADS round-trip is short (low-millisecond), so
//! contention is acceptable at typical metric rates.
//!
//! A [`SymbolReader`] trait abstracts the value-read path so tests can
//! substitute a stub under the `mock-plc` feature.

use crate::error::{ClientError, Result};
use ads::client::Source as AdsSource;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

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

/// Read-only projection of the ADS client API. Real impl is [`AdsClient`];
/// tests can plug in their own under the `mock-plc` feature.
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

/// Real `ads::Client`-backed implementation.
///
/// A single `AdsClient` holds **one** TCP connection to the PLC's AMS router
/// and may be cloned (it is `Clone`, sharing the underlying `Arc<Mutex<_>>`)
/// for multiple pollers/notifiers driving the same target.
#[derive(Clone)]
pub struct AdsClient {
    client: Arc<Mutex<ads::Client>>,
    target: ads::AmsAddr,
}

impl AdsClient {
    /// Dial the PLC's AMS router.
    ///
    /// `router_addr` is `"host:48898"` (usually the PLC's IP). `source` is our
    /// own AMS identity; use `Source::Auto` when driving a remote PLC and
    /// `Source::Request` when connecting to a local TwinCAT router.
    ///
    /// This call blocks. Wrap in `spawn_blocking` from async contexts.
    pub fn connect(
        router_addr: &str,
        source: AdsSource,
        target: ads::AmsAddr,
        timeouts: ads::Timeouts,
    ) -> Result<Self> {
        let client = ads::Client::new(router_addr, timeouts, source)
            .map_err(|e| ClientError::Ads(e.to_string()))?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            target,
        })
    }

    /// Convenience constructor: short timeouts + `Source::Auto`.
    pub fn connect_auto(router_addr: &str, target: ads::AmsAddr) -> Result<Self> {
        Self::connect(
            router_addr,
            AdsSource::Auto,
            target,
            ads::Timeouts::new(Duration::from_secs(5)),
        )
    }

    /// Connect using a caller-supplied source NetID (ephemeral port 58913).
    ///
    /// This is the typical constructor used by the service bridge — it ensures
    /// the peer identity matches the NetID the PLC expects in its route table.
    pub fn connect_with_source_netid(
        router_addr: &str,
        source_netid: ads::AmsNetId,
        target: ads::AmsAddr,
    ) -> Result<Self> {
        Self::connect(
            router_addr,
            AdsSource::Addr(ads::AmsAddr::new(source_netid, 58913)),
            target,
            ads::Timeouts::new(Duration::from_secs(5)),
        )
    }

    /// Register an AMS route on the target PLC via its UDP discovery port
    /// (48899). Necessary before `connect_*` on a PLC that doesn't already
    /// have a static route to our NetID.
    ///
    /// `source_netid` is our own NetID. `our_host` is a short hostname or IP
    /// the PLC should record as the route's peer — used by the PLC only for
    /// display and rDNS.
    ///
    /// Blocking — call from `spawn_blocking` in async contexts.
    pub fn add_route(target_host: &str, source_netid: ads::AmsNetId, our_host: &str) -> Result<()> {
        ads::udp::add_route(
            (target_host, 48899),
            source_netid,
            our_host,
            None,
            None,
            None,
            /* temporary */ true,
        )
        .map_err(|e| ClientError::Ads(format!("add_route: {e}")))
    }

    /// Access the underlying `ads::Client` under the mutex. Mainly used by
    /// `poll` and `notify` modules to issue domain-specific calls.
    pub fn with_client<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&ads::Client, ads::AmsAddr) -> R,
    {
        let guard = self.client.lock();
        f(&guard, self.target)
    }

    /// Resolve a symbol name via the PLC's `GET_SYMINFO_BYNAME` index group.
    /// Returns (index_group, index_offset, size). Type name is not returned —
    /// it must come from the cached [`crate::browse::SymbolTree`] (or be
    /// provided out of band).
    pub fn resolve_location(&self, symbol: &str) -> Result<(u32, u32, u32)> {
        self.with_client(|client, target| {
            let device = client.device(target);
            let loc = ads::symbol::get_location(device, symbol)
                .map_err(|e| ClientError::Ads(e.to_string()))?;
            let size = ads::symbol::get_size(device, symbol)
                .map_err(|e| ClientError::Ads(e.to_string()))?;
            Ok((loc.0, loc.1, size as u32))
        })
    }

    pub fn target(&self) -> ads::AmsAddr {
        self.target
    }

    /// Dump the entire symbol table of the target PLC.
    ///
    /// This is a heavy call (tens to hundreds of kilobytes over AMS, plus the
    /// type-map upload). Blocking — call from `spawn_blocking`.
    ///
    /// The service bridge uses this once per target at connection time to
    /// populate [`crate::cache::SymbolTreeCache`]. The returned tree is also
    /// what the web UI (T7/T8) consumes for symbol browsing.
    pub fn upload_symbols(&self) -> Result<crate::browse::SymbolTree> {
        self.with_client(|client, target| {
            let (symbols, _types) = ads::symbol::get_symbol_info(client.device(target))
                .map_err(|e| ClientError::Ads(e.to_string()))?;
            Ok(crate::browse::SymbolTree::from_ads_symbols(symbols))
        })
    }
}

impl SymbolReader for AdsClient {
    fn read_raw(&self, meta: &SymbolMeta) -> Result<Vec<u8>> {
        self.with_client(|client, target| {
            let mut buf = vec![0u8; meta.size as usize];
            client
                .device(target)
                .read_exact(meta.index_group, meta.index_offset, &mut buf)
                .map_err(|e| ClientError::Ads(e.to_string()))?;
            Ok(buf)
        })
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

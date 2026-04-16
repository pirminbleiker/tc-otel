//! Stateful observer that consumes raw AMS frames and emits [`DiagEvent`]s.
//!
//! Wraps the pure [`crate::diagnostics`] decoder with the invoke-id
//! correlation needed to pair requests and responses. Designed to be fed from
//! a passive MQTT wildcard subscriber that sees both sides of the
//! IDE ↔ PLC conversation without participating in the AMS routing.
//!
//! Unknown frames cost one hash-map lookup and are dropped with no allocation.
//!
//! Pending-request entries expire after [`DiagnosticsObserver::PENDING_TTL`] to bound memory
//! when a request's response is lost or dropped.

use crate::ams::AmsHeader;
use crate::diagnostics::{
    decode_request, decode_response, decode_write_from_request, DiagEvent, PendingRequest,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Correlation key: `(target_net_id, source_net_id, invoke_id)` on the
/// request side. Responses swap the two net IDs, so the observer rebuilds
/// the key with swapped operands when looking up.
type CorrelationKey = ([u8; 6], [u8; 6], u32);

#[derive(Debug, Clone, Copy)]
struct PendingEntry {
    req: PendingRequest,
    inserted: Instant,
}

/// Stateful wrapper around the diagnostics decoder.
#[derive(Debug)]
pub struct DiagnosticsObserver {
    pending: HashMap<CorrelationKey, PendingEntry>,
    last_gc: Instant,
}

impl DiagnosticsObserver {
    /// How long a pending request stays in the correlation map before being
    /// considered orphaned. 30 s easily covers the slowest observed poll RTT.
    pub const PENDING_TTL: Duration = Duration::from_secs(30);

    /// Max entries held before GC is forced irrespective of [`Self::GC_INTERVAL`].
    const MAX_PENDING: usize = 8_192;

    /// Periodic GC interval.
    const GC_INTERVAL: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            last_gc: Instant::now(),
        }
    }

    /// Current size of the correlation table. Exposed for health metrics.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Feed one raw AMS frame. Returns `Some(event)` when the frame (or an
    /// earlier correlated request) yields a diagnostic event.
    ///
    /// Writes are decoded directly from the request side; reads need the
    /// response to carry semantic payload, so the first call (request)
    /// returns `None` and the second (response) returns the event.
    pub fn feed(&mut self, header: &AmsHeader, payload: &[u8]) -> Option<DiagEvent> {
        self.maybe_gc();

        let is_request = header.state_flags & 0x0001 == 0;
        if is_request {
            // Writes have semantic payload in the request; emit immediately
            // when recognised, and don't bother adding to the pending map.
            if let Some(ev) = decode_write_from_request(header, payload) {
                return Some(ev);
            }
            // Known reads → stash for later correlation with response.
            if let Some((invoke, req)) = decode_request(header, payload) {
                let key = (
                    *header.target_net_id.bytes(),
                    *header.source_net_id.bytes(),
                    invoke,
                );
                // Bound the map: if saturation is hit, drop the oldest by
                // forcing a GC sweep first.
                if self.pending.len() >= Self::MAX_PENDING {
                    self.gc_now();
                }
                self.pending.insert(
                    key,
                    PendingEntry {
                        req,
                        inserted: Instant::now(),
                    },
                );
            }
            return None;
        }

        // Response path. Rebuild the key with net IDs swapped back to the
        // original request's perspective.
        let key = (
            *header.source_net_id.bytes(),
            *header.target_net_id.bytes(),
            header.invoke_id,
        );
        let entry = self.pending.remove(&key)?;
        decode_response(header, payload, &entry.req)
    }

    fn maybe_gc(&mut self) {
        if self.last_gc.elapsed() < Self::GC_INTERVAL {
            return;
        }
        self.gc_now();
    }

    fn gc_now(&mut self) {
        let cutoff = Instant::now() - Self::PENDING_TTL;
        self.pending.retain(|_, e| e.inserted >= cutoff);
        self.last_gc = Instant::now();
    }
}

impl Default for DiagnosticsObserver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ams::{AmsNetId, ADS_CMD_READ, ADS_CMD_WRITE, ADS_STATE_REQUEST};

    /// ADS state-flags for a successful response frame (response bit + TCP bit).
    const RESPONSE_OK: u16 = 0x0005;
    use crate::diagnostics::{
        IG_RT_SYSTEM, IG_RT_USAGE, IO_EXCEED_COUNTER, IO_RT_USAGE, IO_TASK_STATS,
    };

    fn net(a: [u8; 6]) -> AmsNetId {
        AmsNetId::from_bytes(a)
    }

    fn req_header(target_port: u16, cmd: u16, invoke: u32) -> AmsHeader {
        AmsHeader {
            target_net_id: net([10, 10, 10, 10, 1, 1]),
            target_port,
            source_net_id: net([75, 3, 166, 18, 1, 1]),
            source_port: 43568,
            command_id: cmd,
            state_flags: ADS_STATE_REQUEST,
            data_length: 0,
            error_code: 0,
            invoke_id: invoke,
        }
    }

    fn resp_header(source_port: u16, cmd: u16, invoke: u32) -> AmsHeader {
        AmsHeader {
            target_net_id: net([75, 3, 166, 18, 1, 1]),
            target_port: 43568,
            source_net_id: net([10, 10, 10, 10, 1, 1]),
            source_port,
            command_id: cmd,
            state_flags: RESPONSE_OK,
            data_length: 0,
            error_code: 0,
            invoke_id: invoke,
        }
    }

    fn read_req_pl(ig: u32, io: u32, size: u32) -> Vec<u8> {
        [ig.to_le_bytes(), io.to_le_bytes(), size.to_le_bytes()].concat()
    }

    fn read_resp_pl(result: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + data.len());
        v.extend_from_slice(&result.to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    fn write_req_pl(ig: u32, io: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + data.len());
        v.extend_from_slice(&ig.to_le_bytes());
        v.extend_from_slice(&io.to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn correlates_read_request_and_response() {
        let mut obs = DiagnosticsObserver::new();

        let req_h = req_header(100, ADS_CMD_READ, 77);
        let req_pl = read_req_pl(IG_RT_SYSTEM, IO_EXCEED_COUNTER, 4);
        assert_eq!(obs.feed(&req_h, &req_pl), None);
        assert_eq!(obs.pending_len(), 1);

        let resp_h = resp_header(100, ADS_CMD_READ, 77);
        let resp_pl = read_resp_pl(0, &0x1234_u32.to_le_bytes());
        assert_eq!(
            obs.feed(&resp_h, &resp_pl),
            Some(DiagEvent::ExceedCounter { value: 0x1234 })
        );
        assert_eq!(obs.pending_len(), 0, "response must drain pending entry");
    }

    #[test]
    fn task_stats_flow_emits_event() {
        let mut obs = DiagnosticsObserver::new();
        let req_h = req_header(350, ADS_CMD_READ, 99);
        let req_pl = read_req_pl(IG_RT_SYSTEM, IO_TASK_STATS, 16);
        obs.feed(&req_h, &req_pl);

        let data = [
            0x07, 0x03, 0x71, 0x00, 0x65, 0x2e, 0xb9, 0xcf, 0x8a, 0x76, 0x59, 0x8b, 0x00, 0x00,
            0x00, 0x00,
        ];
        let resp_h = resp_header(350, ADS_CMD_READ, 99);
        let resp_pl = read_resp_pl(0, &data);
        let ev = obs.feed(&resp_h, &resp_pl).unwrap();
        assert!(matches!(ev, DiagEvent::TaskStats { task_port: 350, .. }));
    }

    #[test]
    fn rt_usage_flow_emits_event() {
        let mut obs = DiagnosticsObserver::new();
        let req_h = req_header(200, ADS_CMD_READ, 50);
        let req_pl = read_req_pl(IG_RT_USAGE, IO_RT_USAGE, 1536);
        obs.feed(&req_h, &req_pl);

        let data = [
            0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0xfa, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00,
        ];
        let resp_h = resp_header(200, ADS_CMD_READ, 50);
        let resp_pl = read_resp_pl(0, &data);
        let ev = obs.feed(&resp_h, &resp_pl).unwrap();
        assert!(matches!(
            ev,
            DiagEvent::RtUsage {
                cpu_percent: 7,
                system_latency_us: 250,
                peak_latency_us: 12
            }
        ));
    }

    #[test]
    fn write_request_emits_without_response() {
        let mut obs = DiagnosticsObserver::new();
        let req_h = req_header(100, ADS_CMD_WRITE, 1);
        let req_pl = write_req_pl(IG_RT_SYSTEM, IO_EXCEED_COUNTER, &[0, 0, 0, 0]);
        assert_eq!(obs.feed(&req_h, &req_pl), Some(DiagEvent::ExceedReset));
        assert_eq!(obs.pending_len(), 0, "writes shouldn't enter pending map");
    }

    #[test]
    fn response_without_matching_request_is_ignored() {
        let mut obs = DiagnosticsObserver::new();
        let resp_h = resp_header(100, ADS_CMD_READ, 404);
        let resp_pl = read_resp_pl(0, &0x1234_u32.to_le_bytes());
        assert_eq!(obs.feed(&resp_h, &resp_pl), None);
    }

    #[test]
    fn unknown_request_is_not_pended() {
        let mut obs = DiagnosticsObserver::new();
        let req_h = req_header(852, ADS_CMD_READ, 1);
        let req_pl = read_req_pl(0xdead, 0xbeef, 16);
        obs.feed(&req_h, &req_pl);
        assert_eq!(obs.pending_len(), 0);
    }

    #[test]
    fn multiple_concurrent_invokes_demux_correctly() {
        let mut obs = DiagnosticsObserver::new();

        // Three concurrent reads on three task ports.
        for (port, invoke) in [(340, 1_u32), (350, 2), (351, 3)] {
            obs.feed(
                &req_header(port, ADS_CMD_READ, invoke),
                &read_req_pl(IG_RT_SYSTEM, IO_TASK_STATS, 16),
            );
        }
        assert_eq!(obs.pending_len(), 3);

        // Responses arrive out of order.
        let data = [0_u8; 16];
        let ev = obs
            .feed(&resp_header(351, ADS_CMD_READ, 3), &read_resp_pl(0, &data))
            .unwrap();
        assert!(matches!(ev, DiagEvent::TaskStats { task_port: 351, .. }));
        let ev = obs
            .feed(&resp_header(340, ADS_CMD_READ, 1), &read_resp_pl(0, &data))
            .unwrap();
        assert!(matches!(ev, DiagEvent::TaskStats { task_port: 340, .. }));
        let ev = obs
            .feed(&resp_header(350, ADS_CMD_READ, 2), &read_resp_pl(0, &data))
            .unwrap();
        assert!(matches!(ev, DiagEvent::TaskStats { task_port: 350, .. }));
        assert_eq!(obs.pending_len(), 0);
    }

    #[test]
    fn gc_evicts_stale_pending_entries() {
        let mut obs = DiagnosticsObserver::new();
        obs.feed(
            &req_header(100, ADS_CMD_READ, 1),
            &read_req_pl(IG_RT_SYSTEM, IO_EXCEED_COUNTER, 4),
        );
        assert_eq!(obs.pending_len(), 1);

        // Forcibly age the single entry past the TTL, then trigger GC.
        for entry in obs.pending.values_mut() {
            entry.inserted =
                Instant::now() - DiagnosticsObserver::PENDING_TTL - Duration::from_secs(1);
        }
        obs.gc_now();
        assert_eq!(obs.pending_len(), 0);
    }
}

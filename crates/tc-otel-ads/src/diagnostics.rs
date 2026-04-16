//! TwinCAT runtime-diagnostics decoder.
//!
//! Passive decoder for the diagnostic polls the TwinCAT engineering (XAE) IDE
//! issues when windows like "Real-Time Usage + System Latency", "PlcTask Online",
//! or "Exceed Counter" are open. All observed traffic is plain `AdsRead` /
//! `AdsWrite` polling on fixed (index-group, index-offset) pairs — no ADS
//! notifications are used.
//!
//! Payload layouts were verified against captures in `captures/` with known
//! cycle-time configuration (PlcTask 1 ms, PlcTask1 10 ms, I/O Idle 1 ms).
//! See `docs/ads-diagnostics-integration-plan.md` §7 for details.
//!
//! Usage pattern: responses don't carry the index group/offset — only the
//! request does. The caller correlates by invoke-id:
//!
//! 1. On each **request** frame, call [`decode_request`]. If `Some`, store the
//!    returned [`PendingRequest`] keyed by `(source_net_id, target_net_id,
//!    invoke_id)`.
//! 2. On each **response** frame, look up the pending request by the matching
//!    key (with src/dst swapped) and call [`decode_response`].
//! 3. Emit the resulting [`DiagEvent`] to the metrics pipeline.

use crate::ams::{AmsHeader, ADS_CMD_READ, ADS_CMD_WRITE, ADS_STATE_REQUEST};

/// ADS state-flags mask selecting the response bit.
const ADS_STATE_RESPONSE_MASK: u16 = 0x0001;

/// Index group for the TwinCAT system/realtime diagnostic service.
pub const IG_RT_SYSTEM: u32 = 0x0000_F200;

/// Index group for the RT-usage + system-latency block (port 200).
pub const IG_RT_USAGE: u32 = 0x0000_0001;

/// Index offset of the exceed counter inside `IG_RT_SYSTEM`.
pub const IO_EXCEED_COUNTER: u32 = 0x0000_0100;

/// Index offset of the task-stats block inside `IG_RT_SYSTEM` (port-scoped).
pub const IO_TASK_STATS: u32 = 0x0000_0000;

/// Index offset of the RT-usage + latency block inside `IG_RT_USAGE`.
pub const IO_RT_USAGE: u32 = 0x0000_000F;

/// Index group for push-diagnostic events (ADS Write targets).
/// Encoded as ASCII "MBC\1" in little-endian byte order: 0x4D42_4301.
/// Chosen for debug-friendliness and collision safety.
pub const IG_PUSH_DIAG: u32 = 0x4D42_4301;

/// Index offset for task snapshots within `IG_PUSH_DIAG`.
pub const IO_PUSH_SNAPSHOT: u32 = 0;

/// Index offset for cycle-exceed edge events within `IG_PUSH_DIAG`.
pub const IO_PUSH_CYCLE_EXCEED_EDGE: u32 = 1;

/// Index offset for RT-violation edge events within `IG_PUSH_DIAG`.
pub const IO_PUSH_RT_VIOLATION_EDGE: u32 = 2;

/// Wire-format version for push-diagnostic events.
pub const PUSH_WIRE_VERSION: u8 = 1;

/// Flag: cycle-exceed condition is active on the current cycle.
pub const PUSH_FLAG_CYCLE_EXCEED_NOW: u32 = 1 << 0;

/// Flag: RT-violation condition is active on the current cycle.
pub const PUSH_FLAG_RT_VIOLATION_NOW: u32 = 1 << 1;

/// Flag: first cycle after PLC start (initial condition).
pub const PUSH_FLAG_FIRST_CYCLE: u32 = 1 << 2;

/// AMS port of the TwinCAT realtime subsystem.
pub const AMSPORT_R0_REALTIME: u16 = 200;

/// Expected response size for the task-stats payload.
pub const TASK_STATS_LEN: usize = 16;

/// Expected response size for the exceed-counter value.
pub const EXCEED_COUNTER_LEN: usize = 4;

/// Expected response size for the RT-usage block. The IDE over-allocates the
/// read buffer (rsz=1536); the PLC returns exactly 24 bytes.
pub const RT_USAGE_LEN: usize = 24;

/// Example task-stats type marker observed for PLC tasks in one target.
///
/// **Target-specific, not universal.** The low byte of the `type_marker`
/// field has been seen to vary across TwinCAT runtimes and project
/// configurations (e.g. 0x71, 0xC0). Do not branch on these constants in
/// decode logic — just surface the raw `type_marker` field.
pub const TASK_MARKER_PLC: u16 = 0x0071;

/// Example task-stats type marker observed for an I/O-idle task. See the
/// note on [`TASK_MARKER_PLC`] — values differ between runtimes.
pub const TASK_MARKER_IDLE: u16 = 0x000B;

/// Pending-request context the caller tracks by invoke-id.
///
/// Responses don't carry the request semantics (index group / offset);
/// callers must correlate request and response by `invoke_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRequest {
    pub command_id: u16,
    pub index_group: u32,
    pub index_offset: u32,
    /// For `Read`: requested read size. For `Write`: write-payload size.
    pub size: u32,
    /// AMS target port of the original request.
    pub target_port: u16,
}

/// A decoded diagnostic event ready for metric emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagEvent {
    /// Current value of the TwinCAT cycle-exceed counter.
    ExceedCounter { value: u32 },
    /// Write to the exceed-counter offset (IDE reset click). Payload is
    /// typically four zero bytes.
    ExceedReset,
    /// Per-task cycle / CPU-time snapshot.
    ///
    /// The AMS port identifies the task (each runtime task owns a dedicated
    /// port — 340 = I/O idle, 350 = PlcTask, 351 = PlcTask1, and so on).
    TaskStats {
        task_port: u16,
        type_marker: u16,
        /// 16-bit task cycle counter. Wraps every 65 536 cycles. Increments
        /// at `1000 / base_time_ms` Hz.
        cycle_counter: u16,
        /// CPU-time accumulator in 100 ns units.
        cpu_ticks_100ns: u32,
        /// Exec/total-time accumulator in 100 ns units.
        exec_ticks_100ns: u32,
    },
    /// Real-time system usage + latency snapshot (port 200).
    ///
    /// Fields listed in payload-offset order.
    RtUsage {
        /// Peak / transient latency tracker in microseconds (payload +0x04).
        peak_latency_us: u32,
        /// Current system latency in microseconds (payload +0x08).
        system_latency_us: u32,
        /// Current CPU usage as whole percent, 0–100 (payload +0x10).
        cpu_percent: u32,
    },
    /// Push-diagnostic: per-task cycle / execution-time / RT-state snapshot.
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_SNAPSHOT`. Contains
    /// configuration, execution stats, and edge-condition flags for all tasks
    /// in a single push event, avoiding the ~200 ms sampling gap of polled
    /// diagnostics.
    TaskSnapshot {
        task_port: u16,
        task_name: String,
        priority: u32,
        cycle_time_configured_us: u32,
        last_exec_time_us: u32,
        cycle_count: u64,
        cycle_exceed_count: u64,
        rt_violation_count: u64,
        flags: u32,
        plc_timestamp_ns: u64,
    },
    /// Push-diagnostic: edge event when a task exceeded its cycle time.
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_CYCLE_EXCEED_EDGE`.
    /// Allows cycle-exceed transients to be captured that would be invisible
    /// in 5 Hz polling.
    CycleExceedEdge {
        task_port: u16,
        task_name: String,
        cycle_count: u64,
        last_exec_time_us: u32,
    },
    /// Push-diagnostic: edge event when a task violated real-time guarantees.
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_RT_VIOLATION_EDGE`.
    /// Allows RT violations to be captured that would be invisible in
    /// polled diagnostics.
    RtViolationEdge {
        task_port: u16,
        task_name: String,
        cycle_count: u64,
        last_exec_time_us: u32,
    },
}

/// Decode a request frame into a [`PendingRequest`] if it matches a
/// diagnostic poll we care about. Returns the invoke-id alongside so the
/// caller can key its correlation map.
///
/// Unknown frames return `None`.
pub fn decode_request(header: &AmsHeader, payload: &[u8]) -> Option<(u32, PendingRequest)> {
    if header.state_flags != ADS_STATE_REQUEST {
        return None;
    }
    match header.command_id {
        ADS_CMD_READ | ADS_CMD_WRITE => {
            if payload.len() < 12 {
                return None;
            }
            let ig = u32::from_le_bytes(payload[0..4].try_into().ok()?);
            let io = u32::from_le_bytes(payload[4..8].try_into().ok()?);
            let size = u32::from_le_bytes(payload[8..12].try_into().ok()?);
            let known = match header.command_id {
                ADS_CMD_READ => is_known_read(ig, io),
                ADS_CMD_WRITE => is_known_write(ig, io),
                _ => false,
            };
            if !known {
                return None;
            }
            Some((
                header.invoke_id,
                PendingRequest {
                    command_id: header.command_id,
                    index_group: ig,
                    index_offset: io,
                    size,
                    target_port: header.target_port,
                },
            ))
        }
        _ => None,
    }
}

/// Decode a response frame into a [`DiagEvent`] given the matching pending
/// request. Returns `None` if the response is malformed, reports an error,
/// or doesn't match a known decoder.
pub fn decode_response(
    header: &AmsHeader,
    payload: &[u8],
    req: &PendingRequest,
) -> Option<DiagEvent> {
    if header.state_flags & ADS_STATE_RESPONSE_MASK == 0 {
        return None;
    }
    match req.command_id {
        ADS_CMD_READ => decode_read_response(payload, req),
        ADS_CMD_WRITE => decode_write_response(req),
        _ => None,
    }
}

/// Decode the write-payload inside a request directly (writes don't need a
/// round-trip — the semantic payload is in the request itself).
///
/// Use this as an alternative to [`decode_request`] + [`decode_response`]
/// when the caller only sees the request side.
pub fn decode_write_from_request(header: &AmsHeader, payload: &[u8]) -> Option<DiagEvent> {
    if header.state_flags != ADS_STATE_REQUEST
        || header.command_id != ADS_CMD_WRITE
        || payload.len() < 12
    {
        return None;
    }
    let ig = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let io = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    match (ig, io) {
        (IG_RT_SYSTEM, IO_EXCEED_COUNTER) => Some(DiagEvent::ExceedReset),
        _ => None,
    }
}

fn is_known_read(ig: u32, io: u32) -> bool {
    matches!(
        (ig, io),
        (IG_RT_SYSTEM, IO_EXCEED_COUNTER)
            | (IG_RT_SYSTEM, IO_TASK_STATS)
            | (IG_RT_USAGE, IO_RT_USAGE)
    )
}

fn is_known_write(ig: u32, io: u32) -> bool {
    matches!((ig, io), (IG_RT_SYSTEM, IO_EXCEED_COUNTER))
}

fn decode_read_response(payload: &[u8], req: &PendingRequest) -> Option<DiagEvent> {
    // Response payload: u32 result + u32 length + data[length].
    if payload.len() < 8 {
        return None;
    }
    let result = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let rlen = u32::from_le_bytes(payload[4..8].try_into().ok()?) as usize;
    if result != 0 {
        return None;
    }
    let data = payload.get(8..8 + rlen)?;

    match (req.index_group, req.index_offset) {
        (IG_RT_SYSTEM, IO_EXCEED_COUNTER) if rlen == EXCEED_COUNTER_LEN => {
            Some(DiagEvent::ExceedCounter {
                value: u32::from_le_bytes(data.try_into().ok()?),
            })
        }
        (IG_RT_SYSTEM, IO_TASK_STATS) if rlen == TASK_STATS_LEN => Some(DiagEvent::TaskStats {
            task_port: req.target_port,
            type_marker: u16::from_le_bytes(data[2..4].try_into().ok()?),
            cycle_counter: u16::from_le_bytes(data[0..2].try_into().ok()?),
            cpu_ticks_100ns: u32::from_le_bytes(data[4..8].try_into().ok()?),
            exec_ticks_100ns: u32::from_le_bytes(data[8..12].try_into().ok()?),
        }),
        (IG_RT_USAGE, IO_RT_USAGE) if rlen == RT_USAGE_LEN => Some(DiagEvent::RtUsage {
            peak_latency_us: u32::from_le_bytes(data[4..8].try_into().ok()?),
            system_latency_us: u32::from_le_bytes(data[8..12].try_into().ok()?),
            cpu_percent: u32::from_le_bytes(data[16..20].try_into().ok()?),
        }),
        _ => None,
    }
}

fn decode_write_response(req: &PendingRequest) -> Option<DiagEvent> {
    match (req.index_group, req.index_offset) {
        (IG_RT_SYSTEM, IO_EXCEED_COUNTER) => Some(DiagEvent::ExceedReset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ams::AmsNetId;

    /// ADS state-flags for a successful response frame (response bit + TCP bit).
    const RESPONSE_OK: u16 = 0x0005;

    fn net(a: [u8; 6]) -> AmsNetId {
        AmsNetId::from_bytes(a)
    }

    fn request_header(target_port: u16, command_id: u16, invoke_id: u32, dlen: u32) -> AmsHeader {
        AmsHeader {
            target_net_id: net([10, 10, 10, 10, 1, 1]),
            target_port,
            source_net_id: net([75, 3, 166, 18, 1, 1]),
            source_port: 43568,
            command_id,
            state_flags: ADS_STATE_REQUEST,
            data_length: dlen,
            error_code: 0,
            invoke_id,
        }
    }

    fn response_header(source_port: u16, command_id: u16, invoke_id: u32, dlen: u32) -> AmsHeader {
        AmsHeader {
            target_net_id: net([75, 3, 166, 18, 1, 1]),
            target_port: 43568,
            source_net_id: net([10, 10, 10, 10, 1, 1]),
            source_port,
            command_id,
            state_flags: RESPONSE_OK,
            data_length: dlen,
            error_code: 0,
            invoke_id,
        }
    }

    fn read_req_payload(ig: u32, io: u32, size: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(12);
        v.extend_from_slice(&ig.to_le_bytes());
        v.extend_from_slice(&io.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v
    }

    fn read_resp_payload(result: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + data.len());
        v.extend_from_slice(&result.to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    fn write_req_payload(ig: u32, io: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + data.len());
        v.extend_from_slice(&ig.to_le_bytes());
        v.extend_from_slice(&io.to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn exceed_counter_read_decodes() {
        let req = request_header(100, ADS_CMD_READ, 42, 12);
        let pl = read_req_payload(IG_RT_SYSTEM, IO_EXCEED_COUNTER, 4);
        let (invoke, pending) = decode_request(&req, &pl).expect("known read");
        assert_eq!(invoke, 42);
        assert_eq!(pending.index_group, IG_RT_SYSTEM);
        assert_eq!(pending.index_offset, IO_EXCEED_COUNTER);

        let resp = response_header(100, ADS_CMD_READ, 42, 12);
        let rpl = read_resp_payload(0, &0x1eead_u32.to_le_bytes());
        let ev = decode_response(&resp, &rpl, &pending).unwrap();
        assert_eq!(ev, DiagEvent::ExceedCounter { value: 0x1eead });
    }

    #[test]
    fn exceed_reset_write_decodes_from_request() {
        let req = request_header(100, ADS_CMD_WRITE, 1, 16);
        let pl = write_req_payload(IG_RT_SYSTEM, IO_EXCEED_COUNTER, &[0, 0, 0, 0]);
        assert_eq!(
            decode_write_from_request(&req, &pl),
            Some(DiagEvent::ExceedReset)
        );
    }

    #[test]
    fn task_stats_decodes_from_real_capture() {
        // Captured sample from plctask_run.log (port 350 = PlcTask @ 1 ms):
        // payload 16B = 07 03 71 00  65 2e b9 cf  8a 76 59 8b  00 00 00 00
        let req = request_header(350, ADS_CMD_READ, 7, 12);
        let pl = read_req_payload(IG_RT_SYSTEM, IO_TASK_STATS, 16);
        let (_, pending) = decode_request(&req, &pl).unwrap();

        let data = [
            0x07, 0x03, 0x71, 0x00, 0x65, 0x2e, 0xb9, 0xcf, 0x8a, 0x76, 0x59, 0x8b, 0x00, 0x00,
            0x00, 0x00,
        ];
        let resp = response_header(350, ADS_CMD_READ, 7, (8 + data.len()) as u32);
        let rpl = read_resp_payload(0, &data);
        let ev = decode_response(&resp, &rpl, &pending).unwrap();
        assert_eq!(
            ev,
            DiagEvent::TaskStats {
                task_port: 350,
                type_marker: TASK_MARKER_PLC,
                cycle_counter: 0x0307,
                cpu_ticks_100ns: 0xcfb9_2e65,
                exec_ticks_100ns: 0x8b59_768a,
            }
        );
    }

    #[test]
    fn task_stats_idle_marker() {
        // Captured from port 340 (I/O idle @ 1 ms):
        // 69 50 0b 00 2a 56 d6 07 bf b9 72 ed 00 00 00 00
        let req = request_header(340, ADS_CMD_READ, 8, 12);
        let pl = read_req_payload(IG_RT_SYSTEM, IO_TASK_STATS, 16);
        let (_, pending) = decode_request(&req, &pl).unwrap();

        let data = [
            0x69, 0x50, 0x0b, 0x00, 0x2a, 0x56, 0xd6, 0x07, 0xbf, 0xb9, 0x72, 0xed, 0x00, 0x00,
            0x00, 0x00,
        ];
        let resp = response_header(340, ADS_CMD_READ, 8, (8 + data.len()) as u32);
        let rpl = read_resp_payload(0, &data);
        let ev = decode_response(&resp, &rpl, &pending).unwrap();
        match ev {
            DiagEvent::TaskStats {
                task_port,
                type_marker,
                ..
            } => {
                assert_eq!(task_port, 340);
                assert_eq!(type_marker, TASK_MARKER_IDLE);
            }
            _ => panic!("expected TaskStats"),
        }
    }

    #[test]
    fn rt_usage_decodes_from_real_capture() {
        // Captured from rt_run.log (port 200 = R0_REALTIME):
        // 24-byte payload, fields: peak=12, latency=250, cpu%=7, scale=100.
        let req = request_header(AMSPORT_R0_REALTIME, ADS_CMD_READ, 99, 12);
        let pl = read_req_payload(IG_RT_USAGE, IO_RT_USAGE, 1536);
        let (_, pending) = decode_request(&req, &pl).unwrap();

        let data = [
            0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0xfa, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00,
        ];
        let resp = response_header(
            AMSPORT_R0_REALTIME,
            ADS_CMD_READ,
            99,
            (8 + data.len()) as u32,
        );
        let rpl = read_resp_payload(0, &data);
        let ev = decode_response(&resp, &rpl, &pending).unwrap();
        assert_eq!(
            ev,
            DiagEvent::RtUsage {
                peak_latency_us: 12,
                system_latency_us: 250,
                cpu_percent: 7,
            }
        );
    }

    #[test]
    fn rt_usage_rejects_short_response() {
        // IDE over-allocates rsz=1536; real responses are 24 bytes. An error
        // response with rlen=0 must not yield a DiagEvent.
        let req = request_header(AMSPORT_R0_REALTIME, ADS_CMD_READ, 100, 12);
        let pl = read_req_payload(IG_RT_USAGE, IO_RT_USAGE, 1536);
        let (_, pending) = decode_request(&req, &pl).unwrap();
        let resp = response_header(AMSPORT_R0_REALTIME, ADS_CMD_READ, 100, 8);
        let rpl = read_resp_payload(0, &[]);
        assert!(decode_response(&resp, &rpl, &pending).is_none());
    }

    #[test]
    fn unknown_read_returns_none() {
        let req = request_header(852, ADS_CMD_READ, 1, 12);
        let pl = read_req_payload(0xdead, 0xbeef, 16);
        assert!(decode_request(&req, &pl).is_none());
    }

    #[test]
    fn read_response_with_error_result_returns_none() {
        let req = request_header(100, ADS_CMD_READ, 5, 12);
        let pl = read_req_payload(IG_RT_SYSTEM, IO_EXCEED_COUNTER, 4);
        let (_, pending) = decode_request(&req, &pl).unwrap();
        let resp = response_header(100, ADS_CMD_READ, 5, 8);
        // result = 0x701 (ADSERR_DEVICE_BUSY etc.)
        let mut rpl = Vec::new();
        rpl.extend_from_slice(&0x701_u32.to_le_bytes());
        rpl.extend_from_slice(&0_u32.to_le_bytes());
        assert!(decode_response(&resp, &rpl, &pending).is_none());
    }

    #[test]
    fn cycle_counter_wraparound_is_safe() {
        let before = 0xFFFE_u16;
        let after = 0x0001_u16;
        assert_eq!(after.wrapping_sub(before), 3);
    }
}

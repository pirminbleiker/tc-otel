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

/// Index group for push-diagnostic events (ADS Write from PLC → tc-otel).
/// Encoded as ASCII "MBC\1" in little-endian byte order: 0x4D42_4301.
pub const IG_PUSH_DIAG: u32 = 0x4D42_4301;

/// Index group for push-diagnostic configuration (ADS Write from tc-otel → PLC).
/// Reserved for outbound writes that adjust per-task windowing / enable state.
/// Encoded as ASCII "MBC\2" in little-endian byte order: 0x4D42_4302.
pub const IG_PUSH_CONFIG: u32 = 0x4D42_4302;

/// Index offset for per-task diagnostic batches within `IG_PUSH_DIAG`.
///
/// A batch = 80-byte header + N × 24-byte samples, where each sample is one
/// PLC cycle's `{cycle_count, exec_time_us, dc_time, flags}`. Aggregates
/// (min/max/avg exec_time, exceed/rtv counts) are in the header.
pub const IO_PUSH_BATCH: u32 = 0;

/// Index offset for push-metric batches within `IG_PUSH_DIAG`.
///
/// A batch = 32-byte header + N × descriptor blocks + M × 16-byte samples.
/// Descriptors announce metric metadata (name, unit, kind, bounds); samples
/// reference metric IDs and carry values.
pub const IO_PUSH_METRIC_BATCH: u32 = 1;

/// Index offset for compact FB_Metrics aggregate batches within `IG_PUSH_DIAG`.
///
/// A batch = 52-byte header, optional 24-byte trace context, name, unit, and
/// body. The body is `sample_count * sample_size` raw bytes interpreted by
/// the header's `body_schema` (see [`MetricBodySchema`]). Designed for the
/// PLC-side `FB_Metrics` user API: one frame per metric instance per push
/// window.
pub const IO_PUSH_METRIC_AGG: u32 = 2;

/// Wire-format version for push-diagnostic events. Bumped from v1 (legacy
/// snapshot + edge frames) to v2 (batch frame with per-cycle samples).
pub const PUSH_WIRE_VERSION: u8 = 2;

/// Batch event-type discriminator for per-task diagnostics in the batch header.
pub const PUSH_BATCH_EVENT_TYPE: u8 = 10;

/// Event-type discriminator for metric batches in the batch header.
pub const PUSH_METRIC_EVENT_TYPE: u8 = 20;

/// Event-type discriminator for FB_Metrics aggregate batches in the header.
pub const PUSH_METRIC_AGG_EVENT_TYPE: u8 = 21;

/// Fixed header size for FB_Metrics aggregate batches.
pub const PUSH_METRIC_AGG_HEADER_SIZE: usize = 52;

/// Optional trace-context block size (trace_id + span_id) when
/// `flags.bit0 = METRIC_FLAG_HAS_TRACE_CTX`.
pub const PUSH_METRIC_AGG_TRACE_SIZE: usize = 24;

/// FB_Metrics flag: header is followed by a 24-byte trace context (trace_id,
/// span_id) before the name+unit+body sections.
pub const METRIC_FLAG_HAS_TRACE_CTX: u8 = 1 << 0;

/// FB_Metrics flag: at least one sample was dropped during the window because
/// the per-instance body buffer filled up. Surfaced for ops dashboards.
pub const METRIC_FLAG_RING_OVERFLOWED: u8 = 1 << 1;

/// FB_Metrics flag: each body slot is preceded by a 2-byte little-endian
/// `u16` cycle offset (relative to `cycle_count_start`). Body slot stride
/// becomes `sample_size + 2`. Used by the receiver to reconstruct accurate
/// per-sample DC timestamps instead of linearly interpolating across the
/// window. Emitted when the PLC calls `FB_Metrics.SetRecordSampleTimes(TRUE)`.
pub const METRIC_FLAG_HAS_SAMPLE_TS: u8 = 1 << 2;

/// Bytes of per-sample prefix when `METRIC_FLAG_HAS_SAMPLE_TS` is set.
pub const METRIC_SAMPLE_TS_SIZE: usize = 2;

/// FB_Metrics aggregation stat bits. The PLC-side ``E_MetricStat`` enum
/// encodes the same values; ``stat_mask`` in the wire header is the OR
/// of zero or more of these.
///
/// When ``body_schema = NumericAggregated`` (6) the body holds
/// ``sample_count * popcount(stat_mask)`` LREALs in **bit-index order**
/// (Min first if set, then Max, then Mean, Sum, Count, StdDev).
pub const METRIC_STAT_MIN: u8 = 1 << 0;
pub const METRIC_STAT_MAX: u8 = 1 << 1;
pub const METRIC_STAT_MEAN: u8 = 1 << 2;
pub const METRIC_STAT_SUM: u8 = 1 << 3;
pub const METRIC_STAT_COUNT: u8 = 1 << 4;
pub const METRIC_STAT_STDDEV: u8 = 1 << 5;

/// All stat bits in canonical (bit-index) order — used by the decoder to
/// walk the mask and by the bridge to derive metric-name suffixes.
pub const METRIC_STAT_ORDER: [(u8, &str); 6] = [
    (METRIC_STAT_MIN, "min"),
    (METRIC_STAT_MAX, "max"),
    (METRIC_STAT_MEAN, "mean"),
    (METRIC_STAT_SUM, "sum"),
    (METRIC_STAT_COUNT, "count"),
    (METRIC_STAT_STDDEV, "stddev"),
];

/// Fixed batch-header size in bytes.
pub const PUSH_BATCH_HEADER_SIZE: usize = 80;

/// Fixed per-sample size in bytes.
pub const PUSH_SAMPLE_SIZE: usize = 24;

/// Maximum samples in a single batch (mirrors PLC ringbuffer depth).
pub const PUSH_BATCH_MAX_SAMPLES: usize = 1024;

/// Per-sample flag: cycle-exceed observed during this cycle.
pub const SAMPLE_FLAG_CYCLE_EXCEED: u8 = 1 << 0;

/// Per-sample flag: RT-violation observed during this cycle.
pub const SAMPLE_FLAG_RT_VIOLATION: u8 = 1 << 1;

/// Per-sample flag: first cycle after PLC start.
pub const SAMPLE_FLAG_FIRST_CYCLE: u8 = 1 << 2;

/// Per-sample flag: ringbuffer overflowed before this sample (data gap).
pub const SAMPLE_FLAG_OVERFLOW: u8 = 1 << 3;

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
#[derive(Debug, Clone, PartialEq)]
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
    /// Push-diagnostic: per-task oversampled batch covering one aggregation
    /// window.
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_BATCH`. The batch
    /// contains one sample per PLC cycle within the window, allowing
    /// cycle-exact reconstruction of execution-time and RT-state transitions.
    /// Aggregates (min/max/avg exec_time, exceed/rtv counts) are pre-computed
    /// on the PLC to reduce downstream processing.
    ///
    /// Samples include `cycle_count` and `dc_time` so each flagged event
    /// (cycle-exceed, RT-violation) can be correlated 1:1 with log entries
    /// emitted on the same cycle.
    TaskDiagBatch {
        /// AMS port of the owning task (340, 350, 351, …).
        task_port: u16,
        /// Human-readable task name (lossy UTF-8, trailing NULs stripped).
        task_name: String,
        /// TwinCAT task object id.
        task_obj_id: u32,
        /// Aggregation window used for this batch, in milliseconds.
        window_ms: u16,
        /// Cycle count at the first sample in the batch.
        cycle_count_start: u32,
        /// Cycle count at the last sample in the batch.
        cycle_count_end: u32,
        /// Wall-clock time at the first sample as FILETIME (100 ns ticks
        /// since 1601-01-01). Same format as log entries' `plc_timestamp`,
        /// so a flagged cycle can be matched to its log line by equality.
        dc_time_start: i64,
        /// Wall-clock time at the last sample, FILETIME ticks.
        dc_time_end: i64,
        /// Minimum observed execution time in the window (µs).
        exec_time_min_us: u32,
        /// Maximum observed execution time in the window (µs).
        exec_time_max_us: u32,
        /// Average execution time in the window (µs).
        exec_time_avg_us: u32,
        /// Total cycles with `CycleTimeExceeded` observed in the window.
        cycle_exceed_count: u32,
        /// Total cycles with `RTViolation` observed in the window.
        rt_violation_count: u32,
        /// Per-cycle samples, ordered from oldest to newest.
        samples: Vec<DiagSample>,
    },
    /// Push-metric: user-defined metrics (gauges, counters, histograms).
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_METRIC_BATCH`. A batch
    /// announces metric metadata (name, unit, kind, histogram bounds) once per
    /// session, then subsequent batches reference those descriptors and ship
    /// only samples.
    MetricBatch {
        /// Aggregation window or sample interval, in milliseconds.
        window_ms: u16,
        /// Base cycle counter or PLC cycle counter.
        cycle_count: u32,
        /// Wall-clock time at first sample, FILETIME ticks (100 ns since 1601).
        dc_time_start: i64,
        /// Wall-clock time at last sample, FILETIME ticks.
        dc_time_end: i64,
        /// Metric metadata descriptors announced in this batch.
        descriptors: Vec<MetricDescriptor>,
        /// Metric value samples with metric ID references.
        samples: Vec<MetricSample>,
    },
    /// Compact FB_Metrics aggregate batch: one PLC `FB_Metrics` instance's
    /// buffered samples for one push window.
    ///
    /// Sent as ADS Write to `IG_PUSH_DIAG` / `IO_PUSH_METRIC_AGG`. The frame
    /// is `[52-byte header | optional 24-byte trace ctx | name | unit | body]`.
    /// Each sample in `samples` is decoded according to the wire `body_schema`
    /// — numerics widen to f64, BOOL becomes bool, discrete/string keep their
    /// raw representation. Per-sample timestamps are not on the wire; the
    /// receiver may interpolate uniformly across `[dc_time_start, dc_time_end]`.
    MetricAggregateBatch {
        /// FNV-1a(name) — stable across batches as long as the name doesn't
        /// change. Receiver may use this as a registration key.
        metric_id: u32,
        /// Owning PLC task index (1..n).
        task_index: u8,
        /// FB_Metrics flags. See `METRIC_FLAG_*` constants.
        flags: u8,
        /// Body schema decoded from the wire byte.
        body_schema: MetricBodySchema,
        /// Bytes per body sample as carried on the wire (8 for Numeric, 1
        /// for Bool, native size for Discrete, configured length for
        /// String/Wstring, ``popcount(stat_mask) * 8`` for NumericAggregated).
        sample_size: u32,
        /// Aggregation stat bitmask from header offset +0x11. Zero for raw
        /// non-aggregated frames; non-zero only meaningful when ``body_schema
        /// = NumericAggregated``. See ``METRIC_STAT_*`` constants.
        stat_mask: u8,
        /// Task cycle counter at the first sample in the window.
        cycle_count_start: u32,
        /// Task cycle counter at the last sample in the window.
        cycle_count_end: u32,
        /// DC time at the first sample (ns since DC epoch 2000-01-01).
        dc_time_start: i64,
        /// DC time at the last sample (ns since DC epoch 2000-01-01).
        dc_time_end: i64,
        /// Metric name (UTF-8, ≤ 63 bytes).
        name: String,
        /// Metric unit (UTF-8, ≤ 15 bytes; empty when unitless).
        unit: String,
        /// Optional trace context for OTel exemplar attachment. `None` when
        /// `flags & METRIC_FLAG_HAS_TRACE_CTX == 0`.
        trace_id: Option<[u8; 16]>,
        /// Optional span_id paired with `trace_id`.
        span_id: Option<[u8; 8]>,
        /// Decoded samples in capture order.
        samples: Vec<MetricAggregateSample>,
        /// Optional per-sample cycle offsets (relative to `cycle_count_start`).
        /// `Some(v)` when `flags & METRIC_FLAG_HAS_SAMPLE_TS != 0`, with
        /// `v.len() == samples.len()`. `None` when the flag is clear — in
        /// which case the receiver falls back to linear interpolation across
        /// `[dc_time_start, dc_time_end]`.
        sample_cycle_offsets: Option<Vec<u16>>,
    },
}

/// Body schema discriminator for FB_Metrics aggregate batches. Mirrors the
/// PLC-side `E_MetricBodySchema` byte at header offset +0x03.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricBodySchema {
    /// 1 byte per sample (0/1).
    Bool,
    /// 8 bytes per sample (LREAL widened from any IEC numeric).
    Numeric,
    /// `sample_size` bytes per sample, raw (BYTE / WORD / DWORD / LWORD / ENUM).
    Discrete,
    /// `sample_size` bytes per sample, UTF-8 zero-padded fixed-size strings.
    String,
    /// `sample_size` bytes per sample, UTF-16LE zero-padded fixed-size strings.
    Wstring,
    /// `popcount(stat_mask)` LREALs per sample in `METRIC_STAT_ORDER`.
    NumericAggregated,
}

impl MetricBodySchema {
    /// Decode the wire byte; returns `None` for unknown values so the parser
    /// can drop the frame without panicking.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(MetricBodySchema::Bool),
            2 => Some(MetricBodySchema::Numeric),
            3 => Some(MetricBodySchema::Discrete),
            4 => Some(MetricBodySchema::String),
            5 => Some(MetricBodySchema::Wstring),
            6 => Some(MetricBodySchema::NumericAggregated),
            _ => None,
        }
    }
}

/// One decoded sample from an FB_Metrics aggregate batch. The variant is
/// determined by the batch's `body_schema`.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricAggregateSample {
    Bool(bool),
    Numeric(f64),
    Discrete(Vec<u8>),
    String(String),
    Wstring(String),
    /// One Welford-aggregated sample. `values` are stored in
    /// `METRIC_STAT_ORDER` (Min, Max, Mean, Sum, Count, StdDev) and only
    /// include the stats whose bit is set in `stat_mask`. Both fields are
    /// surfaced so the bridge can attach the right suffix per emitted metric.
    NumericAggregated {
        stat_mask: u8,
        values: Vec<f64>,
    },
}

/// Metric descriptor — static metadata for a metric announced in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricDescriptor {
    /// Stable metric ID within session; assigned by PLC.
    pub metric_id: u16,
    /// Metric kind: Gauge, Sum/Counter, or Histogram.
    pub kind: u8, // 0=Gauge, 1=Sum, 2=Histogram; directly matches MetricKind
    /// For Sum: bit 0 = is_monotonic.
    pub flags: u8,
    /// Human-readable metric name (UTF-8).
    pub name: String,
    /// Unit of measurement (UTF-8, e.g., "ms", "rpm", "percent").
    pub unit: String,
    /// Human-readable description (UTF-8).
    pub description: String,
    /// Key-value attributes (e.g., device ID, serial number).
    pub attributes: Vec<(String, String)>,
    /// For Histogram kind: bucket boundaries (f32). Empty for other kinds.
    pub histogram_bounds: Option<Vec<f32>>,
}

/// Metric sample — a single value point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricSample {
    /// References a metric_id in the descriptor table.
    pub metric_id: u16,
    /// Bit 0: histogram_observe (only for Histogram kind).
    /// Bit 1: counter_delta_i64 (reserved; always u32 for now).
    pub flags: u8,
    /// Wall-clock time of this sample, FILETIME ticks.
    pub dc_time: i64,
    /// Sample value (32-bit float). For Counter/Sum: delta amount.
    /// For Histogram: observation to accumulate.
    pub value: f32,
}

/// Single PLC-cycle sample inside a [`DiagEvent::TaskDiagBatch`].
///
/// Samples are ordered chronologically. The `flags` byte carries per-cycle
/// signals: bit0=cycle-exceed, bit1=rt-violation, bit2=first-cycle,
/// bit3=overflow (ringbuffer wrap before this sample — previous samples lost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagSample {
    /// TwinCAT task cycle counter (monotonic, may wrap after ~4.3 B cycles).
    pub cycle_count: u32,
    /// Observed execution time of this cycle in microseconds.
    pub exec_time_us: u32,
    /// Wall-clock time at cycle start as FILETIME ticks (100 ns since
    /// 1601-01-01). Same format log entries use — compare by equality.
    pub dc_time: i64,
    /// Per-cycle flags. See `SAMPLE_FLAG_*` constants.
    pub flags: u8,
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

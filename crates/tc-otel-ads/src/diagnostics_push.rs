//! Decoder for push-diagnostic batch frames sent by TwinCAT PLCs.
//!
//! Each task on the PLC runs its own per-cycle sampler (see `FB_TcOtelTaskDiag`
//! in the PLC library). Every aggregation window it flushes one batch frame
//! containing the per-cycle samples captured in the window plus a pre-computed
//! aggregate. Batches are pushed via AdsWrite to index group `IG_PUSH_DIAG`
//! and index offset `IO_PUSH_BATCH`. All frames are little-endian.
//!
//! Wire layout:
//!
//! - 80-byte header: version, event_type, task_obj_id, cycle/dc windows,
//!   sample count, window_ms, exec-time min/max/avg, exceed/rtv counts,
//!   task name, reserved.
//! - N × 24-byte samples: cycle_count, exec_time_us, dc_time, flags.
//!
//! The decoder rejects malformed frames (wrong version, wrong event_type,
//! truncated body, sample count above `PUSH_BATCH_MAX_SAMPLES`) by returning
//! `None`.

use crate::diagnostics::{
    DiagEvent, DiagSample, MetricAggregateSample, MetricBodySchema, MetricDescriptor, MetricSample,
    METRIC_FLAG_HAS_TRACE_CTX, PUSH_BATCH_EVENT_TYPE, PUSH_BATCH_HEADER_SIZE,
    PUSH_BATCH_MAX_SAMPLES, PUSH_METRIC_AGG_EVENT_TYPE, PUSH_METRIC_AGG_HEADER_SIZE,
    PUSH_METRIC_AGG_TRACE_SIZE, PUSH_METRIC_EVENT_TYPE, PUSH_SAMPLE_SIZE, PUSH_WIRE_VERSION,
};

/// Decode a task-diagnostic batch frame (80 + 24 × sample_count bytes).
///
/// Returns `Some(DiagEvent::TaskDiagBatch)` on success. Returns `None` on any
/// validation failure: wrong version, wrong event_type, truncated payload,
/// sample count out of bounds.
///
/// Task names are decoded with lossy UTF-8 and trailing NULs stripped.
pub fn decode_batch(bytes: &[u8]) -> Option<DiagEvent> {
    if bytes.len() < PUSH_BATCH_HEADER_SIZE {
        return None;
    }

    let version = bytes[0];
    let event_type = bytes[1];
    if version != PUSH_WIRE_VERSION || event_type != PUSH_BATCH_EVENT_TYPE {
        return None;
    }

    // Header layout (LE, pack_mode=1):
    //   +0x00 version      : u8
    //   +0x01 event_type   : u8
    //   +0x02 reserved0    : u16
    //   +0x04 task_obj_id  : u32
    //   +0x08 task_port    : u16
    //   +0x0A window_ms    : u16
    //   +0x0C sample_count : u16
    //   +0x0E reserved1    : u16
    //   +0x10 cycle_count_start : u32
    //   +0x14 cycle_count_end   : u32
    //   +0x18 dc_time_start     : i64
    //   +0x20 dc_time_end       : i64
    //   +0x28 exec_time_min_us  : u32
    //   +0x2C exec_time_max_us  : u32
    //   +0x30 exec_time_avg_us  : u32
    //   +0x34 cycle_exceed_count: u32
    //   +0x38 rt_violation_count: u32
    //   +0x3C task_name [20]
    //   +0x50 total = 80 bytes
    let task_obj_id = read_u32(bytes, 4);
    let task_port = read_u16(bytes, 8);
    let window_ms = read_u16(bytes, 0x0A);
    let sample_count = read_u16(bytes, 0x0C) as usize;
    let cycle_count_start = read_u32(bytes, 0x10);
    let cycle_count_end = read_u32(bytes, 0x14);
    let dc_time_start = read_i64(bytes, 0x18);
    let dc_time_end = read_i64(bytes, 0x20);
    let exec_time_min_us = read_u32(bytes, 0x28);
    let exec_time_max_us = read_u32(bytes, 0x2C);
    let exec_time_avg_us = read_u32(bytes, 0x30);
    let cycle_exceed_count = read_u32(bytes, 0x34);
    let rt_violation_count = read_u32(bytes, 0x38);
    let task_name = decode_task_name(&bytes[0x3C..0x50]);

    if sample_count > PUSH_BATCH_MAX_SAMPLES {
        return None;
    }
    let body_len = sample_count.checked_mul(PUSH_SAMPLE_SIZE)?;
    let total_len = PUSH_BATCH_HEADER_SIZE.checked_add(body_len)?;
    if bytes.len() < total_len {
        return None;
    }

    let mut samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let off = PUSH_BATCH_HEADER_SIZE + i * PUSH_SAMPLE_SIZE;
        // Sample layout (LE, pack_mode=1):
        //   +0x00 cycle_count  : u32
        //   +0x04 exec_time_us : u32
        //   +0x08 dc_time      : i64
        //   +0x10 flags        : u8
        //   +0x11 reserved     : 7 bytes (padding to 24)
        samples.push(DiagSample {
            cycle_count: read_u32(bytes, off),
            exec_time_us: read_u32(bytes, off + 4),
            dc_time: read_i64(bytes, off + 8),
            flags: bytes[off + 0x10],
        });
    }

    Some(DiagEvent::TaskDiagBatch {
        task_port,
        task_name,
        task_obj_id,
        window_ms,
        cycle_count_start,
        cycle_count_end,
        dc_time_start,
        dc_time_end,
        exec_time_min_us,
        exec_time_max_us,
        exec_time_avg_us,
        cycle_exceed_count,
        rt_violation_count,
        samples,
    })
}

fn read_u16(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap_or([0; 2]))
}

fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]))
}

fn read_i64(bytes: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap_or([0; 8]))
}

fn decode_task_name(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Decode a metric batch frame (32 + descriptors + 16 × sample_count bytes).
///
/// Returns `Some(DiagEvent::MetricBatch)` on success. Returns `None` on any
/// validation failure: wrong version, wrong event_type, truncated payload,
/// invalid UTF-8, attribute count > 8, malformed descriptor.
pub fn decode_metric_batch(bytes: &[u8]) -> Option<DiagEvent> {
    if bytes.len() < 32 {
        return None;
    }

    let version = bytes[0];
    let event_type = bytes[1];
    if version != PUSH_WIRE_VERSION || event_type != PUSH_METRIC_EVENT_TYPE {
        return None;
    }

    // Header layout (LE, pack_mode=1):
    //   +0x00 version               : u8
    //   +0x01 event_type            : u8
    //   +0x02 reserved0             : u16
    //   +0x04 descriptor_count      : u16
    //   +0x06 sample_count          : u16
    //   +0x08 window_ms             : u16
    //   +0x0A reserved1             : u16
    //   +0x0C cycle_count           : u32
    //   +0x10 dc_time_start         : i64
    //   +0x18 dc_time_end           : i64
    //   +0x20 total = 32 bytes
    let descriptor_count = read_u16(bytes, 4) as usize;
    let sample_count = read_u16(bytes, 6) as usize;
    let window_ms = read_u16(bytes, 8);
    let cycle_count = read_u32(bytes, 0x0C);
    let dc_time_start = read_i64(bytes, 0x10);
    let dc_time_end = read_i64(bytes, 0x18);

    // Parse descriptors.
    let mut offset = 32;
    let mut descriptors = Vec::with_capacity(descriptor_count);
    for _ in 0..descriptor_count {
        let (desc, bytes_read) = parse_descriptor(&bytes[offset..])?;
        descriptors.push(desc);
        offset = offset.checked_add(bytes_read)?;
    }

    // Parse samples (16 bytes each).
    let mut samples = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if offset + 16 > bytes.len() {
            return None;
        }
        samples.push(MetricSample {
            metric_id: read_u16(bytes, offset),
            flags: bytes[offset + 2],
            dc_time: read_i64(bytes, offset + 4),
            value: read_f32(bytes, offset + 12),
        });
        offset += 16;
    }

    if offset != bytes.len() {
        // Extra trailing bytes suggest truncation or misparse; be strict.
        return None;
    }

    Some(DiagEvent::MetricBatch {
        window_ms,
        cycle_count,
        dc_time_start,
        dc_time_end,
        descriptors,
        samples,
    })
}

/// Parse a single metric descriptor from bytes. Returns the descriptor and the
/// number of bytes consumed, or None on error.
fn parse_descriptor(bytes: &[u8]) -> Option<(MetricDescriptor, usize)> {
    if bytes.len() < 12 {
        return None;
    }

    let metric_id = read_u16(bytes, 0);
    let kind = bytes[2];
    let flags = bytes[3];
    let name_len = bytes[4] as usize;
    let unit_len = bytes[5] as usize;
    let description_len = bytes[6] as usize;
    let attr_count = bytes[7] as usize;
    let histogram_bucket_count = bytes[8] as usize;

    if attr_count > 8 {
        return None;
    }

    let mut offset = 12;

    // Read name.
    if offset + name_len > bytes.len() {
        return None;
    }
    let name = String::from_utf8(bytes[offset..offset + name_len].to_vec()).ok()?;
    offset += name_len;

    // Read unit.
    if offset + unit_len > bytes.len() {
        return None;
    }
    let unit = String::from_utf8(bytes[offset..offset + unit_len].to_vec()).ok()?;
    offset += unit_len;

    // Read description.
    if offset + description_len > bytes.len() {
        return None;
    }
    let description = String::from_utf8(bytes[offset..offset + description_len].to_vec()).ok()?;
    offset += description_len;

    // Read attributes.
    let mut attributes = Vec::with_capacity(attr_count);
    for _ in 0..attr_count {
        if offset + 2 > bytes.len() {
            return None;
        }
        let key_len = bytes[offset] as usize;
        let value_len = bytes[offset + 1] as usize;
        offset += 2;

        if offset + key_len > bytes.len() {
            return None;
        }
        let key = String::from_utf8(bytes[offset..offset + key_len].to_vec()).ok()?;
        offset += key_len;

        if offset + value_len > bytes.len() {
            return None;
        }
        let value = String::from_utf8(bytes[offset..offset + value_len].to_vec()).ok()?;
        offset += value_len;

        attributes.push((key, value));
    }

    // Read histogram bounds (if kind == 2).
    let histogram_bounds = if kind == 2 && histogram_bucket_count > 0 {
        let bounds_size = histogram_bucket_count * 4; // f32 = 4 bytes
        if offset + bounds_size > bytes.len() {
            return None;
        }
        let mut bounds = Vec::with_capacity(histogram_bucket_count);
        for i in 0..histogram_bucket_count {
            bounds.push(read_f32(bytes, offset + i * 4));
        }
        offset += bounds_size;
        Some(bounds)
    } else {
        None
    };

    let descriptor = MetricDescriptor {
        metric_id,
        kind,
        flags,
        name,
        unit,
        description,
        attributes,
        histogram_bounds,
    };

    Some((descriptor, offset))
}

fn read_f32(bytes: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]))
}

fn read_f64(bytes: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap_or([0; 8]))
}

/// Decode an FB_Metrics aggregate batch frame.
///
/// Layout: 52-byte header + (24-byte trace ctx if `flags.bit0`) + name +
/// unit + body, where body = `sample_count * sample_size` bytes interpreted
/// per `body_schema`. Returns `None` on any validation failure (wrong
/// version / event_type, truncated payload, unknown body_schema, invalid
/// UTF-8 in name/unit, sample_size = 0 with non-empty body).
pub fn decode_metric_aggregate(bytes: &[u8]) -> Option<DiagEvent> {
    if bytes.len() < PUSH_METRIC_AGG_HEADER_SIZE {
        return None;
    }

    let version = bytes[0];
    let event_type = bytes[1];
    if version != PUSH_WIRE_VERSION || event_type != PUSH_METRIC_AGG_EVENT_TYPE {
        return None;
    }

    let flags = bytes[2];
    let body_schema = MetricBodySchema::from_byte(bytes[3])?;
    let sample_size = read_u32(bytes, 0x04);
    let sample_count = read_u32(bytes, 0x08);
    let metric_id = read_u32(bytes, 0x0C);
    let task_index = bytes[0x10];
    let cycle_count_start = read_u32(bytes, 0x14);
    let cycle_count_end = read_u32(bytes, 0x18);
    let dc_time_start = read_i64(bytes, 0x20);
    let dc_time_end = read_i64(bytes, 0x28);
    let name_len = bytes[0x30] as usize;
    let unit_len = bytes[0x31] as usize;

    // Schema-vs-sample-size sanity. Numeric is always 8 bytes; Bool is always 1.
    // Discrete / String / Wstring honour the wire `sample_size`. Reject early
    // when the combination is impossible — keeps the body slicer simple.
    match body_schema {
        MetricBodySchema::Bool if sample_size != 1 => return None,
        MetricBodySchema::Numeric if sample_size != 8 => return None,
        MetricBodySchema::Discrete | MetricBodySchema::String | MetricBodySchema::Wstring => {
            if sample_size == 0 || sample_size > 65535 {
                return None;
            }
        }
        _ => {}
    }

    let mut offset = PUSH_METRIC_AGG_HEADER_SIZE;

    // Optional trace context.
    let (trace_id, span_id) = if flags & METRIC_FLAG_HAS_TRACE_CTX != 0 {
        if bytes.len() < offset + PUSH_METRIC_AGG_TRACE_SIZE {
            return None;
        }
        let mut tid = [0_u8; 16];
        tid.copy_from_slice(&bytes[offset..offset + 16]);
        let mut sid = [0_u8; 8];
        sid.copy_from_slice(&bytes[offset + 16..offset + 24]);
        offset += PUSH_METRIC_AGG_TRACE_SIZE;
        (Some(tid), Some(sid))
    } else {
        (None, None)
    };

    // Name + unit (both UTF-8, capped on the PLC side).
    if bytes.len() < offset + name_len {
        return None;
    }
    let name = String::from_utf8(bytes[offset..offset + name_len].to_vec()).ok()?;
    offset += name_len;

    if bytes.len() < offset + unit_len {
        return None;
    }
    let unit = String::from_utf8(bytes[offset..offset + unit_len].to_vec()).ok()?;
    offset += unit_len;

    // Body — sample_count * sample_size bytes.
    let body_len = (sample_count as usize).checked_mul(sample_size as usize)?;
    if bytes.len() < offset + body_len {
        return None;
    }
    let body = &bytes[offset..offset + body_len];
    offset += body_len;

    // Strict trailing-bytes check matches decode_metric_batch.
    if offset != bytes.len() {
        return None;
    }

    let mut samples: Vec<MetricAggregateSample> = Vec::with_capacity(sample_count as usize);
    for i in 0..sample_count as usize {
        let off = i * sample_size as usize;
        let slot = &body[off..off + sample_size as usize];
        let sample = match body_schema {
            MetricBodySchema::Bool => MetricAggregateSample::Bool(slot[0] != 0),
            MetricBodySchema::Numeric => MetricAggregateSample::Numeric(read_f64(slot, 0)),
            MetricBodySchema::Discrete => MetricAggregateSample::Discrete(slot.to_vec()),
            MetricBodySchema::String => {
                // Trim trailing zero padding so callers see the live bytes only.
                let end = slot.iter().rposition(|&b| b != 0).map(|p| p + 1).unwrap_or(0);
                MetricAggregateSample::String(
                    String::from_utf8_lossy(&slot[..end]).into_owned(),
                )
            }
            MetricBodySchema::Wstring => {
                // 2 bytes per char, UTF-16LE, trim trailing zero u16s.
                let mut units: Vec<u16> = slot
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                while units.last() == Some(&0) {
                    units.pop();
                }
                MetricAggregateSample::Wstring(String::from_utf16_lossy(&units))
            }
        };
        samples.push(sample);
    }

    Some(DiagEvent::MetricAggregateBatch {
        metric_id,
        task_index,
        flags,
        body_schema,
        sample_size,
        cycle_count_start,
        cycle_count_end,
        dc_time_start,
        dc_time_end,
        name,
        unit,
        trace_id,
        span_id,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{
        METRIC_FLAG_RING_OVERFLOWED, SAMPLE_FLAG_CYCLE_EXCEED, SAMPLE_FLAG_OVERFLOW,
        SAMPLE_FLAG_RT_VIOLATION,
    };

    #[allow(clippy::too_many_arguments)]
    fn batch_header(
        task_obj_id: u32,
        task_port: u16,
        window_ms: u16,
        sample_count: u16,
        cycle_count_start: u32,
        cycle_count_end: u32,
        dc_time_start: i64,
        dc_time_end: i64,
        exec_time_min_us: u32,
        exec_time_max_us: u32,
        exec_time_avg_us: u32,
        cycle_exceed_count: u32,
        rt_violation_count: u32,
        task_name: &str,
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(PUSH_BATCH_HEADER_SIZE);
        v.push(PUSH_WIRE_VERSION);
        v.push(PUSH_BATCH_EVENT_TYPE);
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved0
        v.extend_from_slice(&task_obj_id.to_le_bytes());
        v.extend_from_slice(&task_port.to_le_bytes());
        v.extend_from_slice(&window_ms.to_le_bytes());
        v.extend_from_slice(&sample_count.to_le_bytes());
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved1
        v.extend_from_slice(&cycle_count_start.to_le_bytes());
        v.extend_from_slice(&cycle_count_end.to_le_bytes());
        v.extend_from_slice(&dc_time_start.to_le_bytes());
        v.extend_from_slice(&dc_time_end.to_le_bytes());
        v.extend_from_slice(&exec_time_min_us.to_le_bytes());
        v.extend_from_slice(&exec_time_max_us.to_le_bytes());
        v.extend_from_slice(&exec_time_avg_us.to_le_bytes());
        v.extend_from_slice(&cycle_exceed_count.to_le_bytes());
        v.extend_from_slice(&rt_violation_count.to_le_bytes());

        let mut name_bytes = [0_u8; 20];
        let n = task_name.len().min(20);
        name_bytes[..n].copy_from_slice(&task_name.as_bytes()[..n]);
        v.extend_from_slice(&name_bytes);
        assert_eq!(v.len(), PUSH_BATCH_HEADER_SIZE, "header size mismatch");
        v
    }

    fn sample_bytes(cycle_count: u32, exec_time_us: u32, dc_time: i64, flags: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(PUSH_SAMPLE_SIZE);
        v.extend_from_slice(&cycle_count.to_le_bytes());
        v.extend_from_slice(&exec_time_us.to_le_bytes());
        v.extend_from_slice(&dc_time.to_le_bytes());
        v.push(flags);
        v.extend_from_slice(&[0_u8; 7]); // padding
        assert_eq!(v.len(), PUSH_SAMPLE_SIZE, "sample size mismatch");
        v
    }

    #[test]
    fn decode_batch_empty_sample_count() {
        let frame = batch_header(100, 350, 100, 0, 1000, 1000, 0, 0, 0, 0, 0, 0, 0, "PlcTask");
        let ev = decode_batch(&frame).expect("empty batch decodes");
        match ev {
            DiagEvent::TaskDiagBatch {
                task_port,
                task_name,
                samples,
                window_ms,
                ..
            } => {
                assert_eq!(task_port, 350);
                assert_eq!(task_name, "PlcTask");
                assert_eq!(window_ms, 100);
                assert!(samples.is_empty());
            }
            _ => panic!("expected TaskDiagBatch"),
        }
    }

    #[test]
    fn decode_batch_with_three_samples_preserves_order_and_flags() {
        let mut frame = batch_header(
            100, 350, 100, 3, 1000, 1002, 1_000_000, 1_200_000, 150, 800, 400, 1, 1, "PlcTask",
        );
        frame.extend_from_slice(&sample_bytes(1000, 150, 1_000_000, 0));
        frame.extend_from_slice(&sample_bytes(
            1001,
            800,
            1_100_000,
            SAMPLE_FLAG_CYCLE_EXCEED,
        ));
        frame.extend_from_slice(&sample_bytes(
            1002,
            250,
            1_200_000,
            SAMPLE_FLAG_RT_VIOLATION,
        ));

        let ev = decode_batch(&frame).expect("valid batch decodes");
        match ev {
            DiagEvent::TaskDiagBatch {
                samples,
                exec_time_min_us,
                exec_time_max_us,
                exec_time_avg_us,
                cycle_exceed_count,
                rt_violation_count,
                cycle_count_start,
                cycle_count_end,
                ..
            } => {
                assert_eq!(samples.len(), 3);
                assert_eq!(samples[0].cycle_count, 1000);
                assert_eq!(samples[1].exec_time_us, 800);
                assert_eq!(samples[1].flags, SAMPLE_FLAG_CYCLE_EXCEED);
                assert_eq!(samples[2].flags, SAMPLE_FLAG_RT_VIOLATION);
                assert_eq!(samples[2].dc_time, 1_200_000);
                assert_eq!(exec_time_min_us, 150);
                assert_eq!(exec_time_max_us, 800);
                assert_eq!(exec_time_avg_us, 400);
                assert_eq!(cycle_exceed_count, 1);
                assert_eq!(rt_violation_count, 1);
                assert_eq!(cycle_count_start, 1000);
                assert_eq!(cycle_count_end, 1002);
            }
            _ => panic!("expected TaskDiagBatch"),
        }
    }

    #[test]
    fn decode_batch_overflow_flag_preserved() {
        let mut frame = batch_header(
            100, 350, 100, 1, 2000, 2000, 0, 0, 100, 100, 100, 0, 0, "PlcTask",
        );
        frame.extend_from_slice(&sample_bytes(2000, 100, 0, SAMPLE_FLAG_OVERFLOW));
        let ev = decode_batch(&frame).unwrap();
        match ev {
            DiagEvent::TaskDiagBatch { samples, .. } => {
                assert_eq!(samples[0].flags, SAMPLE_FLAG_OVERFLOW);
            }
            _ => panic!("expected TaskDiagBatch"),
        }
    }

    #[test]
    fn decode_batch_rejects_short_header() {
        let frame = vec![0_u8; PUSH_BATCH_HEADER_SIZE - 1];
        assert!(decode_batch(&frame).is_none());
    }

    #[test]
    fn decode_batch_rejects_wrong_version() {
        let mut frame = batch_header(100, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, "PlcTask");
        frame[0] = 99;
        assert!(decode_batch(&frame).is_none());
    }

    #[test]
    fn decode_batch_rejects_wrong_event_type() {
        let mut frame = batch_header(100, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, "PlcTask");
        frame[1] = 0;
        assert!(decode_batch(&frame).is_none());
    }

    #[test]
    fn decode_batch_rejects_truncated_samples() {
        let mut frame = batch_header(
            100, 350, 100, 2, 1000, 1001, 0, 0, 100, 200, 150, 0, 0, "PlcTask",
        );
        frame.extend_from_slice(&sample_bytes(1000, 100, 0, 0));
        // Missing second sample
        assert!(decode_batch(&frame).is_none());
    }

    #[test]
    fn decode_batch_rejects_sample_count_above_max() {
        let mut frame = batch_header(
            100,
            350,
            100,
            (PUSH_BATCH_MAX_SAMPLES + 1) as u16,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            "PlcTask",
        );
        // Even if we then provide bytes, header alone must be rejected.
        frame.resize(
            frame.len() + (PUSH_BATCH_MAX_SAMPLES + 1) * PUSH_SAMPLE_SIZE,
            0,
        );
        assert!(decode_batch(&frame).is_none());
    }

    #[test]
    fn decode_batch_task_name_strips_trailing_nulls() {
        let frame = batch_header(100, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, "Task\0\0\0");
        let ev = decode_batch(&frame).unwrap();
        match ev {
            DiagEvent::TaskDiagBatch { task_name, .. } => assert_eq!(task_name, "Task"),
            _ => panic!("expected TaskDiagBatch"),
        }
    }

    #[test]
    fn decode_batch_handles_non_utf8_task_name() {
        let mut frame = batch_header(100, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, "");
        frame[0x3C] = 0xFF; // first name byte invalid UTF-8
        let ev = decode_batch(&frame).expect("must not panic on invalid UTF-8");
        if let DiagEvent::TaskDiagBatch { task_name, .. } = ev {
            assert!(task_name.contains('\u{FFFD}'));
        } else {
            panic!("expected TaskDiagBatch");
        }
    }

    // ─── Metric batch tests ───────────────────────────────────

    fn metric_batch_header(
        descriptor_count: u16,
        sample_count: u16,
        window_ms: u16,
        cycle_count: u32,
        dc_time_start: i64,
        dc_time_end: i64,
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(32);
        v.push(PUSH_WIRE_VERSION);
        v.push(PUSH_METRIC_EVENT_TYPE);
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved0
        v.extend_from_slice(&descriptor_count.to_le_bytes());
        v.extend_from_slice(&sample_count.to_le_bytes());
        v.extend_from_slice(&window_ms.to_le_bytes());
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved1
        v.extend_from_slice(&cycle_count.to_le_bytes());
        v.extend_from_slice(&dc_time_start.to_le_bytes());
        v.extend_from_slice(&dc_time_end.to_le_bytes());
        assert_eq!(v.len(), 32, "metric header size mismatch");
        v
    }

    fn metric_sample_bytes(metric_id: u16, flags: u8, dc_time: i64, value: f32) -> Vec<u8> {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(&metric_id.to_le_bytes());
        v.push(flags);
        v.push(0); // reserved
        v.extend_from_slice(&dc_time.to_le_bytes());
        v.extend_from_slice(&value.to_le_bytes());
        assert_eq!(v.len(), 16, "metric sample size mismatch");
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn metric_descriptor_bytes(
        metric_id: u16,
        kind: u8,
        flags: u8,
        name: &str,
        unit: &str,
        description: &str,
        attributes: &[(String, String)],
        histogram_bounds: Option<&[f32]>,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&metric_id.to_le_bytes());
        v.push(kind);
        v.push(flags);
        v.push(name.len() as u8);
        v.push(unit.len() as u8);
        v.push(description.len() as u8);
        v.push(attributes.len() as u8);
        let bucket_count = histogram_bounds.as_ref().map(|b| b.len()).unwrap_or(0) as u8;
        v.push(bucket_count);
        v.push(0); // reserved
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved
        v.extend_from_slice(name.as_bytes());
        v.extend_from_slice(unit.as_bytes());
        v.extend_from_slice(description.as_bytes());
        for (key, val) in attributes {
            v.push(key.len() as u8);
            v.push(val.len() as u8);
            v.extend_from_slice(key.as_bytes());
            v.extend_from_slice(val.as_bytes());
        }
        if let Some(bounds) = histogram_bounds {
            for bound in bounds {
                v.extend_from_slice(&bound.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn decode_metric_batch_empty() {
        let frame = metric_batch_header(0, 0, 100, 1000, 0, 0);
        let ev = decode_metric_batch(&frame).expect("empty metric batch decodes");
        match ev {
            DiagEvent::MetricBatch {
                window_ms,
                cycle_count,
                descriptors,
                samples,
                ..
            } => {
                assert_eq!(window_ms, 100);
                assert_eq!(cycle_count, 1000);
                assert!(descriptors.is_empty());
                assert!(samples.is_empty());
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_with_gauge_descriptor_and_samples() {
        let mut frame = metric_batch_header(1, 2, 100, 1000, 5000, 6000);
        frame.extend_from_slice(&metric_descriptor_bytes(
            10,
            0, // Gauge
            0, // flags
            "temperature",
            "Cel",
            "Motor temperature",
            &[],
            None,
        ));
        frame.extend_from_slice(&metric_sample_bytes(10, 0, 5000, 42.5));
        frame.extend_from_slice(&metric_sample_bytes(10, 0, 6000, 43.1));

        let ev = decode_metric_batch(&frame).expect("gauge batch decodes");
        match ev {
            DiagEvent::MetricBatch {
                descriptors,
                samples,
                ..
            } => {
                assert_eq!(descriptors.len(), 1);
                assert_eq!(descriptors[0].metric_id, 10);
                assert_eq!(descriptors[0].kind, 0);
                assert_eq!(descriptors[0].name, "temperature");
                assert_eq!(descriptors[0].unit, "Cel");
                assert_eq!(descriptors[0].description, "Motor temperature");

                assert_eq!(samples.len(), 2);
                assert_eq!(samples[0].metric_id, 10);
                assert_eq!(samples[0].value, 42.5);
                assert_eq!(samples[1].value, 43.1);
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_with_counter_descriptor() {
        let mut frame = metric_batch_header(1, 1, 100, 2000, 1000, 2000);
        frame.extend_from_slice(&metric_descriptor_bytes(
            20,
            1, // Sum/Counter
            1, // is_monotonic
            "request_count",
            "1",
            "HTTP requests",
            &[],
            None,
        ));
        frame.extend_from_slice(&metric_sample_bytes(20, 0, 1000, 5.0));

        let ev = decode_metric_batch(&frame).expect("counter batch decodes");
        match ev {
            DiagEvent::MetricBatch { descriptors, .. } => {
                assert_eq!(descriptors[0].kind, 1);
                assert_eq!(descriptors[0].flags & 1, 1); // is_monotonic
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_with_histogram_descriptor() {
        let bounds = vec![10.0, 20.0, 50.0, 100.0];
        let mut frame = metric_batch_header(1, 3, 100, 3000, 1000, 4000);
        frame.extend_from_slice(&metric_descriptor_bytes(
            30,
            2, // Histogram
            0,
            "response_time",
            "ms",
            "HTTP response time",
            &[],
            Some(&bounds),
        ));
        frame.extend_from_slice(&metric_sample_bytes(30, 1, 1000, 5.0)); // bucket 0-10
        frame.extend_from_slice(&metric_sample_bytes(30, 1, 2000, 25.0)); // bucket 20-50
        frame.extend_from_slice(&metric_sample_bytes(30, 1, 4000, 150.0)); // bucket >100

        let ev = decode_metric_batch(&frame).expect("histogram batch decodes");
        match ev {
            DiagEvent::MetricBatch { descriptors, .. } => {
                assert_eq!(descriptors[0].kind, 2);
                let bounds_opt = descriptors[0].histogram_bounds.as_ref();
                assert!(bounds_opt.is_some());
                let hist_bounds = bounds_opt.unwrap();
                assert_eq!(hist_bounds.len(), 4);
                assert_eq!(hist_bounds[0], 10.0);
                assert_eq!(hist_bounds[3], 100.0);
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_with_attributes() {
        let attrs = vec![
            ("device_id".to_string(), "motor_1".to_string()),
            ("location".to_string(), "warehouse_A".to_string()),
        ];
        let mut frame = metric_batch_header(1, 0, 100, 1000, 0, 0);
        frame.extend_from_slice(&metric_descriptor_bytes(
            40,
            0,
            0,
            "vibration",
            "mm/s",
            "Motor vibration level",
            &attrs,
            None,
        ));

        let ev = decode_metric_batch(&frame).expect("batch with attrs decodes");
        match ev {
            DiagEvent::MetricBatch { descriptors, .. } => {
                assert_eq!(descriptors[0].attributes.len(), 2);
                assert_eq!(descriptors[0].attributes[0].0, "device_id");
                assert_eq!(descriptors[0].attributes[0].1, "motor_1");
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_rejects_short_header() {
        let frame = vec![0_u8; 31];
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_wrong_version() {
        let mut frame = metric_batch_header(0, 0, 100, 1000, 0, 0);
        frame[0] = 99;
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_wrong_event_type() {
        let mut frame = metric_batch_header(0, 0, 100, 1000, 0, 0);
        frame[1] = 10; // task diag event type
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_attr_count_gt_8() {
        let mut frame = metric_batch_header(1, 0, 100, 1000, 0, 0);
        // Build a descriptor with attr_count=9
        let mut desc = Vec::new();
        desc.extend_from_slice(&10_u16.to_le_bytes());
        desc.push(0); // kind
        desc.push(0); // flags
        desc.push(0); // name_len
        desc.push(0); // unit_len
        desc.push(0); // description_len
        desc.push(9); // attr_count = 9 (invalid)
        desc.push(0); // histogram_bucket_count
        desc.push(0); // reserved
        desc.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&desc);
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_invalid_utf8_name() {
        let mut frame = metric_batch_header(1, 0, 100, 1000, 0, 0);
        let mut desc = Vec::new();
        desc.extend_from_slice(&10_u16.to_le_bytes());
        desc.push(0); // kind
        desc.push(0); // flags
        desc.push(2); // name_len = 2
        desc.push(0); // unit_len
        desc.push(0); // description_len
        desc.push(0); // attr_count
        desc.push(0); // histogram_bucket_count
        desc.push(0); // reserved
        desc.extend_from_slice(&0_u16.to_le_bytes());
        desc.push(0xFF);
        desc.push(0xFF); // Invalid UTF-8
        frame.extend_from_slice(&desc);
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_truncated_descriptor() {
        let mut frame = metric_batch_header(1, 0, 100, 1000, 0, 0);
        // Add a descriptor header but don't complete it
        frame.push(10); // partial metric_id
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_truncated_samples() {
        let mut frame = metric_batch_header(0, 2, 100, 1000, 0, 0);
        frame.extend_from_slice(&metric_sample_bytes(10, 0, 1000, 42.0));
        // Missing second sample
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_rejects_extra_trailing_bytes() {
        let mut frame = metric_batch_header(0, 0, 100, 1000, 0, 0);
        frame.push(0xFF); // Extra byte
        assert!(decode_metric_batch(&frame).is_none());
    }

    #[test]
    fn decode_metric_batch_multi_descriptors_then_samples() {
        let mut frame = metric_batch_header(2, 2, 100, 1000, 1000, 3000);
        // First descriptor: gauge temperature
        frame.extend_from_slice(&metric_descriptor_bytes(
            10,
            0,
            0,
            "temperature",
            "Cel",
            "",
            &[],
            None,
        ));
        // Second descriptor: counter requests
        frame.extend_from_slice(&metric_descriptor_bytes(
            20,
            1,
            1,
            "requests",
            "1",
            "",
            &[],
            None,
        ));
        // Two samples referencing different metrics
        frame.extend_from_slice(&metric_sample_bytes(10, 0, 1000, 25.5));
        frame.extend_from_slice(&metric_sample_bytes(20, 0, 3000, 10.0));

        let ev = decode_metric_batch(&frame).expect("multi-descriptor batch decodes");
        match ev {
            DiagEvent::MetricBatch {
                descriptors,
                samples,
                ..
            } => {
                assert_eq!(descriptors.len(), 2);
                assert_eq!(samples.len(), 2);
                assert_eq!(samples[0].metric_id, 10);
                assert_eq!(samples[1].metric_id, 20);
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    #[test]
    fn decode_metric_batch_samples_only_follow_up() {
        // A follow-up batch with descriptor_count=0 and only samples
        let mut frame = metric_batch_header(0, 1, 100, 2000, 5000, 6000);
        frame.extend_from_slice(&metric_sample_bytes(10, 0, 5000, 50.0));

        let ev = decode_metric_batch(&frame).expect("samples-only batch decodes");
        match ev {
            DiagEvent::MetricBatch {
                descriptors,
                samples,
                cycle_count,
                ..
            } => {
                assert!(descriptors.is_empty());
                assert_eq!(samples.len(), 1);
                assert_eq!(cycle_count, 2000);
            }
            _ => panic!("expected MetricBatch"),
        }
    }

    // ----- decode_metric_aggregate ------------------------------------------------

    /// Build a 52-byte FB_Metrics aggregate header. Helper to keep the
    /// per-test setup short.
    #[allow(clippy::too_many_arguments)]
    fn agg_header(
        flags: u8,
        body_schema: u8,
        sample_size: u32,
        sample_count: u32,
        metric_id: u32,
        task_index: u8,
        cycle_count_start: u32,
        cycle_count_end: u32,
        dc_time_start: i64,
        dc_time_end: i64,
        name_len: u8,
        unit_len: u8,
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(PUSH_METRIC_AGG_HEADER_SIZE);
        v.push(PUSH_WIRE_VERSION);
        v.push(PUSH_METRIC_AGG_EVENT_TYPE);
        v.push(flags);
        v.push(body_schema);
        v.extend_from_slice(&sample_size.to_le_bytes());
        v.extend_from_slice(&sample_count.to_le_bytes());
        v.extend_from_slice(&metric_id.to_le_bytes());
        v.push(task_index);
        v.extend_from_slice(&[0_u8; 3]); // pad to 0x14
        v.extend_from_slice(&cycle_count_start.to_le_bytes());
        v.extend_from_slice(&cycle_count_end.to_le_bytes());
        v.extend_from_slice(&0_u32.to_le_bytes()); // pad to 0x20
        v.extend_from_slice(&dc_time_start.to_le_bytes());
        v.extend_from_slice(&dc_time_end.to_le_bytes());
        v.push(name_len);
        v.push(unit_len);
        v.extend_from_slice(&[0_u8; 2]); // pad to 52
        assert_eq!(v.len(), PUSH_METRIC_AGG_HEADER_SIZE);
        v
    }

    #[test]
    fn decode_metric_aggregate_numeric_round_trip() {
        let mut frame = agg_header(
            0,                  // no trace ctx, no overflow
            2,                  // Numeric
            8,                  // sample_size
            3,                  // sample_count
            0xCAFEBABE,         // metric_id
            5,                  // task_index
            1000,               // cycle_count_start
            1010,               // cycle_count_end
            100_000,            // dc_time_start
            120_000,            // dc_time_end
            17,                 // name_len ("motor.temperature")
            3,                  // unit_len ("Cel")
        );
        frame.extend_from_slice(b"motor.temperature");
        frame.extend_from_slice(b"Cel");
        for v in [21.5_f64, 22.0, 22.7] {
            frame.extend_from_slice(&v.to_le_bytes());
        }

        let ev = decode_metric_aggregate(&frame).expect("numeric agg decodes");
        match ev {
            DiagEvent::MetricAggregateBatch {
                metric_id,
                task_index,
                flags,
                body_schema,
                sample_size,
                cycle_count_start,
                cycle_count_end,
                dc_time_start,
                dc_time_end,
                name,
                unit,
                trace_id,
                span_id,
                samples,
            } => {
                assert_eq!(metric_id, 0xCAFEBABE);
                assert_eq!(task_index, 5);
                assert_eq!(flags, 0);
                assert_eq!(body_schema, MetricBodySchema::Numeric);
                assert_eq!(sample_size, 8);
                assert_eq!(cycle_count_start, 1000);
                assert_eq!(cycle_count_end, 1010);
                assert_eq!(dc_time_start, 100_000);
                assert_eq!(dc_time_end, 120_000);
                assert_eq!(name, "motor.temperature");
                assert_eq!(unit, "Cel");
                assert!(trace_id.is_none());
                assert!(span_id.is_none());
                assert_eq!(samples.len(), 3);
                assert_eq!(samples[0], MetricAggregateSample::Numeric(21.5));
                assert_eq!(samples[2], MetricAggregateSample::Numeric(22.7));
            }
            _ => panic!("expected MetricAggregateBatch"),
        }
    }

    #[test]
    fn decode_metric_aggregate_bool_round_trip() {
        let mut frame = agg_header(0, 1, 1, 4, 1, 1, 0, 0, 0, 0, 4, 0);
        frame.extend_from_slice(b"door");
        frame.extend_from_slice(&[1, 0, 1, 1]);

        let ev = decode_metric_aggregate(&frame).expect("bool decodes");
        if let DiagEvent::MetricAggregateBatch { samples, .. } = ev {
            assert_eq!(
                samples,
                vec![
                    MetricAggregateSample::Bool(true),
                    MetricAggregateSample::Bool(false),
                    MetricAggregateSample::Bool(true),
                    MetricAggregateSample::Bool(true),
                ]
            );
        } else {
            panic!("expected MetricAggregateBatch");
        }
    }

    #[test]
    fn decode_metric_aggregate_string_strips_zero_padding() {
        // sample_size = 8 bytes; samples shorter than 8 are zero-padded.
        let mut frame = agg_header(0, 4, 8, 2, 0, 0, 0, 0, 0, 0, 0, 0);
        frame.extend_from_slice(b"hello\0\0\0");
        frame.extend_from_slice(b"hi\0\0\0\0\0\0");

        let ev = decode_metric_aggregate(&frame).expect("string decodes");
        if let DiagEvent::MetricAggregateBatch { samples, .. } = ev {
            assert_eq!(samples[0], MetricAggregateSample::String("hello".into()));
            assert_eq!(samples[1], MetricAggregateSample::String("hi".into()));
        } else {
            panic!("expected MetricAggregateBatch");
        }
    }

    #[test]
    fn decode_metric_aggregate_with_trace_context() {
        // 1-sample bool with full trace_id/span_id.
        let mut frame = agg_header(METRIC_FLAG_HAS_TRACE_CTX, 1, 1, 1, 0, 0, 0, 0, 0, 0, 4, 0);
        let trace_id_bytes: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00,
        ];
        let span_id_bytes: [u8; 8] = [0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe];
        frame.extend_from_slice(&trace_id_bytes);
        frame.extend_from_slice(&span_id_bytes);
        frame.extend_from_slice(b"trip");
        frame.push(1);

        let ev = decode_metric_aggregate(&frame).expect("trace ctx decodes");
        if let DiagEvent::MetricAggregateBatch {
            trace_id, span_id, ..
        } = ev
        {
            assert_eq!(trace_id, Some(trace_id_bytes));
            assert_eq!(span_id, Some(span_id_bytes));
        } else {
            panic!("expected MetricAggregateBatch");
        }
    }

    #[test]
    fn decode_metric_aggregate_overflow_flag_preserved() {
        let mut frame = agg_header(METRIC_FLAG_RING_OVERFLOWED, 2, 8, 1, 0, 0, 0, 0, 0, 0, 0, 0);
        frame.extend_from_slice(&3.14_f64.to_le_bytes());
        let ev = decode_metric_aggregate(&frame).unwrap();
        if let DiagEvent::MetricAggregateBatch { flags, .. } = ev {
            assert_eq!(flags & METRIC_FLAG_RING_OVERFLOWED, METRIC_FLAG_RING_OVERFLOWED);
        } else {
            panic!("expected MetricAggregateBatch");
        }
    }

    #[test]
    fn decode_metric_aggregate_rejects_short_header() {
        let frame = vec![0_u8; PUSH_METRIC_AGG_HEADER_SIZE - 1];
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_wrong_version() {
        let mut frame = agg_header(0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        frame[0] = 99;
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_wrong_event_type() {
        let mut frame = agg_header(0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        frame[1] = 22;
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_unknown_body_schema() {
        let frame = agg_header(0, 99, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_numeric_with_wrong_sample_size() {
        let frame = agg_header(0, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0); // numeric must be 8
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_bool_with_wrong_sample_size() {
        let frame = agg_header(0, 1, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0); // bool must be 1
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_truncated_body() {
        // Promises 3 samples × 8B but only delivers 16 bytes.
        let mut frame = agg_header(0, 2, 8, 3, 0, 0, 0, 0, 0, 0, 0, 0);
        frame.extend_from_slice(&[0_u8; 16]);
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_extra_trailing_bytes() {
        let mut frame = agg_header(0, 2, 8, 1, 0, 0, 0, 0, 0, 0, 0, 0);
        frame.extend_from_slice(&1.0_f64.to_le_bytes());
        frame.push(0xff); // garbage tail
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_rejects_invalid_utf8_name() {
        let mut frame = agg_header(0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 2, 0);
        frame.extend_from_slice(&[0xff, 0xff]); // invalid UTF-8
        frame.push(1);
        assert!(decode_metric_aggregate(&frame).is_none());
    }

    #[test]
    fn decode_metric_aggregate_empty_body_ok() {
        let frame = agg_header(0, 2, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        let ev = decode_metric_aggregate(&frame).expect("empty body decodes");
        if let DiagEvent::MetricAggregateBatch { samples, .. } = ev {
            assert!(samples.is_empty());
        } else {
            panic!("expected MetricAggregateBatch");
        }
    }
}

//! Decoder for push-diagnostic batch frames sent by TwinCAT PLCs.
//!
//! Each task on the PLC runs its own per-cycle sampler (see `FB_Log4TcTaskDiag`
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
    DiagEvent, DiagSample, PUSH_BATCH_EVENT_TYPE, PUSH_BATCH_HEADER_SIZE, PUSH_BATCH_MAX_SAMPLES,
    PUSH_SAMPLE_SIZE, PUSH_WIRE_VERSION,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{
        SAMPLE_FLAG_CYCLE_EXCEED, SAMPLE_FLAG_OVERFLOW, SAMPLE_FLAG_RT_VIOLATION,
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
}

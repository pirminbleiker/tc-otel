//! Decoder for push-diagnostic write payloads.
//!
//! This module decodes ADS Write payloads sent to `IG_PUSH_DIAG` with various
//! index offsets, transforming them into `DiagEvent` instances for the metrics
//! pipeline.

use crate::diagnostics::{DiagEvent, PUSH_WIRE_VERSION};

/// Edge kind classifier for push-diagnostic edge events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    CycleExceed,
    RtViolation,
}

/// Decode a push-diagnostic snapshot write payload into a list of DiagEvents.
///
/// A snapshot contains configuration and execution stats for all tasks,
/// typically emitted once per PLC cycle to avoid sampling gaps.
///
/// Wire format (little-endian):
/// +0x00  u8  version = 1
/// +0x01  u8  event_type = 0
/// +0x02  u16 num_tasks
/// +0x04  u64 plc_timestamp_ns
/// +0x0C  u32 reserved
/// +0x10  per-task × num_tasks, 72 B each
pub fn decode_snapshot(bytes: &[u8]) -> Vec<DiagEvent> {
    if bytes.len() < 16 {
        return Vec::new();
    }

    let version = bytes[0];
    let _event_type = bytes[1];
    let num_tasks = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let plc_timestamp_ns = u64::from_le_bytes([
        bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
    ]);

    // Check version
    if version != PUSH_WIRE_VERSION {
        return Vec::new();
    }

    // Verify payload size: 16-byte header + 72 bytes per task
    let expected_len = 16 + 72 * num_tasks;
    if bytes.len() < expected_len {
        return Vec::new();
    }

    let mut events = Vec::new();

    for i in 0..num_tasks {
        let offset = 16 + i * 72;
        let task_data = &bytes[offset..offset + 72];

        let _task_obj_id =
            u32::from_le_bytes([task_data[0], task_data[1], task_data[2], task_data[3]]);
        let ads_port = u32::from_le_bytes([task_data[4], task_data[5], task_data[6], task_data[7]]);
        let priority =
            u32::from_le_bytes([task_data[8], task_data[9], task_data[10], task_data[11]]);
        let cycle_time_us =
            u32::from_le_bytes([task_data[12], task_data[13], task_data[14], task_data[15]]);
        let last_exec_time_us =
            u32::from_le_bytes([task_data[16], task_data[17], task_data[18], task_data[19]]);
        // task_data[20..24] is reserved
        let cycle_count = u64::from_le_bytes([
            task_data[24],
            task_data[25],
            task_data[26],
            task_data[27],
            task_data[28],
            task_data[29],
            task_data[30],
            task_data[31],
        ]);
        let cycle_exceed_count = u64::from_le_bytes([
            task_data[32],
            task_data[33],
            task_data[34],
            task_data[35],
            task_data[36],
            task_data[37],
            task_data[38],
            task_data[39],
        ]);
        let rt_violation_count = u64::from_le_bytes([
            task_data[40],
            task_data[41],
            task_data[42],
            task_data[43],
            task_data[44],
            task_data[45],
            task_data[46],
            task_data[47],
        ]);
        let flags =
            u32::from_le_bytes([task_data[48], task_data[49], task_data[50], task_data[51]]);
        let task_name_bytes = &task_data[52..72];
        let task_name = String::from_utf8_lossy(task_name_bytes)
            .trim_end_matches('\0')
            .to_string();

        events.push(DiagEvent::TaskSnapshot {
            task_port: ads_port as u16,
            task_name,
            priority,
            cycle_time_configured_us: cycle_time_us,
            last_exec_time_us,
            cycle_count,
            cycle_exceed_count,
            rt_violation_count,
            flags,
            plc_timestamp_ns,
        });
    }

    events
}

/// Decode a push-diagnostic edge event (cycle-exceed or RT-violation).
///
/// These are transient events that would be invisible in polled diagnostics.
///
/// Wire format (little-endian, 44 B):
/// +0x00 u8  version = 1
/// +0x01 u8  event_type (1 or 2)
/// +0x02 u16 reserved
/// +0x04 u32 ads_port
/// +0x08 u64 cycle_count
/// +0x10 u32 last_exec_time_us
/// +0x14 u32 reserved
/// +0x18 [u8; 20] task_name
pub fn decode_edge(bytes: &[u8], which: EdgeKind) -> Option<DiagEvent> {
    if bytes.len() < 44 {
        return None;
    }

    let version = bytes[0];
    let _event_type = bytes[1];
    // bytes[2..4] reserved

    // Check version
    if version != PUSH_WIRE_VERSION {
        return None;
    }

    let ads_port = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let cycle_count = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    let last_exec_time_us = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    // bytes[20..24] reserved
    let task_name_bytes = &bytes[24..44];
    let task_name = String::from_utf8_lossy(task_name_bytes)
        .trim_end_matches('\0')
        .to_string();

    match which {
        EdgeKind::CycleExceed => Some(DiagEvent::CycleExceedEdge {
            task_port: ads_port as u16,
            task_name,
            cycle_count,
            last_exec_time_us,
        }),
        EdgeKind::RtViolation => Some(DiagEvent::RtViolationEdge {
            task_port: ads_port as u16,
            task_name,
            cycle_count,
            last_exec_time_us,
        }),
    }
}

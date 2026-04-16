//! Decoder for push-diagnostic frames sent by TwinCAT PLCs.
//!
//! PLCs push per-task diagnostics (cycle time, exec time, edge events) via AdsWrite
//! to index group `IG_PUSH_DIAG` with three distinct index offsets: snapshot,
//! cycle-exceed edge, and real-time violation edge. All frames are little-endian.
//!
//! Snapshot frames contain a batch of one or more task snapshots (up to N tasks per frame).
//! Edge frames contain a single event for one task and use the wire format to signal
//! the event kind via `event_type`.

use crate::diagnostics::{DiagEvent, PUSH_WIRE_VERSION};

/// Discriminates edge event type during decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// event_type must be 1.
    CycleExceed,
    /// event_type must be 2.
    RtViolation,
}

/// Decode a snapshot frame (16 + 72 × num_tasks bytes).
///
/// Returns one `TaskSnapshot` DiagEvent per task in the frame.
/// Rejects frames with wrong version or event_type; returns empty vec on malformed input.
/// Names are decoded with lossy UTF-8 and trailing NULs stripped.
pub fn decode_snapshot(bytes: &[u8]) -> Vec<DiagEvent> {
    // Minimum: header is 16 bytes (0x00..0x0F).
    if bytes.len() < 16 {
        return vec![];
    }

    let version = bytes[0];
    let event_type = bytes[1];
    let num_tasks = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;

    // Validate version and event_type.
    if version != PUSH_WIRE_VERSION || event_type != 0 {
        return vec![];
    }

    // Check that we have enough bytes: 16 + 72*num_tasks.
    let body_len = num_tasks.saturating_mul(72);
    if bytes.len() < 16 + body_len {
        return vec![];
    }

    let plc_timestamp_ns =
        u64::from_le_bytes(bytes[4..12].try_into().unwrap_or([0; 8]));

    let mut events = Vec::with_capacity(num_tasks);
    for i in 0..num_tasks {
        let offset = 16 + i * 72;
        let task_bytes = &bytes[offset..offset + 72];

        // +0x04: ads_port → u16 truncation
        let ads_port = u32::from_le_bytes(task_bytes[4..8].try_into().unwrap_or([0; 4]));
        let task_port = (ads_port & 0xFFFF) as u16;

        // +0x08: priority
        let priority = u32::from_le_bytes(task_bytes[8..12].try_into().unwrap_or([0; 4]));

        // +0x0C: cycle_time_us
        let cycle_time_configured_us =
            u32::from_le_bytes(task_bytes[12..16].try_into().unwrap_or([0; 4]));

        // +0x10: last_exec_time_us
        let last_exec_time_us =
            u32::from_le_bytes(task_bytes[16..20].try_into().unwrap_or([0; 4]));

        // +0x18: cycle_count
        let cycle_count =
            u64::from_le_bytes(task_bytes[24..32].try_into().unwrap_or([0; 8]));

        // +0x20: cycle_exceed_count
        let cycle_exceed_count =
            u64::from_le_bytes(task_bytes[32..40].try_into().unwrap_or([0; 8]));

        // +0x28: rt_violation_count
        let rt_violation_count =
            u64::from_le_bytes(task_bytes[40..48].try_into().unwrap_or([0; 8]));

        // +0x30: flags
        let flags = u32::from_le_bytes(task_bytes[48..52].try_into().unwrap_or([0; 4]));

        // +0x34: task_name [20]
        let name_bytes = &task_bytes[52..72];
        let task_name = decode_task_name(name_bytes);

        events.push(DiagEvent::TaskSnapshot {
            task_port,
            task_name,
            priority,
            cycle_time_configured_us,
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

/// Decode one edge event frame (44 bytes).
///
/// The `which` parameter selects the expected `event_type` (1 for CycleExceed, 2 for RtViolation).
/// Returns `None` if version/event_type mismatch, frame is too short, or malformed.
/// Names are decoded with lossy UTF-8 and trailing NULs stripped.
pub fn decode_edge(bytes: &[u8], which: EdgeKind) -> Option<DiagEvent> {
    // Minimum frame size is 44 bytes.
    if bytes.len() < 44 {
        return None;
    }

    let version = bytes[0];
    let event_type = bytes[1];

    // Validate version.
    if version != PUSH_WIRE_VERSION {
        return None;
    }

    // Validate event_type matches the expected kind.
    let expected_event_type = match which {
        EdgeKind::CycleExceed => 1_u8,
        EdgeKind::RtViolation => 2_u8,
    };
    if event_type != expected_event_type {
        return None;
    }

    // +0x04: ads_port → u16 truncation
    let ads_port = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
    let task_port = (ads_port & 0xFFFF) as u16;

    // +0x08: cycle_count
    let cycle_count = u64::from_le_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));

    // +0x10: last_exec_time_us
    let last_exec_time_us = u32::from_le_bytes(bytes[16..20].try_into().unwrap_or([0; 4]));

    // +0x18: task_name [20]
    let name_bytes = &bytes[24..44];
    let task_name = decode_task_name(name_bytes);

    let event = match which {
        EdgeKind::CycleExceed => DiagEvent::CycleExceedEdge {
            task_port,
            task_name,
            cycle_count,
            last_exec_time_us,
        },
        EdgeKind::RtViolation => DiagEvent::RtViolationEdge {
            task_port,
            task_name,
            cycle_count,
            last_exec_time_us,
        },
    };

    Some(event)
}

/// Decode a task name from a fixed 20-byte array.
///
/// Strips trailing NULs and decodes with lossy UTF-8 to avoid panics on invalid UTF-8.
fn decode_task_name(bytes: &[u8]) -> String {
    // Find the last non-NUL byte.
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }

    // Decode with lossy UTF-8.
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{PUSH_FLAG_CYCLE_EXCEED_NOW, PUSH_FLAG_RT_VIOLATION_NOW};

    /// Helper: build a snapshot frame header.
    fn snapshot_header(num_tasks: usize, plc_timestamp_ns: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(16);
        v.push(PUSH_WIRE_VERSION);
        v.push(0); // event_type = 0
        v.extend_from_slice(&(num_tasks as u16).to_le_bytes());
        v.extend_from_slice(&plc_timestamp_ns.to_le_bytes());
        v.extend_from_slice(&0_u32.to_le_bytes()); // reserved
        v
    }

    /// Helper: build a single task record (72 bytes).
    #[allow(clippy::too_many_arguments)]
    fn task_record(
        task_obj_id: u32,
        ads_port: u32,
        priority: u32,
        cycle_time_us: u32,
        last_exec_time_us: u32,
        cycle_count: u64,
        cycle_exceed_count: u64,
        rt_violation_count: u64,
        flags: u32,
        task_name: &str,
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(72);
        v.extend_from_slice(&task_obj_id.to_le_bytes());
        v.extend_from_slice(&ads_port.to_le_bytes());
        v.extend_from_slice(&priority.to_le_bytes());
        v.extend_from_slice(&cycle_time_us.to_le_bytes());
        v.extend_from_slice(&last_exec_time_us.to_le_bytes());
        v.extend_from_slice(&0_u32.to_le_bytes()); // reserved +0x14
        v.extend_from_slice(&cycle_count.to_le_bytes());
        v.extend_from_slice(&cycle_exceed_count.to_le_bytes());
        v.extend_from_slice(&rt_violation_count.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());

        // Encode task_name into 20-byte array with NUL padding.
        let mut name_bytes = [0_u8; 20];
        let name_len = task_name.len().min(20);
        name_bytes[..name_len].copy_from_slice(&task_name.as_bytes()[..name_len]);
        v.extend_from_slice(&name_bytes);

        v
    }

    /// Helper: build an edge frame (44 bytes).
    fn edge_frame(
        event_type: u8,
        ads_port: u32,
        cycle_count: u64,
        last_exec_time_us: u32,
        task_name: &str,
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(44);
        v.push(PUSH_WIRE_VERSION);
        v.push(event_type);
        v.extend_from_slice(&0_u16.to_le_bytes()); // reserved +0x02
        v.extend_from_slice(&ads_port.to_le_bytes());
        v.extend_from_slice(&cycle_count.to_le_bytes());
        v.extend_from_slice(&last_exec_time_us.to_le_bytes());
        v.extend_from_slice(&0_u32.to_le_bytes()); // reserved +0x14

        // task_name [20]
        let mut name_bytes = [0_u8; 20];
        let name_len = task_name.len().min(20);
        name_bytes[..name_len].copy_from_slice(&task_name.as_bytes()[..name_len]);
        v.extend_from_slice(&name_bytes);

        v
    }

    #[test]
    fn decode_snapshot_with_one_task() {
        let mut frame = snapshot_header(1, 1_000_000_000);
        frame.extend_from_slice(&task_record(
            100,           // task_obj_id
            350,           // ads_port
            20,            // priority
            1000,          // cycle_time_us
            500,           // last_exec_time_us
            1234,          // cycle_count
            5,             // cycle_exceed_count
            0,             // rt_violation_count
            PUSH_FLAG_CYCLE_EXCEED_NOW, // flags
            "PlcTask",     // task_name
        ));

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 1, "should decode exactly one task");

        match &events[0] {
            DiagEvent::TaskSnapshot {
                task_port,
                task_name,
                priority,
                cycle_time_configured_us,
                last_exec_time_us,
                cycle_count,
                cycle_exceed_count,
                rt_violation_count,
                flags,
                plc_timestamp_ns,
            } => {
                assert_eq!(*task_port, 350);
                assert_eq!(task_name, "PlcTask");
                assert_eq!(*priority, 20);
                assert_eq!(*cycle_time_configured_us, 1000);
                assert_eq!(*last_exec_time_us, 500);
                assert_eq!(*cycle_count, 1234);
                assert_eq!(*cycle_exceed_count, 5);
                assert_eq!(*rt_violation_count, 0);
                assert_eq!(*flags, PUSH_FLAG_CYCLE_EXCEED_NOW);
                assert_eq!(*plc_timestamp_ns, 1_000_000_000);
            }
            _ => panic!("expected TaskSnapshot variant"),
        }
    }

    #[test]
    fn decode_snapshot_with_three_tasks() {
        let mut frame = snapshot_header(3, 2_000_000_000);
        frame.extend_from_slice(&task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "PlcTask"));
        frame.extend_from_slice(&task_record(101, 351, 10, 10000, 5000, 123, 0, 1, PUSH_FLAG_RT_VIOLATION_NOW, "PlcTask1"));
        frame.extend_from_slice(&task_record(102, 340, 5, 1000, 100, 9999, 0, 0, 0, "IoIdle"));

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 3, "should decode all three tasks");

        // Verify order is preserved.
        if let DiagEvent::TaskSnapshot { task_name: n1, .. } = &events[0] {
            assert_eq!(n1, "PlcTask");
        } else {
            panic!("expected first event to be TaskSnapshot");
        }
        if let DiagEvent::TaskSnapshot { task_name: n2, .. } = &events[1] {
            assert_eq!(n2, "PlcTask1");
        } else {
            panic!("expected second event to be TaskSnapshot");
        }
        if let DiagEvent::TaskSnapshot { task_name: n3, .. } = &events[2] {
            assert_eq!(n3, "IoIdle");
        } else {
            panic!("expected third event to be TaskSnapshot");
        }
    }

    #[test]
    fn decode_snapshot_rejects_wrong_version() {
        let mut frame = snapshot_header(1, 1_000_000_000);
        frame[0] = 99; // wrong version
        frame.extend_from_slice(&task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "PlcTask"));

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 0, "should reject wrong version");
    }

    #[test]
    fn decode_snapshot_rejects_wrong_event_type() {
        let mut frame = snapshot_header(1, 1_000_000_000);
        frame[1] = 1; // wrong event_type (should be 0)
        frame.extend_from_slice(&task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "PlcTask"));

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 0, "should reject wrong event_type");
    }

    #[test]
    fn decode_snapshot_rejects_truncated_payload() {
        let mut frame = snapshot_header(2, 1_000_000_000); // claims 2 tasks
        frame.extend_from_slice(&task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "PlcTask"));
        // Missing second task

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 0, "should reject truncated payload");
    }

    #[test]
    fn decode_edge_cycle_exceed() {
        let frame = edge_frame(1, 350, 5000, 750, "PlcTask");
        let event = decode_edge(&frame, EdgeKind::CycleExceed);

        assert!(event.is_some());
        match event.unwrap() {
            DiagEvent::CycleExceedEdge {
                task_port,
                task_name,
                cycle_count,
                last_exec_time_us,
            } => {
                assert_eq!(task_port, 350);
                assert_eq!(task_name, "PlcTask");
                assert_eq!(cycle_count, 5000);
                assert_eq!(last_exec_time_us, 750);
            }
            _ => panic!("expected CycleExceedEdge variant"),
        }
    }

    #[test]
    fn decode_edge_rejects_wrong_event_type_for_kind() {
        let frame = edge_frame(2, 350, 5000, 750, "PlcTask"); // event_type=2 (RtViolation)
        let event = decode_edge(&frame, EdgeKind::CycleExceed); // but we expect CycleExceed
        assert!(event.is_none(), "should reject mismatched event_type");
    }

    #[test]
    fn decode_edge_rejects_short_frame() {
        let frame = &[0_u8; 40]; // too short
        let event = decode_edge(frame, EdgeKind::CycleExceed);
        assert!(event.is_none(), "should reject short frame");
    }

    #[test]
    fn task_name_handles_null_padding() {
        let mut frame = snapshot_header(1, 1_000_000_000);
        frame.extend_from_slice(&task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "PlcTask"));

        let events = decode_snapshot(&frame);
        if let DiagEvent::TaskSnapshot { task_name, .. } = &events[0] {
            assert_eq!(task_name, "PlcTask", "should strip NUL padding");
        } else {
            panic!("expected TaskSnapshot");
        }
    }

    #[test]
    fn task_name_handles_non_utf8_gracefully() {
        // Build a frame with invalid UTF-8 in task name.
        let mut frame = snapshot_header(1, 1_000_000_000);

        // Create a task record but patch the name bytes with 0xFF.
        let mut record = task_record(100, 350, 20, 1000, 500, 1234, 5, 0, 0, "");
        // The name bytes start at offset 52 in the record (72 - 20).
        record[52] = 0xFF; // invalid UTF-8

        frame.extend_from_slice(&record);

        let events = decode_snapshot(&frame);
        assert_eq!(events.len(), 1, "should not panic on invalid UTF-8");

        if let DiagEvent::TaskSnapshot { task_name, .. } = &events[0] {
            // String::from_utf8_lossy will replace invalid sequences with U+FFFD.
            // We just check that decoding succeeded without panicking.
            assert!(!task_name.is_empty() || task_name.contains('\u{FFFD}'), "should use lossy decoding");
        } else {
            panic!("expected TaskSnapshot");
        }
    }
}

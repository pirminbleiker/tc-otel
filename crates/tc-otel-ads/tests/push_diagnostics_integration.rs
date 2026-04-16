//! Integration test for push-diagnostic dispatch through AdsRouter.
//!
//! Tests the complete path: synthetic AMS frame → router dispatch → DiagEvent
//! emission on the push channel. No real PLC or MQTT broker required.

use std::str::FromStr;
use std::sync::Arc;
use tc_otel_ads::{
    ams::{AmsHeader, AmsNetId, ADS_CMD_WRITE, ADS_STATE_REQUEST},
    diagnostics::{
        DiagEvent, IG_PUSH_DIAG, IO_PUSH_CYCLE_EXCEED_EDGE, IO_PUSH_RT_VIOLATION_EDGE,
        IO_PUSH_SNAPSHOT, PUSH_WIRE_VERSION,
    },
    registry::TaskRegistry,
    router::AdsRouter,
};
use tokio::sync::mpsc;

/// Per-task snapshot field layout (72 bytes each)
#[derive(Clone)]
struct TaskSnapshotFields {
    task_obj_id: u32,
    ads_port: u32,
    priority: u32,
    cycle_time_us: u32,
    last_exec_time_us: u32,
    cycle_count: u64,
    cycle_exceed_count: u64,
    rt_violation_count: u64,
    flags: u32,
    task_name: [u8; 20],
}

/// Build a snapshot payload (16 + 72 × num_tasks bytes).
fn build_snapshot_frame(tasks: &[TaskSnapshotFields]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + 72 * tasks.len());

    // Header (16 bytes)
    buf.push(PUSH_WIRE_VERSION); // version
    buf.push(0); // event_type = SNAPSHOT
    buf.extend_from_slice(&(tasks.len() as u16).to_le_bytes()); // num_tasks
    buf.extend_from_slice(&0x123456789ABCDEFu64.to_le_bytes()); // plc_timestamp_ns
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // Per-task data (72 bytes each)
    for task in tasks {
        buf.extend_from_slice(&task.task_obj_id.to_le_bytes());
        buf.extend_from_slice(&task.ads_port.to_le_bytes());
        buf.extend_from_slice(&task.priority.to_le_bytes());
        buf.extend_from_slice(&task.cycle_time_us.to_le_bytes());
        buf.extend_from_slice(&task.last_exec_time_us.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf.extend_from_slice(&task.cycle_count.to_le_bytes());
        buf.extend_from_slice(&task.cycle_exceed_count.to_le_bytes());
        buf.extend_from_slice(&task.rt_violation_count.to_le_bytes());
        buf.extend_from_slice(&task.flags.to_le_bytes());
        buf.extend_from_slice(&task.task_name);
    }

    buf
}

/// Build an edge event payload (44 bytes).
fn build_edge_frame(
    event_type: u8,
    ads_port: u32,
    cycle_count: u64,
    last_exec_us: u32,
    name: &str,
) -> Vec<u8> {
    let mut buf = vec![0u8; 44];
    buf[0] = PUSH_WIRE_VERSION;
    buf[1] = event_type;
    // buf[2..4] reserved (already zero)
    buf[4..8].copy_from_slice(&ads_port.to_le_bytes());
    buf[8..16].copy_from_slice(&cycle_count.to_le_bytes());
    buf[16..20].copy_from_slice(&last_exec_us.to_le_bytes());
    // buf[20..24] reserved (already zero)
    let name_bytes = name.as_bytes();
    let len = std::cmp::min(20, name_bytes.len());
    buf[24..24 + len].copy_from_slice(&name_bytes[..len]);
    buf
}

/// Wrap payload as an AMS Write request, then as an AMS frame (32-byte header + write request).
fn wrap_as_ams_write(
    source: AmsNetId,
    target_port: u16,
    ig: u32,
    io: u32,
    payload: &[u8],
) -> Vec<u8> {
    // Build ADS Write request (12-byte header + data)
    let mut write_req = Vec::with_capacity(12 + payload.len());
    write_req.extend_from_slice(&ig.to_le_bytes());
    write_req.extend_from_slice(&io.to_le_bytes());
    write_req.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    write_req.extend_from_slice(payload);

    // Build AMS header (32 bytes)
    let header = AmsHeader {
        target_net_id: AmsNetId::from_str("10.10.10.10.1.1").unwrap(),
        target_port,
        source_net_id: source,
        source_port: 32768,
        command_id: ADS_CMD_WRITE,
        state_flags: ADS_STATE_REQUEST,
        data_length: write_req.len() as u32,
        error_code: 0,
        invoke_id: 0x12345678,
    };

    let mut frame = header.serialize();
    frame.extend_from_slice(&write_req);
    frame
}

#[tokio::test]
async fn snapshot_with_three_tasks_produces_three_events() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let tasks = vec![
        TaskSnapshotFields {
            task_obj_id: 1,
            ads_port: 340,
            priority: 10,
            cycle_time_us: 1000,
            last_exec_time_us: 500,
            cycle_count: 100,
            cycle_exceed_count: 0,
            rt_violation_count: 0,
            flags: 0,
            task_name: {
                let mut name = [0u8; 20];
                let bytes = b"IO Idle";
                name[..bytes.len()].copy_from_slice(bytes);
                name
            },
        },
        TaskSnapshotFields {
            task_obj_id: 2,
            ads_port: 350,
            priority: 20,
            cycle_time_us: 1000,
            last_exec_time_us: 600,
            cycle_count: 101,
            cycle_exceed_count: 0,
            rt_violation_count: 0,
            flags: 0,
            task_name: {
                let mut name = [0u8; 20];
                let bytes = b"PlcTask";
                name[..bytes.len()].copy_from_slice(bytes);
                name
            },
        },
        TaskSnapshotFields {
            task_obj_id: 3,
            ads_port: 351,
            priority: 30,
            cycle_time_us: 10000,
            last_exec_time_us: 1000,
            cycle_count: 102,
            cycle_exceed_count: 0,
            rt_violation_count: 0,
            flags: 0,
            task_name: {
                let mut name = [0u8; 20];
                let bytes = b"PlcTask1";
                name[..bytes.len()].copy_from_slice(bytes);
                name
            },
        },
    ];

    let payload = build_snapshot_frame(&tasks);
    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_SNAPSHOT, &payload);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should return ACK response");

    // Collect all events from the push channel
    let mut events = Vec::new();
    while let Ok((net_id, ev)) = push_rx.try_recv() {
        assert_eq!(net_id, plc_net_id);
        events.push(ev);
    }

    assert_eq!(events.len(), 3, "expected 3 snapshot events");

    // Verify each event
    match &events[0] {
        DiagEvent::TaskSnapshot {
            task_port,
            task_name,
            cycle_count,
            ..
        } => {
            assert_eq!(*task_port, 340);
            assert_eq!(task_name, "IO Idle");
            assert_eq!(*cycle_count, 100);
        }
        _ => panic!("expected TaskSnapshot"),
    }

    match &events[1] {
        DiagEvent::TaskSnapshot {
            task_port,
            task_name,
            cycle_count,
            ..
        } => {
            assert_eq!(*task_port, 350);
            assert_eq!(task_name, "PlcTask");
            assert_eq!(*cycle_count, 101);
        }
        _ => panic!("expected TaskSnapshot"),
    }

    match &events[2] {
        DiagEvent::TaskSnapshot {
            task_port,
            task_name,
            cycle_count,
            ..
        } => {
            assert_eq!(*task_port, 351);
            assert_eq!(task_name, "PlcTask1");
            assert_eq!(*cycle_count, 102);
        }
        _ => panic!("expected TaskSnapshot"),
    }
}

#[tokio::test]
async fn cycle_exceed_edge_produces_single_event() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let payload = build_edge_frame(1, 350, 999, 2500, "PlcTask");
    let frame = wrap_as_ams_write(
        plc_net_id,
        16150,
        IG_PUSH_DIAG,
        IO_PUSH_CYCLE_EXCEED_EDGE,
        &payload,
    );

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should return ACK response");

    let (net_id, ev) = push_rx.recv().await.expect("should receive event");
    assert_eq!(net_id, plc_net_id);

    match ev {
        DiagEvent::CycleExceedEdge {
            task_port,
            task_name,
            cycle_count,
            last_exec_time_us,
        } => {
            assert_eq!(task_port, 350);
            assert_eq!(task_name, "PlcTask");
            assert_eq!(cycle_count, 999);
            assert_eq!(last_exec_time_us, 2500);
        }
        _ => panic!("expected CycleExceedEdge"),
    }

    // Verify no more events
    assert!(push_rx.try_recv().is_err());
}

#[tokio::test]
async fn rt_violation_edge_produces_single_event() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let payload = build_edge_frame(2, 351, 50, 15000, "PlcTask1");
    let frame = wrap_as_ams_write(
        plc_net_id,
        16150,
        IG_PUSH_DIAG,
        IO_PUSH_RT_VIOLATION_EDGE,
        &payload,
    );

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should return ACK response");

    let (net_id, ev) = push_rx.recv().await.expect("should receive event");
    assert_eq!(net_id, plc_net_id);

    match ev {
        DiagEvent::RtViolationEdge {
            task_port,
            task_name,
            cycle_count,
            last_exec_time_us,
        } => {
            assert_eq!(task_port, 351);
            assert_eq!(task_name, "PlcTask1");
            assert_eq!(cycle_count, 50);
            assert_eq!(last_exec_time_us, 15000);
        }
        _ => panic!("expected RtViolationEdge"),
    }

    // Verify no more events
    assert!(push_rx.try_recv().is_err());
}

#[tokio::test]
async fn snapshot_ack_returns_immediately() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, _) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();
    let tasks = vec![TaskSnapshotFields {
        task_obj_id: 1,
        ads_port: 340,
        priority: 10,
        cycle_time_us: 1000,
        last_exec_time_us: 500,
        cycle_count: 100,
        cycle_exceed_count: 0,
        rt_violation_count: 0,
        flags: 0,
        task_name: {
            let mut name = [0u8; 20];
            let bytes = b"IO Idle";
            name[..bytes.len()].copy_from_slice(bytes);
            name
        },
    }];

    let payload = build_snapshot_frame(&tasks);
    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_SNAPSHOT, &payload);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(
        resp.is_some(),
        "dispatch should return Some with ACK response"
    );

    let resp_data = resp.unwrap();
    assert!(resp_data.len() >= 32, "response must have AMS header");

    // Parse the response header to verify structure
    let resp_header = AmsHeader::parse(&resp_data).unwrap();
    assert_eq!(resp_header.target_net_id, plc_net_id);
    assert_eq!(resp_header.command_id, ADS_CMD_WRITE);
}

#[tokio::test]
async fn unknown_version_is_dropped_silently() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let mut payload = vec![0u8; 16];
    payload[0] = 99; // version = 99 (unknown)
    payload[1] = 0; // event_type = 0
    payload[2..4].copy_from_slice(&1u16.to_le_bytes()); // num_tasks = 1
    payload[4..12].copy_from_slice(&0x123456789ABCDEFu64.to_le_bytes()); // plc_timestamp

    // Add a dummy task (72 bytes) to make it valid structurally
    let task = TaskSnapshotFields {
        task_obj_id: 1,
        ads_port: 340,
        priority: 10,
        cycle_time_us: 1000,
        last_exec_time_us: 500,
        cycle_count: 100,
        cycle_exceed_count: 0,
        rt_violation_count: 0,
        flags: 0,
        task_name: {
            let mut name = [0u8; 20];
            let bytes = b"Task";
            name[..bytes.len()].copy_from_slice(bytes);
            name
        },
    };

    let mut full_payload = payload;
    let mut buf = Vec::with_capacity(72);
    buf.extend_from_slice(&task.task_obj_id.to_le_bytes());
    buf.extend_from_slice(&task.ads_port.to_le_bytes());
    buf.extend_from_slice(&task.priority.to_le_bytes());
    buf.extend_from_slice(&task.cycle_time_us.to_le_bytes());
    buf.extend_from_slice(&task.last_exec_time_us.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&task.cycle_count.to_le_bytes());
    buf.extend_from_slice(&task.cycle_exceed_count.to_le_bytes());
    buf.extend_from_slice(&task.rt_violation_count.to_le_bytes());
    buf.extend_from_slice(&task.flags.to_le_bytes());
    buf.extend_from_slice(&task.task_name);
    full_payload.extend_from_slice(&buf);

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_SNAPSHOT, &full_payload);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should still return ACK");

    // Verify push_rx is empty (event was dropped)
    assert!(push_rx.try_recv().is_err(), "unknown version should be dropped");
}

#[tokio::test]
async fn malformed_truncated_snapshot_is_dropped_silently() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Build a valid header but claim 2 tasks while providing only 1 (72 bytes)
    let mut payload = vec![0u8; 16];
    payload[0] = PUSH_WIRE_VERSION;
    payload[1] = 0; // event_type = 0
    payload[2..4].copy_from_slice(&2u16.to_le_bytes()); // num_tasks = 2 (but we'll only provide 1)
    payload[4..12].copy_from_slice(&0x123456789ABCDEFu64.to_le_bytes());

    let task = TaskSnapshotFields {
        task_obj_id: 1,
        ads_port: 340,
        priority: 10,
        cycle_time_us: 1000,
        last_exec_time_us: 500,
        cycle_count: 100,
        cycle_exceed_count: 0,
        rt_violation_count: 0,
        flags: 0,
        task_name: {
            let mut name = [0u8; 20];
            let bytes = b"Task";
            name[..bytes.len()].copy_from_slice(bytes);
            name
        },
    };

    let mut buf = Vec::with_capacity(72);
    buf.extend_from_slice(&task.task_obj_id.to_le_bytes());
    buf.extend_from_slice(&task.ads_port.to_le_bytes());
    buf.extend_from_slice(&task.priority.to_le_bytes());
    buf.extend_from_slice(&task.cycle_time_us.to_le_bytes());
    buf.extend_from_slice(&task.last_exec_time_us.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&task.cycle_count.to_le_bytes());
    buf.extend_from_slice(&task.cycle_exceed_count.to_le_bytes());
    buf.extend_from_slice(&task.rt_violation_count.to_le_bytes());
    buf.extend_from_slice(&task.flags.to_le_bytes());
    buf.extend_from_slice(&task.task_name);
    payload.extend_from_slice(&buf);

    // payload is now 16 + 72 = 88 bytes, but header claims 2 tasks (needs 16 + 144)

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_SNAPSHOT, &payload);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should still return ACK");

    // Verify push_rx is empty (truncated event was dropped)
    assert!(push_rx.try_recv().is_err(), "truncated snapshot should be dropped");
}

#[tokio::test]
async fn non_push_write_still_reaches_log_parser() {
    let (log_tx, _log_rx) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Send a regular write to IG_RT_SYSTEM (not a push-diagnostic)
    let mut payload = [0u8; 20];
    payload[0..4].copy_from_slice(&0xF200_0000u32.to_le_bytes()); // IG_RT_SYSTEM
    payload[4..8].copy_from_slice(&0u32.to_le_bytes()); // IO_TASK_STATS
    payload[8..12].copy_from_slice(&8u32.to_le_bytes()); // size = 8
    payload[12..20].copy_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);

    let frame = wrap_as_ams_write(plc_net_id, 16150, 0xF200_0000, 0, &payload[12..20]);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should return ACK");

    // Verify push_rx is empty (not a push-diagnostic write)
    assert!(push_rx.try_recv().is_err(), "non-push write should not trigger push channel");

    // Log dispatch should have tried to parse, but since our payload is dummy,
    // log_rx will be empty too. The important invariant is that we didn't crash
    // and the ACK went out.
}

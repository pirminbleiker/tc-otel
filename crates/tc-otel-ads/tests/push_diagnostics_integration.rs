//! Integration test for push-diagnostic dispatch through AdsRouter.
//!
//! Tests the complete path: synthetic AMS frame → router dispatch → DiagEvent
//! emission on the push channel. No real PLC or MQTT broker required.

use std::str::FromStr;
use std::sync::Arc;
use tc_otel_ads::{
    ams::{AmsHeader, AmsNetId, ADS_CMD_WRITE, ADS_STATE_REQUEST},
    diagnostics::{
        DiagEvent, IG_PUSH_DIAG, IO_PUSH_BATCH, PUSH_BATCH_EVENT_TYPE, PUSH_BATCH_HEADER_SIZE,
        PUSH_BATCH_MAX_SAMPLES, PUSH_SAMPLE_SIZE, PUSH_WIRE_VERSION, SAMPLE_FLAG_CYCLE_EXCEED,
        SAMPLE_FLAG_RT_VIOLATION,
    },
    registry::TaskRegistry,
    router::AdsRouter,
};
use tokio::sync::mpsc;

/// Build a complete batch payload (80-byte header + N × 24-byte samples).
///
/// `samples` is a slice of `(cycle_count, exec_time_us, dc_time, flags)`.
#[allow(clippy::too_many_arguments)]
fn build_batch_payload(
    task_obj_id: u32,
    task_port: u16,
    window_ms: u16,
    cycle_start: u32,
    cycle_end: u32,
    dc_start: i64,
    dc_end: i64,
    exec_min: u32,
    exec_max: u32,
    exec_avg: u32,
    exceed_count: u32,
    rtv_count: u32,
    task_name: &str,
    samples: &[(u32, u32, i64, u8)],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PUSH_BATCH_HEADER_SIZE + samples.len() * PUSH_SAMPLE_SIZE);

    buf.push(PUSH_WIRE_VERSION);
    buf.push(PUSH_BATCH_EVENT_TYPE);
    buf.extend_from_slice(&0_u16.to_le_bytes()); // reserved0
    buf.extend_from_slice(&task_obj_id.to_le_bytes());
    buf.extend_from_slice(&task_port.to_le_bytes());
    buf.extend_from_slice(&window_ms.to_le_bytes());
    buf.extend_from_slice(&(samples.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0_u16.to_le_bytes()); // reserved1
    buf.extend_from_slice(&cycle_start.to_le_bytes());
    buf.extend_from_slice(&cycle_end.to_le_bytes());
    buf.extend_from_slice(&dc_start.to_le_bytes());
    buf.extend_from_slice(&dc_end.to_le_bytes());
    buf.extend_from_slice(&exec_min.to_le_bytes());
    buf.extend_from_slice(&exec_max.to_le_bytes());
    buf.extend_from_slice(&exec_avg.to_le_bytes());
    buf.extend_from_slice(&exceed_count.to_le_bytes());
    buf.extend_from_slice(&rtv_count.to_le_bytes());

    let mut name_bytes = [0_u8; 20];
    let n = task_name.len().min(20);
    name_bytes[..n].copy_from_slice(&task_name.as_bytes()[..n]);
    buf.extend_from_slice(&name_bytes);

    assert_eq!(buf.len(), PUSH_BATCH_HEADER_SIZE);

    for (cycle, exec_us, dc, flags) in samples {
        buf.extend_from_slice(&cycle.to_le_bytes());
        buf.extend_from_slice(&exec_us.to_le_bytes());
        buf.extend_from_slice(&dc.to_le_bytes());
        buf.push(*flags);
        buf.extend_from_slice(&[0_u8; 7]);
    }

    buf
}

/// Wrap a push-diagnostic payload as an AMS write frame.
fn wrap_as_ams_write(
    source: AmsNetId,
    target_port: u16,
    ig: u32,
    io: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut write_req = Vec::with_capacity(12 + payload.len());
    write_req.extend_from_slice(&ig.to_le_bytes());
    write_req.extend_from_slice(&io.to_le_bytes());
    write_req.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    write_req.extend_from_slice(payload);

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
async fn batch_with_samples_produces_single_event_with_all_samples() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // 3 samples: one normal, one cycle-exceed, one rt-violation.
    let samples = vec![
        (1000, 400, 1_000_000_000, 0),
        (1001, 900, 1_001_000_000, SAMPLE_FLAG_CYCLE_EXCEED),
        (1002, 300, 1_002_000_000, SAMPLE_FLAG_RT_VIOLATION),
    ];
    let payload = build_batch_payload(
        42,
        350,
        100,
        1000,
        1002,
        1_000_000_000,
        1_002_000_000,
        300,
        900,
        533,
        1,
        1,
        "PlcTask",
        &samples,
    );
    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);

    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "dispatch should return ACK response");

    let (net_id, ev) = push_rx.recv().await.expect("should receive one batch");
    assert_eq!(net_id, plc_net_id);

    match ev {
        DiagEvent::TaskDiagBatch {
            task_port,
            task_name,
            task_obj_id,
            window_ms,
            cycle_count_start,
            cycle_count_end,
            exec_time_min_us,
            exec_time_max_us,
            exec_time_avg_us,
            cycle_exceed_count,
            rt_violation_count,
            samples: decoded,
            ..
        } => {
            assert_eq!(task_port, 350);
            assert_eq!(task_name, "PlcTask");
            assert_eq!(task_obj_id, 42);
            assert_eq!(window_ms, 100);
            assert_eq!(cycle_count_start, 1000);
            assert_eq!(cycle_count_end, 1002);
            assert_eq!(exec_time_min_us, 300);
            assert_eq!(exec_time_max_us, 900);
            assert_eq!(exec_time_avg_us, 533);
            assert_eq!(cycle_exceed_count, 1);
            assert_eq!(rt_violation_count, 1);
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[1].flags, SAMPLE_FLAG_CYCLE_EXCEED);
            assert_eq!(decoded[2].flags, SAMPLE_FLAG_RT_VIOLATION);
            assert_eq!(decoded[2].cycle_count, 1002);
        }
        _ => panic!("expected TaskDiagBatch"),
    }

    assert!(push_rx.try_recv().is_err());
}

#[tokio::test]
async fn empty_batch_still_produces_event_with_aggregates() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let payload = build_batch_payload(1, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, "PlcTask", &[]);
    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    let (_, ev) = push_rx.recv().await.expect("should receive event");
    match ev {
        DiagEvent::TaskDiagBatch { samples, .. } => {
            assert!(samples.is_empty());
        }
        _ => panic!("expected TaskDiagBatch"),
    }
}

#[tokio::test]
async fn batch_ack_returns_immediately() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, _) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();
    let payload = build_batch_payload(1, 340, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, "IO Idle", &[]);
    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);

    let resp = router.dispatch(&frame).await.unwrap();
    let resp_data = resp.expect("dispatch returned ACK");
    assert!(resp_data.len() >= 32);

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

    let mut payload = build_batch_payload(1, 340, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, "Task", &[]);
    payload[0] = 99; // invalid version

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    assert!(
        push_rx.try_recv().is_err(),
        "unknown version should be dropped"
    );
}

#[tokio::test]
async fn truncated_batch_is_dropped_silently() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Claim 2 samples in header, provide only 1.
    let samples = vec![(1000, 100, 0, 0), (1001, 100, 0, 0)];
    let mut payload = build_batch_payload(
        1, 350, 100, 1000, 1001, 0, 0, 100, 100, 100, 0, 0, "Task", &samples,
    );
    // Truncate the last sample (24 bytes) → body now claims 2 samples but only has 1.
    payload.truncate(payload.len() - PUSH_SAMPLE_SIZE);

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    assert!(
        push_rx.try_recv().is_err(),
        "truncated batch should be dropped"
    );
}

#[tokio::test]
async fn sample_count_above_max_is_rejected() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Forge a header with sample_count = MAX + 1.
    let mut payload = build_batch_payload(1, 350, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, "Task", &[]);
    let forged = (PUSH_BATCH_MAX_SAMPLES + 1) as u16;
    payload[0x0C..0x0E].copy_from_slice(&forged.to_le_bytes());

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_BATCH, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    assert!(
        push_rx.try_recv().is_err(),
        "oversized sample_count should be rejected"
    );
}

#[tokio::test]
async fn non_push_write_still_reaches_log_parser() {
    let (log_tx, _log_rx) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Send a regular write to IG_RT_SYSTEM (not push-diagnostic).
    let dummy = [0x01_u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    let frame = wrap_as_ams_write(plc_net_id, 16150, 0xF200_0000, 0, &dummy);
    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some());

    assert!(
        push_rx.try_recv().is_err(),
        "non-push write must not surface on push channel"
    );
}

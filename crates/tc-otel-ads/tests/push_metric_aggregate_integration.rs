//! Integration test for FB_Metrics aggregate-batch dispatch through AdsRouter.
//!
//! Builds a synthetic AMS write frame addressed to
//! `IG_PUSH_DIAG / IO_PUSH_METRIC_AGG`, feeds it through the router, and
//! asserts the matching `DiagEvent::MetricAggregateBatch` arrives on the push
//! channel. No real PLC required — exercises the same path the SmokeTest hits
//! at runtime.

use std::str::FromStr;
use std::sync::Arc;
use tc_otel_ads::{
    ams::{AmsHeader, AmsNetId, ADS_CMD_WRITE, ADS_STATE_REQUEST},
    diagnostics::{
        DiagEvent, MetricAggregateSample, MetricBodySchema, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG,
        METRIC_FLAG_HAS_TRACE_CTX, METRIC_FLAG_RING_OVERFLOWED, PUSH_METRIC_AGG_EVENT_TYPE,
        PUSH_METRIC_AGG_HEADER_SIZE, PUSH_WIRE_VERSION,
    },
    registry::TaskRegistry,
    router::AdsRouter,
};
use tokio::sync::mpsc;

/// Build a 52-byte FB_Metrics aggregate header. Body bytes are appended by
/// the test after calling this.
#[allow(clippy::too_many_arguments)]
fn build_agg_header(
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
    let mut buf = Vec::with_capacity(PUSH_METRIC_AGG_HEADER_SIZE);
    buf.push(PUSH_WIRE_VERSION);
    buf.push(PUSH_METRIC_AGG_EVENT_TYPE);
    buf.push(flags);
    buf.push(body_schema);
    buf.extend_from_slice(&sample_size.to_le_bytes());
    buf.extend_from_slice(&sample_count.to_le_bytes());
    buf.extend_from_slice(&metric_id.to_le_bytes());
    buf.push(task_index);
    buf.push(0); // stat_mask = 0 for non-aggregated frames
    buf.extend_from_slice(&[0_u8; 2]); // pad to 0x14
    buf.extend_from_slice(&cycle_count_start.to_le_bytes());
    buf.extend_from_slice(&cycle_count_end.to_le_bytes());
    buf.extend_from_slice(&0_u32.to_le_bytes()); // pad to 0x20
    buf.extend_from_slice(&dc_time_start.to_le_bytes());
    buf.extend_from_slice(&dc_time_end.to_le_bytes());
    buf.push(name_len);
    buf.push(unit_len);
    buf.extend_from_slice(&[0_u8; 2]); // pad to 52
    assert_eq!(buf.len(), PUSH_METRIC_AGG_HEADER_SIZE);
    buf
}

/// Wrap a payload as an AMS write request frame addressed to (ig, io).
fn wrap_as_ams_write(source: AmsNetId, target_port: u16, ig: u32, io: u32, payload: &[u8]) -> Vec<u8> {
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
        invoke_id: 0xDEADBEEF,
    };

    let mut frame = header.serialize();
    frame.extend_from_slice(&write_req);
    frame
}

#[tokio::test]
async fn numeric_aggregate_dispatches_with_lreal_samples() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();
    let name = "motor.temperature";
    let unit = "Cel";
    let values = [21.5_f64, 22.0, 22.7];

    let mut payload = build_agg_header(
        0,
        2,                                // eNumeric
        8,
        values.len() as u32,
        0xCAFEBABE,
        4,
        1000,
        1010,
        100_000,
        120_000,
        name.len() as u8,
        unit.len() as u8,
    );
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(unit.as_bytes());
    for v in values {
        payload.extend_from_slice(&v.to_le_bytes());
    }

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let resp = router.dispatch(&frame).await.unwrap();
    assert!(resp.is_some(), "router should ACK the write");

    let (received_net_id, ev) = push_rx.recv().await.expect("expected one MetricAggregateBatch");
    assert_eq!(received_net_id, plc_net_id);

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
            name: rx_name,
            unit: rx_unit,
            trace_id,
            span_id,
            samples,
            ..
        } => {
            assert_eq!(metric_id, 0xCAFEBABE);
            assert_eq!(task_index, 4);
            assert_eq!(flags, 0);
            assert_eq!(body_schema, MetricBodySchema::Numeric);
            assert_eq!(sample_size, 8);
            assert_eq!(cycle_count_start, 1000);
            assert_eq!(cycle_count_end, 1010);
            assert_eq!(dc_time_start, 100_000);
            assert_eq!(dc_time_end, 120_000);
            assert_eq!(rx_name, name);
            assert_eq!(rx_unit, unit);
            assert!(trace_id.is_none());
            assert!(span_id.is_none());
            assert_eq!(samples.len(), 3);
            assert_eq!(samples[0], MetricAggregateSample::Numeric(21.5));
            assert_eq!(samples[1], MetricAggregateSample::Numeric(22.0));
            assert_eq!(samples[2], MetricAggregateSample::Numeric(22.7));
        }
        other => panic!("expected MetricAggregateBatch, got {:?}", other),
    }

    assert!(push_rx.try_recv().is_err(), "no extra event expected");
}

#[tokio::test]
async fn bool_aggregate_dispatches_with_correct_samples() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();
    let name = "door";
    let mut payload = build_agg_header(0, 1, 1, 4, 7, 1, 0, 0, 0, 0, name.len() as u8, 0);
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(&[1, 0, 1, 1]);

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    let (_, ev) = push_rx.recv().await.expect("expected event");
    if let DiagEvent::MetricAggregateBatch { samples, body_schema, .. } = ev {
        assert_eq!(body_schema, MetricBodySchema::Bool);
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

#[tokio::test]
async fn aggregate_with_trace_context_propagates_ids() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();
    let trace_id_bytes: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x00,
    ];
    let span_id_bytes: [u8; 8] = [0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe];

    let mut payload = build_agg_header(METRIC_FLAG_HAS_TRACE_CTX, 1, 1, 1, 0, 0, 0, 0, 0, 0, 4, 0);
    payload.extend_from_slice(&trace_id_bytes);
    payload.extend_from_slice(&span_id_bytes);
    payload.extend_from_slice(b"trip");
    payload.push(1);

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    let (_, ev) = push_rx.recv().await.expect("expected event");
    if let DiagEvent::MetricAggregateBatch {
        flags,
        trace_id,
        span_id,
        ..
    } = ev
    {
        assert_eq!(flags & METRIC_FLAG_HAS_TRACE_CTX, METRIC_FLAG_HAS_TRACE_CTX);
        assert_eq!(trace_id, Some(trace_id_bytes));
        assert_eq!(span_id, Some(span_id_bytes));
    } else {
        panic!("expected MetricAggregateBatch");
    }
}

#[tokio::test]
async fn aggregate_overflow_flag_propagates() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    let mut payload = build_agg_header(METRIC_FLAG_RING_OVERFLOWED, 2, 8, 1, 1, 0, 0, 0, 0, 0, 0, 0);
    payload.extend_from_slice(&3.14_f64.to_le_bytes());

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    let (_, ev) = push_rx.recv().await.expect("expected event");
    if let DiagEvent::MetricAggregateBatch { flags, .. } = ev {
        assert_eq!(flags & METRIC_FLAG_RING_OVERFLOWED, METRIC_FLAG_RING_OVERFLOWED);
    } else {
        panic!("expected MetricAggregateBatch");
    }
}

#[tokio::test]
async fn aggregate_with_wrong_event_type_is_dropped() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Header advertises event_type=99 (unknown); router/decoder must drop it.
    let mut payload = build_agg_header(0, 2, 8, 1, 0, 0, 0, 0, 0, 0, 0, 0);
    payload[1] = 99;
    payload.extend_from_slice(&0.0_f64.to_le_bytes());

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    assert!(
        push_rx.try_recv().is_err(),
        "frame with wrong event_type should be dropped silently"
    );
}

#[tokio::test]
async fn aggregate_truncated_body_is_dropped() {
    let (log_tx, _) = mpsc::channel(100);
    let (push_tx, mut push_rx) = mpsc::channel(100);
    let registry = Arc::new(TaskRegistry::new());
    let router = AdsRouter::new(16150, log_tx, None, registry).with_push_sender(push_tx);

    let plc_net_id = AmsNetId::from_str("172.28.41.37.1.1").unwrap();

    // Promises 3 numeric samples but supplies only 16 of the 24 needed body bytes.
    let mut payload = build_agg_header(0, 2, 8, 3, 0, 0, 0, 0, 0, 0, 0, 0);
    payload.extend_from_slice(&[0_u8; 16]);

    let frame = wrap_as_ams_write(plc_net_id, 16150, IG_PUSH_DIAG, IO_PUSH_METRIC_AGG, &payload);
    let _ = router.dispatch(&frame).await.unwrap();

    assert!(
        push_rx.try_recv().is_err(),
        "truncated body should be dropped silently"
    );
}

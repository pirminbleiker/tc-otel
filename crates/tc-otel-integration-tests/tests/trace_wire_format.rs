//! Wire format tests for OpenTelemetry trace events (Phase 2)
//!
//! Tests the new streaming wire format (5=BEGIN, 6=ATTR, 7=EVENT, 8=END)
//! and ensures parser correctly dispatches trace events while preserving
//! backward compatibility with logs (1, 2, 9).

#![allow(clippy::vec_init_then_push)]

use chrono::Datelike;
use std::time::Duration;
use tc_otel_ads::{AdsParser, AmsNetId, AttrValue, TraceWireEvent};
use tc_otel_service::span_dispatcher::SpanDispatcher;
use tokio::sync::mpsc;

#[test]
fn test_round_trip_begin_attr_event_end() {
    let mut data = Vec::new();
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let trace_id = [
        11u8, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    ];

    // BEGIN (Phase 6 Stage 3: parent_span_id(8) + kind(1) + name_len(1) + reserved(2) + trace_id(16) + span_id(8) + name)
    data.push(5); // event_type
    data.push(5); // local_id (for header)
    data.push(2); // task_index
    data.push(0); // flags
    data.extend_from_slice(&1000i64.to_le_bytes());
    data.extend_from_slice(&[0u8; 8]); // parent_span_id (all-zero = root)
    data.push(0); // kind
    data.push(9); // name_len
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved(2)
    data.extend_from_slice(&trace_id); // trace_id(16)
    data.extend_from_slice(&span_id); // span_id(8)
    data.extend_from_slice(b"test_span");

    // ATTR i64 (Phase 6 Stage 3: first byte of span_id in position 1, rest after dc_time)
    data.push(6);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1100i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]); // Remaining 7 bytes of span_id
    data.push(1);
    data.push(5);
    data.push(0);
    data.push(0);
    data.extend_from_slice(b"count");
    data.extend_from_slice(&42i64.to_le_bytes());

    // ATTR string (Phase 6 Stage 3: span_id in header)
    data.push(6);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1200i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(4);
    data.push(7);
    data.push(5);
    data.push(0);
    data.extend_from_slice(b"user_id");
    data.extend_from_slice(b"alice");

    // EVENT (Phase 6 Stage 3: span_id in header)
    data.push(7);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1300i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(10);
    data.push(1);
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(b"checkpoint");
    data.push(1);
    data.push(4);
    data.push(0);
    data.push(0);
    data.extend_from_slice(b"step");
    data.extend_from_slice(&1i64.to_le_bytes());

    // END (Phase 6 Stage 3: span_id in header)
    data.push(8);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1400i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(1);
    data.push(0);
    data.extend_from_slice(&0u16.to_le_bytes());

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 5);
    assert!(matches!(
        &result.trace_events[0],
        TraceWireEvent::Begin { .. }
    ));
    assert!(matches!(
        &result.trace_events[1],
        TraceWireEvent::Attr { .. }
    ));
    assert!(matches!(
        &result.trace_events[2],
        TraceWireEvent::Attr { .. }
    ));
    assert!(matches!(
        &result.trace_events[3],
        TraceWireEvent::Event { .. }
    ));
    assert!(matches!(
        &result.trace_events[4],
        TraceWireEvent::End { .. }
    ));
}

#[test]
fn test_multi_frame_dispatch() {
    let mut data = Vec::new();
    let span_id = [10u8, 11, 12, 13, 14, 15, 16, 17];
    let trace_id = [
        30u8, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
    ];

    // BEGIN (Phase 6 Stage 3 format)
    data.push(5); // event_type
    data.push(1); // local_id (for header)
    data.push(2); // task_index
    data.push(0); // flags
    data.extend_from_slice(&100i64.to_le_bytes());
    data.extend_from_slice(&[0u8; 8]); // parent_span_id (all-zero = root)
    data.push(0); // kind
    data.push(2); // name_len
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved(2)
    data.extend_from_slice(&trace_id); // trace_id(16)
    data.extend_from_slice(&span_id); // span_id(8)
    data.extend_from_slice(b"op");

    // ATTR (Phase 6 Stage 3: span_id in header)
    data.push(6);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&110i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(1);
    data.push(8);
    data.push(0);
    data.push(0);
    data.extend_from_slice(b"duration");
    data.extend_from_slice(&500i64.to_le_bytes());

    // EVENT (Phase 6 Stage 3: span_id in header)
    data.push(7);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&120i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(4);
    data.push(0);
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(b"done");

    // END (Phase 6 Stage 3: span_id in header)
    data.push(8);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&130i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(1);
    data.push(0);
    data.extend_from_slice(&0u16.to_le_bytes());

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 4);
    assert!(matches!(
        &result.trace_events[0],
        TraceWireEvent::Begin { .. }
    ));
    assert!(matches!(
        &result.trace_events[1],
        TraceWireEvent::Attr { .. }
    ));
    assert!(matches!(
        &result.trace_events[2],
        TraceWireEvent::Event { .. }
    ));
    assert!(matches!(
        &result.trace_events[3],
        TraceWireEvent::End { .. }
    ));
}

#[test]
fn test_coexistence_with_legacy_v2_logs() {
    let mut data = Vec::new();

    // v2 log 1
    let log1_payload_start = data.len() + 3;
    data.push(0x02);
    data.extend_from_slice(&0u16.to_le_bytes());
    data.push(1);
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    data.extend_from_slice(&filetime.to_le_bytes());
    data.extend_from_slice(&filetime.to_le_bytes());
    data.push(2);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.push(0);
    data.push(0);
    data.push(5);
    data.extend_from_slice(b"hello");
    data.push(7);
    data.extend_from_slice(b"logger1");
    let log1_len = (data.len() - log1_payload_start) as u16;
    data[log1_payload_start - 2..log1_payload_start].copy_from_slice(&log1_len.to_le_bytes());

    // v2 log 2
    let log2_payload_start = data.len() + 3;
    data.push(0x02);
    data.extend_from_slice(&0u16.to_le_bytes());
    data.push(1);
    data.extend_from_slice(&filetime.to_le_bytes());
    data.extend_from_slice(&filetime.to_le_bytes());
    data.push(3);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.push(0);
    data.push(0);
    data.push(5);
    data.extend_from_slice(b"world");
    data.push(7);
    data.extend_from_slice(b"logger2");
    let log2_len = (data.len() - log2_payload_start) as u16;
    data[log2_payload_start - 2..log2_payload_start].copy_from_slice(&log2_len.to_le_bytes());

    let span_id = [20u8, 21, 22, 23, 24, 25, 26, 27];

    // BEGIN (Phase 6 Stage 3)
    data.push(5); // event_type
    data.push(1); // local_id
    data.push(2); // task_index
    data.push(0); // flags
    data.extend_from_slice(&500i64.to_le_bytes()); // dc_time
                                                   // payload starts at +0x0C
    data.extend_from_slice(&[0u8; 8]); // parent_span_id (all-zero = root)
    data.push(0); // kind
    data.push(4); // name_len = "span".len()
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved(2)
    data.extend_from_slice(&[0u8; 16]); // trace_id(16)
    data.extend_from_slice(&span_id); // span_id(8)
    data.extend_from_slice(b"span");

    // END (Phase 6 Stage 3: span_id in header)
    data.push(8);
    data.push(span_id[0]);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&600i64.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(1);
    data.push(0);
    data.extend_from_slice(&0u16.to_le_bytes());

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 2, "Should have 2 log entries");
    assert_eq!(result.trace_events.len(), 2, "Should have 2 trace events");
}

#[test]
fn test_dispatcher_lifecycle() {
    let (tx, mut rx) = mpsc::channel(10);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(5), 100);
    let net_id = AmsNetId::from_bytes([5, 80, 201, 232, 1, 1]);
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0,
            dc_time: 1_000_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "test_op".to_string(),
            traceparent: None,
            trace_id: [0; 16],
            span_id,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Attr {
            span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_001_000_000_000_000_000,
            key: "user".to_string(),
            value: AttrValue::String("bob".to_string()),
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Event {
            span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_002_000_000_000_000_000,
            name: "progress".to_string(),
            attrs: vec![("percent".to_string(), AttrValue::I64(50))],
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_003_000_000_000_000_000,
            status: 1,
            message: "".to_string(),
        },
    );

    let trace_record = rx.blocking_recv().expect("Should receive trace record");
    assert_eq!(
        trace_record
            .resource_attributes
            .get("service.name")
            .and_then(|v| v.as_str()),
        Some("plc-5.80.201.232.1.1")
    );
    assert_eq!(
        trace_record
            .resource_attributes
            .get("plc.task_index")
            .and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        trace_record.span_attributes.get("user"),
        Some(&serde_json::json!("bob"))
    );
    assert_eq!(trace_record.events.len(), 1);
    assert_eq!(trace_record.events[0].name, "progress");
    assert!(trace_record.start_time.year() >= 2026);
    assert!(trace_record.end_time.year() >= 2026);
}

#[test]
fn test_nested_spans_parent_child() {
    let (tx, mut rx) = mpsc::channel(10);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(5), 100);
    let net_id = AmsNetId::from_bytes([5, 80, 201, 232, 1, 1]);
    let parent_span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let child_span_id = [2u8, 3, 4, 5, 6, 7, 8, 9];

    let trace_id = [
        11u8, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    ];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0,
            dc_time: 900_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "parent".to_string(),
            traceparent: None,
            trace_id,
            span_id: parent_span_id,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 2,
            task_index: 2,
            flags: 0,
            dc_time: 1_000_000_000_000_000_000,
            parent_span_id, // points to parent's span_id
            kind: 0,
            name: "child".to_string(),
            traceparent: None,
            trace_id,
            span_id: child_span_id,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id: child_span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_001_000_000_000_000_000,
            status: 1,
            message: "".to_string(),
        },
    );

    let inner_record = rx.blocking_recv().expect("Should receive inner trace");
    assert_eq!(inner_record.name, "child");

    let parent_span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id: parent_span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_002_000_000_000_000_000,
            status: 1,
            message: "".to_string(),
        },
    );

    let outer_record = rx.blocking_recv().expect("Should receive outer trace");
    assert_eq!(outer_record.name, "parent");
    assert_eq!(inner_record.parent_span_id, outer_record.span_id);
}

#[test]
fn test_unknown_outer_byte_doesnt_panic() {
    let mut data = Vec::new();
    data.push(0x42);

    let result = AdsParser::parse_all(&data);
    assert!(result.is_ok());
    let parsed = result.unwrap();
    assert_eq!(parsed.trace_events.len(), 0);
    assert_eq!(parsed.entries.len(), 0);
}

// ─── Phase 5: local-ID minting round-trip ─────────────────────

fn build_begin_with_local_ids(
    local_id: u8,
    task_index: u8,
    _parent_local_id: u8, // Deprecated in Stage 3, now use parent_span_id
    name: &[u8],
    trace_id: [u8; 16],
    span_id: [u8; 8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(5); // SPAN_BEGIN
    data.push(local_id);
    data.push(task_index);
    data.push(0); // flags (no external parent)
    data.extend_from_slice(&1_000i64.to_le_bytes()); // dc_time
                                                     // payload starts at +0x0C
    data.extend_from_slice(&[0u8; 8]); // parent_span_id (all-zero = root)
    data.push(0); // kind
    data.push(name.len() as u8);
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved (2 bytes)
    data.extend_from_slice(&trace_id); // 16 bytes
    data.extend_from_slice(&span_id); // 8 bytes
    data.extend_from_slice(name);
    data
}

#[test]
fn test_begin_with_local_ids_round_trip() {
    let expected_trace = [
        0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];
    let expected_span = [0xDEu8, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xF0, 0x0D];

    let data = build_begin_with_local_ids(7, 3, 0xFF, b"hello", expected_trace, expected_span);
    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 1);

    if let TraceWireEvent::Begin {
        trace_id,
        span_id,
        name,
        ..
    } = &result.trace_events[0]
    {
        assert_eq!(name, "hello");
        assert_eq!(
            *trace_id, expected_trace,
            "trace_id must survive the wire exactly"
        );
        assert_eq!(
            *span_id, expected_span,
            "span_id must survive the wire exactly"
        );
    } else {
        panic!("expected Begin variant");
    }
}

#[test]
fn test_begin_without_local_ids_leaves_options_none() {
    // Stage 3 BEGIN frame: parent_span_id(8) + kind(1) + name_len(1) + reserved(2) + trace_id(16) + span_id(8) + name
    let mut data = Vec::new();
    let trace_id = [
        11u8, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    ];
    let span_id = [30u8, 31, 32, 33, 34, 35, 36, 37];

    data.push(5); // event_type
    data.push(5); // local_id
    data.push(2); // task_index
    data.push(0); // flags
    data.extend_from_slice(&1000i64.to_le_bytes()); // dc_time
                                                    // payload starts at +0x0C
    data.extend_from_slice(&[0u8; 8]); // parent_span_id (all-zero = root)
    data.push(0); // kind
    data.push(9); // name_len = "test_span".len()
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved(2)
    data.extend_from_slice(&trace_id); // trace_id(16)
    data.extend_from_slice(&span_id); // span_id(8)
    data.extend_from_slice(b"test_span");

    let result = AdsParser::parse_all(&data).unwrap();
    if let TraceWireEvent::Begin {
        trace_id: parsed_trace_id,
        span_id: parsed_span_id,
        ..
    } = &result.trace_events[0]
    {
        // Stage 3: IDs are always present
        assert_eq!(*parsed_trace_id, trace_id);
        assert_eq!(*parsed_span_id, span_id);
    } else {
        panic!("expected Begin");
    }
}

#[tokio::test]
async fn test_span_dispatcher_honours_pregenerated_ids() {
    // Wire a dispatcher and feed a BEGIN frame whose PLC-minted IDs
    // must appear verbatim on the finalised TraceRecord.
    let (tx, mut rx) = mpsc::channel(8);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 64);

    let expected_trace = [
        0xA0u8, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE,
        0xAF,
    ];
    let expected_span = [0xB0u8, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];

    let net_id = AmsNetId::from_bytes([10, 0, 0, 1, 1, 1]);
    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0x08,
            dc_time: 1_776_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "producer_op".to_string(),
            traceparent: None,
            trace_id: expected_trace,
            span_id: expected_span,
        },
    );
    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id: expected_span,
            task_index: 2,
            flags: 0,
            dc_time: 1_776_000_000_001_000_000,
            status: 1,
            message: String::new(),
        },
    );

    let record = rx.recv().await.expect("expected TraceRecord");
    assert_eq!(
        record.trace_id,
        hex::encode(expected_trace),
        "dispatcher must use the PLC-minted trace_id verbatim"
    );
    assert_eq!(
        record.span_id,
        hex::encode(expected_span),
        "dispatcher must use the PLC-minted span_id verbatim"
    );
}

#[tokio::test]
async fn test_external_traceparent_overrides_pregenerated_trace_id() {
    // Upstream propagation must win: the trace_id from the W3C
    // traceparent string takes priority over any PLC-minted one.
    let (tx, mut rx) = mpsc::channel(8);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 64);

    let pregen_trace = [0x11u8; 16];
    let pregen_span = [0x22u8; 8];
    let upstream_trace_hex = "aabbccddeeff00112233445566778899";
    let upstream_parent_hex = "1122334455667788";
    let traceparent = format!("00-{upstream_trace_hex}-{upstream_parent_hex}-01");

    let net_id = AmsNetId::from_bytes([10, 0, 0, 1, 1, 1]);
    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0x02 | 0x08, // external parent + local IDs
            dc_time: 1_776_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "consumer_op".to_string(),
            traceparent: Some(traceparent),
            trace_id: pregen_trace,
            span_id: pregen_span,
        },
    );
    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id: pregen_span,
            task_index: 2,
            flags: 0,
            dc_time: 1_776_000_000_001_000_000,
            status: 1,
            message: String::new(),
        },
    );

    let record = rx.recv().await.expect("expected TraceRecord");
    assert_eq!(
        record.trace_id, upstream_trace_hex,
        "upstream traceparent must dictate trace_id"
    );
    assert_eq!(
        record.parent_span_id, upstream_parent_hex,
        "upstream traceparent must dictate parent_span_id"
    );
    // span_id: THIS span is new → PLC-minted span_id still applies so
    // the producer's `CurrentTraceParent()` cites the correct bytes.
    assert_eq!(
        record.span_id,
        hex::encode(pregen_span),
        "PLC-minted span_id should still identify this span"
    );
}

// ─── Stage 1: span_id secondary index ─────────────────────────

#[tokio::test]
async fn test_span_dispatcher_indexes_pregenerated_span_ids() {
    // Stage 1 (Rust side) must index spans by their pregenerated span_id.
    // This unblocks Stage 3 on the PLC side (retire aStack/aSlots).
    let (tx, _rx) = mpsc::channel(8);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 64);

    let net_id = AmsNetId::from_bytes([5, 80, 201, 232, 1, 1]);
    let pregenerated_span_id = [0xDEu8, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xF0, 0x0D];
    let pregenerated_trace_id = [
        0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0x08, // flag_local_ids
            dc_time: 1_776_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "indexed_span".to_string(),
            traceparent: None,
            trace_id: pregenerated_trace_id,
            span_id: pregenerated_span_id,
        },
    );

    // Should be retrievable via the new secondary index
    let pending = dispatcher.pending_by_span_id(&pregenerated_span_id);
    assert!(pending.is_some(), "span should be indexed by span_id");
    let p = pending.unwrap();
    assert_eq!(p.span_id, pregenerated_span_id);
    assert_eq!(p.trace_id, pregenerated_trace_id);
    assert_eq!(p.name, "indexed_span");
}

#[tokio::test]
async fn test_span_dispatcher_span_id_index_end_cleanup() {
    // When a span is ended, the span_id index entry must also be removed.
    let (tx, _rx) = mpsc::channel(8);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 64);

    let net_id = AmsNetId::from_bytes([5, 80, 201, 232, 1, 1]);
    let pregenerated_span_id = [0xCAu8, 0xFE, 0xBA, 0xBE, 0xDE, 0xAD, 0xBE, 0xEF];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0x08,
            dc_time: 1_000_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "span_to_end".to_string(),
            traceparent: None,
            trace_id: [0; 16],
            span_id: pregenerated_span_id,
        },
    );

    assert!(
        dispatcher
            .pending_by_span_id(&pregenerated_span_id)
            .is_some(),
        "span should be indexed before end"
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id: pregenerated_span_id,
            task_index: 2,
            flags: 0,
            dc_time: 1_001_000_000_000_000_000,
            status: 1,
            message: "complete".to_string(),
        },
    );

    assert!(
        dispatcher
            .pending_by_span_id(&pregenerated_span_id)
            .is_none(),
        "span_id index should be cleaned up after end"
    );
}

#[tokio::test]
async fn test_span_dispatcher_parallel_indexed_spans() {
    // Two parallel spans with different pregenerated span_ids must each be
    // independently retrievable via the secondary index.
    let (tx, _rx) = mpsc::channel(8);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 64);

    let net_id = AmsNetId::from_bytes([5, 80, 201, 232, 1, 1]);
    let span_id_1 = [1u8; 8];
    let span_id_2 = [2u8; 8];

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0x08,
            dc_time: 1_000_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "span_a".to_string(),
            traceparent: None,
            trace_id: [0; 16],
            span_id: span_id_1,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 2,
            task_index: 2,
            flags: 0x08,
            dc_time: 1_050_000_000_000_000_000,
            parent_span_id: [0; 8], // root
            kind: 0,
            name: "span_b".to_string(),
            traceparent: None,
            trace_id: [0; 16],
            span_id: span_id_2,
        },
    );

    // Both should be independently retrievable
    let pending_1 = dispatcher.pending_by_span_id(&span_id_1);
    let pending_2 = dispatcher.pending_by_span_id(&span_id_2);

    assert!(pending_1.is_some());
    assert!(pending_2.is_some());
    assert_eq!(pending_1.unwrap().name, "span_a");
    assert_eq!(pending_2.unwrap().name, "span_b");
}

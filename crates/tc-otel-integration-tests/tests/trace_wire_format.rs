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
    // BEGIN
    data.push(5);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1000i64.to_le_bytes());
    data.push(0xFF);
    data.push(0);
    data.push(9);
    data.push(0);
    data.extend_from_slice(b"test_span");

    // ATTR i64
    data.push(6);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1100i64.to_le_bytes());
    data.push(1);
    data.push(5);
    data.push(0);
    data.push(0);
    data.extend_from_slice(b"count");
    data.extend_from_slice(&42i64.to_le_bytes());

    // ATTR string
    data.push(6);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1200i64.to_le_bytes());
    data.push(4);
    data.push(7);
    data.push(5);
    data.push(0);
    data.extend_from_slice(b"user_id");
    data.extend_from_slice(b"alice");

    // EVENT
    data.push(7);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1300i64.to_le_bytes());
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

    // END
    data.push(8);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1400i64.to_le_bytes());
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
    data.push(5);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&100i64.to_le_bytes());
    data.push(0xFF);
    data.push(0);
    data.push(2);
    data.push(0);
    data.extend_from_slice(b"op");

    data.push(6);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&110i64.to_le_bytes());
    data.push(1);
    data.push(8);
    data.push(0);
    data.push(0);
    data.extend_from_slice(b"duration");
    data.extend_from_slice(&500i64.to_le_bytes());

    data.push(7);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&120i64.to_le_bytes());
    data.push(4);
    data.push(0);
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(b"done");

    data.push(8);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&130i64.to_le_bytes());
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

    // BEGIN
    data.push(5);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&500i64.to_le_bytes());
    data.push(0xFF);
    data.push(0);
    data.push(4);
    data.push(0);
    data.extend_from_slice(b"span");

    // END
    data.push(8);
    data.push(1);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&600i64.to_le_bytes());
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

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0,
            dc_time: 1_000_000_000_000_000_000,
            parent_local_id: 0xFF,
            kind: 0,
            name: "test_op".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: None,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Attr {
            local_id: 1,
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
            local_id: 1,
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
            local_id: 1,
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

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 2,
            flags: 0,
            dc_time: 900_000_000_000_000_000,
            parent_local_id: 0xFF,
            kind: 0,
            name: "parent".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: None,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 2,
            task_index: 2,
            flags: 0,
            dc_time: 1_000_000_000_000_000_000,
            parent_local_id: 1,
            kind: 0,
            name: "child".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: None,
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            local_id: 2,
            task_index: 2,
            flags: 0,
            dc_time: 1_001_000_000_000_000_000,
            status: 1,
            message: "".to_string(),
        },
    );

    let inner_record = rx.blocking_recv().expect("Should receive inner trace");
    assert_eq!(inner_record.name, "child");

    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            local_id: 1,
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
    parent_local_id: u8,
    name: &[u8],
    trace_id: [u8; 16],
    span_id: [u8; 8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(5); // SPAN_BEGIN
    data.push(local_id);
    data.push(task_index);
    data.push(0x08); // flag_local_ids only
    data.extend_from_slice(&1_000i64.to_le_bytes()); // dc_time
    data.push(parent_local_id);
    data.push(0); // kind
    data.push(name.len() as u8);
    data.push(0); // reserved
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
        pregenerated_trace_id,
        pregenerated_span_id,
        name,
        flags,
        ..
    } = &result.trace_events[0]
    {
        assert_eq!(flags & 0x08, 0x08, "flag_local_ids should round-trip");
        assert_eq!(name, "hello");
        assert_eq!(
            *pregenerated_trace_id,
            Some(expected_trace),
            "trace_id must survive the wire exactly"
        );
        assert_eq!(
            *pregenerated_span_id,
            Some(expected_span),
            "span_id must survive the wire exactly"
        );
    } else {
        panic!("expected Begin variant");
    }
}

#[test]
fn test_begin_without_local_ids_leaves_options_none() {
    // Same frame as the existing round_trip test — no flag_local_ids,
    // no 24-byte trailer.
    let mut data = Vec::new();
    data.push(5);
    data.push(5);
    data.push(2);
    data.push(0);
    data.extend_from_slice(&1000i64.to_le_bytes());
    data.push(0xFF);
    data.push(0);
    data.push(9);
    data.push(0);
    data.extend_from_slice(b"test_span");

    let result = AdsParser::parse_all(&data).unwrap();
    if let TraceWireEvent::Begin {
        pregenerated_trace_id,
        pregenerated_span_id,
        ..
    } = &result.trace_events[0]
    {
        assert!(pregenerated_trace_id.is_none());
        assert!(pregenerated_span_id.is_none());
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
            parent_local_id: 0xFF,
            kind: 0,
            name: "producer_op".to_string(),
            traceparent: None,
            pregenerated_trace_id: Some(expected_trace),
            pregenerated_span_id: Some(expected_span),
        },
    );
    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            local_id: 1,
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
            parent_local_id: 0xFF,
            kind: 0,
            name: "consumer_op".to_string(),
            traceparent: Some(traceparent),
            pregenerated_trace_id: Some(pregen_trace),
            pregenerated_span_id: Some(pregen_span),
        },
    );
    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            local_id: 1,
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
            parent_local_id: 0xFF,
            kind: 0,
            name: "indexed_span".to_string(),
            traceparent: None,
            pregenerated_trace_id: Some(pregenerated_trace_id),
            pregenerated_span_id: Some(pregenerated_span_id),
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
            parent_local_id: 0xFF,
            kind: 0,
            name: "span_to_end".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: Some(pregenerated_span_id),
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
            local_id: 1,
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
            parent_local_id: 0xFF,
            kind: 0,
            name: "span_a".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: Some(span_id_1),
        },
    );

    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 2,
            task_index: 2,
            flags: 0x08,
            dc_time: 1_050_000_000_000_000_000,
            parent_local_id: 0xFF,
            kind: 0,
            name: "span_b".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: Some(span_id_2),
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

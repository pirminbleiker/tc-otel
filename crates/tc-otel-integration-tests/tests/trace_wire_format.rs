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

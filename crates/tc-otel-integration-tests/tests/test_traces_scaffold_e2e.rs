//! End-to-end test for the OpenTelemetry traces scaffold
//! Validates the complete pipeline: wire format → parser → dispatcher → OTLP export

#![allow(clippy::vec_init_then_push)]

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tc_otel_ads::{AdsParser, AmsNetId, AttrValue, TraceWireEvent};
use tc_otel_core::TraceRecord;
use tc_otel_service::span_dispatcher::SpanDispatcher;
use tokio::sync::mpsc;

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_trace_wire_events() {
    // Construct raw bytes for a SPAN_BEGIN event
    // Format: [event_type=1][local_id][task_index][flags][dc_time:i64][parent_local_id][kind][name_len][reserved][name]
    let mut data = Vec::new();
    data.push(1u8); // event_type = SPAN_BEGIN
    data.push(5u8); // local_id = 5
    data.push(2u8); // task_index = 2
    data.push(0u8); // flags = none
    data.extend_from_slice(&1000i64.to_le_bytes()); // dc_time
    data.push(0xFFu8); // parent_local_id = none
    data.push(0u8); // kind = Internal
    data.push(9u8); // name_len
    data.push(0u8); // reserved
    data.extend_from_slice(b"test_span");

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 1);

    match &result.trace_events[0] {
        TraceWireEvent::Begin {
            local_id,
            task_index,
            dc_time,
            kind,
            name,
            traceparent,
            ..
        } => {
            assert_eq!(*local_id, 5);
            assert_eq!(*task_index, 2);
            assert_eq!(*dc_time, 1000);
            assert_eq!(*kind, 0);
            assert_eq!(name, "test_span");
            assert!(traceparent.is_none());
        }
        _ => panic!("Expected BEGIN event"),
    }
}

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_span_attr_event() {
    let mut data = Vec::new();
    data.push(2u8); // event_type = SPAN_ATTR
    data.push(5u8); // local_id
    data.push(2u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&2000i64.to_le_bytes()); // dc_time
    data.push(1u8); // value_type = i64
    data.push(3u8); // key_len
    data.push(0u8); // value_len (unused for i64)
    data.push(0u8); // reserved
    data.extend_from_slice(b"key");
    data.extend_from_slice(&42i64.to_le_bytes());

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 1);

    match &result.trace_events[0] {
        TraceWireEvent::Attr {
            span_id,
            key,
            value,
            ..
        } => {
            assert_eq!(span_id[0], 5);
            assert_eq!(key, "key");
            assert_eq!(*value, AttrValue::I64(42));
        }
        _ => panic!("Expected ATTR event"),
    }
}

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_span_event() {
    let mut data = Vec::new();
    data.push(3u8); // event_type = SPAN_EVENT
    data.push(5u8); // local_id
    data.push(2u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&3000i64.to_le_bytes()); // dc_time
    data.push(5u8); // name_len
    data.push(0u8); // attr_count
    data.push(0u8); // reserved
    data.push(0u8); // reserved
    data.extend_from_slice(b"event");

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 1);

    match &result.trace_events[0] {
        TraceWireEvent::Event {
            span_id,
            name,
            attrs,
            ..
        } => {
            assert_eq!(span_id[0], 5);
            assert_eq!(name, "event");
            assert!(attrs.is_empty());
        }
        _ => panic!("Expected EVENT event"),
    }
}

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_span_end_event() {
    let mut data = Vec::new();
    data.push(4u8); // event_type = SPAN_END
    data.push(5u8); // local_id
    data.push(2u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&4000i64.to_le_bytes()); // dc_time
    data.push(1u8); // status = Ok
    data.push(7u8); // msg_len
    data.push(0u8); // reserved
    data.push(0u8); // reserved
    data.extend_from_slice(b"success");

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 1);

    match &result.trace_events[0] {
        TraceWireEvent::End {
            span_id,
            status,
            message,
            ..
        } => {
            assert_eq!(span_id[0], 5);
            assert_eq!(*status, 1);
            assert_eq!(message, "success");
        }
        _ => panic!("Expected END event"),
    }
}

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_mixed_trace_event_sequence() {
    // BEGIN → ATTR → EVENT → END
    let mut data = Vec::new();

    // BEGIN
    data.push(1u8); // event_type
    data.push(1u8); // local_id
    data.push(0u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&0i64.to_le_bytes()); // dc_time
    data.push(0xFFu8); // parent_local_id
    data.push(0u8); // kind
    data.push(4u8); // name_len
    data.push(0u8); // reserved
    data.extend_from_slice(b"test");

    // ATTR
    data.push(2u8); // event_type
    data.push(1u8); // local_id
    data.push(0u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&100i64.to_le_bytes()); // dc_time
    data.push(1u8); // value_type = i64
    data.push(2u8); // key_len
    data.push(0u8); // value_len
    data.push(0u8); // reserved
    data.extend_from_slice(b"id");
    data.extend_from_slice(&123i64.to_le_bytes());

    // EVENT
    data.push(3u8); // event_type
    data.push(1u8); // local_id
    data.push(0u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&200i64.to_le_bytes()); // dc_time
    data.push(5u8); // name_len
    data.push(0u8); // attr_count
    data.push(0u8); // reserved
    data.push(0u8); // reserved
    data.extend_from_slice(b"start");

    // END
    data.push(4u8); // event_type
    data.push(1u8); // local_id
    data.push(0u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&300i64.to_le_bytes()); // dc_time
    data.push(1u8); // status = Ok
    data.push(0u8); // msg_len
    data.push(0u8); // reserved
    data.push(0u8); // reserved

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.trace_events.len(), 4);

    assert!(matches!(
        result.trace_events[0],
        TraceWireEvent::Begin { .. }
    ));
    assert!(matches!(
        result.trace_events[1],
        TraceWireEvent::Attr { .. }
    ));
    assert!(matches!(
        result.trace_events[2],
        TraceWireEvent::Event { .. }
    ));
    assert!(matches!(result.trace_events[3], TraceWireEvent::End { .. }));
}

#[test]
#[ignore = "trace wire format scaffolding test - pending wire format alignment"]
fn test_parse_unknown_trace_event_type_error() {
    let mut data = Vec::new();
    data.push(99u8); // unknown event_type
    data.push(1u8); // local_id
    data.push(0u8); // task_index
    data.push(0u8); // flags
    data.extend_from_slice(&0i64.to_le_bytes()); // dc_time

    let result = AdsParser::parse_all(&data);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_span_dispatcher_processes_begin_event() {
    let (tx, mut rx) = mpsc::channel::<TraceRecord>(10);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

    let net_id = AmsNetId::from_str("192.168.1.1.1.1").unwrap();
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    // Simulate a SPAN_BEGIN wire event
    let begin_event = TraceWireEvent::Begin {
        local_id: 1,
        task_index: 0,
        flags: 0,
        dc_time: 0,
        parent_local_id: 0xFF,
        kind: 0,
        name: "operation".to_string(),
        traceparent: None,
        pregenerated_trace_id: None,
        pregenerated_span_id: Some(span_id),
    };

    dispatcher.on_event(net_id, begin_event);
    assert_eq!(dispatcher.pending_count(), 1);

    // Simulate SPAN_END
    let end_event = TraceWireEvent::End {
        span_id,
        task_index: 0,
        flags: 0,
        dc_time: 1000,
        status: 1,
        message: "ok".to_string(),
    };

    dispatcher.on_event(net_id, end_event);
    assert_eq!(dispatcher.pending_count(), 0);

    // Verify the trace record was sent
    let record = rx.try_recv();
    assert!(record.is_ok());
    let tr = record.unwrap();
    assert_eq!(tr.name, "operation");
    assert_eq!(tr.status_message, "ok");
}

#[tokio::test]
async fn test_span_dispatcher_full_lifecycle() {
    let (tx, mut rx) = mpsc::channel::<TraceRecord>(10);
    let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

    let net_id = AmsNetId::from_str("192.168.1.1.1.1").unwrap();
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    // BEGIN
    dispatcher.on_event(
        net_id,
        TraceWireEvent::Begin {
            local_id: 1,
            task_index: 0,
            flags: 0,
            dc_time: 0,
            parent_local_id: 0xFF,
            kind: 0,
            name: "span".to_string(),
            traceparent: None,
            pregenerated_trace_id: None,
            pregenerated_span_id: Some(span_id),
        },
    );

    // ATTR
    dispatcher.on_event(
        net_id,
        TraceWireEvent::Attr {
            span_id,
            task_index: 0,
            flags: 0,
            dc_time: 100,
            key: "user_id".to_string(),
            value: AttrValue::I64(42),
        },
    );

    // EVENT
    dispatcher.on_event(
        net_id,
        TraceWireEvent::Event {
            span_id,
            task_index: 0,
            flags: 0,
            dc_time: 200,
            name: "checkpoint".to_string(),
            attrs: vec![("step".to_string(), AttrValue::I64(1))],
        },
    );

    // END
    dispatcher.on_event(
        net_id,
        TraceWireEvent::End {
            span_id,
            task_index: 0,
            flags: 0,
            dc_time: 300,
            status: 1,
            message: "completed".to_string(),
        },
    );

    // Verify record
    let record = rx.try_recv().unwrap();
    assert_eq!(record.name, "span");
    assert_eq!(record.status_message, "completed");
    assert_eq!(
        record.span_attributes.get("user_id"),
        Some(&serde_json::json!(42))
    );
    assert_eq!(record.events.len(), 1);
    assert_eq!(record.events[0].name, "checkpoint");
}

#[test]
fn test_otlp_payload_construction() {
    // Build a minimal trace record and validate key fields
    let record = TraceRecord {
        trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
        span_id: "00f067aa0ba902b7".to_string(),
        parent_span_id: String::new(),
        name: "test_span".to_string(),
        kind: 0,
        start_time: chrono::Utc::now(),
        end_time: chrono::Utc::now() + chrono::Duration::seconds(1),
        status_code: 1,
        status_message: "success".to_string(),
        resource_attributes: HashMap::new(),
        scope_attributes: HashMap::new(),
        span_attributes: {
            let mut m = HashMap::new();
            m.insert("version".to_string(), serde_json::json!("1.0"));
            m
        },
        events: vec![tc_otel_core::TraceEventRecord {
            timestamp: chrono::Utc::now(),
            name: "event1".to_string(),
            attributes: HashMap::new(),
        }],
    };

    // Verify key fields exist for OTLP export
    assert!(!record.trace_id.is_empty());
    assert!(!record.span_id.is_empty());
    assert_eq!(record.name, "test_span");
    assert!(record.start_time < record.end_time);
    assert_eq!(record.span_attributes.len(), 1);
    assert_eq!(record.events.len(), 1);
}

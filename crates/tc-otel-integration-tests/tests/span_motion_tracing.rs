//! Integration tests for motion sequence tracing (to-4ob.1)
//!
//! Tests the complete span flow: ADS binary parsing → SpanEntry → OTEL attributes
//! Focused on axis movement start/end spans with motion-specific attributes.

use chrono::Utc;
use std::collections::HashMap;
use tc_otel_ads::AdsParser;
use tc_otel_core::{SpanEntry, SpanEvent, SpanKind, SpanStatusCode};

// ─── Helper: build ADS binary span message ─────────────────────────

/// Build a minimal ADS binary span (type 0x05) with the given fields.
/// Returns the raw bytes that AdsParser::parse_all can consume.
fn build_ads_span_bytes(
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: [u8; 8],
    kind: SpanKind,
    status: SpanStatusCode,
    name: &str,
    status_message: &str,
    attributes: &[(&str, u8, &[u8])], // (key, type_id, raw_value_bytes)
    events: &[(&str, &[(&str, u8, &[u8])])], // (name, [(key, type_id, raw_value_bytes)])
) -> Vec<u8> {
    let mut payload = Vec::new();

    // trace_id (16 bytes)
    payload.extend_from_slice(&trace_id);
    // span_id (8 bytes)
    payload.extend_from_slice(&span_id);
    // parent_span_id (8 bytes)
    payload.extend_from_slice(&parent_span_id);
    // kind (1 byte)
    payload.push(kind.as_u8());
    // status_code (1 byte)
    payload.push(status.as_u8());
    // start_time (8 bytes FILETIME — use epoch diff + some value)
    let filetime_base: u64 = 116444736000000000 + 1_000_000_000; // ~100s after epoch
    payload.extend_from_slice(&filetime_base.to_le_bytes());
    // end_time (8 bytes FILETIME — 10s later)
    let filetime_end: u64 = filetime_base + 100_000_000; // +10s
    payload.extend_from_slice(&filetime_end.to_le_bytes());
    // task_index (1 byte)
    payload.push(1);
    // cycle_counter (4 bytes LE)
    payload.extend_from_slice(&500u32.to_le_bytes());
    // attr_count (1 byte)
    payload.push(attributes.len() as u8);
    // event_count (1 byte)
    payload.push(events.len() as u8);
    // name (string: 1-byte len + bytes)
    append_string(&mut payload, name);
    // status_message (string)
    append_string(&mut payload, status_message);
    // attributes
    for (key, type_id, value_bytes) in attributes {
        append_string(&mut payload, key);
        payload.push(*type_id);
        payload.extend_from_slice(value_bytes);
    }
    // events
    for (ev_name, ev_attrs) in events {
        // event timestamp (FILETIME)
        let ev_time: u64 = filetime_base + 50_000_000; // midway
        payload.extend_from_slice(&ev_time.to_le_bytes());
        // event name
        append_string(&mut payload, ev_name);
        // event attr_count
        payload.push(ev_attrs.len() as u8);
        for (key, type_id, value_bytes) in *ev_attrs {
            append_string(&mut payload, key);
            payload.push(*type_id);
            payload.extend_from_slice(value_bytes);
        }
    }

    // Now wrap: [type=0x05] [entry_length: u16 LE] [payload]
    let mut data = Vec::new();
    data.push(0x05);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

fn append_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
}

/// Encode an f64 as LREAL (type 5) raw bytes
fn lreal_bytes(v: f64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Encode a u32 as UDINT (type 11) raw bytes
fn udint_bytes(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Encode a string value (type 12) as raw bytes (1-byte len + UTF-8)
fn string_value_bytes(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(s.len() as u8);
    v.extend_from_slice(s.as_bytes());
    v
}

// ─── Core span type tests ──────────────────────────────────────────

#[test]
fn test_span_entry_motion_axis_move() {
    let trace_id = [0xAB; 16];
    let span_id = [0xCD; 8];

    let mut entry = SpanEntry::new(trace_id, span_id, "motion.axis_move".to_string());
    entry.kind = SpanKind::Internal;
    entry.status_code = SpanStatusCode::Ok;
    entry.hostname = "plc-01".to_string();
    entry.source = "192.168.1.10".to_string();
    entry.task_name = "MotionTask".to_string();
    entry.task_index = 1;
    entry.project_name = "PackagingLine".to_string();
    entry.app_name = "AxisControl".to_string();

    // Motion-specific attributes
    entry
        .attributes
        .insert("motion.axis_id".to_string(), serde_json::json!(3));
    entry
        .attributes
        .insert("motion.axis_name".to_string(), serde_json::json!("X-Axis"));
    entry.attributes.insert(
        "motion.target_position".to_string(),
        serde_json::json!(250.0),
    );
    entry
        .attributes
        .insert("motion.start_position".to_string(), serde_json::json!(0.0));
    entry
        .attributes
        .insert("motion.velocity".to_string(), serde_json::json!(100.0));
    entry
        .attributes
        .insert("motion.acceleration".to_string(), serde_json::json!(500.0));

    assert_eq!(entry.name, "motion.axis_move");
    assert_eq!(entry.kind, SpanKind::Internal);
    assert_eq!(entry.status_code, SpanStatusCode::Ok);
    assert_eq!(entry.attributes.len(), 6);
    assert_eq!(
        entry.trace_id_hex(),
        "abababababababababababababababab" // 16 × "ab"
    );

    // Verify all motion attributes present
    assert!(entry.attributes.contains_key("motion.axis_id"));
    assert!(entry.attributes.contains_key("motion.target_position"));
    assert!(entry.attributes.contains_key("motion.velocity"));
}

#[test]
fn test_span_entry_motion_with_events() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "motion.axis_move".to_string());

    // Add motion sequence events
    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "motion.command_issued".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "motion.command".to_string(),
                serde_json::json!("MC_MoveAbsolute"),
            );
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "motion.in_velocity".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "motion.current_velocity".to_string(),
                serde_json::json!(100.0),
            );
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "motion.target_reached".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "motion.final_position".to_string(),
                serde_json::json!(250.0),
            );
            m.insert(
                "motion.position_error".to_string(),
                serde_json::json!(0.001),
            );
            m
        },
    });

    assert_eq!(entry.events.len(), 3);
    assert_eq!(entry.events[0].name, "motion.command_issued");
    assert_eq!(entry.events[1].name, "motion.in_velocity");
    assert_eq!(entry.events[2].name, "motion.target_reached");
}

#[test]
fn test_span_entry_motion_error() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "motion.axis_move".to_string());
    entry.status_code = SpanStatusCode::Error;
    entry.status_message = "Drive fault: following error exceeded".to_string();

    entry
        .attributes
        .insert("motion.axis_id".to_string(), serde_json::json!(2));
    entry
        .attributes
        .insert("motion.error_code".to_string(), serde_json::json!(0x4550));

    assert_eq!(entry.status_code, SpanStatusCode::Error);
    assert!(!entry.status_message.is_empty());
}

#[test]
fn test_span_entry_parent_child_motion_sequence() {
    let trace_id = [0x01; 16];
    let parent_span_id = [0x10; 8];
    let child_span_id = [0x20; 8];

    // Parent: overall motion sequence
    let parent = SpanEntry::new(trace_id, parent_span_id, "motion.sequence".to_string());
    assert!(!parent.has_parent());

    // Child: individual axis move within the sequence
    let mut child = SpanEntry::new(trace_id, child_span_id, "motion.axis_move".to_string());
    child.parent_span_id = parent_span_id;
    assert!(child.has_parent());
    assert_eq!(child.parent_span_id, parent_span_id);
    assert_eq!(child.trace_id, parent.trace_id);
}

// ─── ADS binary parser tests ───────────────────────────────────────

#[test]
fn test_parse_minimal_span() {
    let data = build_ads_span_bytes(
        [0xAA; 16],
        [0xBB; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.entries.len(), 0, "no log entries");
    assert_eq!(result.spans.len(), 1, "one span");

    let span = &result.spans[0];
    assert_eq!(span.trace_id, [0xAA; 16]);
    assert_eq!(span.span_id, [0xBB; 8]);
    assert_eq!(span.parent_span_id, [0x00; 8]);
    assert_eq!(span.name, "motion.axis_move");
    assert_eq!(span.kind, SpanKind::Internal);
    assert_eq!(span.status_code, SpanStatusCode::Ok);
    assert_eq!(span.status_message, "");
    assert_eq!(span.task_index, 1);
    assert_eq!(span.task_cycle_counter, 500);
    assert_eq!(span.attributes.len(), 0);
    assert_eq!(span.events.len(), 0);
}

#[test]
fn test_parse_span_with_attributes() {
    let axis_id_bytes = udint_bytes(3);
    let target_pos_bytes = lreal_bytes(250.0);
    let velocity_bytes = lreal_bytes(100.0);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[
            ("motion.axis_id", 11, &axis_id_bytes),           // UDINT
            ("motion.target_position", 5, &target_pos_bytes), // LREAL
            ("motion.velocity", 5, &velocity_bytes),          // LREAL
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.attributes.len(), 3);
    assert_eq!(span.attributes["motion.axis_id"], serde_json::json!(3));
    assert_eq!(
        span.attributes["motion.target_position"],
        serde_json::json!(250.0)
    );
    assert_eq!(span.attributes["motion.velocity"], serde_json::json!(100.0));
}

#[test]
fn test_parse_span_with_events() {
    let pos_bytes = lreal_bytes(250.0);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[],
        &[(
            "motion.target_reached",
            &[("motion.final_position", 5, pos_bytes.as_slice())],
        )],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.events.len(), 1);
    assert_eq!(span.events[0].name, "motion.target_reached");
    assert_eq!(
        span.events[0].attributes["motion.final_position"],
        serde_json::json!(250.0)
    );
}

#[test]
fn test_parse_span_with_parent() {
    let parent_id = [0xFF; 8];

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        parent_id,
        SpanKind::Internal,
        SpanStatusCode::Unset,
        "motion.axis_move",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.parent_span_id, parent_id);
}

#[test]
fn test_parse_span_error_status() {
    let error_code_bytes = udint_bytes(0x4550);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Error,
        "motion.axis_move",
        "Following error exceeded",
        &[("motion.error_code", 11, &error_code_bytes)],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.status_code, SpanStatusCode::Error);
    assert_eq!(span.status_message, "Following error exceeded");
    assert_eq!(
        span.attributes["motion.error_code"],
        serde_json::json!(0x4550u32)
    );
}

#[test]
fn test_parse_mixed_logs_and_spans() {
    // Build a v2 log entry first, then a span
    let mut data = Vec::new();

    // Simple v2 log entry (type byte 2)
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
                         // timestamps (16 bytes total)
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // plc_timestamp
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // clock_timestamp
    log_payload.push(1); // task_index
    log_payload.extend_from_slice(&100u32.to_le_bytes()); // cycle_counter
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "Motor started"); // message
    append_string(&mut log_payload, "motion.log"); // logger

    data.push(0x02); // type byte
    data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&log_payload);

    // Then a span
    let span_data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[],
        &[],
    );
    data.extend_from_slice(&span_data);

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.entries.len(), 1, "one log entry");
    assert_eq!(result.spans.len(), 1, "one span");
    assert_eq!(result.entries[0].message, "Motor started");
    assert_eq!(result.spans[0].name, "motion.axis_move");
}

#[test]
fn test_parse_multiple_spans() {
    let mut data = Vec::new();

    for i in 0..3u8 {
        let span_data = build_ads_span_bytes(
            [0x01; 16],
            [i + 1; 8],
            [0x00; 8],
            SpanKind::Internal,
            SpanStatusCode::Ok,
            &format!("motion.axis_{}", i),
            "",
            &[],
            &[],
        );
        data.extend_from_slice(&span_data);
    }

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 3);
    assert_eq!(result.spans[0].name, "motion.axis_0");
    assert_eq!(result.spans[1].name, "motion.axis_1");
    assert_eq!(result.spans[2].name, "motion.axis_2");
}

// ─── ADS to SpanEntry conversion test ──────────────────────────────

#[test]
fn test_ads_span_to_span_entry_conversion() {
    // Parse a span from binary
    let target_bytes = lreal_bytes(250.0);
    let vel_bytes = lreal_bytes(100.0);
    let axis_name = string_value_bytes("X-Axis");

    let data = build_ads_span_bytes(
        [0xAB; 16],
        [0xCD; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[
            ("motion.target_position", 5, &target_bytes),
            ("motion.velocity", 5, &vel_bytes),
            ("motion.axis_name", 12, &axis_name),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let ads_span = &result.spans[0];

    // Convert AdsSpanEntry → SpanEntry (manual, mirroring how the listener would do it)
    let mut span_entry = SpanEntry::new(ads_span.trace_id, ads_span.span_id, ads_span.name.clone());
    span_entry.parent_span_id = ads_span.parent_span_id;
    span_entry.kind = ads_span.kind;
    span_entry.status_code = ads_span.status_code;
    span_entry.status_message = ads_span.status_message.clone();
    span_entry.start_time = ads_span.start_time;
    span_entry.end_time = ads_span.end_time;
    span_entry.task_index = ads_span.task_index;
    span_entry.task_cycle_counter = ads_span.task_cycle_counter;
    span_entry.source = "192.168.1.10".to_string();
    span_entry.hostname = "plc-01".to_string();
    span_entry.attributes = ads_span.attributes.clone();
    for ev in &ads_span.events {
        span_entry.events.push(SpanEvent {
            timestamp: ev.timestamp,
            name: ev.name.clone(),
            attributes: ev.attributes.clone(),
        });
    }

    assert_eq!(
        span_entry.trace_id_hex(),
        "abababababababababababababababab"
    );
    assert_eq!(span_entry.span_id_hex(), "cdcdcdcdcdcdcdcd");
    assert_eq!(span_entry.name, "motion.axis_move");
    assert_eq!(span_entry.kind, SpanKind::Internal);
    assert_eq!(
        span_entry.attributes["motion.target_position"],
        serde_json::json!(250.0)
    );
    assert_eq!(
        span_entry.attributes["motion.axis_name"],
        serde_json::json!("X-Axis")
    );
}

// ─── Security / limit tests ────────────────────────────────────────

#[test]
fn test_parse_span_rejects_invalid_kind() {
    let mut data = Vec::new();
    let mut payload = Vec::new();

    payload.extend_from_slice(&[0u8; 16]); // trace_id
    payload.extend_from_slice(&[0u8; 8]); // span_id
    payload.extend_from_slice(&[0u8; 8]); // parent_span_id
    payload.push(99); // invalid kind
    payload.push(0); // status
    payload.extend_from_slice(&[0u8; 8]); // start_time
    payload.extend_from_slice(&[0u8; 8]); // end_time
    payload.push(0); // task_index
    payload.extend_from_slice(&0u32.to_le_bytes()); // cycle
    payload.push(0); // attr_count
    payload.push(0); // event_count
    append_string(&mut payload, "test");
    append_string(&mut payload, "");

    data.push(0x05);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);

    let result = AdsParser::parse_all(&data);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Invalid span kind"), "error: {}", err);
}

#[test]
fn test_parse_span_rejects_invalid_status() {
    let mut data = Vec::new();
    let mut payload = Vec::new();

    payload.extend_from_slice(&[0u8; 16]); // trace_id
    payload.extend_from_slice(&[0u8; 8]); // span_id
    payload.extend_from_slice(&[0u8; 8]); // parent_span_id
    payload.push(0); // kind = Internal
    payload.push(99); // invalid status
    payload.extend_from_slice(&[0u8; 8]); // start_time
    payload.extend_from_slice(&[0u8; 8]); // end_time
    payload.push(0); // task_index
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.push(0); // attr_count
    payload.push(0); // event_count
    append_string(&mut payload, "test");
    append_string(&mut payload, "");

    data.push(0x05);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);

    let result = AdsParser::parse_all(&data);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Invalid span status"), "error: {}", err);
}

#[test]
fn test_parse_span_rejects_too_many_attributes() {
    let mut data = Vec::new();
    let mut payload = Vec::new();

    payload.extend_from_slice(&[0u8; 16]); // trace_id
    payload.extend_from_slice(&[0u8; 8]); // span_id
    payload.extend_from_slice(&[0u8; 8]); // parent_span_id
    payload.push(0); // kind
    payload.push(0); // status
    payload.extend_from_slice(&[0u8; 8]); // start_time
    payload.extend_from_slice(&[0u8; 8]); // end_time
    payload.push(0); // task_index
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.push(65); // attr_count = 65 (exceeds MAX_SPAN_ATTRIBUTES=64)
    payload.push(0); // event_count
    append_string(&mut payload, "test");
    append_string(&mut payload, "");

    data.push(0x05);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);

    let result = AdsParser::parse_all(&data);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("attribute count") && err.contains("exceeds maximum"),
        "error: {}",
        err
    );
}

#[test]
fn test_parse_span_rejects_too_many_events() {
    let mut data = Vec::new();
    let mut payload = Vec::new();

    payload.extend_from_slice(&[0u8; 16]); // trace_id
    payload.extend_from_slice(&[0u8; 8]); // span_id
    payload.extend_from_slice(&[0u8; 8]); // parent_span_id
    payload.push(0); // kind
    payload.push(0); // status
    payload.extend_from_slice(&[0u8; 8]); // start_time
    payload.extend_from_slice(&[0u8; 8]); // end_time
    payload.push(0); // task_index
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.push(0); // attr_count
    payload.push(129); // event_count = 129 (exceeds MAX_SPAN_EVENTS=128)
    append_string(&mut payload, "test");
    append_string(&mut payload, "");

    data.push(0x05);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);

    let result = AdsParser::parse_all(&data);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("event count") && err.contains("exceeds maximum"),
        "error: {}",
        err
    );
}

// ─── Backward compatibility ────────────────────────────────────────

#[test]
fn test_parse_all_still_works_for_logs_only() {
    // Ensure adding spans to ParseResult doesn't break existing log parsing
    let mut data = Vec::new();

    // Build a simple v2 log
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.push(1); // task_index
    log_payload.extend_from_slice(&100u32.to_le_bytes());
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "Test message");
    append_string(&mut log_payload, "test.logger");

    data.push(0x02);
    data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&log_payload);

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.spans.len(), 0); // No spans in log-only buffer
    assert_eq!(result.entries[0].message, "Test message");
}

#[test]
fn test_span_entry_all_span_kinds() {
    let kinds = [
        (SpanKind::Internal, 0),
        (SpanKind::Server, 1),
        (SpanKind::Client, 2),
        (SpanKind::Producer, 3),
        (SpanKind::Consumer, 4),
    ];

    for (kind, byte_val) in kinds {
        let data = build_ads_span_bytes(
            [0x01; 16],
            [0x02; 8],
            [0x00; 8],
            kind,
            SpanStatusCode::Unset,
            "test",
            "",
            &[],
            &[],
        );

        let result = AdsParser::parse_all(&data).unwrap();
        assert_eq!(result.spans[0].kind, kind, "kind byte {}", byte_val);
    }
}

#[test]
fn test_span_entry_all_status_codes() {
    let statuses = [
        SpanStatusCode::Unset,
        SpanStatusCode::Ok,
        SpanStatusCode::Error,
    ];

    for status in statuses {
        let data = build_ads_span_bytes(
            [0x01; 16],
            [0x02; 8],
            [0x00; 8],
            SpanKind::Internal,
            status,
            "test",
            "",
            &[],
            &[],
        );

        let result = AdsParser::parse_all(&data).unwrap();
        assert_eq!(result.spans[0].status_code, status);
    }
}

// ─── Motion-specific end-to-end scenarios ──────────────────────────

#[test]
fn test_e2e_motion_homing_sequence() {
    // Simulate a homing sequence: parent span + child axis move
    let trace_id = [0x42; 16];
    let parent_id = [0x10; 8];
    let child_id = [0x20; 8];

    // Parent span: homing sequence
    let data1 = build_ads_span_bytes(
        trace_id,
        parent_id,
        [0x00; 8], // no parent (root span)
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.homing_sequence",
        "",
        &[],
        &[],
    );

    // Child span: single axis homing within the sequence
    let axis_id_bytes = udint_bytes(1);
    let data2 = build_ads_span_bytes(
        trace_id,
        child_id,
        parent_id, // child of homing sequence
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_home",
        "",
        &[("motion.axis_id", 11, &axis_id_bytes)],
        &[],
    );

    let mut combined = data1;
    combined.extend_from_slice(&data2);

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.spans.len(), 2);

    let parent = &result.spans[0];
    let child = &result.spans[1];

    assert_eq!(parent.name, "motion.homing_sequence");
    assert_eq!(parent.parent_span_id, [0x00; 8]);
    assert_eq!(child.name, "motion.axis_home");
    assert_eq!(child.parent_span_id, parent_id);
    assert_eq!(child.trace_id, parent.trace_id);
}

#[test]
fn test_e2e_motion_multi_axis_coordinated_move() {
    let trace_id = [0x55; 16];
    let sequence_id = [0x01; 8];

    let mut combined = Vec::new();

    // Root span: coordinated move
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        sequence_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.coordinated_move",
        "",
        &[],
        &[],
    ));

    // 3 child spans: one per axis
    for i in 0..3u8 {
        let axis_id_bytes = udint_bytes(i as u32);
        let target_bytes = lreal_bytes((i as f64 + 1.0) * 100.0);
        combined.extend_from_slice(&build_ads_span_bytes(
            trace_id,
            [0x10 + i; 8],
            sequence_id,
            SpanKind::Internal,
            SpanStatusCode::Ok,
            "motion.axis_move",
            "",
            &[
                ("motion.axis_id", 11, &axis_id_bytes),
                ("motion.target_position", 5, &target_bytes),
            ],
            &[],
        ));
    }

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.spans.len(), 4);
    assert_eq!(result.spans[0].name, "motion.coordinated_move");

    for i in 1..4 {
        assert_eq!(result.spans[i].name, "motion.axis_move");
        assert_eq!(result.spans[i].parent_span_id, sequence_id);
        assert!(result.spans[i].attributes.contains_key("motion.axis_id"));
        assert!(result.spans[i]
            .attributes
            .contains_key("motion.target_position"));
    }
}

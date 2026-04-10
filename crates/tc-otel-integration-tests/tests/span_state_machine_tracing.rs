//! Integration tests for state machine transition tracing (to-4ob.3)
//!
//! Tests the complete span flow for PLC state machine transitions:
//! ADS binary parsing → SpanEntry → OTEL attributes
//!
//! State machine spans use the same wire protocol (type 0x05) as motion spans,
//! with domain-specific attributes:
//!   - state_machine.name    — state machine instance name
//!   - state_machine.old_state — state before transition
//!   - state_machine.new_state — state after transition
//!   - state_machine.trigger   — event/condition that caused the transition
//!   - state_machine.guard     — guard condition evaluated (if any)
//!   - state_machine.action    — action executed during transition (if any)

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

/// Encode a string value (type 12) as raw bytes (1-byte len + UTF-8)
fn string_value_bytes(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(s.len() as u8);
    v.extend_from_slice(s.as_bytes());
    v
}

/// Encode a u32 as UDINT (type 11) raw bytes
fn udint_bytes(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Encode an f64 as LREAL (type 5) raw bytes
fn lreal_bytes(v: f64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Encode a bool (type 13) raw bytes
fn bool_bytes(v: bool) -> Vec<u8> {
    vec![if v { 1 } else { 0 }]
}

// ─── Core span type tests for state machine transitions ────────────

#[test]
fn test_span_entry_state_machine_transition() {
    let trace_id = [0xAA; 16];
    let span_id = [0xBB; 8];

    let mut entry = SpanEntry::new(trace_id, span_id, "state_machine.transition".to_string());
    entry.kind = SpanKind::Internal;
    entry.status_code = SpanStatusCode::Ok;
    entry.hostname = "plc-01".to_string();
    entry.source = "192.168.1.10".to_string();
    entry.task_name = "MainTask".to_string();
    entry.task_index = 1;
    entry.project_name = "PackagingLine".to_string();
    entry.app_name = "StateMachineControl".to_string();

    // State machine transition attributes
    entry.attributes.insert(
        "state_machine.name".to_string(),
        serde_json::json!("PackagingStateMachine"),
    );
    entry.attributes.insert(
        "state_machine.old_state".to_string(),
        serde_json::json!("Idle"),
    );
    entry.attributes.insert(
        "state_machine.new_state".to_string(),
        serde_json::json!("Running"),
    );
    entry.attributes.insert(
        "state_machine.trigger".to_string(),
        serde_json::json!("StartCommand"),
    );

    assert_eq!(entry.name, "state_machine.transition");
    assert_eq!(entry.kind, SpanKind::Internal);
    assert_eq!(entry.status_code, SpanStatusCode::Ok);
    assert_eq!(entry.attributes.len(), 4);
    assert_eq!(
        entry.attributes["state_machine.name"],
        serde_json::json!("PackagingStateMachine"),
    );
    assert_eq!(
        entry.attributes["state_machine.old_state"],
        serde_json::json!("Idle"),
    );
    assert_eq!(
        entry.attributes["state_machine.new_state"],
        serde_json::json!("Running"),
    );
    assert_eq!(
        entry.attributes["state_machine.trigger"],
        serde_json::json!("StartCommand"),
    );
}

#[test]
fn test_span_entry_state_machine_with_guard_and_action() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "state_machine.transition".to_string());
    entry.kind = SpanKind::Internal;
    entry.status_code = SpanStatusCode::Ok;

    entry.attributes.insert(
        "state_machine.name".to_string(),
        serde_json::json!("ConveyorControl"),
    );
    entry.attributes.insert(
        "state_machine.old_state".to_string(),
        serde_json::json!("Stopped"),
    );
    entry.attributes.insert(
        "state_machine.new_state".to_string(),
        serde_json::json!("Accelerating"),
    );
    entry.attributes.insert(
        "state_machine.trigger".to_string(),
        serde_json::json!("RunCommand"),
    );
    entry.attributes.insert(
        "state_machine.guard".to_string(),
        serde_json::json!("SafetyOk"),
    );
    entry.attributes.insert(
        "state_machine.action".to_string(),
        serde_json::json!("EnableDrive"),
    );

    assert_eq!(entry.attributes.len(), 6);
    assert_eq!(
        entry.attributes["state_machine.guard"],
        serde_json::json!("SafetyOk"),
    );
    assert_eq!(
        entry.attributes["state_machine.action"],
        serde_json::json!("EnableDrive"),
    );
}

#[test]
fn test_span_entry_state_machine_with_events() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "state_machine.transition".to_string());

    // Events recording what happened during the transition
    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "state_machine.guard_evaluated".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "state_machine.guard".to_string(),
                serde_json::json!("SafetyInterlockOk"),
            );
            m.insert(
                "state_machine.guard_result".to_string(),
                serde_json::json!(true),
            );
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "state_machine.action_executed".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "state_machine.action".to_string(),
                serde_json::json!("ActivateClamp"),
            );
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "state_machine.state_entered".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "state_machine.new_state".to_string(),
                serde_json::json!("Clamping"),
            );
            m
        },
    });

    assert_eq!(entry.events.len(), 3);
    assert_eq!(entry.events[0].name, "state_machine.guard_evaluated");
    assert_eq!(entry.events[1].name, "state_machine.action_executed");
    assert_eq!(entry.events[2].name, "state_machine.state_entered");
    assert_eq!(
        entry.events[0].attributes["state_machine.guard_result"],
        serde_json::json!(true),
    );
}

#[test]
fn test_span_entry_state_machine_error_transition() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "state_machine.transition".to_string());
    entry.status_code = SpanStatusCode::Error;
    entry.status_message = "Guard condition failed: safety interlock open".to_string();

    entry.attributes.insert(
        "state_machine.name".to_string(),
        serde_json::json!("PackagingStateMachine"),
    );
    entry.attributes.insert(
        "state_machine.old_state".to_string(),
        serde_json::json!("Running"),
    );
    entry.attributes.insert(
        "state_machine.new_state".to_string(),
        serde_json::json!("Faulted"),
    );
    entry.attributes.insert(
        "state_machine.trigger".to_string(),
        serde_json::json!("SafetyFault"),
    );
    entry.attributes.insert(
        "state_machine.error_code".to_string(),
        serde_json::json!(0x8001_u32),
    );

    assert_eq!(entry.status_code, SpanStatusCode::Error);
    assert!(!entry.status_message.is_empty());
    assert_eq!(
        entry.attributes["state_machine.new_state"],
        serde_json::json!("Faulted"),
    );
}

#[test]
fn test_span_entry_parent_child_state_machine_lifecycle() {
    let trace_id = [0x01; 16];
    let lifecycle_span_id = [0x10; 8];
    let transition_span_id = [0x20; 8];

    // Parent: overall state machine lifecycle span
    let parent = SpanEntry::new(
        trace_id,
        lifecycle_span_id,
        "state_machine.lifecycle".to_string(),
    );
    assert!(!parent.has_parent());

    // Child: individual transition within the lifecycle
    let mut child = SpanEntry::new(
        trace_id,
        transition_span_id,
        "state_machine.transition".to_string(),
    );
    child.parent_span_id = lifecycle_span_id;
    child.attributes.insert(
        "state_machine.old_state".to_string(),
        serde_json::json!("Idle"),
    );
    child.attributes.insert(
        "state_machine.new_state".to_string(),
        serde_json::json!("Running"),
    );

    assert!(child.has_parent());
    assert_eq!(child.parent_span_id, lifecycle_span_id);
    assert_eq!(child.trace_id, parent.trace_id);
}

// ─── ADS binary parser tests ───────────────────────────────────────

#[test]
fn test_parse_minimal_state_machine_span() {
    let data = build_ads_span_bytes(
        [0xAA; 16],
        [0xBB; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.entries.len(), 0, "no log entries");
    assert_eq!(result.spans.len(), 1, "one span");

    let span = &result.spans[0];
    assert_eq!(span.name, "state_machine.transition");
    assert_eq!(span.kind, SpanKind::Internal);
    assert_eq!(span.status_code, SpanStatusCode::Ok);
    assert_eq!(span.attributes.len(), 0);
    assert_eq!(span.events.len(), 0);
}

#[test]
fn test_parse_state_machine_span_with_attributes() {
    let sm_name = string_value_bytes("PackagingStateMachine");
    let old_state = string_value_bytes("Idle");
    let new_state = string_value_bytes("Running");
    let trigger = string_value_bytes("StartCommand");

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
            ("state_machine.trigger", 12, &trigger),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.attributes.len(), 4);
    assert_eq!(
        span.attributes["state_machine.name"],
        serde_json::json!("PackagingStateMachine"),
    );
    assert_eq!(
        span.attributes["state_machine.old_state"],
        serde_json::json!("Idle"),
    );
    assert_eq!(
        span.attributes["state_machine.new_state"],
        serde_json::json!("Running"),
    );
    assert_eq!(
        span.attributes["state_machine.trigger"],
        serde_json::json!("StartCommand"),
    );
}

#[test]
fn test_parse_state_machine_span_with_guard_and_action() {
    let sm_name = string_value_bytes("ConveyorControl");
    let old_state = string_value_bytes("Stopped");
    let new_state = string_value_bytes("Accelerating");
    let trigger = string_value_bytes("RunCommand");
    let guard = string_value_bytes("SafetyOk");
    let action = string_value_bytes("EnableDrive");

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
            ("state_machine.trigger", 12, &trigger),
            ("state_machine.guard", 12, &guard),
            ("state_machine.action", 12, &action),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.attributes.len(), 6);
    assert_eq!(
        span.attributes["state_machine.guard"],
        serde_json::json!("SafetyOk"),
    );
    assert_eq!(
        span.attributes["state_machine.action"],
        serde_json::json!("EnableDrive"),
    );
}

#[test]
fn test_parse_state_machine_span_with_events() {
    let guard_name = string_value_bytes("SafetyInterlockOk");
    let guard_result = bool_bytes(true);
    let action_name = string_value_bytes("ActivateClamp");

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[],
        &[
            (
                "state_machine.guard_evaluated",
                &[
                    ("state_machine.guard", 12, guard_name.as_slice()),
                    ("state_machine.guard_result", 13, guard_result.as_slice()),
                ],
            ),
            (
                "state_machine.action_executed",
                &[("state_machine.action", 12, action_name.as_slice())],
            ),
        ],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.events.len(), 2);
    assert_eq!(span.events[0].name, "state_machine.guard_evaluated");
    assert_eq!(
        span.events[0].attributes["state_machine.guard"],
        serde_json::json!("SafetyInterlockOk"),
    );
    assert_eq!(
        span.events[0].attributes["state_machine.guard_result"],
        serde_json::json!(true),
    );
    assert_eq!(span.events[1].name, "state_machine.action_executed");
    assert_eq!(
        span.events[1].attributes["state_machine.action"],
        serde_json::json!("ActivateClamp"),
    );
}

#[test]
fn test_parse_state_machine_error_transition() {
    let sm_name = string_value_bytes("PackagingStateMachine");
    let old_state = string_value_bytes("Running");
    let new_state = string_value_bytes("Faulted");
    let trigger = string_value_bytes("SafetyFault");
    let error_code = udint_bytes(0x8001);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Error,
        "state_machine.transition",
        "Safety interlock open",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
            ("state_machine.trigger", 12, &trigger),
            ("state_machine.error_code", 11, &error_code),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.status_code, SpanStatusCode::Error);
    assert_eq!(span.status_message, "Safety interlock open");
    assert_eq!(
        span.attributes["state_machine.new_state"],
        serde_json::json!("Faulted"),
    );
    assert_eq!(
        span.attributes["state_machine.error_code"],
        serde_json::json!(0x8001_u32),
    );
}

#[test]
fn test_parse_state_machine_span_with_parent() {
    let parent_id = [0xFF; 8];

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        parent_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.parent_span_id, parent_id);
}

// ─── ADS to SpanEntry conversion test ──────────────────────────────

#[test]
fn test_ads_state_machine_span_to_span_entry_conversion() {
    let sm_name = string_value_bytes("ConveyorControl");
    let old_state = string_value_bytes("Stopped");
    let new_state = string_value_bytes("Running");
    let guard = string_value_bytes("SafetyOk");

    let data = build_ads_span_bytes(
        [0xAB; 16],
        [0xCD; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
            ("state_machine.guard", 12, &guard),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let ads_span = &result.spans[0];

    // Convert AdsSpanEntry → SpanEntry (mirrors how the service layer would do it)
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

    assert_eq!(span_entry.name, "state_machine.transition");
    assert_eq!(
        span_entry.trace_id_hex(),
        "abababababababababababababababab",
    );
    assert_eq!(
        span_entry.attributes["state_machine.name"],
        serde_json::json!("ConveyorControl"),
    );
    assert_eq!(
        span_entry.attributes["state_machine.old_state"],
        serde_json::json!("Stopped"),
    );
    assert_eq!(
        span_entry.attributes["state_machine.new_state"],
        serde_json::json!("Running"),
    );
}

// ─── Mixed message type tests ──────────────────────────────────────

#[test]
fn test_parse_mixed_logs_and_state_machine_spans() {
    let mut data = Vec::new();

    // A v2 log entry first
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // plc_timestamp
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // clock_timestamp
    log_payload.push(1); // task_index
    log_payload.extend_from_slice(&100u32.to_le_bytes()); // cycle_counter
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "State machine started"); // message
    append_string(&mut log_payload, "sm.log"); // logger

    data.push(0x02); // type byte
    data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&log_payload);

    // Then a state machine transition span
    let sm_name = string_value_bytes("PackagingSM");
    let old_state = string_value_bytes("Init");
    let new_state = string_value_bytes("Idle");
    let span_data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
        ],
        &[],
    );
    data.extend_from_slice(&span_data);

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.entries.len(), 1, "one log entry");
    assert_eq!(result.spans.len(), 1, "one span");
    assert_eq!(result.entries[0].message, "State machine started");
    assert_eq!(result.spans[0].name, "state_machine.transition");
    assert_eq!(
        result.spans[0].attributes["state_machine.name"],
        serde_json::json!("PackagingSM"),
    );
}

#[test]
fn test_parse_mixed_motion_and_state_machine_spans() {
    let mut data = Vec::new();

    // Motion span
    let axis_id = udint_bytes(1);
    let target = lreal_bytes(250.0);
    data.extend_from_slice(&build_ads_span_bytes(
        [0x01; 16],
        [0x10; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[
            ("motion.axis_id", 11, &axis_id),
            ("motion.target_position", 5, &target),
        ],
        &[],
    ));

    // State machine span
    let sm_name = string_value_bytes("PickAndPlace");
    let old_state = string_value_bytes("Moving");
    let new_state = string_value_bytes("Placing");
    data.extend_from_slice(&build_ads_span_bytes(
        [0x01; 16],
        [0x20; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 2);
    assert_eq!(result.spans[0].name, "motion.axis_move");
    assert_eq!(result.spans[1].name, "state_machine.transition");
    assert!(result.spans[0].attributes.contains_key("motion.axis_id"));
    assert!(result.spans[1]
        .attributes
        .contains_key("state_machine.name"));
}

// ─── End-to-end state machine lifecycle scenarios ──────────────────

#[test]
fn test_e2e_state_machine_full_lifecycle() {
    // Simulate a complete state machine lifecycle:
    //   Init → Idle → Running → Stopping → Idle
    // Parent: lifecycle span. Children: individual transition spans.
    let trace_id = [0x42; 16];
    let lifecycle_id = [0x01; 8];

    let mut combined = Vec::new();

    // Root span: state machine lifecycle
    let sm_name = string_value_bytes("PackagingSM");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        lifecycle_id,
        [0x00; 8], // root span
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.lifecycle",
        "",
        &[("state_machine.name", 12, &sm_name)],
        &[],
    ));

    // Transition 1: Init → Idle
    let old1 = string_value_bytes("Init");
    let new1 = string_value_bytes("Idle");
    let trigger1 = string_value_bytes("InitComplete");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x10; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &old1),
            ("state_machine.new_state", 12, &new1),
            ("state_machine.trigger", 12, &trigger1),
        ],
        &[],
    ));

    // Transition 2: Idle → Running
    let old2 = string_value_bytes("Idle");
    let new2 = string_value_bytes("Running");
    let trigger2 = string_value_bytes("StartCommand");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x20; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &old2),
            ("state_machine.new_state", 12, &new2),
            ("state_machine.trigger", 12, &trigger2),
        ],
        &[],
    ));

    // Transition 3: Running → Stopping
    let old3 = string_value_bytes("Running");
    let new3 = string_value_bytes("Stopping");
    let trigger3 = string_value_bytes("StopCommand");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x30; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &old3),
            ("state_machine.new_state", 12, &new3),
            ("state_machine.trigger", 12, &trigger3),
        ],
        &[],
    ));

    // Transition 4: Stopping → Idle
    let old4 = string_value_bytes("Stopping");
    let new4 = string_value_bytes("Idle");
    let trigger4 = string_value_bytes("StopComplete");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x40; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &old4),
            ("state_machine.new_state", 12, &new4),
            ("state_machine.trigger", 12, &trigger4),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.spans.len(), 5, "1 lifecycle + 4 transitions");

    // Lifecycle root span
    let lifecycle = &result.spans[0];
    assert_eq!(lifecycle.name, "state_machine.lifecycle");
    assert_eq!(lifecycle.parent_span_id, [0x00; 8]);
    assert_eq!(
        lifecycle.attributes["state_machine.name"],
        serde_json::json!("PackagingSM"),
    );

    // All transitions are children of the lifecycle
    for i in 1..5 {
        assert_eq!(result.spans[i].name, "state_machine.transition");
        assert_eq!(result.spans[i].parent_span_id, lifecycle_id);
        assert_eq!(result.spans[i].trace_id, trace_id);
        assert!(result.spans[i]
            .attributes
            .contains_key("state_machine.old_state"));
        assert!(result.spans[i]
            .attributes
            .contains_key("state_machine.new_state"));
        assert!(result.spans[i]
            .attributes
            .contains_key("state_machine.trigger"));
    }

    // Verify transition chain
    assert_eq!(
        result.spans[1].attributes["state_machine.old_state"],
        serde_json::json!("Init"),
    );
    assert_eq!(
        result.spans[1].attributes["state_machine.new_state"],
        serde_json::json!("Idle"),
    );
    assert_eq!(
        result.spans[2].attributes["state_machine.old_state"],
        serde_json::json!("Idle"),
    );
    assert_eq!(
        result.spans[2].attributes["state_machine.new_state"],
        serde_json::json!("Running"),
    );
    assert_eq!(
        result.spans[3].attributes["state_machine.old_state"],
        serde_json::json!("Running"),
    );
    assert_eq!(
        result.spans[3].attributes["state_machine.new_state"],
        serde_json::json!("Stopping"),
    );
    assert_eq!(
        result.spans[4].attributes["state_machine.old_state"],
        serde_json::json!("Stopping"),
    );
    assert_eq!(
        result.spans[4].attributes["state_machine.new_state"],
        serde_json::json!("Idle"),
    );
}

#[test]
fn test_e2e_state_machine_fault_and_recovery() {
    // Scenario: Running → Faulted (error) → Idle (recovery)
    let trace_id = [0x55; 16];
    let lifecycle_id = [0x01; 8];

    let mut combined = Vec::new();

    // Lifecycle root
    let sm_name = string_value_bytes("SortingMachine");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        lifecycle_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Error, // lifecycle ends in error state
        "state_machine.lifecycle",
        "Machine faulted during operation",
        &[("state_machine.name", 12, &sm_name)],
        &[],
    ));

    // Fault transition: Running → Faulted
    let old_fault = string_value_bytes("Running");
    let new_fault = string_value_bytes("Faulted");
    let trigger_fault = string_value_bytes("EmergencyStop");
    let error_code = udint_bytes(0xE001);
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x10; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Error,
        "state_machine.transition",
        "Emergency stop activated",
        &[
            ("state_machine.old_state", 12, &old_fault),
            ("state_machine.new_state", 12, &new_fault),
            ("state_machine.trigger", 12, &trigger_fault),
            ("state_machine.error_code", 11, &error_code),
        ],
        &[],
    ));

    // Recovery transition: Faulted → Idle
    let old_recover = string_value_bytes("Faulted");
    let new_recover = string_value_bytes("Idle");
    let trigger_recover = string_value_bytes("ResetCommand");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x20; 8],
        lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &old_recover),
            ("state_machine.new_state", 12, &new_recover),
            ("state_machine.trigger", 12, &trigger_recover),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.spans.len(), 3);

    // Lifecycle root
    assert_eq!(result.spans[0].name, "state_machine.lifecycle");
    assert_eq!(result.spans[0].status_code, SpanStatusCode::Error);

    // Fault transition
    assert_eq!(result.spans[1].status_code, SpanStatusCode::Error);
    assert_eq!(result.spans[1].status_message, "Emergency stop activated");
    assert_eq!(
        result.spans[1].attributes["state_machine.new_state"],
        serde_json::json!("Faulted"),
    );
    assert_eq!(
        result.spans[1].attributes["state_machine.error_code"],
        serde_json::json!(0xE001_u32),
    );

    // Recovery transition
    assert_eq!(result.spans[2].status_code, SpanStatusCode::Ok);
    assert_eq!(
        result.spans[2].attributes["state_machine.old_state"],
        serde_json::json!("Faulted"),
    );
    assert_eq!(
        result.spans[2].attributes["state_machine.new_state"],
        serde_json::json!("Idle"),
    );
}

#[test]
fn test_e2e_nested_state_machines() {
    // Scenario: outer state machine triggers inner state machine transitions
    // Outer: PackagingLine (Running state), Inner: FillerStation (filling cycle)
    let trace_id = [0x77; 16];
    let outer_id = [0x01; 8];
    let inner_lifecycle_id = [0x02; 8];

    let mut combined = Vec::new();

    // Outer state machine transition that triggers the inner
    let outer_name = string_value_bytes("PackagingLine");
    let outer_old = string_value_bytes("Idle");
    let outer_new = string_value_bytes("Running");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        outer_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &outer_name),
            ("state_machine.old_state", 12, &outer_old),
            ("state_machine.new_state", 12, &outer_new),
        ],
        &[],
    ));

    // Inner state machine lifecycle (child of outer transition)
    let inner_name = string_value_bytes("FillerStation");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        inner_lifecycle_id,
        outer_id, // child of outer transition
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.lifecycle",
        "",
        &[("state_machine.name", 12, &inner_name)],
        &[],
    ));

    // Inner transitions (children of inner lifecycle)
    let inner_old1 = string_value_bytes("Idle");
    let inner_new1 = string_value_bytes("Filling");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x10; 8],
        inner_lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &inner_old1),
            ("state_machine.new_state", 12, &inner_new1),
        ],
        &[],
    ));

    let inner_old2 = string_value_bytes("Filling");
    let inner_new2 = string_value_bytes("Complete");
    combined.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x20; 8],
        inner_lifecycle_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.old_state", 12, &inner_old2),
            ("state_machine.new_state", 12, &inner_new2),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&combined).unwrap();
    assert_eq!(result.spans.len(), 4);

    // Outer transition
    assert_eq!(result.spans[0].name, "state_machine.transition");
    assert_eq!(
        result.spans[0].attributes["state_machine.name"],
        serde_json::json!("PackagingLine"),
    );

    // Inner lifecycle is child of outer
    assert_eq!(result.spans[1].name, "state_machine.lifecycle");
    assert_eq!(result.spans[1].parent_span_id, outer_id);
    assert_eq!(
        result.spans[1].attributes["state_machine.name"],
        serde_json::json!("FillerStation"),
    );

    // Inner transitions are children of inner lifecycle
    assert_eq!(result.spans[2].parent_span_id, inner_lifecycle_id);
    assert_eq!(result.spans[3].parent_span_id, inner_lifecycle_id);

    // All share the same trace
    for span in &result.spans {
        assert_eq!(span.trace_id, trace_id);
    }
}

#[test]
fn test_e2e_state_machine_with_numeric_state_ids() {
    // Some state machines use numeric state IDs instead of string names
    let trace_id = [0x88; 16];
    let state_id_old = udint_bytes(10); // state 10 = "Running"
    let state_id_new = udint_bytes(20); // state 20 = "Paused"
    let sm_name = string_value_bytes("NumericSM");

    let data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state_id", 11, &state_id_old),
            ("state_machine.new_state_id", 11, &state_id_new),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let span = &result.spans[0];
    assert_eq!(
        span.attributes["state_machine.old_state_id"],
        serde_json::json!(10),
    );
    assert_eq!(
        span.attributes["state_machine.new_state_id"],
        serde_json::json!(20),
    );
}

#[test]
fn test_e2e_state_machine_transition_with_duration_attribute() {
    // Transition duration tracked as LREAL in milliseconds
    let sm_name = string_value_bytes("SlowMachine");
    let old_state = string_value_bytes("Warming");
    let new_state = string_value_bytes("Ready");
    let duration_ms = lreal_bytes(1523.7);

    let data = build_ads_span_bytes(
        [0x99; 16],
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "state_machine.transition",
        "",
        &[
            ("state_machine.name", 12, &sm_name),
            ("state_machine.old_state", 12, &old_state),
            ("state_machine.new_state", 12, &new_state),
            ("state_machine.transition_duration_ms", 5, &duration_ms),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let span = &result.spans[0];
    assert_eq!(
        span.attributes["state_machine.transition_duration_ms"],
        serde_json::json!(1523.7),
    );
}

// ─── Backward compatibility ────────────────────────────────────────

#[test]
fn test_state_machine_spans_dont_affect_log_parsing() {
    // Ensure state machine spans don't break existing log-only parsing
    let mut data = Vec::new();

    // Build a simple v2 log
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.push(1);
    log_payload.extend_from_slice(&100u32.to_le_bytes());
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "SM transition logged");
    append_string(&mut log_payload, "sm.logger");

    data.push(0x02);
    data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&log_payload);

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.spans.len(), 0);
    assert_eq!(result.entries[0].message, "SM transition logged");
}

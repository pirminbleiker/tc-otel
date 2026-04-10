//! Integration tests for recipe execution spans (to-4ob.2)
//!
//! Tests the complete span flow: ADS binary parsing → SpanEntry → OTEL attributes
//! Focused on recipe execution start/end spans with recipe-specific attributes.

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

/// Encode a bool as BOOL (type 0) raw bytes
fn bool_bytes(v: bool) -> Vec<u8> {
    vec![v as u8]
}

// ─── Core span type tests ──────────────────────────────────────────

#[test]
fn test_span_entry_recipe_execution() {
    let trace_id = [0xAB; 16];
    let span_id = [0xCD; 8];

    let mut entry = SpanEntry::new(trace_id, span_id, "recipe.execute".to_string());
    entry.kind = SpanKind::Internal;
    entry.status_code = SpanStatusCode::Ok;
    entry.hostname = "plc-01".to_string();
    entry.source = "192.168.1.10".to_string();
    entry.task_name = "RecipeTask".to_string();
    entry.task_index = 2;
    entry.project_name = "PackagingLine".to_string();
    entry.app_name = "RecipeControl".to_string();

    // Recipe-specific attributes
    entry
        .attributes
        .insert("recipe.recipe_id".to_string(), serde_json::json!(42));
    entry.attributes.insert(
        "recipe.recipe_name".to_string(),
        serde_json::json!("FillAndSeal_500ml"),
    );
    entry
        .attributes
        .insert("recipe.version".to_string(), serde_json::json!(3));
    entry
        .attributes
        .insert("recipe.step_count".to_string(), serde_json::json!(5));
    entry.attributes.insert(
        "recipe.batch_id".to_string(),
        serde_json::json!("BATCH-2026-0042"),
    );
    entry
        .attributes
        .insert("recipe.parameter_count".to_string(), serde_json::json!(12));

    assert_eq!(entry.name, "recipe.execute");
    assert_eq!(entry.kind, SpanKind::Internal);
    assert_eq!(entry.status_code, SpanStatusCode::Ok);
    assert_eq!(entry.attributes.len(), 6);
    assert_eq!(
        entry.trace_id_hex(),
        "abababababababababababababababab" // 16 × "ab"
    );

    // Verify all recipe attributes present
    assert!(entry.attributes.contains_key("recipe.recipe_id"));
    assert!(entry.attributes.contains_key("recipe.recipe_name"));
    assert!(entry.attributes.contains_key("recipe.batch_id"));
}

#[test]
fn test_span_entry_recipe_step() {
    let trace_id = [0x01; 16];
    let span_id = [0x02; 8];
    let parent_span_id = [0x10; 8]; // parent = recipe.execute

    let mut entry = SpanEntry::new(trace_id, span_id, "recipe.step".to_string());
    entry.parent_span_id = parent_span_id;
    entry.kind = SpanKind::Internal;
    entry.status_code = SpanStatusCode::Ok;
    entry.task_name = "RecipeTask".to_string();

    entry
        .attributes
        .insert("recipe.step_index".to_string(), serde_json::json!(2));
    entry.attributes.insert(
        "recipe.step_name".to_string(),
        serde_json::json!("FillContainer"),
    );
    entry
        .attributes
        .insert("recipe.target_volume".to_string(), serde_json::json!(500.0));
    entry
        .attributes
        .insert("recipe.fill_rate".to_string(), serde_json::json!(120.5));

    assert_eq!(entry.name, "recipe.step");
    assert!(entry.has_parent());
    assert_eq!(entry.parent_span_id, parent_span_id);
    assert_eq!(entry.attributes.len(), 4);
    assert_eq!(
        entry.attributes["recipe.step_name"],
        serde_json::json!("FillContainer")
    );
}

#[test]
fn test_span_entry_recipe_with_events() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "recipe.execute".to_string());

    // Recipe lifecycle events
    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "recipe.loaded".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert(
                "recipe.recipe_name".to_string(),
                serde_json::json!("FillAndSeal_500ml"),
            );
            m.insert("recipe.parameter_count".to_string(), serde_json::json!(12));
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "recipe.step_started".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert("recipe.step_index".to_string(), serde_json::json!(0));
            m.insert("recipe.step_name".to_string(), serde_json::json!("Preheat"));
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "recipe.step_completed".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert("recipe.step_index".to_string(), serde_json::json!(0));
            m.insert(
                "recipe.step_result".to_string(),
                serde_json::json!("passed"),
            );
            m
        },
    });

    entry.events.push(SpanEvent {
        timestamp: Utc::now(),
        name: "recipe.completed".to_string(),
        attributes: {
            let mut m = HashMap::new();
            m.insert("recipe.steps_executed".to_string(), serde_json::json!(5));
            m.insert("recipe.quality_passed".to_string(), serde_json::json!(true));
            m
        },
    });

    assert_eq!(entry.events.len(), 4);
    assert_eq!(entry.events[0].name, "recipe.loaded");
    assert_eq!(entry.events[1].name, "recipe.step_started");
    assert_eq!(entry.events[2].name, "recipe.step_completed");
    assert_eq!(entry.events[3].name, "recipe.completed");
}

#[test]
fn test_span_entry_recipe_error() {
    let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "recipe.execute".to_string());
    entry.status_code = SpanStatusCode::Error;
    entry.status_message = "Recipe aborted: temperature out of range".to_string();

    entry
        .attributes
        .insert("recipe.recipe_id".to_string(), serde_json::json!(42));
    entry
        .attributes
        .insert("recipe.error_code".to_string(), serde_json::json!(0x8010));
    entry.attributes.insert(
        "recipe.failed_step".to_string(),
        serde_json::json!("Preheat"),
    );

    assert_eq!(entry.status_code, SpanStatusCode::Error);
    assert!(!entry.status_message.is_empty());
    assert!(entry.attributes.contains_key("recipe.error_code"));
    assert!(entry.attributes.contains_key("recipe.failed_step"));
}

#[test]
fn test_span_entry_parent_child_recipe_steps() {
    let trace_id = [0x01; 16];
    let recipe_span_id = [0x10; 8];
    let step_span_id = [0x20; 8];

    // Parent: overall recipe execution
    let parent = SpanEntry::new(trace_id, recipe_span_id, "recipe.execute".to_string());
    assert!(!parent.has_parent());

    // Child: individual recipe step within the execution
    let mut child = SpanEntry::new(trace_id, step_span_id, "recipe.step".to_string());
    child.parent_span_id = recipe_span_id;
    assert!(child.has_parent());
    assert_eq!(child.parent_span_id, recipe_span_id);
    assert_eq!(child.trace_id, parent.trace_id);
}

// ─── ADS binary parser tests ───────────────────────────────────────

#[test]
fn test_parse_recipe_span_minimal() {
    let data = build_ads_span_bytes(
        [0xAA; 16],
        [0xBB; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
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
    assert_eq!(span.name, "recipe.execute");
    assert_eq!(span.kind, SpanKind::Internal);
    assert_eq!(span.status_code, SpanStatusCode::Ok);
    assert_eq!(span.status_message, "");
    assert_eq!(span.attributes.len(), 0);
    assert_eq!(span.events.len(), 0);
}

#[test]
fn test_parse_recipe_span_with_attributes() {
    let recipe_id_bytes = udint_bytes(42);
    let recipe_name = string_value_bytes("FillAndSeal_500ml");
    let version_bytes = udint_bytes(3);
    let step_count_bytes = udint_bytes(5);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[
            ("recipe.recipe_id", 11, &recipe_id_bytes),   // UDINT
            ("recipe.recipe_name", 12, &recipe_name),     // STRING
            ("recipe.version", 11, &version_bytes),       // UDINT
            ("recipe.step_count", 11, &step_count_bytes), // UDINT
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.attributes.len(), 4);
    assert_eq!(span.attributes["recipe.recipe_id"], serde_json::json!(42));
    assert_eq!(
        span.attributes["recipe.recipe_name"],
        serde_json::json!("FillAndSeal_500ml")
    );
    assert_eq!(span.attributes["recipe.version"], serde_json::json!(3));
    assert_eq!(span.attributes["recipe.step_count"], serde_json::json!(5));
}

#[test]
fn test_parse_recipe_step_span_with_attributes() {
    let step_index_bytes = udint_bytes(2);
    let step_name = string_value_bytes("FillContainer");
    let target_volume_bytes = lreal_bytes(500.0);
    let fill_rate_bytes = lreal_bytes(120.5);

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x10; 8], // child of recipe.execute
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.step",
        "",
        &[
            ("recipe.step_index", 11, &step_index_bytes), // UDINT
            ("recipe.step_name", 12, &step_name),         // STRING
            ("recipe.target_volume", 5, &target_volume_bytes), // LREAL
            ("recipe.fill_rate", 5, &fill_rate_bytes),    // LREAL
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.name, "recipe.step");
    assert_eq!(span.parent_span_id, [0x10; 8]);
    assert_eq!(span.attributes.len(), 4);
    assert_eq!(span.attributes["recipe.step_index"], serde_json::json!(2));
    assert_eq!(
        span.attributes["recipe.step_name"],
        serde_json::json!("FillContainer")
    );
    assert_eq!(
        span.attributes["recipe.target_volume"],
        serde_json::json!(500.0)
    );
    assert_eq!(
        span.attributes["recipe.fill_rate"],
        serde_json::json!(120.5)
    );
}

#[test]
fn test_parse_recipe_span_with_events() {
    let step_idx_bytes = udint_bytes(0);
    let step_name = string_value_bytes("Preheat");

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[],
        &[(
            "recipe.step_started",
            &[
                ("recipe.step_index", 11, step_idx_bytes.as_slice()),
                ("recipe.step_name", 12, step_name.as_slice()),
            ],
        )],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.events.len(), 1);
    assert_eq!(span.events[0].name, "recipe.step_started");
    assert_eq!(
        span.events[0].attributes["recipe.step_index"],
        serde_json::json!(0)
    );
    assert_eq!(
        span.events[0].attributes["recipe.step_name"],
        serde_json::json!("Preheat")
    );
}

#[test]
fn test_parse_recipe_span_error_status() {
    let error_code_bytes = udint_bytes(0x8010);
    let failed_step = string_value_bytes("Preheat");

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Error,
        "recipe.execute",
        "Temperature out of range",
        &[
            ("recipe.error_code", 11, &error_code_bytes),
            ("recipe.failed_step", 12, &failed_step),
        ],
        &[],
    );

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    let span = &result.spans[0];
    assert_eq!(span.status_code, SpanStatusCode::Error);
    assert_eq!(span.status_message, "Temperature out of range");
    assert_eq!(
        span.attributes["recipe.error_code"],
        serde_json::json!(0x8010u32)
    );
    assert_eq!(
        span.attributes["recipe.failed_step"],
        serde_json::json!("Preheat")
    );
}

#[test]
fn test_parse_recipe_span_with_parent() {
    let parent_id = [0xFF; 8];

    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        parent_id,
        SpanKind::Internal,
        SpanStatusCode::Unset,
        "recipe.step",
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
fn test_ads_recipe_span_to_span_entry_conversion() {
    let recipe_id_bytes = udint_bytes(42);
    let recipe_name = string_value_bytes("FillAndSeal_500ml");
    let batch_id = string_value_bytes("BATCH-2026-0042");

    let data = build_ads_span_bytes(
        [0xAB; 16],
        [0xCD; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[
            ("recipe.recipe_id", 11, &recipe_id_bytes),
            ("recipe.recipe_name", 12, &recipe_name),
            ("recipe.batch_id", 12, &batch_id),
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
    assert_eq!(span_entry.name, "recipe.execute");
    assert_eq!(span_entry.kind, SpanKind::Internal);
    assert_eq!(
        span_entry.attributes["recipe.recipe_id"],
        serde_json::json!(42)
    );
    assert_eq!(
        span_entry.attributes["recipe.recipe_name"],
        serde_json::json!("FillAndSeal_500ml")
    );
    assert_eq!(
        span_entry.attributes["recipe.batch_id"],
        serde_json::json!("BATCH-2026-0042")
    );
}

// ─── Mixed payload tests ──────────────────────────────────────────

#[test]
fn test_parse_mixed_logs_and_recipe_spans() {
    let mut data = Vec::new();

    // Simple v2 log entry (type byte 2)
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // plc_timestamp
    log_payload.extend_from_slice(&filetime.to_le_bytes()); // clock_timestamp
    log_payload.push(1); // task_index
    log_payload.extend_from_slice(&100u32.to_le_bytes()); // cycle_counter
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "Recipe started"); // message
    append_string(&mut log_payload, "recipe.log"); // logger

    data.push(0x02); // type byte
    data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&log_payload);

    // Then a recipe span
    let span_data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[],
        &[],
    );
    data.extend_from_slice(&span_data);

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.entries.len(), 1, "one log entry");
    assert_eq!(result.spans.len(), 1, "one span");
    assert_eq!(result.entries[0].message, "Recipe started");
    assert_eq!(result.spans[0].name, "recipe.execute");
}

#[test]
fn test_parse_multiple_recipe_step_spans() {
    let trace_id = [0x01; 16];
    let recipe_span_id = [0x10; 8];
    let step_names = ["Preheat", "FillContainer", "Seal", "QualityCheck", "Eject"];
    let mut data = Vec::new();

    for (i, step_name) in step_names.iter().enumerate() {
        let step_idx = udint_bytes(i as u32);
        let name = string_value_bytes(step_name);
        data.extend_from_slice(&build_ads_span_bytes(
            trace_id,
            [0x20 + i as u8; 8],
            recipe_span_id,
            SpanKind::Internal,
            SpanStatusCode::Ok,
            "recipe.step",
            "",
            &[
                ("recipe.step_index", 11, &step_idx),
                ("recipe.step_name", 12, &name),
            ],
            &[],
        ));
    }

    let result = AdsParser::parse_all(&data).expect("parse should succeed");
    assert_eq!(result.spans.len(), 5);
    for (i, span) in result.spans.iter().enumerate() {
        assert_eq!(span.name, "recipe.step");
        assert_eq!(span.parent_span_id, recipe_span_id);
        assert_eq!(
            span.attributes["recipe.step_index"],
            serde_json::json!(i as u32)
        );
        assert_eq!(
            span.attributes["recipe.step_name"],
            serde_json::json!(step_names[i])
        );
    }
}

// ─── Recipe-specific end-to-end scenarios ──────────────────────────

#[test]
fn test_e2e_recipe_full_execution() {
    // Simulate a complete recipe execution: parent recipe span + 3 child step spans
    let trace_id = [0x42; 16];
    let recipe_id = [0x10; 8];

    let recipe_id_attr = udint_bytes(42);
    let recipe_name = string_value_bytes("FillAndSeal_500ml");
    let step_count = udint_bytes(3);
    let batch_id = string_value_bytes("BATCH-2026-0042");

    // Parent span: recipe execution
    let mut data = build_ads_span_bytes(
        trace_id,
        recipe_id,
        [0x00; 8], // no parent (root span)
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[
            ("recipe.recipe_id", 11, &recipe_id_attr),
            ("recipe.recipe_name", 12, &recipe_name),
            ("recipe.step_count", 11, &step_count),
            ("recipe.batch_id", 12, &batch_id),
        ],
        &[],
    );

    // Child steps
    let steps = [
        ("Preheat", 85.0f64),
        ("FillContainer", 500.0f64),
        ("Seal", 200.0f64),
    ];

    for (i, (name, target)) in steps.iter().enumerate() {
        let step_idx = udint_bytes(i as u32);
        let step_name = string_value_bytes(name);
        let target_val = lreal_bytes(*target);
        data.extend_from_slice(&build_ads_span_bytes(
            trace_id,
            [0x20 + i as u8; 8],
            recipe_id, // child of recipe execution
            SpanKind::Internal,
            SpanStatusCode::Ok,
            "recipe.step",
            "",
            &[
                ("recipe.step_index", 11, &step_idx),
                ("recipe.step_name", 12, &step_name),
                ("recipe.target_value", 5, &target_val),
            ],
            &[],
        ));
    }

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 4);

    // Verify parent
    let parent = &result.spans[0];
    assert_eq!(parent.name, "recipe.execute");
    assert_eq!(parent.parent_span_id, [0x00; 8]);
    assert_eq!(parent.attributes["recipe.recipe_id"], serde_json::json!(42));
    assert_eq!(
        parent.attributes["recipe.recipe_name"],
        serde_json::json!("FillAndSeal_500ml")
    );
    assert_eq!(parent.attributes["recipe.step_count"], serde_json::json!(3));

    // Verify children
    for i in 1..4 {
        let child = &result.spans[i];
        assert_eq!(child.name, "recipe.step");
        assert_eq!(child.parent_span_id, recipe_id);
        assert_eq!(child.trace_id, parent.trace_id);
        assert!(child.attributes.contains_key("recipe.step_index"));
        assert!(child.attributes.contains_key("recipe.step_name"));
        assert!(child.attributes.contains_key("recipe.target_value"));
    }

    // Verify specific step attributes
    assert_eq!(
        result.spans[1].attributes["recipe.step_name"],
        serde_json::json!("Preheat")
    );
    assert_eq!(
        result.spans[2].attributes["recipe.step_name"],
        serde_json::json!("FillContainer")
    );
    assert_eq!(
        result.spans[3].attributes["recipe.step_name"],
        serde_json::json!("Seal")
    );
}

#[test]
fn test_e2e_recipe_with_step_events() {
    // Recipe execution span with detailed lifecycle events
    let trace_id = [0x55; 16];
    let recipe_id = [0x01; 8];

    let recipe_name = string_value_bytes("MixAndDispense");
    let step_idx_0 = udint_bytes(0);
    let step_name_0 = string_value_bytes("LoadIngredients");
    let step_idx_1 = udint_bytes(1);
    let step_name_1 = string_value_bytes("Mix");

    let data = build_ads_span_bytes(
        trace_id,
        recipe_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[("recipe.recipe_name", 12, &recipe_name)],
        &[
            (
                "recipe.step_started",
                &[
                    ("recipe.step_index", 11, step_idx_0.as_slice()),
                    ("recipe.step_name", 12, step_name_0.as_slice()),
                ],
            ),
            (
                "recipe.step_completed",
                &[("recipe.step_index", 11, step_idx_0.as_slice())],
            ),
            (
                "recipe.step_started",
                &[
                    ("recipe.step_index", 11, step_idx_1.as_slice()),
                    ("recipe.step_name", 12, step_name_1.as_slice()),
                ],
            ),
            (
                "recipe.step_completed",
                &[("recipe.step_index", 11, step_idx_1.as_slice())],
            ),
        ],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.name, "recipe.execute");
    assert_eq!(span.events.len(), 4);
    assert_eq!(span.events[0].name, "recipe.step_started");
    assert_eq!(
        span.events[0].attributes["recipe.step_name"],
        serde_json::json!("LoadIngredients")
    );
    assert_eq!(span.events[1].name, "recipe.step_completed");
    assert_eq!(span.events[2].name, "recipe.step_started");
    assert_eq!(
        span.events[2].attributes["recipe.step_name"],
        serde_json::json!("Mix")
    );
    assert_eq!(span.events[3].name, "recipe.step_completed");
}

#[test]
fn test_e2e_recipe_abort_mid_execution() {
    // Recipe that fails partway through: 2 steps succeed, 3rd fails, recipe errors
    let trace_id = [0x77; 16];
    let recipe_id = [0x10; 8];

    let recipe_name = string_value_bytes("FillAndSeal_500ml");
    let step_count = udint_bytes(5);

    // Parent recipe span — error status
    let mut data = build_ads_span_bytes(
        trace_id,
        recipe_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Error,
        "recipe.execute",
        "Recipe aborted at step 2: Seal pressure fault",
        &[
            ("recipe.recipe_name", 12, &recipe_name),
            ("recipe.step_count", 11, &step_count),
        ],
        &[],
    );

    // Step 0: OK
    let step_0_idx = udint_bytes(0);
    let step_0_name = string_value_bytes("Preheat");
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x20; 8],
        recipe_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.step",
        "",
        &[
            ("recipe.step_index", 11, &step_0_idx),
            ("recipe.step_name", 12, &step_0_name),
        ],
        &[],
    ));

    // Step 1: OK
    let step_1_idx = udint_bytes(1);
    let step_1_name = string_value_bytes("FillContainer");
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x21; 8],
        recipe_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.step",
        "",
        &[
            ("recipe.step_index", 11, &step_1_idx),
            ("recipe.step_name", 12, &step_1_name),
        ],
        &[],
    ));

    // Step 2: ERROR
    let step_2_idx = udint_bytes(2);
    let step_2_name = string_value_bytes("Seal");
    let error_code = udint_bytes(0x4001);
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x22; 8],
        recipe_id,
        SpanKind::Internal,
        SpanStatusCode::Error,
        "recipe.step",
        "Seal pressure fault",
        &[
            ("recipe.step_index", 11, &step_2_idx),
            ("recipe.step_name", 12, &step_2_name),
            ("recipe.error_code", 11, &error_code),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 4);

    // Parent recipe is in error
    assert_eq!(result.spans[0].name, "recipe.execute");
    assert_eq!(result.spans[0].status_code, SpanStatusCode::Error);
    assert!(result.spans[0]
        .status_message
        .contains("Seal pressure fault"));

    // First two steps OK
    assert_eq!(result.spans[1].status_code, SpanStatusCode::Ok);
    assert_eq!(result.spans[2].status_code, SpanStatusCode::Ok);

    // Third step is the failure
    assert_eq!(result.spans[3].status_code, SpanStatusCode::Error);
    assert_eq!(result.spans[3].status_message, "Seal pressure fault");
    assert_eq!(
        result.spans[3].attributes["recipe.error_code"],
        serde_json::json!(0x4001u32)
    );
}

#[test]
fn test_e2e_recipe_nested_substeps() {
    // Recipe → Step → SubStep (3-level hierarchy)
    let trace_id = [0x88; 16];
    let recipe_id = [0x10; 8];
    let step_id = [0x20; 8];
    let substep_id = [0x30; 8];

    let mut data = Vec::new();

    // Root: recipe execution
    let recipe_name = string_value_bytes("ComplexAssembly");
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        recipe_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[("recipe.recipe_name", 12, &recipe_name)],
        &[],
    ));

    // Step: within recipe
    let step_name = string_value_bytes("AssembleModule");
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        step_id,
        recipe_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.step",
        "",
        &[("recipe.step_name", 12, &step_name)],
        &[],
    ));

    // SubStep: within step
    let substep_name = string_value_bytes("TightenBolt");
    let torque_bytes = lreal_bytes(25.5);
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        substep_id,
        step_id, // parent is the step, not the recipe
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.substep",
        "",
        &[
            ("recipe.step_name", 12, &substep_name),
            ("recipe.torque_target", 5, &torque_bytes),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 3);

    // Verify hierarchy
    let recipe = &result.spans[0];
    let step = &result.spans[1];
    let substep = &result.spans[2];

    assert_eq!(recipe.name, "recipe.execute");
    assert_eq!(recipe.parent_span_id, [0x00; 8]);

    assert_eq!(step.name, "recipe.step");
    assert_eq!(step.parent_span_id, recipe_id);

    assert_eq!(substep.name, "recipe.substep");
    assert_eq!(substep.parent_span_id, step_id); // parent is step, not recipe
    assert_eq!(substep.trace_id, trace_id);
    assert_eq!(
        substep.attributes["recipe.torque_target"],
        serde_json::json!(25.5)
    );
}

#[test]
fn test_e2e_recipe_mixed_with_motion_spans() {
    // Verify recipe and motion spans coexist in the same ADS buffer
    let trace_id = [0x99; 16];
    let recipe_id = [0x10; 8];
    let motion_id = [0x20; 8];

    let mut data = Vec::new();

    // Recipe span
    let recipe_name = string_value_bytes("PickAndPlace");
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        recipe_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.execute",
        "",
        &[("recipe.recipe_name", 12, &recipe_name)],
        &[],
    ));

    // Motion span (child of recipe — recipe triggers axis movement)
    let axis_id = udint_bytes(1);
    let target_pos = lreal_bytes(350.0);
    data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        motion_id,
        recipe_id, // motion is child of recipe step
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[
            ("motion.axis_id", 11, &axis_id),
            ("motion.target_position", 5, &target_pos),
        ],
        &[],
    ));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 2);

    assert_eq!(result.spans[0].name, "recipe.execute");
    assert_eq!(result.spans[1].name, "motion.axis_move");
    assert_eq!(result.spans[1].parent_span_id, recipe_id);
    assert_eq!(result.spans[1].trace_id, trace_id);
}

#[test]
fn test_e2e_recipe_quality_check_event() {
    // Recipe step with quality check events using BOOL and LREAL attributes
    let trace_id = [0xAA; 16];
    let span_id = [0x01; 8];

    let measured_val = lreal_bytes(499.8);
    let tolerance_bytes = lreal_bytes(1.0);
    let passed = bool_bytes(true);

    let data = build_ads_span_bytes(
        trace_id,
        span_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "recipe.step",
        "",
        &[],
        &[(
            "recipe.quality_check",
            &[
                ("recipe.measured_value", 5, measured_val.as_slice()),
                ("recipe.tolerance", 5, tolerance_bytes.as_slice()),
                ("recipe.check_passed", 13, passed.as_slice()), // BOOL = type 13
            ],
        )],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.spans.len(), 1);

    let span = &result.spans[0];
    assert_eq!(span.events.len(), 1);
    assert_eq!(span.events[0].name, "recipe.quality_check");
    assert_eq!(
        span.events[0].attributes["recipe.measured_value"],
        serde_json::json!(499.8)
    );
    assert_eq!(
        span.events[0].attributes["recipe.tolerance"],
        serde_json::json!(1.0)
    );
    assert_eq!(
        span.events[0].attributes["recipe.check_passed"],
        serde_json::json!(true) // BOOL type 13: 1 byte, value 1 = true
    );
}

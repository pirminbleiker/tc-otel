//! Integration tests for distributed tracing across multiple PLCs (to-4ob.4)
//!
//! Tests the complete span flow for distributed traces that span multiple PLCs:
//! ADS binary parsing → SpanEntry → cross-PLC correlation via shared trace_id
//!

#![allow(clippy::too_many_arguments, clippy::type_complexity)]
//! Distributed tracing key concepts:
//!   - Shared trace_id across PLCs links spans into a single distributed trace
//!   - parent_span_id enables cross-PLC parent-child relationships
//!   - ams_net_id + source uniquely identify the originating PLC
//!   - SpanKind (Client/Server, Producer/Consumer) models inter-PLC communication
//!   - Each PLC sends spans independently; the service aggregates by trace_id

use std::collections::HashMap;
use tc_otel_ads::AdsParser;
use tc_otel_core::{SpanEntry, SpanEvent, SpanKind, SpanStatusCode};

// ─── Helper: build ADS binary span message ─────────────────────────

/// Build a minimal ADS binary span (type 0x05) with the given fields.
/// Returns the raw bytes that AdsParser::parse_all can consume.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
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

/// Encode a bool as BOOL (type 13) raw bytes
fn bool_bytes(v: bool) -> Vec<u8> {
    vec![if v { 1 } else { 0 }]
}

/// Convert an AdsSpanEntry to a SpanEntry with PLC source identification.
/// Mirrors how the AMS server enriches spans with connection metadata.
fn ads_span_to_span_entry(
    ads_span: &tc_otel_ads::AdsSpanEntry,
    source_ip: &str,
    hostname: &str,
    ams_net_id: &str,
    ams_source_port: u16,
) -> SpanEntry {
    let mut entry = SpanEntry::new(ads_span.trace_id, ads_span.span_id, ads_span.name.clone());
    entry.parent_span_id = ads_span.parent_span_id;
    entry.kind = ads_span.kind;
    entry.status_code = ads_span.status_code;
    entry.status_message = ads_span.status_message.clone();
    entry.start_time = ads_span.start_time;
    entry.end_time = ads_span.end_time;
    entry.task_index = ads_span.task_index;
    entry.task_cycle_counter = ads_span.task_cycle_counter;
    entry.source = source_ip.to_string();
    entry.hostname = hostname.to_string();
    entry.ams_net_id = ams_net_id.to_string();
    entry.ams_source_port = ams_source_port;
    entry.attributes = ads_span.attributes.clone();
    for ev in &ads_span.events {
        entry.events.push(SpanEvent {
            timestamp: ev.timestamp,
            name: ev.name.clone(),
            attributes: ev.attributes.clone(),
        });
    }
    entry
}

// ─── Shared trace_id across multiple PLCs ─────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_shared_trace_id_two_plcs() {
    // Two PLCs contribute spans to the same distributed trace.
    // PLC-1 initiates a production sequence, PLC-2 handles a downstream step.
    let shared_trace_id = [0xD1; 16]; // same trace across both PLCs
    let plc1_span_id = [0x01; 8];
    let plc2_span_id = [0x02; 8];

    // PLC-1 span: production.sequence (root)
    let plc1_data = build_ads_span_bytes(
        shared_trace_id,
        plc1_span_id,
        [0x00; 8], // root span
        SpanKind::Server,
        SpanStatusCode::Ok,
        "production.sequence",
        "",
        &[],
        &[],
    );

    // PLC-2 span: production.downstream_step (child of PLC-1 root)
    let plc2_data = build_ads_span_bytes(
        shared_trace_id,
        plc2_span_id,
        plc1_span_id, // parent is on PLC-1
        SpanKind::Server,
        SpanStatusCode::Ok,
        "production.downstream_step",
        "",
        &[],
        &[],
    );

    // Parse each PLC's buffer independently (they arrive on separate connections)
    let result1 = AdsParser::parse_all(&plc1_data).expect("PLC-1 parse");
    let result2 = AdsParser::parse_all(&plc2_data).expect("PLC-2 parse");

    assert_eq!(result1.spans.len(), 1);
    assert_eq!(result2.spans.len(), 1);

    // Convert to SpanEntry with distinct PLC source info
    let span1 = ads_span_to_span_entry(
        &result1.spans[0],
        "192.168.1.100",
        "plc-01",
        "192.168.1.100.1.1",
        851,
    );
    let span2 = ads_span_to_span_entry(
        &result2.spans[0],
        "192.168.1.101",
        "plc-02",
        "192.168.1.101.1.1",
        851,
    );

    // Both share the same trace_id
    assert_eq!(span1.trace_id, span2.trace_id);
    assert_eq!(span1.trace_id, shared_trace_id);

    // Different source PLCs
    assert_ne!(span1.source, span2.source);
    assert_ne!(span1.hostname, span2.hostname);
    assert_ne!(span1.ams_net_id, span2.ams_net_id);

    // PLC-2 span is child of PLC-1 span
    assert!(!span1.has_parent());
    assert!(span2.has_parent());
    assert_eq!(span2.parent_span_id, span1.span_id);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_three_plc_pipeline() {
    // Three PLCs form a processing pipeline:
    // PLC-1 (sensor) → PLC-2 (controller) → PLC-3 (actuator)
    let shared_trace_id = [0xAA; 16];

    let sensor_span_id = [0x10; 8];
    let controller_span_id = [0x20; 8];
    let actuator_span_id = [0x30; 8];

    let plc_name_sensor = string_value_bytes("SensorPLC");
    let plc_name_controller = string_value_bytes("ControllerPLC");
    let plc_name_actuator = string_value_bytes("ActuatorPLC");

    // PLC-1: sensor reading (root span)
    let data1 = build_ads_span_bytes(
        shared_trace_id,
        sensor_span_id,
        [0x00; 8],
        SpanKind::Producer,
        SpanStatusCode::Ok,
        "pipeline.sensor_read",
        "",
        &[("plc.name", 12, &plc_name_sensor)],
        &[],
    );

    // PLC-2: controller processes sensor data (child of sensor)
    let data2 = build_ads_span_bytes(
        shared_trace_id,
        controller_span_id,
        sensor_span_id,
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "pipeline.control_compute",
        "",
        &[("plc.name", 12, &plc_name_controller)],
        &[],
    );

    // PLC-3: actuator executes command (child of controller)
    let data3 = build_ads_span_bytes(
        shared_trace_id,
        actuator_span_id,
        controller_span_id,
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "pipeline.actuator_execute",
        "",
        &[("plc.name", 12, &plc_name_actuator)],
        &[],
    );

    // Parse each independently
    let r1 = AdsParser::parse_all(&data1).unwrap();
    let r2 = AdsParser::parse_all(&data2).unwrap();
    let r3 = AdsParser::parse_all(&data3).unwrap();

    // Convert with distinct PLC identities
    let spans: Vec<SpanEntry> = vec![
        ads_span_to_span_entry(&r1.spans[0], "10.0.0.1", "sensor-plc", "10.0.0.1.1.1", 851),
        ads_span_to_span_entry(
            &r2.spans[0],
            "10.0.0.2",
            "controller-plc",
            "10.0.0.2.1.1",
            851,
        ),
        ads_span_to_span_entry(
            &r3.spans[0],
            "10.0.0.3",
            "actuator-plc",
            "10.0.0.3.1.1",
            851,
        ),
    ];

    // All three share the same trace_id
    assert!(spans.iter().all(|s| s.trace_id == shared_trace_id));

    // Chain: sensor (root) → controller → actuator
    assert!(!spans[0].has_parent());
    assert_eq!(spans[1].parent_span_id, sensor_span_id);
    assert_eq!(spans[2].parent_span_id, controller_span_id);

    // All from different PLCs
    let sources: Vec<&str> = spans.iter().map(|s| s.source.as_str()).collect();
    assert_eq!(sources.len(), 3);
    assert_ne!(sources[0], sources[1]);
    assert_ne!(sources[1], sources[2]);
    assert_ne!(sources[0], sources[2]);

    // Verify Producer/Consumer kinds model the inter-PLC flow
    assert_eq!(spans[0].kind, SpanKind::Producer);
    assert_eq!(spans[1].kind, SpanKind::Consumer);
    assert_eq!(spans[2].kind, SpanKind::Consumer);
}

// ─── Cross-PLC parent-child relationships ─────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_cross_plc_parent_child_spans() {
    // PLC-1 creates a root span, PLC-2 creates multiple child spans under it.
    let shared_trace_id = [0xCC; 16];
    let root_span_id = [0x01; 8];

    // PLC-1: root coordination span
    let root_data = build_ads_span_bytes(
        shared_trace_id,
        root_span_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "coordination.batch_start",
        "",
        &[],
        &[],
    );

    // PLC-2: three child spans referencing PLC-1's root
    let mut plc2_data = Vec::new();
    for i in 0..3u8 {
        let step_name = string_value_bytes(&format!("step_{}", i));
        plc2_data.extend_from_slice(&build_ads_span_bytes(
            shared_trace_id,
            [0x10 + i; 8],
            root_span_id, // parent is on PLC-1
            SpanKind::Internal,
            SpanStatusCode::Ok,
            "coordination.batch_step",
            "",
            &[("batch.step_name", 12, &step_name)],
            &[],
        ));
    }

    let root_result = AdsParser::parse_all(&root_data).unwrap();
    let children_result = AdsParser::parse_all(&plc2_data).unwrap();

    assert_eq!(root_result.spans.len(), 1);
    assert_eq!(children_result.spans.len(), 3);

    let root_entry = ads_span_to_span_entry(
        &root_result.spans[0],
        "192.168.1.10",
        "plc-coordinator",
        "192.168.1.10.1.1",
        851,
    );

    // All children reference the root span on PLC-1
    for child_ads in &children_result.spans {
        let child = ads_span_to_span_entry(
            child_ads,
            "192.168.1.20",
            "plc-worker",
            "192.168.1.20.1.1",
            851,
        );

        assert_eq!(child.trace_id, root_entry.trace_id);
        assert_eq!(child.parent_span_id, root_entry.span_id);
        assert!(child.has_parent());
        assert_ne!(child.ams_net_id, root_entry.ams_net_id);
    }
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_cross_plc_nested_hierarchy() {
    // Deep nesting across PLCs:
    // PLC-1: root → PLC-2: mid → PLC-3: leaf
    let trace_id = [0xBB; 16];
    let root_id = [0x01; 8];
    let mid_id = [0x02; 8];
    let leaf_id = [0x03; 8];

    let root_data = build_ads_span_bytes(
        trace_id,
        root_id,
        [0x00; 8],
        SpanKind::Server,
        SpanStatusCode::Ok,
        "distributed.root",
        "",
        &[],
        &[],
    );

    let mid_data = build_ads_span_bytes(
        trace_id,
        mid_id,
        root_id,
        SpanKind::Server,
        SpanStatusCode::Ok,
        "distributed.mid",
        "",
        &[],
        &[],
    );

    let leaf_data = build_ads_span_bytes(
        trace_id,
        leaf_id,
        mid_id,
        SpanKind::Server,
        SpanStatusCode::Ok,
        "distributed.leaf",
        "",
        &[],
        &[],
    );

    let r1 = AdsParser::parse_all(&root_data).unwrap();
    let r2 = AdsParser::parse_all(&mid_data).unwrap();
    let r3 = AdsParser::parse_all(&leaf_data).unwrap();

    let root = ads_span_to_span_entry(&r1.spans[0], "10.0.1.1", "plc-a", "10.0.1.1.1.1", 851);
    let mid = ads_span_to_span_entry(&r2.spans[0], "10.0.1.2", "plc-b", "10.0.1.2.1.1", 851);
    let leaf = ads_span_to_span_entry(&r3.spans[0], "10.0.1.3", "plc-c", "10.0.1.3.1.1", 851);

    // Verify full chain
    assert!(!root.has_parent());
    assert_eq!(mid.parent_span_id, root.span_id);
    assert_eq!(leaf.parent_span_id, mid.span_id);

    // All share trace_id
    assert_eq!(root.trace_id, trace_id);
    assert_eq!(mid.trace_id, trace_id);
    assert_eq!(leaf.trace_id, trace_id);

    // All from different PLCs
    assert_ne!(root.ams_net_id, mid.ams_net_id);
    assert_ne!(mid.ams_net_id, leaf.ams_net_id);
}

// ─── PLC source identification ────────────────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_plc_source_identification_fields() {
    // Verify all source identification fields are preserved correctly
    let data = build_ads_span_bytes(
        [0x01; 16],
        [0x02; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "test.span",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();
    let entry = ads_span_to_span_entry(
        &result.spans[0],
        "172.16.0.50",
        "packaging-plc-main",
        "172.16.0.50.1.1",
        851,
    );

    assert_eq!(entry.source, "172.16.0.50");
    assert_eq!(entry.hostname, "packaging-plc-main");
    assert_eq!(entry.ams_net_id, "172.16.0.50.1.1");
    assert_eq!(entry.ams_source_port, 851);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_same_trace_different_ams_net_ids() {
    // Two PLCs with same trace_id but distinguishable by AMS Net ID
    let trace_id = [0xEE; 16];

    let data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "distributed.operation",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();

    // Same ADS span data, enriched with different PLC identities
    let plc1_span = ads_span_to_span_entry(
        &result.spans[0],
        "192.168.10.1",
        "line-1-plc",
        "192.168.10.1.1.1",
        851,
    );
    let plc2_span = ads_span_to_span_entry(
        &result.spans[0],
        "192.168.10.2",
        "line-2-plc",
        "192.168.10.2.1.1",
        852,
    );

    // Same trace context
    assert_eq!(plc1_span.trace_id, plc2_span.trace_id);
    assert_eq!(plc1_span.span_id, plc2_span.span_id);

    // Different PLC identification
    assert_ne!(plc1_span.source, plc2_span.source);
    assert_ne!(plc1_span.hostname, plc2_span.hostname);
    assert_ne!(plc1_span.ams_net_id, plc2_span.ams_net_id);
    assert_ne!(plc1_span.ams_source_port, plc2_span.ams_source_port);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_multiple_ams_source_ports_same_plc() {
    // A single PLC can have multiple ADS ports (runtime instances)
    // Each gets its own ams_source_port
    let trace_id = [0xFF; 16];

    let data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "multi_runtime.operation",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();

    let runtime1 = ads_span_to_span_entry(
        &result.spans[0],
        "192.168.1.50",
        "plc-dual",
        "192.168.1.50.1.1",
        851, // Runtime 1
    );
    let runtime2 = ads_span_to_span_entry(
        &result.spans[0],
        "192.168.1.50",
        "plc-dual",
        "192.168.1.50.1.1",
        852, // Runtime 2
    );

    // Same PLC (same IP, hostname, AMS Net ID)
    assert_eq!(runtime1.source, runtime2.source);
    assert_eq!(runtime1.hostname, runtime2.hostname);
    assert_eq!(runtime1.ams_net_id, runtime2.ams_net_id);

    // Different runtime ports
    assert_ne!(runtime1.ams_source_port, runtime2.ams_source_port);
}

// ─── Client/Server span kinds for inter-PLC communication ────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_client_server_span_kind_cross_plc() {
    // PLC-1 sends a command (Client), PLC-2 processes it (Server)
    let trace_id = [0xAB; 16];
    let client_span_id = [0x01; 8];
    let server_span_id = [0x02; 8];

    let target_plc = string_value_bytes("192.168.1.101");

    let client_data = build_ads_span_bytes(
        trace_id,
        client_span_id,
        [0x00; 8],
        SpanKind::Client,
        SpanStatusCode::Ok,
        "ads.write_request",
        "",
        &[("rpc.target", 12, &target_plc)],
        &[],
    );

    let source_plc = string_value_bytes("192.168.1.100");

    let server_data = build_ads_span_bytes(
        trace_id,
        server_span_id,
        client_span_id,
        SpanKind::Server,
        SpanStatusCode::Ok,
        "ads.write_handler",
        "",
        &[("rpc.source", 12, &source_plc)],
        &[],
    );

    let r1 = AdsParser::parse_all(&client_data).unwrap();
    let r2 = AdsParser::parse_all(&server_data).unwrap();

    let client = ads_span_to_span_entry(
        &r1.spans[0],
        "192.168.1.100",
        "plc-requester",
        "192.168.1.100.1.1",
        851,
    );
    let server = ads_span_to_span_entry(
        &r2.spans[0],
        "192.168.1.101",
        "plc-responder",
        "192.168.1.101.1.1",
        851,
    );

    assert_eq!(client.kind, SpanKind::Client);
    assert_eq!(server.kind, SpanKind::Server);
    assert_eq!(server.parent_span_id, client.span_id);
    assert_eq!(client.trace_id, server.trace_id);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_producer_consumer_span_kind_cross_plc() {
    // PLC-1 produces data (Producer), PLC-2 consumes it (Consumer)
    // Asynchronous communication pattern (e.g., shared memory or ADS notification)
    let trace_id = [0xCD; 16];

    let producer_data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Producer,
        SpanStatusCode::Ok,
        "data.publish",
        "",
        &[],
        &[],
    );

    let consumer_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        [0x01; 8],
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "data.consume",
        "",
        &[],
        &[],
    );

    let r1 = AdsParser::parse_all(&producer_data).unwrap();
    let r2 = AdsParser::parse_all(&consumer_data).unwrap();

    let producer = ads_span_to_span_entry(
        &r1.spans[0],
        "10.0.0.1",
        "plc-producer",
        "10.0.0.1.1.1",
        851,
    );
    let consumer = ads_span_to_span_entry(
        &r2.spans[0],
        "10.0.0.2",
        "plc-consumer",
        "10.0.0.2.1.1",
        851,
    );

    assert_eq!(producer.kind, SpanKind::Producer);
    assert_eq!(consumer.kind, SpanKind::Consumer);
    assert_eq!(consumer.parent_span_id, producer.span_id);
}

// ─── Distributed trace context attributes ─────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_with_domain_attributes() {
    // Distributed trace across two PLCs with rich domain-specific attributes.
    // PLC-1 runs a packaging sequence, PLC-2 runs the labeling step.
    let trace_id = [0xDD; 16];
    let packaging_span_id = [0x01; 8];
    let labeling_span_id = [0x02; 8];

    let batch_id = string_value_bytes("BATCH-2026-0410-001");
    let product_id = string_value_bytes("SKU-12345");
    let line_id = udint_bytes(3);

    let packaging_data = build_ads_span_bytes(
        trace_id,
        packaging_span_id,
        [0x00; 8],
        SpanKind::Producer,
        SpanStatusCode::Ok,
        "packaging.sequence",
        "",
        &[
            ("batch.id", 12, &batch_id),
            ("product.sku", 12, &product_id),
            ("production.line_id", 11, &line_id),
        ],
        &[],
    );

    let label_type = string_value_bytes("barcode-128");
    let labeling_data = build_ads_span_bytes(
        trace_id,
        labeling_span_id,
        packaging_span_id,
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "labeling.apply",
        "",
        &[("batch.id", 12, &batch_id), ("label.type", 12, &label_type)],
        &[],
    );

    let r1 = AdsParser::parse_all(&packaging_data).unwrap();
    let r2 = AdsParser::parse_all(&labeling_data).unwrap();

    let packaging = ads_span_to_span_entry(
        &r1.spans[0],
        "192.168.2.10",
        "packaging-plc",
        "192.168.2.10.1.1",
        851,
    );
    let labeling = ads_span_to_span_entry(
        &r2.spans[0],
        "192.168.2.20",
        "labeling-plc",
        "192.168.2.20.1.1",
        851,
    );

    // Both share batch.id for correlation
    assert_eq!(
        packaging.attributes["batch.id"],
        serde_json::json!("BATCH-2026-0410-001")
    );
    assert_eq!(
        labeling.attributes["batch.id"],
        serde_json::json!("BATCH-2026-0410-001")
    );

    // Domain-specific attributes per PLC
    assert_eq!(
        packaging.attributes["product.sku"],
        serde_json::json!("SKU-12345")
    );
    assert_eq!(
        packaging.attributes["production.line_id"],
        serde_json::json!(3)
    );
    assert_eq!(
        labeling.attributes["label.type"],
        serde_json::json!("barcode-128")
    );
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_with_events_across_plcs() {
    // Distributed trace where both PLCs emit span events
    let trace_id = [0xEF; 16];

    let temp_bytes = lreal_bytes(185.5);

    let plc1_data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "oven.heat_cycle",
        "",
        &[],
        &[(
            "oven.temperature_reached",
            &[("oven.temperature_celsius", 5, temp_bytes.as_slice())],
        )],
    );

    let conveyor_speed = lreal_bytes(0.5);

    let plc2_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        [0x01; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "conveyor.transfer",
        "",
        &[],
        &[(
            "conveyor.item_transferred",
            &[("conveyor.speed_mps", 5, conveyor_speed.as_slice())],
        )],
    );

    let r1 = AdsParser::parse_all(&plc1_data).unwrap();
    let r2 = AdsParser::parse_all(&plc2_data).unwrap();

    let oven = ads_span_to_span_entry(&r1.spans[0], "10.0.0.10", "oven-plc", "10.0.0.10.1.1", 851);
    let conveyor = ads_span_to_span_entry(
        &r2.spans[0],
        "10.0.0.11",
        "conveyor-plc",
        "10.0.0.11.1.1",
        851,
    );

    assert_eq!(oven.events.len(), 1);
    assert_eq!(oven.events[0].name, "oven.temperature_reached");
    assert_eq!(
        oven.events[0].attributes["oven.temperature_celsius"],
        serde_json::json!(185.5)
    );

    assert_eq!(conveyor.events.len(), 1);
    assert_eq!(conveyor.events[0].name, "conveyor.item_transferred");
    assert_eq!(
        conveyor.events[0].attributes["conveyor.speed_mps"],
        serde_json::json!(0.5)
    );
}

// ─── Error propagation across PLCs ────────────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_error_on_downstream_plc() {
    // PLC-1 initiates, PLC-2 fails — error is visible in the distributed trace
    let trace_id = [0xFA; 16];

    let plc1_data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Client,
        SpanStatusCode::Ok,
        "transfer.initiate",
        "",
        &[],
        &[],
    );

    let error_code = udint_bytes(0x8001);
    let plc2_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        [0x01; 8],
        SpanKind::Server,
        SpanStatusCode::Error,
        "transfer.receive",
        "Buffer overflow on receiving PLC",
        &[("error.code", 11, &error_code)],
        &[],
    );

    let r1 = AdsParser::parse_all(&plc1_data).unwrap();
    let r2 = AdsParser::parse_all(&plc2_data).unwrap();

    let initiator = ads_span_to_span_entry(
        &r1.spans[0],
        "192.168.1.100",
        "plc-sender",
        "192.168.1.100.1.1",
        851,
    );
    let receiver = ads_span_to_span_entry(
        &r2.spans[0],
        "192.168.1.200",
        "plc-receiver",
        "192.168.1.200.1.1",
        851,
    );

    assert_eq!(initiator.status_code, SpanStatusCode::Ok);
    assert_eq!(receiver.status_code, SpanStatusCode::Error);
    assert_eq!(receiver.status_message, "Buffer overflow on receiving PLC");
    assert_eq!(
        receiver.attributes["error.code"],
        serde_json::json!(0x8001u32)
    );

    // Still part of the same trace
    assert_eq!(initiator.trace_id, receiver.trace_id);
    assert_eq!(receiver.parent_span_id, initiator.span_id);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_partial_failure() {
    // Fan-out: PLC-1 root, PLC-2 OK, PLC-3 Error
    let trace_id = [0xFB; 16];
    let root_id = [0x01; 8];

    let root_data = build_ads_span_bytes(
        trace_id,
        root_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "fanout.coordinator",
        "",
        &[],
        &[],
    );

    let ok_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        root_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "fanout.worker_a",
        "",
        &[],
        &[],
    );

    let err_code = udint_bytes(42);
    let err_data = build_ads_span_bytes(
        trace_id,
        [0x03; 8],
        root_id,
        SpanKind::Internal,
        SpanStatusCode::Error,
        "fanout.worker_b",
        "Timeout waiting for sensor",
        &[("error.code", 11, &err_code)],
        &[],
    );

    let r1 = AdsParser::parse_all(&root_data).unwrap();
    let r2 = AdsParser::parse_all(&ok_data).unwrap();
    let r3 = AdsParser::parse_all(&err_data).unwrap();

    let root = ads_span_to_span_entry(&r1.spans[0], "10.0.0.1", "coordinator", "10.0.0.1.1.1", 851);
    let ok_worker =
        ads_span_to_span_entry(&r2.spans[0], "10.0.0.2", "worker-a", "10.0.0.2.1.1", 851);
    let err_worker =
        ads_span_to_span_entry(&r3.spans[0], "10.0.0.3", "worker-b", "10.0.0.3.1.1", 851);

    // All in same trace
    assert_eq!(root.trace_id, ok_worker.trace_id);
    assert_eq!(root.trace_id, err_worker.trace_id);

    // Both workers are children of root
    assert_eq!(ok_worker.parent_span_id, root.span_id);
    assert_eq!(err_worker.parent_span_id, root.span_id);

    // Partial failure: one OK, one Error
    assert_eq!(root.status_code, SpanStatusCode::Ok);
    assert_eq!(ok_worker.status_code, SpanStatusCode::Ok);
    assert_eq!(err_worker.status_code, SpanStatusCode::Error);
    assert_eq!(err_worker.status_message, "Timeout waiting for sensor");
}

// ─── Aggregation of spans by trace_id ─────────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_aggregate_spans_by_trace_id() {
    // Simulate what the service does: collect spans from multiple PLCs,
    // group by trace_id to reconstruct the distributed trace.
    let trace_a = [0xA0; 16];
    let trace_b = [0xB0; 16];

    // Trace A spans (2 PLCs)
    let a1 = build_ads_span_bytes(
        trace_a,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "trace_a.root",
        "",
        &[],
        &[],
    );
    let a2 = build_ads_span_bytes(
        trace_a,
        [0x02; 8],
        [0x01; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "trace_a.child",
        "",
        &[],
        &[],
    );

    // Trace B spans (different trace, different PLCs)
    let b1 = build_ads_span_bytes(
        trace_b,
        [0x11; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "trace_b.root",
        "",
        &[],
        &[],
    );
    let b2 = build_ads_span_bytes(
        trace_b,
        [0x12; 8],
        [0x11; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "trace_b.child",
        "",
        &[],
        &[],
    );

    // Parse all (simulating arrival from different connections)
    let ra1 = AdsParser::parse_all(&a1).unwrap();
    let ra2 = AdsParser::parse_all(&a2).unwrap();
    let rb1 = AdsParser::parse_all(&b1).unwrap();
    let rb2 = AdsParser::parse_all(&b2).unwrap();

    let all_spans: Vec<SpanEntry> = vec![
        ads_span_to_span_entry(&ra1.spans[0], "10.0.0.1", "plc-1", "10.0.0.1.1.1", 851),
        ads_span_to_span_entry(&ra2.spans[0], "10.0.0.2", "plc-2", "10.0.0.2.1.1", 851),
        ads_span_to_span_entry(&rb1.spans[0], "10.0.0.3", "plc-3", "10.0.0.3.1.1", 851),
        ads_span_to_span_entry(&rb2.spans[0], "10.0.0.4", "plc-4", "10.0.0.4.1.1", 851),
    ];

    // Group by trace_id
    let mut trace_groups: HashMap<[u8; 16], Vec<&SpanEntry>> = HashMap::new();
    for span in &all_spans {
        trace_groups.entry(span.trace_id).or_default().push(span);
    }

    assert_eq!(trace_groups.len(), 2);
    assert_eq!(trace_groups[&trace_a].len(), 2);
    assert_eq!(trace_groups[&trace_b].len(), 2);

    // Verify trace A spans are correctly grouped
    let trace_a_names: Vec<&str> = trace_groups[&trace_a]
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(trace_a_names.contains(&"trace_a.root"));
    assert!(trace_a_names.contains(&"trace_a.child"));

    // Verify trace B spans are correctly grouped
    let trace_b_names: Vec<&str> = trace_groups[&trace_b]
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(trace_b_names.contains(&"trace_b.root"));
    assert!(trace_b_names.contains(&"trace_b.child"));
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_trace_id_hex_consistency_across_plcs() {
    // Verify trace_id_hex() produces consistent output for correlation
    let trace_id = [
        0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
        0x89,
    ];

    let data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "test",
        "",
        &[],
        &[],
    );

    let result = AdsParser::parse_all(&data).unwrap();

    let plc1 = ads_span_to_span_entry(&result.spans[0], "10.0.0.1", "plc-1", "10.0.0.1.1.1", 851);
    let plc2 = ads_span_to_span_entry(&result.spans[0], "10.0.0.2", "plc-2", "10.0.0.2.1.1", 851);

    // Hex representation must be identical for grouping
    assert_eq!(plc1.trace_id_hex(), plc2.trace_id_hex());
    assert_eq!(plc1.trace_id_hex(), "abcdef0123456789abcdef0123456789");
}

// ─── Real-world distributed trace scenarios ───────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_e2e_bottling_line_distributed_trace() {
    // Real-world scenario: a bottling line with 3 PLCs
    // PLC-1 (fill): fills bottles
    // PLC-2 (cap): caps filled bottles
    // PLC-3 (inspect): inspects capped bottles
    let trace_id = [0x42; 16];

    let fill_span_id = [0x01; 8];
    let cap_span_id = [0x02; 8];
    let inspect_span_id = [0x03; 8];

    let volume = lreal_bytes(500.0);
    let fill_data = build_ads_span_bytes(
        trace_id,
        fill_span_id,
        [0x00; 8],
        SpanKind::Producer,
        SpanStatusCode::Ok,
        "bottling.fill",
        "",
        &[("bottle.volume_ml", 5, &volume)],
        &[(
            "bottling.fill_complete",
            &[("bottle.volume_ml", 5, volume.as_slice())],
        )],
    );

    let torque = lreal_bytes(2.5);
    let cap_data = build_ads_span_bytes(
        trace_id,
        cap_span_id,
        fill_span_id,
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "bottling.cap",
        "",
        &[("cap.torque_nm", 5, &torque)],
        &[],
    );

    let pass = bool_bytes(true);
    let inspect_data = build_ads_span_bytes(
        trace_id,
        inspect_span_id,
        cap_span_id,
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "bottling.inspect",
        "",
        &[("inspection.passed", 13, &pass)],
        &[],
    );

    let r1 = AdsParser::parse_all(&fill_data).unwrap();
    let r2 = AdsParser::parse_all(&cap_data).unwrap();
    let r3 = AdsParser::parse_all(&inspect_data).unwrap();

    let fill = ads_span_to_span_entry(
        &r1.spans[0],
        "192.168.50.1",
        "fill-plc",
        "192.168.50.1.1.1",
        851,
    );
    let cap = ads_span_to_span_entry(
        &r2.spans[0],
        "192.168.50.2",
        "cap-plc",
        "192.168.50.2.1.1",
        851,
    );
    let inspect = ads_span_to_span_entry(
        &r3.spans[0],
        "192.168.50.3",
        "inspect-plc",
        "192.168.50.3.1.1",
        851,
    );

    // All in the same distributed trace
    assert_eq!(fill.trace_id, cap.trace_id);
    assert_eq!(cap.trace_id, inspect.trace_id);

    // Sequential pipeline: fill → cap → inspect
    assert!(!fill.has_parent());
    assert_eq!(cap.parent_span_id, fill.span_id);
    assert_eq!(inspect.parent_span_id, cap.span_id);

    // Producer/Consumer flow
    assert_eq!(fill.kind, SpanKind::Producer);
    assert_eq!(cap.kind, SpanKind::Consumer);
    assert_eq!(inspect.kind, SpanKind::Consumer);

    // Domain attributes preserved
    assert_eq!(
        fill.attributes["bottle.volume_ml"],
        serde_json::json!(500.0)
    );
    assert_eq!(cap.attributes["cap.torque_nm"], serde_json::json!(2.5));
    assert_eq!(
        inspect.attributes["inspection.passed"],
        serde_json::json!(true)
    );

    // Events on fill span
    assert_eq!(fill.events.len(), 1);
    assert_eq!(fill.events[0].name, "bottling.fill_complete");

    // All from different PLCs
    assert_ne!(fill.ams_net_id, cap.ams_net_id);
    assert_ne!(cap.ams_net_id, inspect.ams_net_id);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_e2e_multi_plc_coordinated_motion() {
    // Multi-PLC coordinated motion: PLC-1 coordinates, PLC-2 and PLC-3 each move axes
    let trace_id = [0x55; 16];
    let coord_span_id = [0x01; 8];

    let coord_data = build_ads_span_bytes(
        trace_id,
        coord_span_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.coordinated_multi_plc",
        "",
        &[],
        &[],
    );

    // PLC-2: X and Y axes
    let mut plc2_data = Vec::new();
    for (i, axis_name) in ["X-Axis", "Y-Axis"].iter().enumerate() {
        let axis_id = udint_bytes(i as u32);
        let name_bytes = string_value_bytes(axis_name);
        let target = lreal_bytes((i as f64 + 1.0) * 100.0);
        plc2_data.extend_from_slice(&build_ads_span_bytes(
            trace_id,
            [0x10 + i as u8; 8],
            coord_span_id,
            SpanKind::Internal,
            SpanStatusCode::Ok,
            "motion.axis_move",
            "",
            &[
                ("motion.axis_id", 11, &axis_id),
                ("motion.axis_name", 12, &name_bytes),
                ("motion.target_position", 5, &target),
            ],
            &[],
        ));
    }

    // PLC-3: Z axis
    let z_axis_id = udint_bytes(2);
    let z_name = string_value_bytes("Z-Axis");
    let z_target = lreal_bytes(300.0);
    let plc3_data = build_ads_span_bytes(
        trace_id,
        [0x20; 8],
        coord_span_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "motion.axis_move",
        "",
        &[
            ("motion.axis_id", 11, &z_axis_id),
            ("motion.axis_name", 12, &z_name),
            ("motion.target_position", 5, &z_target),
        ],
        &[],
    );

    let r1 = AdsParser::parse_all(&coord_data).unwrap();
    let r2 = AdsParser::parse_all(&plc2_data).unwrap();
    let r3 = AdsParser::parse_all(&plc3_data).unwrap();

    let coordinator = ads_span_to_span_entry(
        &r1.spans[0],
        "192.168.1.1",
        "motion-coordinator",
        "192.168.1.1.1.1",
        851,
    );

    assert_eq!(r2.spans.len(), 2, "PLC-2 should have 2 axis spans");
    let plc2_x = ads_span_to_span_entry(
        &r2.spans[0],
        "192.168.1.2",
        "motion-xy-plc",
        "192.168.1.2.1.1",
        851,
    );
    let plc2_y = ads_span_to_span_entry(
        &r2.spans[1],
        "192.168.1.2",
        "motion-xy-plc",
        "192.168.1.2.1.1",
        851,
    );
    let plc3_z = ads_span_to_span_entry(
        &r3.spans[0],
        "192.168.1.3",
        "motion-z-plc",
        "192.168.1.3.1.1",
        851,
    );

    // All share the same trace
    assert_eq!(coordinator.trace_id, plc2_x.trace_id);
    assert_eq!(coordinator.trace_id, plc2_y.trace_id);
    assert_eq!(coordinator.trace_id, plc3_z.trace_id);

    // All axis moves are children of the coordinator
    assert_eq!(plc2_x.parent_span_id, coord_span_id);
    assert_eq!(plc2_y.parent_span_id, coord_span_id);
    assert_eq!(plc3_z.parent_span_id, coord_span_id);

    // PLC-2 spans from same PLC (same ams_net_id)
    assert_eq!(plc2_x.ams_net_id, plc2_y.ams_net_id);
    // PLC-3 from different PLC
    assert_ne!(plc2_x.ams_net_id, plc3_z.ams_net_id);

    // Axis attributes preserved
    assert_eq!(
        plc2_x.attributes["motion.axis_name"],
        serde_json::json!("X-Axis")
    );
    assert_eq!(
        plc2_y.attributes["motion.axis_name"],
        serde_json::json!("Y-Axis")
    );
    assert_eq!(
        plc3_z.attributes["motion.axis_name"],
        serde_json::json!("Z-Axis")
    );
    assert_eq!(
        plc3_z.attributes["motion.target_position"],
        serde_json::json!(300.0)
    );
}

// ─── Interleaved buffers from multiple PLCs ───────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_interleaved_logs_and_spans_from_multiple_plcs() {
    // A single PLC buffer can contain both logs and spans.
    // Verify parsing handles this correctly for distributed trace context.
    let trace_id = [0x99; 16];

    // PLC-1 buffer: log + span
    let mut plc1_data = Vec::new();
    // Log entry first
    let mut log_payload = Vec::new();
    log_payload.push(2); // level = Info
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.extend_from_slice(&filetime.to_le_bytes());
    log_payload.push(1); // task_index
    log_payload.extend_from_slice(&100u32.to_le_bytes());
    log_payload.push(0); // arg_count
    log_payload.push(0); // context_count
    append_string(&mut log_payload, "Starting distributed operation");
    append_string(&mut log_payload, "distributed.logger");

    plc1_data.push(0x02);
    plc1_data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
    plc1_data.extend_from_slice(&log_payload);

    // Then a span
    plc1_data.extend_from_slice(&build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8],
        SpanKind::Producer,
        SpanStatusCode::Ok,
        "distributed.initiate",
        "",
        &[],
        &[],
    ));

    // PLC-2 buffer: just a span (child of PLC-1)
    let plc2_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        [0x01; 8],
        SpanKind::Consumer,
        SpanStatusCode::Ok,
        "distributed.process",
        "",
        &[],
        &[],
    );

    let r1 = AdsParser::parse_all(&plc1_data).unwrap();
    let r2 = AdsParser::parse_all(&plc2_data).unwrap();

    // PLC-1 has both a log and a span
    assert_eq!(r1.entries.len(), 1);
    assert_eq!(r1.spans.len(), 1);
    assert_eq!(r1.entries[0].message, "Starting distributed operation");

    // PLC-2 has only a span
    assert_eq!(r2.entries.len(), 0);
    assert_eq!(r2.spans.len(), 1);

    // Spans form a distributed trace
    assert_eq!(r1.spans[0].trace_id, r2.spans[0].trace_id);
    assert_eq!(r2.spans[0].parent_span_id, r1.spans[0].span_id);
}

// ─── Edge cases ───────────────────────────────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_root_span_per_plc() {
    // Multiple PLCs each emit root spans with the same trace_id.
    // This is valid: OTEL allows multiple roots in a trace.
    let trace_id = [0xDE; 16];

    let plc1_data = build_ads_span_bytes(
        trace_id,
        [0x01; 8],
        [0x00; 8], // root
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "plc1.root_operation",
        "",
        &[],
        &[],
    );

    let plc2_data = build_ads_span_bytes(
        trace_id,
        [0x02; 8],
        [0x00; 8], // also root
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "plc2.root_operation",
        "",
        &[],
        &[],
    );

    let r1 = AdsParser::parse_all(&plc1_data).unwrap();
    let r2 = AdsParser::parse_all(&plc2_data).unwrap();

    let span1 = ads_span_to_span_entry(&r1.spans[0], "10.0.0.1", "plc-1", "10.0.0.1.1.1", 851);
    let span2 = ads_span_to_span_entry(&r2.spans[0], "10.0.0.2", "plc-2", "10.0.0.2.1.1", 851);

    // Both are root spans in the same trace
    assert_eq!(span1.trace_id, span2.trace_id);
    assert!(!span1.has_parent());
    assert!(!span2.has_parent());
    assert_ne!(span1.span_id, span2.span_id);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_large_fan_out() {
    // One coordinator PLC fans out to 8 worker PLCs
    let trace_id = [0x77; 16];
    let coord_id = [0x01; 8];

    let coord_data = build_ads_span_bytes(
        trace_id,
        coord_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "fanout.coordinator",
        "",
        &[],
        &[],
    );

    let coord_result = AdsParser::parse_all(&coord_data).unwrap();
    let coordinator = ads_span_to_span_entry(
        &coord_result.spans[0],
        "10.0.0.1",
        "coordinator",
        "10.0.0.1.1.1",
        851,
    );

    let mut worker_spans = Vec::new();
    for i in 0..8u8 {
        let worker_data = build_ads_span_bytes(
            trace_id,
            [0x10 + i; 8],
            coord_id,
            SpanKind::Internal,
            SpanStatusCode::Ok,
            &format!("fanout.worker_{}", i),
            "",
            &[],
            &[],
        );

        let result = AdsParser::parse_all(&worker_data).unwrap();
        let worker = ads_span_to_span_entry(
            &result.spans[0],
            &format!("10.0.0.{}", 10 + i),
            &format!("worker-{}", i),
            &format!("10.0.0.{}.1.1", 10 + i),
            851,
        );
        worker_spans.push(worker);
    }

    // All workers are children of coordinator
    for worker in &worker_spans {
        assert_eq!(worker.trace_id, coordinator.trace_id);
        assert_eq!(worker.parent_span_id, coordinator.span_id);
        assert!(worker.has_parent());
    }

    // All workers from different PLCs
    let worker_sources: Vec<&str> = worker_spans.iter().map(|s| s.source.as_str()).collect();
    let unique_sources: std::collections::HashSet<&str> = worker_sources.iter().copied().collect();
    assert_eq!(unique_sources.len(), 8);
}

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_diamond_topology() {
    // Diamond pattern: PLC-1 → PLC-2 + PLC-3 → PLC-4
    // PLC-1 fans out to PLC-2 and PLC-3, both feed into PLC-4
    let trace_id = [0x88; 16];

    let plc1_id = [0x01; 8];
    let plc2_id = [0x02; 8];
    let plc3_id = [0x03; 8];
    let plc4_from_2_id = [0x04; 8];
    let plc4_from_3_id = [0x05; 8];

    let d1 = build_ads_span_bytes(
        trace_id,
        plc1_id,
        [0x00; 8],
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "diamond.source",
        "",
        &[],
        &[],
    );
    let d2 = build_ads_span_bytes(
        trace_id,
        plc2_id,
        plc1_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "diamond.branch_a",
        "",
        &[],
        &[],
    );
    let d3 = build_ads_span_bytes(
        trace_id,
        plc3_id,
        plc1_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "diamond.branch_b",
        "",
        &[],
        &[],
    );
    // PLC-4 receives from both PLC-2 and PLC-3 (two separate spans)
    let d4a = build_ads_span_bytes(
        trace_id,
        plc4_from_2_id,
        plc2_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "diamond.merge_from_a",
        "",
        &[],
        &[],
    );
    let d4b = build_ads_span_bytes(
        trace_id,
        plc4_from_3_id,
        plc3_id,
        SpanKind::Internal,
        SpanStatusCode::Ok,
        "diamond.merge_from_b",
        "",
        &[],
        &[],
    );

    let r1 = AdsParser::parse_all(&d1).unwrap();
    let r2 = AdsParser::parse_all(&d2).unwrap();
    let r3 = AdsParser::parse_all(&d3).unwrap();
    let r4a = AdsParser::parse_all(&d4a).unwrap();
    let r4b = AdsParser::parse_all(&d4b).unwrap();

    let s1 = ads_span_to_span_entry(&r1.spans[0], "10.0.0.1", "plc-1", "10.0.0.1.1.1", 851);
    let s2 = ads_span_to_span_entry(&r2.spans[0], "10.0.0.2", "plc-2", "10.0.0.2.1.1", 851);
    let s3 = ads_span_to_span_entry(&r3.spans[0], "10.0.0.3", "plc-3", "10.0.0.3.1.1", 851);
    let s4a = ads_span_to_span_entry(&r4a.spans[0], "10.0.0.4", "plc-4", "10.0.0.4.1.1", 851);
    let s4b = ads_span_to_span_entry(&r4b.spans[0], "10.0.0.4", "plc-4", "10.0.0.4.1.1", 851);

    // All in same trace
    for s in [&s1, &s2, &s3, &s4a, &s4b] {
        assert_eq!(s.trace_id, trace_id);
    }

    // Diamond structure
    assert!(!s1.has_parent()); // root
    assert_eq!(s2.parent_span_id, plc1_id); // branch A from source
    assert_eq!(s3.parent_span_id, plc1_id); // branch B from source
    assert_eq!(s4a.parent_span_id, plc2_id); // merge from branch A
    assert_eq!(s4b.parent_span_id, plc3_id); // merge from branch B

    // PLC-4 merges are from the same PLC
    assert_eq!(s4a.ams_net_id, s4b.ams_net_id);
    assert_eq!(s4a.source, s4b.source);
}

// ─── Backward compatibility ───────────────────────────────────────

#[test]
#[ignore = "legacy AdsSpanEntry one-shot format retired in Phase 1; rewrite pending"]
fn test_distributed_trace_backward_compat_logs_unaffected() {
    // Adding distributed trace support must not break existing log parsing.
    // Parse a buffer with only v2 log entries — no spans.
    let mut data = Vec::new();

    for i in 0..3 {
        let mut log_payload = Vec::new();
        log_payload.push(2); // Info
        let filetime: u64 = 116444736000000000 + 1_000_000_000;
        log_payload.extend_from_slice(&filetime.to_le_bytes());
        log_payload.extend_from_slice(&filetime.to_le_bytes());
        log_payload.push(1);
        log_payload.extend_from_slice(&100u32.to_le_bytes());
        log_payload.push(0); // arg_count
        log_payload.push(0); // context_count
        append_string(&mut log_payload, &format!("Log message {}", i));
        append_string(&mut log_payload, "test.logger");

        data.push(0x02);
        data.extend_from_slice(&(log_payload.len() as u16).to_le_bytes());
        data.extend_from_slice(&log_payload);
    }

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 3);
    assert_eq!(result.spans.len(), 0);
    assert_eq!(result.entries[0].message, "Log message 0");
    assert_eq!(result.entries[1].message, "Log message 1");
    assert_eq!(result.entries[2].message, "Log message 2");
}

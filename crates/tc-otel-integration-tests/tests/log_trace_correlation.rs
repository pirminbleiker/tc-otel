//! Integration tests for log-trace correlation (to-4ob.5)
//!
//! Tests the correlation of logs within trace context:
//! - LogEntry carries trace_id and span_id when emitted within a trace
//! - LogRecord includes trace context in OTEL export
//! - ADS message type 0x06 (traced log) correctly parses trace_id and span_id
//! - Backward compatibility: v2 logs (type 0x02) still work without trace context
//! - Mixed buffers: traced logs + regular logs + spans in same ADS Write

use tc_otel_ads::AdsParser;
use tc_otel_core::{LogEntry, LogLevel, LogRecord};

// ─── Helper: build ADS binary v2 log (type 0x02, no trace context) ─────

/// Build a minimal v2 log entry (type 0x02) without trace context.
fn build_v2_log_bytes(
    level: u8,
    task_index: u8,
    cycle_counter: u32,
    message: &str,
    logger: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();

    // level (1 byte)
    payload.push(level);
    // plc_timestamp (8 bytes FILETIME)
    let filetime: u64 = 116444736000000000 + 1_000_000_000; // ~100s after epoch
    payload.extend_from_slice(&filetime.to_le_bytes());
    // clock_timestamp (8 bytes FILETIME)
    payload.extend_from_slice(&filetime.to_le_bytes());
    // task_index (1 byte)
    payload.push(task_index);
    // cycle_counter (4 bytes LE)
    payload.extend_from_slice(&cycle_counter.to_le_bytes());
    // arg_count (1 byte)
    payload.push(0);
    // context_count (1 byte)
    payload.push(0);
    // message (string: 1-byte len + bytes)
    append_string(&mut payload, message);
    // logger (string)
    append_string(&mut payload, logger);

    // Wrap: [type=0x02] [entry_length: u16 LE] [payload]
    let mut data = Vec::new();
    data.push(0x02);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

/// Build a v2 traced log entry (type 0x06) with trace context.
fn build_traced_log_bytes(
    trace_id: [u8; 16],
    span_id: [u8; 8],
    level: u8,
    task_index: u8,
    cycle_counter: u32,
    message: &str,
    logger: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();

    // trace_id (16 bytes)
    payload.extend_from_slice(&trace_id);
    // span_id (8 bytes)
    payload.extend_from_slice(&span_id);
    // level (1 byte)
    payload.push(level);
    // plc_timestamp (8 bytes FILETIME)
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    payload.extend_from_slice(&filetime.to_le_bytes());
    // clock_timestamp (8 bytes FILETIME)
    payload.extend_from_slice(&filetime.to_le_bytes());
    // task_index (1 byte)
    payload.push(task_index);
    // cycle_counter (4 bytes LE)
    payload.extend_from_slice(&cycle_counter.to_le_bytes());
    // arg_count (1 byte)
    payload.push(0);
    // context_count (1 byte)
    payload.push(0);
    // message (string: 1-byte len + bytes)
    append_string(&mut payload, message);
    // logger (string)
    append_string(&mut payload, logger);

    // Wrap: [type=0x06] [entry_length: u16 LE] [payload]
    let mut data = Vec::new();
    data.push(0x06);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

/// Build a minimal ADS span (type 0x05) for mixed-buffer tests.
fn build_span_bytes(trace_id: [u8; 16], span_id: [u8; 8], name: &str) -> Vec<u8> {
    let mut payload = Vec::new();

    payload.extend_from_slice(&trace_id);
    payload.extend_from_slice(&span_id);
    payload.extend_from_slice(&[0u8; 8]); // parent_span_id (none)
    payload.push(0); // kind: Internal
    payload.push(0); // status: Unset
    let filetime: u64 = 116444736000000000 + 1_000_000_000;
    payload.extend_from_slice(&filetime.to_le_bytes()); // start_time
    let end_time: u64 = filetime + 100_000_000; // +10s
    payload.extend_from_slice(&end_time.to_le_bytes()); // end_time
    payload.push(1); // task_index
    payload.extend_from_slice(&500u32.to_le_bytes()); // cycle_counter
    payload.push(0); // attr_count
    payload.push(0); // event_count
    append_string(&mut payload, name); // span name
    append_string(&mut payload, ""); // status_message

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

// ─── Core model tests ─────────────────────────────────────────────────

#[test]
fn test_log_entry_default_no_trace_context() {
    let entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Motor started".to_string(),
        "motion.logger".to_string(),
        LogLevel::Info,
    );

    assert!(!entry.has_trace_context());
    assert_eq!(entry.trace_id, [0u8; 16]);
    assert_eq!(entry.span_id, [0u8; 8]);
    assert_eq!(entry.trace_id_hex(), "00000000000000000000000000000000");
    assert_eq!(entry.span_id_hex(), "0000000000000000");
}

#[test]
fn test_log_entry_with_trace_context() {
    let trace_id: [u8; 16] = [
        0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45,
        0x67, 0x89,
    ];
    let span_id: [u8; 8] = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];

    let mut entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Motor started".to_string(),
        "motion.logger".to_string(),
        LogLevel::Info,
    );
    entry.trace_id = trace_id;
    entry.span_id = span_id;

    assert!(entry.has_trace_context());
    assert_eq!(entry.trace_id_hex(), "abcdef0123456789abcdef0123456789");
    assert_eq!(entry.span_id_hex(), "fedcba9876543210");
}

// ─── LogRecord trace context export tests ─────────────────────────────

#[test]
fn test_log_record_without_trace_context() {
    let entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "msg".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    let record = LogRecord::from_log_entry(entry);

    // trace_id and span_id should be empty strings when no trace context
    assert!(record.trace_id.is_empty());
    assert!(record.span_id.is_empty());
}

#[test]
fn test_log_record_with_trace_context() {
    let trace_id: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10,
    ];
    let span_id: [u8; 8] = [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8];

    let mut entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        "Axis moving".to_string(),
        "motion.controller".to_string(),
        LogLevel::Info,
    );
    entry.trace_id = trace_id;
    entry.span_id = span_id;
    entry.project_name = "TestProject".to_string();

    let record = LogRecord::from_log_entry(entry);

    // Verify trace context is included in LogRecord
    assert_eq!(record.trace_id, "0102030405060708090a0b0c0d0e0f10");
    assert_eq!(record.span_id, "a1a2a3a4a5a6a7a8");

    // Other fields should still be correct
    assert_eq!(record.severity_number, 9); // Info
    assert_eq!(
        record.resource_attributes.get("service.name"),
        Some(&serde_json::json!("TestProject"))
    );
}

#[test]
fn test_log_record_trace_context_preserved_through_serialization() {
    let trace_id: [u8; 16] = [0xff; 16];
    let span_id: [u8; 8] = [0xaa; 8];

    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "msg".to_string(),
        "logger".to_string(),
        LogLevel::Warn,
    );
    entry.trace_id = trace_id;
    entry.span_id = span_id;

    let record = LogRecord::from_log_entry(entry);
    let json_str = serde_json::to_string(&record).unwrap();
    let deserialized: LogRecord = serde_json::from_str(&json_str).unwrap();

    assert_eq!(deserialized.trace_id, "ffffffffffffffffffffffffffffffff");
    assert_eq!(deserialized.span_id, "aaaaaaaaaaaaaaaa");
}

// ─── ADS parser tests for type 0x06 (traced log) ─────────────────────

#[test]
fn test_parse_traced_log_basic() {
    let trace_id: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ];
    let span_id: [u8; 8] = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];

    let data = build_traced_log_bytes(
        trace_id,
        span_id,
        2, // Info
        1, // task_index
        1000,
        "Motor started",
        "motion.logger",
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert!(result.spans.is_empty());

    let entry = &result.entries[0];
    assert_eq!(entry.trace_id, trace_id);
    assert_eq!(entry.span_id, span_id);
    assert_eq!(entry.message, "Motor started");
    assert_eq!(entry.logger, "motion.logger");
    assert_eq!(entry.level, LogLevel::Info);
    assert_eq!(entry.task_index, 1);
    assert_eq!(entry.task_cycle_counter, 1000);
}

#[test]
fn test_parse_traced_log_zero_trace_context_is_valid() {
    // Traced log type with all-zero trace/span IDs (unusual but valid)
    let data = build_traced_log_bytes(
        [0u8; 16],
        [0u8; 8],
        0, // Trace level
        0,
        0,
        "Debug msg",
        "debug.logger",
    );

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);

    let entry = &result.entries[0];
    assert_eq!(entry.trace_id, [0u8; 16]);
    assert_eq!(entry.span_id, [0u8; 8]);
    assert_eq!(entry.message, "Debug msg");
}

// ─── Backward compatibility tests ─────────────────────────────────────

#[test]
fn test_v2_log_has_no_trace_context() {
    let data = build_v2_log_bytes(2, 1, 500, "Regular log", "app.logger");

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);

    let entry = &result.entries[0];
    assert_eq!(entry.trace_id, [0u8; 16]);
    assert_eq!(entry.span_id, [0u8; 8]);
    assert_eq!(entry.message, "Regular log");
}

#[test]
fn test_v2_log_record_export_no_trace_context() {
    let data = build_v2_log_bytes(2, 1, 500, "No trace", "logger");

    let result = AdsParser::parse_all(&data).unwrap();
    let ads_entry = &result.entries[0];

    // Convert AdsLogEntry → LogEntry → LogRecord
    let mut log_entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-01".to_string(),
        ads_entry.message.clone(),
        ads_entry.logger.clone(),
        ads_entry.level,
    );
    log_entry.trace_id = ads_entry.trace_id;
    log_entry.span_id = ads_entry.span_id;

    let record = LogRecord::from_log_entry(log_entry);
    assert!(record.trace_id.is_empty());
    assert!(record.span_id.is_empty());
}

// ─── Mixed buffer tests (traced + regular + spans) ────────────────────

#[test]
fn test_mixed_buffer_traced_and_regular_logs() {
    let trace_id: [u8; 16] = [0xaa; 16];
    let span_id: [u8; 8] = [0xbb; 8];

    // Build a buffer with: regular v2 log + traced log + regular v2 log
    let mut data = Vec::new();
    data.extend_from_slice(&build_v2_log_bytes(2, 1, 100, "Before trace", "logger"));
    data.extend_from_slice(&build_traced_log_bytes(
        trace_id,
        span_id,
        2,
        1,
        101,
        "Within trace",
        "logger",
    ));
    data.extend_from_slice(&build_v2_log_bytes(2, 1, 102, "After trace", "logger"));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 3);

    // First: regular v2 log, no trace context
    assert_eq!(result.entries[0].message, "Before trace");
    assert_eq!(result.entries[0].trace_id, [0u8; 16]);
    assert_eq!(result.entries[0].span_id, [0u8; 8]);

    // Second: traced log, has trace context
    assert_eq!(result.entries[1].message, "Within trace");
    assert_eq!(result.entries[1].trace_id, trace_id);
    assert_eq!(result.entries[1].span_id, span_id);

    // Third: regular v2 log, no trace context
    assert_eq!(result.entries[2].message, "After trace");
    assert_eq!(result.entries[2].trace_id, [0u8; 16]);
    assert_eq!(result.entries[2].span_id, [0u8; 8]);
}

#[test]
fn test_mixed_buffer_traced_logs_and_spans() {
    let trace_id: [u8; 16] = [0xcc; 16];
    let span_id: [u8; 8] = [0xdd; 8];
    let log_span_id: [u8; 8] = [0xee; 8];

    // Build a buffer with: traced log + span (sharing same trace_id)
    let mut data = Vec::new();
    data.extend_from_slice(&build_traced_log_bytes(
        trace_id,
        log_span_id,
        3, // Warn
        2,
        200,
        "Warning within span",
        "motion.logger",
    ));
    data.extend_from_slice(&build_span_bytes(trace_id, span_id, "axis.move"));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.spans.len(), 1);

    // Log should have trace context
    let log = &result.entries[0];
    assert_eq!(log.trace_id, trace_id);
    assert_eq!(log.span_id, log_span_id);
    assert_eq!(log.message, "Warning within span");
    assert_eq!(log.level, LogLevel::Warn);

    // Span should be parsed correctly with same trace_id
    let span = &result.spans[0];
    assert_eq!(span.trace_id, trace_id);
    assert_eq!(span.span_id, span_id);
    assert_eq!(span.name, "axis.move");
}

// ─── End-to-end: ADS → LogEntry → LogRecord correlation ──────────────

#[test]
fn test_e2e_traced_log_to_otel_record() {
    let trace_id: [u8; 16] = [
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
        0x47, 0x36,
    ];
    let span_id: [u8; 8] = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];

    // Step 1: Parse traced log from ADS binary
    let data = build_traced_log_bytes(
        trace_id,
        span_id,
        4, // Error level
        3, // task_index
        5000,
        "Axis fault detected",
        "motion.safety",
    );
    let result = AdsParser::parse_all(&data).unwrap();
    let ads_entry = &result.entries[0];

    // Step 2: Convert to LogEntry (simulating ams_server flow)
    let mut log_entry = LogEntry::new(
        "192.168.1.100".to_string(),
        "plc-hub".to_string(),
        ads_entry.message.clone(),
        ads_entry.logger.clone(),
        ads_entry.level,
    );
    log_entry.plc_timestamp = ads_entry.plc_timestamp;
    log_entry.task_index = ads_entry.task_index;
    log_entry.task_cycle_counter = ads_entry.task_cycle_counter;
    log_entry.project_name = "MotionControl".to_string();
    log_entry.app_name = "SafetyApp".to_string();
    log_entry.trace_id = ads_entry.trace_id;
    log_entry.span_id = ads_entry.span_id;

    assert!(log_entry.has_trace_context());

    // Step 3: Convert to OTEL LogRecord
    let record = LogRecord::from_log_entry(log_entry);

    // Verify full correlation
    assert_eq!(record.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(record.span_id, "00f067aa0ba902b7");
    assert_eq!(record.severity_number, 17); // Error
    assert_eq!(record.severity_text, "ERROR");
    assert_eq!(
        record.body,
        serde_json::json!("Axis fault detected")
    );
    assert_eq!(
        record.resource_attributes.get("service.name"),
        Some(&serde_json::json!("MotionControl"))
    );
}

#[test]
fn test_e2e_regular_log_no_correlation() {
    // Step 1: Parse regular v2 log (no trace context)
    let data = build_v2_log_bytes(2, 1, 1000, "Heartbeat", "system.health");
    let result = AdsParser::parse_all(&data).unwrap();
    let ads_entry = &result.entries[0];

    // Step 2: Convert to LogEntry
    let mut log_entry = LogEntry::new(
        "192.168.1.100".to_string(),
        "plc-01".to_string(),
        ads_entry.message.clone(),
        ads_entry.logger.clone(),
        ads_entry.level,
    );
    log_entry.trace_id = ads_entry.trace_id;
    log_entry.span_id = ads_entry.span_id;

    assert!(!log_entry.has_trace_context());

    // Step 3: Convert to OTEL LogRecord
    let record = LogRecord::from_log_entry(log_entry);

    // No trace context should be present
    assert!(record.trace_id.is_empty());
    assert!(record.span_id.is_empty());
}

// ─── Multiple traced logs sharing same trace ID ───────────────────────

#[test]
fn test_multiple_logs_same_trace() {
    let trace_id: [u8; 16] = [0x11; 16];
    let span_id_1: [u8; 8] = [0x01; 8];
    let span_id_2: [u8; 8] = [0x02; 8];

    let mut data = Vec::new();
    data.extend_from_slice(&build_traced_log_bytes(
        trace_id, span_id_1, 2, 1, 100, "Step 1 started", "recipe.logger",
    ));
    data.extend_from_slice(&build_traced_log_bytes(
        trace_id, span_id_2, 2, 1, 101, "Step 2 started", "recipe.logger",
    ));

    let result = AdsParser::parse_all(&data).unwrap();
    assert_eq!(result.entries.len(), 2);

    // Both logs share the same trace_id
    assert_eq!(result.entries[0].trace_id, trace_id);
    assert_eq!(result.entries[1].trace_id, trace_id);

    // But have different span_ids
    assert_eq!(result.entries[0].span_id, span_id_1);
    assert_eq!(result.entries[1].span_id, span_id_2);

    assert_eq!(result.entries[0].message, "Step 1 started");
    assert_eq!(result.entries[1].message, "Step 2 started");
}

// ─── Trace context hex formatting edge cases ──────────────────────────

#[test]
fn test_trace_id_hex_all_values() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "msg".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    // Test with sequential bytes to verify hex encoding
    entry.trace_id = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    entry.span_id = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];

    assert_eq!(entry.trace_id_hex(), "000102030405060708090a0b0c0d0e0f");
    assert_eq!(entry.span_id_hex(), "1020304050607080");
    assert!(entry.has_trace_context()); // Non-zero trace_id
}

#[test]
fn test_has_trace_context_only_checks_trace_id() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "msg".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    // Zero trace_id but non-zero span_id: no trace context
    entry.trace_id = [0u8; 16];
    entry.span_id = [0xff; 8];
    assert!(!entry.has_trace_context());

    // Non-zero trace_id: has trace context
    entry.trace_id = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    assert!(entry.has_trace_context());
}

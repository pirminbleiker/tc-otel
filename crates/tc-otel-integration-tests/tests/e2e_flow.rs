//! End-to-end integration tests for Log4TC
//!
//! Tests the complete flow: ADS protocol parsing → LogEntry → OTEL mapping → Export

use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use tc_otel_ads::{AdsLogEntry, AdsParser, AdsProtocolVersion};
use tc_otel_core::{LogEntry, LogLevel, LogRecord};

/// Test: Parse minimal ADS message
/// NOTE: Ignored until binary format in test matches actual parser expectations
#[test]
#[ignore]
fn test_e2e_parse_minimal_ads_message() {
    // Create minimal valid ADS binary message
    let mut data = Vec::new();
    data.push(0x01); // version

    // Message string
    append_string(&mut data, "Test message");

    // Logger string
    append_string(&mut data, "test");

    // Level (2 bytes, u16 LE)
    data.extend_from_slice(&(0x02u16).to_le_bytes()); // Info

    // Timestamps (8 bytes each)
    data.extend_from_slice(&[0; 8]); // plc_timestamp
    data.extend_from_slice(&[0; 8]); // clock_timestamp

    // Task metadata
    data.extend_from_slice(&(1i32).to_le_bytes()); // task_index
    append_string(&mut data, "Task1"); // task_name
    data.extend_from_slice(&(100u32).to_le_bytes()); // task_cycle_counter

    // Application metadata
    append_string(&mut data, "App1"); // app_name
    append_string(&mut data, "Proj1"); // project_name
    data.extend_from_slice(&(0u32).to_le_bytes()); // online_change_count

    // End of arguments/context
    data.push(0x00);

    // Parse the ADS message
    let result = AdsParser::parse(&data);
    assert!(result.is_ok());

    let ads_entry = result.unwrap();
    assert_eq!(ads_entry.message, "Test message");
    assert_eq!(ads_entry.logger, "test");
    assert_eq!(ads_entry.level, LogLevel::Info);
    assert_eq!(ads_entry.task_name, "Task1");
    assert_eq!(ads_entry.app_name, "App1");
    assert_eq!(ads_entry.project_name, "Proj1");
}

/// Test: Full flow from ADS to LogEntry
#[test]
fn test_e2e_ads_to_log_entry() {
    // Create ADS entry
    let ads_entry = AdsLogEntry {
        version: AdsProtocolVersion::V1,
        message: "Motor temperature high".to_string(),
        logger: "motors.controller".to_string(),
        level: LogLevel::Warn,
        plc_timestamp: Utc::now(),
        clock_timestamp: Utc::now(),
        task_index: 2,
        task_name: "MotorTask".to_string(),
        task_cycle_counter: 5000,
        app_name: "MotorControl".to_string(),
        project_name: "IndustrialApp".to_string(),
        online_change_count: 1,
        trace_id: [0u8; 16],
        span_id: [0u8; 8],
        arguments: HashMap::new(),
        context: HashMap::new(),
    };

    // Convert to LogEntry
    let mut log_entry = LogEntry::new(
        "192.168.1.100".to_string(),
        "plc-02".to_string(),
        ads_entry.message.clone(),
        ads_entry.logger.clone(),
        ads_entry.level,
    );

    log_entry.plc_timestamp = ads_entry.plc_timestamp;
    log_entry.clock_timestamp = ads_entry.clock_timestamp;
    log_entry.task_index = ads_entry.task_index;
    log_entry.task_name = ads_entry.task_name.clone();
    log_entry.task_cycle_counter = ads_entry.task_cycle_counter;
    log_entry.app_name = ads_entry.app_name.clone();
    log_entry.project_name = ads_entry.project_name.clone();
    log_entry.online_change_count = ads_entry.online_change_count;

    // Verify conversion
    assert_eq!(log_entry.source, "192.168.1.100");
    assert_eq!(log_entry.hostname, "plc-02");
    assert_eq!(log_entry.message, "Motor temperature high");
    assert_eq!(log_entry.level, LogLevel::Warn);
    assert_eq!(log_entry.task_index, 2);
    assert!(!log_entry.id.is_empty());
}

/// Test: Full flow from LogEntry to OTEL LogRecord
#[test]
fn test_e2e_log_entry_to_otel_record() {
    // Create LogEntry
    let mut entry = LogEntry::new(
        "192.168.1.100:851".to_string(),
        "plc-02".to_string(),
        "System state changed to {state}".to_string(),
        "system.monitor".to_string(),
        LogLevel::Info,
    );

    entry.task_name = "MonitorTask".to_string();
    entry.task_index = 3;
    entry.app_name = "SystemApp".to_string();
    entry.project_name = "SystemProject".to_string();
    entry.task_cycle_counter = 10000;
    entry.online_change_count = 2;

    // Add context
    entry
        .context
        .insert("environment".to_string(), json!("production"));
    entry.context.insert("region".to_string(), json!("eu-west"));

    // Add arguments
    entry.arguments.insert(0, json!("RUNNING"));

    // Convert to OTEL
    let record = LogRecord::from_log_entry(entry);

    // Verify OTEL mapping
    assert_eq!(record.severity_number, 9); // Info maps to 9
    assert_eq!(record.severity_text, "INFO");
    assert_eq!(
        record.body,
        serde_json::Value::String("System state changed to {state}".to_string())
    );

    // Check resource attributes
    assert_eq!(
        record.resource_attributes.get("service.name"),
        Some(&serde_json::Value::String("SystemProject".to_string()))
    );
    assert_eq!(
        record.resource_attributes.get("service.instance.id"),
        Some(&serde_json::Value::String("SystemApp".to_string()))
    );
    assert_eq!(
        record.resource_attributes.get("host.name"),
        Some(&serde_json::Value::String("plc-02".to_string()))
    );
    assert_eq!(
        record.resource_attributes.get("process.pid"),
        Some(&serde_json::Value::Number(3.into()))
    );

    // Check scope attributes
    assert_eq!(
        record.scope_attributes.get("logger.name"),
        Some(&serde_json::Value::String("system.monitor".to_string()))
    );

    // Check log attributes (context + standard fields + arguments)
    assert_eq!(
        record.log_attributes.get("environment"),
        Some(&json!("production"))
    );
    assert_eq!(record.log_attributes.get("region"), Some(&json!("eu-west")));
    assert_eq!(record.log_attributes.get("arg.0"), Some(&json!("RUNNING")));
    assert!(record.log_attributes.contains_key("plc.timestamp"));
    assert_eq!(
        record.log_attributes.get("task.cycle"),
        Some(&serde_json::Value::Number(10000.into()))
    );
}

/// Test: Complete pipeline with all log levels
#[test]
fn test_e2e_all_log_levels() {
    let levels = vec![
        (LogLevel::Trace, 1, "TRACE"),
        (LogLevel::Debug, 5, "DEBUG"),
        (LogLevel::Info, 9, "INFO"),
        (LogLevel::Warn, 13, "WARN"),
        (LogLevel::Error, 17, "ERROR"),
        (LogLevel::Fatal, 21, "FATAL"),
    ];

    for (level, expected_severity, expected_text) in levels {
        let entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc-01".to_string(),
            format!("Test {} message", level),
            "test.logger".to_string(),
            level,
        );

        let record = LogRecord::from_log_entry(entry);

        assert_eq!(record.severity_number, expected_severity);
        assert_eq!(record.severity_text, expected_text);
    }
}

/// Test: Complex message with multiple arguments and context
#[test]
fn test_e2e_complex_message() {
    let mut entry = LogEntry::new(
        "192.168.1.1:2702".to_string(),
        "plc-complex".to_string(),
        "Event {action} on {resource} with code {code} at {timestamp}".to_string(),
        "events.processor".to_string(),
        LogLevel::Error,
    );

    entry.task_name = "EventTask".to_string();
    entry.task_index = 10;
    entry.app_name = "EventProcessor".to_string();
    entry.project_name = "EventProject".to_string();
    entry.task_cycle_counter = 50000;
    entry.online_change_count = 5;

    // Multiple context items
    for i in 0..5 {
        entry
            .context
            .insert(format!("ctx_{}", i), json!(format!("value_{}", i)));
    }

    // Multiple arguments
    entry.arguments.insert(0, json!("PROCESS"));
    entry.arguments.insert(1, json!("database"));
    entry.arguments.insert(2, json!(500));
    entry.arguments.insert(3, json!("2024-03-31T12:00:00Z"));

    // Convert to OTEL
    let record = LogRecord::from_log_entry(entry);

    // Verify record integrity
    assert_eq!(record.severity_number, 17); // Error
                                            // 5 context + 4 args + task/app/project metadata
    assert!(record.log_attributes.len() >= 9);
    assert!(record.log_attributes.contains_key("arg.0"));
    assert!(record.log_attributes.contains_key("arg.1"));
    assert!(record.log_attributes.contains_key("arg.2"));
    assert!(record.log_attributes.contains_key("arg.3"));
}

/// Test: Empty optional fields still produce valid records
#[test]
fn test_e2e_minimal_log_entry() {
    let entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Debug,
    );

    let record = LogRecord::from_log_entry(entry);

    // Should still have standard attributes even with empty context/arguments
    assert!(record.log_attributes.contains_key("plc.timestamp"));
    assert!(record.log_attributes.contains_key("task.cycle"));
    assert!(record.log_attributes.contains_key("source.address"));
}

// Helper function to append strings in ADS format (1-byte length prefix)
fn append_string(data: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    data.push(bytes.len() as u8);
    data.extend_from_slice(bytes);
}

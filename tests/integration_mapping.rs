//! Integration tests for LogEntry to OTEL LogRecord mapping

use tc_otel_core::{LogEntry, LogLevel, LogRecord};
use std::collections::HashMap;

fn create_test_entry(
    message: &str,
    level: LogLevel,
) -> LogEntry {
    LogEntry::new(
        "192.168.1.100:851".to_string(),
        "PLC-01".to_string(),
        message.to_string(),
        "app.logger".to_string(),
        level,
    )
}

#[test]
fn test_map_complete_log_entry_flow() {
    let mut entry = create_test_entry("Motor started successfully", LogLevel::Info);
    entry.task_name = "MotorControl".to_string();
    entry.task_index = 2;
    entry.task_cycle_counter = 5000;
    entry.app_name = "HydraulicSystem".to_string();
    entry.project_name = "ProductionLine".to_string();

    let record = LogRecord::from_log_entry(entry);

    assert_eq!(record.severity_number, 9);
    assert_eq!(record.body, serde_json::Value::String("Motor started successfully".to_string()));
    assert!(record.resource_attributes.contains_key("service.name"));
    assert!(record.log_attributes.contains_key("plc.timestamp"));
}

#[test]
fn test_map_with_context_and_arguments() {
    let mut entry = create_test_entry("User {0} logged from {source}", LogLevel::Info);

    entry.arguments.insert(0, serde_json::json!("alice"));
    entry.context.insert("source".to_string(), serde_json::json!("192.168.1.50"));
    entry.context.insert("session_id".to_string(), serde_json::json!("sess_123"));

    let record = LogRecord::from_log_entry(entry);

    assert_eq!(record.log_attributes.get("arg.0").unwrap(), &serde_json::json!("alice"));
    assert_eq!(
        record.log_attributes.get("source").unwrap(),
        &serde_json::json!("192.168.1.50")
    );
    assert_eq!(
        record.log_attributes.get("session_id").unwrap(),
        &serde_json::json!("sess_123")
    );
}

#[test]
fn test_map_all_severity_levels() {
    let levels = vec![
        (LogLevel::Trace, 1, "TRACE"),
        (LogLevel::Debug, 5, "DEBUG"),
        (LogLevel::Info, 9, "INFO"),
        (LogLevel::Warn, 13, "WARN"),
        (LogLevel::Error, 17, "ERROR"),
        (LogLevel::Fatal, 21, "FATAL"),
    ];

    for (level, expected_num, expected_text) in levels {
        let entry = create_test_entry("test", level);
        let record = LogRecord::from_log_entry(entry);

        assert_eq!(record.severity_number, expected_num);
        assert_eq!(record.severity_text, expected_text);
    }
}

#[test]
fn test_map_preserves_message_integrity() {
    let messages = vec![
        "Simple message",
        "Message with special chars: !@#$%^&*()",
        "Message\nwith\nmultiple\nlines",
        "Message with unicode: 你好世界 🌍",
        "",
    ];

    for msg in messages {
        let entry = create_test_entry(msg, LogLevel::Info);
        let record = LogRecord::from_log_entry(entry);

        assert_eq!(
            record.body,
            serde_json::Value::String(msg.to_string()),
            "Message integrity lost for: {}",
            msg
        );
    }
}

#[test]
fn test_map_complex_context_structure() {
    let mut entry = create_test_entry("Operation completed", LogLevel::Info);

    let complex_context = serde_json::json!({
        "user": {
            "id": "user_123",
            "role": "admin",
            "permissions": ["read", "write", "delete"]
        },
        "request": {
            "id": "req_456",
            "method": "POST",
            "duration_ms": 123
        }
    });

    entry.context.insert("details".to_string(), complex_context);

    let record = LogRecord::from_log_entry(entry);
    assert!(record.log_attributes.contains_key("details"));
}

#[test]
fn test_map_task_metadata_preservation() {
    let mut entry = create_test_entry("Cycle complete", LogLevel::Info);

    entry.task_name = "CycleTask".to_string();
    entry.task_index = 5;
    entry.task_cycle_counter = 10000;
    entry.online_change_count = 3;

    let record = LogRecord::from_log_entry(entry);

    assert_eq!(
        record.resource_attributes.get("process.command_line").unwrap(),
        &serde_json::Value::String("CycleTask".to_string())
    );
    assert_eq!(
        record.log_attributes.get("task.cycle").unwrap(),
        &serde_json::json!(10000)
    );
    assert_eq!(
        record.log_attributes.get("online.changes").unwrap(),
        &serde_json::json!(3)
    );
}

#[test]
fn test_map_multiple_arguments() {
    let mut entry = create_test_entry(
        "Operation {0} on {1} with code {2}",
        LogLevel::Info,
    );

    entry.arguments.insert(0, serde_json::json!("DELETE"));
    entry.arguments.insert(1, serde_json::json!("item_456"));
    entry.arguments.insert(2, serde_json::json!(200));

    let record = LogRecord::from_log_entry(entry);

    assert_eq!(record.log_attributes.get("arg.0").unwrap(), &serde_json::json!("DELETE"));
    assert_eq!(record.log_attributes.get("arg.1").unwrap(), &serde_json::json!("item_456"));
    assert_eq!(record.log_attributes.get("arg.2").unwrap(), &serde_json::json!(200));
}

#[test]
fn test_map_timestamp_consistency() {
    let entry = create_test_entry("test", LogLevel::Info);
    let original_timestamp = entry.clock_timestamp;

    let record = LogRecord::from_log_entry(entry);

    assert_eq!(record.timestamp, original_timestamp);
}

#[test]
fn test_map_error_and_fatal_levels() {
    let error_entry = create_test_entry("An error occurred", LogLevel::Error);
    let error_record = LogRecord::from_log_entry(error_entry);
    assert_eq!(error_record.severity_number, 17);

    let fatal_entry = create_test_entry("System shutdown", LogLevel::Fatal);
    let fatal_record = LogRecord::from_log_entry(fatal_entry);
    assert_eq!(fatal_record.severity_number, 21);
}

#[test]
fn test_map_empty_optional_fields() {
    let entry = create_test_entry("message", LogLevel::Info);
    // No additional context, arguments, or metadata

    let record = LogRecord::from_log_entry(entry);

    // Should still have all standard attributes
    assert!(record.resource_attributes.contains_key("service.name"));
    assert!(record.resource_attributes.contains_key("host.name"));
    assert!(record.scope_attributes.contains_key("logger.name"));
    assert!(record.log_attributes.contains_key("plc.timestamp"));
}

#[test]
fn test_map_roundtrip_to_json() {
    let mut entry = create_test_entry("Test message", LogLevel::Info);
    entry.context.insert("key".to_string(), serde_json::json!("value"));

    let record = LogRecord::from_log_entry(entry);
    let json = serde_json::to_string(&record).unwrap();

    // Verify JSON is valid
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.is_object());
    assert!(parsed["body"].is_string());
    assert!(parsed["severity_number"].is_number());
}

//! Integration tests for error handling and edge cases

use serde_json::json;
use tc_otel_ads::AdsParser;
use tc_otel_core::{LogEntry, LogLevel};

/// Test: Parser handles truncated/invalid ADS message
#[test]
fn test_parser_invalid_message_handling() {
    // Empty data
    assert!(AdsParser::parse(&[]).is_err());

    // Invalid version
    let mut data = vec![0xFF]; // Invalid version
    append_string(&mut data, "message");
    assert!(AdsParser::parse(&data).is_err());

    // Incomplete message (truncated before logger)
    let data = vec![0x01, 0x05, 0x00]; // version + incomplete string header
    assert!(AdsParser::parse(&data).is_err());
}

/// Test: LogEntry handles edge case values
#[test]
fn test_log_entry_edge_cases() {
    // Empty strings
    let entry = LogEntry::new(
        "".to_string(),
        "".to_string(),
        "".to_string(),
        "".to_string(),
        LogLevel::Info,
    );
    assert_eq!(entry.source, "");
    assert_eq!(entry.hostname, "");
    assert_eq!(entry.message, "");
    assert_eq!(entry.logger, "");

    // Very long strings
    let long_str = "x".repeat(10000);
    let entry = LogEntry::new(
        long_str.clone(),
        long_str.clone(),
        long_str.clone(),
        long_str.clone(),
        LogLevel::Debug,
    );
    assert_eq!(entry.source.len(), 10000);
    assert_eq!(entry.message.len(), 10000);

    // Special characters
    let special = "Special: <>&\"'`\n\r\t";
    let entry = LogEntry::new(
        special.to_string(),
        special.to_string(),
        special.to_string(),
        special.to_string(),
        LogLevel::Warn,
    );
    assert_eq!(entry.source, special);
    assert_eq!(entry.hostname, special);
}

/// Test: LogRecord handles null/missing context
#[test]
fn test_log_record_null_context() {
    let mut entry = LogEntry::new(
        "192.168.1.1".to_string(),
        "plc".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Error,
    );

    // Add null value
    entry
        .context
        .insert("null_field".to_string(), serde_json::Value::Null);

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should preserve null
    assert_eq!(
        record.log_attributes.get("null_field"),
        Some(&serde_json::Value::Null)
    );
}

/// Test: Arguments with non-sequential indices
#[test]
fn test_arguments_with_gaps() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "Message {0} {5}".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    // Non-sequential argument indices
    entry.arguments.insert(0, json!("first"));
    entry.arguments.insert(5, json!("sixth"));

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should preserve both arguments
    assert_eq!(record.log_attributes.get("arg.0"), Some(&json!("first")));
    assert_eq!(record.log_attributes.get("arg.5"), Some(&json!("sixth")));
}

/// Test: Large number of context items
#[test]
fn test_large_context() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Debug,
    );

    // Add many context items
    for i in 0..1000 {
        entry
            .context
            .insert(format!("ctx_{:04}", i), json!(format!("value_{}", i)));
    }

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should have all context items plus standard attributes
    assert!(record.log_attributes.len() >= 1000);
    assert_eq!(
        record.log_attributes.get("ctx_0000"),
        Some(&json!("value_0"))
    );
    assert_eq!(
        record.log_attributes.get("ctx_0999"),
        Some(&json!("value_999"))
    );
}

/// Test: Complex nested JSON in context
#[test]
fn test_nested_json_context() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    // Add deeply nested structure
    let nested = json!({
        "level1": {
            "level2": {
                "level3": {
                    "level4": {
                        "value": "deep"
                    }
                }
            }
        }
    });

    entry.context.insert("nested".to_string(), nested.clone());

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should preserve nested structure
    assert_eq!(record.log_attributes.get("nested"), Some(&nested));
}

/// Test: Mixed data types in arguments
#[test]
fn test_mixed_type_arguments() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Warn,
    );

    entry.arguments.insert(0, json!(42));
    entry.arguments.insert(1, json!(3.15));
    entry.arguments.insert(2, json!(true));
    entry.arguments.insert(3, json!("string"));
    entry.arguments.insert(4, json!(null));
    entry.arguments.insert(5, json!([]));
    entry.arguments.insert(6, json!({}));

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // All types should be preserved
    assert_eq!(record.log_attributes.get("arg.0"), Some(&json!(42)));
    assert_eq!(record.log_attributes.get("arg.1"), Some(&json!(3.15)));
    assert_eq!(record.log_attributes.get("arg.2"), Some(&json!(true)));
    assert_eq!(record.log_attributes.get("arg.3"), Some(&json!("string")));
    assert_eq!(record.log_attributes.get("arg.4"), Some(&json!(null)));
    assert_eq!(record.log_attributes.get("arg.5"), Some(&json!([])));
    assert_eq!(record.log_attributes.get("arg.6"), Some(&json!({})));
}

/// Test: Timestamps with zero values
#[test]
fn test_zero_timestamp_handling() {
    let mut entry = LogEntry::new(
        "src".to_string(),
        "host".to_string(),
        "message".to_string(),
        "logger".to_string(),
        LogLevel::Info,
    );

    // Set timestamps to epoch (which might occur in edge cases)
    use chrono::DateTime;
    entry.plc_timestamp = DateTime::from(std::time::UNIX_EPOCH);
    entry.clock_timestamp = DateTime::from(std::time::UNIX_EPOCH);

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should still produce valid record
    assert!(record.log_attributes.contains_key("plc.timestamp"));
    assert!(!record
        .log_attributes
        .get("plc.timestamp")
        .unwrap()
        .is_null());
}

/// Test: Unicode characters in all fields
#[test]
fn test_unicode_characters() {
    let unicode_str = "测试 🎉 мир 🌍 עברית العربية";

    let mut entry = LogEntry::new(
        unicode_str.to_string(),
        unicode_str.to_string(),
        unicode_str.to_string(),
        unicode_str.to_string(),
        LogLevel::Info,
    );

    entry
        .context
        .insert("unicode".to_string(), json!(unicode_str));
    entry.arguments.insert(0, json!(unicode_str));

    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Should preserve Unicode throughout
    assert_eq!(
        record
            .resource_attributes
            .get("host.name")
            .unwrap()
            .as_str()
            .unwrap(),
        unicode_str
    );
    assert_eq!(
        record.log_attributes.get("unicode"),
        Some(&json!(unicode_str))
    );
}

// Helper function (1-byte length prefix)
fn append_string(data: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    data.push(bytes.len() as u8);
    data.extend_from_slice(bytes);
}

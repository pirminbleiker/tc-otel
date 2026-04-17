//! Module integration tests - verifying multiple crates work together

use serde_json::json;
use tc_otel_ads::AdsParser;
use tc_otel_core::{AppSettings, LogLevel};
use tc_otel_export::OtelMapping;

/// Test: Parser + Core + OTEL integration
#[test]
fn test_parser_core_otel_pipeline() {
    // Create a minimal ADS binary message
    let mut data = Vec::new();
    data.push(0x01); // version

    append_string(&mut data, "Integration test message");
    append_string(&mut data, "integration.test");
    data.extend_from_slice(&(0x02u16).to_le_bytes()); // Info level (2 bytes)

    // Timestamps (FILETIME: 100-nanosecond intervals since 1601-01-01)
    // Use a valid timestamp: 2024-01-01 00:00:00 UTC = 133477536000000000
    let valid_filetime: u64 = 133_477_536_000_000_000;
    data.extend_from_slice(&valid_filetime.to_le_bytes());
    data.extend_from_slice(&valid_filetime.to_le_bytes());

    // Task metadata
    data.extend_from_slice(&(1i32).to_le_bytes());
    append_string(&mut data, "IntegrationTask");
    data.extend_from_slice(&(1000u32).to_le_bytes());

    // App metadata
    append_string(&mut data, "IntegrationApp");
    append_string(&mut data, "IntegrationProject");
    data.extend_from_slice(&(0u32).to_le_bytes());

    // End
    data.push(0x00);

    // Step 1: Parse with tc-otel-ads
    let ads_entry = AdsParser::parse(&data).expect("Failed to parse ADS message");
    assert_eq!(ads_entry.message, "Integration test message");

    // Step 2: Convert to LogEntry (from tc-otel-core)
    let mut log_entry = tc_otel_core::LogEntry::new(
        "192.168.1.1".to_string(),
        "plc-integration".to_string(),
        ads_entry.message.clone(),
        ads_entry.logger.clone(),
        ads_entry.level,
    );
    log_entry.task_name = ads_entry.task_name;
    log_entry.app_name = ads_entry.app_name;
    log_entry.project_name = ads_entry.project_name;

    // Step 3: Convert to OTEL LogRecord (from tc-otel-export)
    let record = OtelMapping::log_entry_to_record(log_entry);

    // Verify the complete pipeline
    assert_eq!(record.severity_number, 9);
    assert_eq!(record.severity_text, "INFO");
    assert!(record.body.is_string());
}

/// Test: Configuration parsing with core types
#[test]
fn test_config_core_integration() {
    // Create a sample TOML configuration string
    let toml_config = r#"
[logging]
log_level = "debug"
format = "json"

[receiver]
host = "0.0.0.0"
http_port = 4318
grpc_port = 4317
max_body_size = 4194304
request_timeout_secs = 30

[service]
name = "TcOtelService"
display_name = "TC-OTel Logging Service"
worker_threads = 4
channel_capacity = 10000
shutdown_timeout_secs = 30

[[outputs]]
Type = "otel"
"#;

    // Parse configuration
    let config: AppSettings = toml::from_str(toml_config).expect("Failed to parse config");

    // Verify configuration is parsed correctly
    assert_eq!(config.logging.log_level, "debug");
    assert_eq!(config.receiver.host, "0.0.0.0");
    assert_eq!(config.receiver.http_port, 4318);
    assert_eq!(config.receiver.grpc_port, 4317);
    assert_eq!(config.service.name, "TcOtelService");
    assert_eq!(config.service.worker_threads, Some(4));
    assert_eq!(config.outputs.len(), 1);
}

/// Test: LogLevel enum compatibility across modules
#[test]
fn test_loglevel_across_modules() {
    // Use LogLevel from tc-otel-core in multiple contexts
    let levels = vec![
        LogLevel::Trace,
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
        LogLevel::Fatal,
    ];

    for level in levels {
        // Create entry with tc-otel-core
        let entry = tc_otel_core::LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            format!("Test {:?}", level),
            "test".to_string(),
            level,
        );

        // Use with tc-otel-export
        let record = OtelMapping::log_entry_to_record(entry);

        // Verify mapping is consistent
        assert_eq!(record.severity_number, level.to_otel_severity_number());
        assert_eq!(record.severity_text, level.to_otel_severity_text());
    }
}

/// Test: Complex JSON serialization round-trip
#[test]
fn test_serde_integration() {
    // Create a LogEntry with complex data
    let mut entry = tc_otel_core::LogEntry::new(
        "192.168.1.1:2702".to_string(),
        "plc-serde".to_string(),
        "Complex message {arg1} {arg2}".to_string(),
        "serde.test".to_string(),
        LogLevel::Info,
    );

    entry.task_name = "SerdeTask".to_string();
    entry.task_index = 42;
    entry.app_name = "SerdeApp".to_string();
    entry.project_name = "SerdeProject".to_string();

    // Add complex context
    entry.context.insert(
        "user".to_string(),
        json!({
            "id": "user123",
            "role": "admin",
            "permissions": ["read", "write", "execute"]
        }),
    );

    entry.arguments.insert(0, json!("value1"));
    entry.arguments.insert(1, json!({"nested": "object"}));

    // Serialize to JSON
    let json = serde_json::to_string(&entry).expect("Failed to serialize");

    // Deserialize back
    let deserialized: tc_otel_core::LogEntry =
        serde_json::from_str(&json).expect("Failed to deserialize");

    // Verify round-trip
    assert_eq!(deserialized.source, entry.source);
    assert_eq!(deserialized.message, entry.message);
    assert_eq!(deserialized.level, entry.level);
    assert_eq!(deserialized.context.len(), entry.context.len());
    assert_eq!(deserialized.arguments.len(), entry.arguments.len());
}

/// Test: Type conversions between modules
#[test]
fn test_type_conversions() {
    // Create entry in tc-otel-core
    let entry = tc_otel_core::LogEntry::new(
        "192.168.1.1".to_string(),
        "plc".to_string(),
        "Test".to_string(),
        "logger".to_string(),
        LogLevel::Warn,
    );

    // Convert to OTEL with tc-otel-export
    let record = tc_otel_core::LogRecord::from_log_entry(entry);

    // Verify types are correct
    assert!(!record.timestamp.to_rfc3339().is_empty());
    assert!(record.severity_text.is_ascii());
    assert!(record.body.is_string());
    assert!(!record.resource_attributes.is_empty());
    assert!(!record.scope_attributes.is_empty());
    assert!(!record.log_attributes.is_empty());
}

// Helper function (1-byte length prefix)
fn append_string(data: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    data.push(bytes.len() as u8);
    data.extend_from_slice(bytes);
}

//! Mapping utilities between Log4TC and OTEL formats

use tc_otel_core::{LogEntry, LogRecord};

/// Helper for mapping Log4TC types to OTEL types
pub struct OtelMapping;

impl OtelMapping {
    /// Convert a LogEntry to OTEL LogRecord
    pub fn log_entry_to_record(entry: LogEntry) -> LogRecord {
        LogRecord::from_log_entry(entry)
    }

    /// Convert OTEL LogRecord to JSON for HTTP export
    pub fn record_to_json(record: &LogRecord) -> serde_json::Result<String> {
        serde_json::to_string(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_otel_core::LogLevel;

    #[test]
    fn test_mapping_log_entry_to_record() {
        let entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc-01".to_string(),
            "Test message".to_string(),
            "test.logger".to_string(),
            LogLevel::Info,
        );

        let record = OtelMapping::log_entry_to_record(entry);
        assert_eq!(record.severity_number, 9); // OTEL severity number for Info
    }

    #[test]
    fn test_map_all_log_levels() {
        let levels = vec![
            (LogLevel::Trace, 1),
            (LogLevel::Debug, 5),
            (LogLevel::Info, 9),
            (LogLevel::Warn, 13),
            (LogLevel::Error, 17),
            (LogLevel::Fatal, 21),
        ];

        for (level, expected_severity) in levels {
            let entry = LogEntry::new(
                "src".to_string(),
                "host".to_string(),
                "msg".to_string(),
                "logger".to_string(),
                level,
            );

            let record = OtelMapping::log_entry_to_record(entry);
            assert_eq!(
                record.severity_number, expected_severity,
                "Level {:?} should map to {}",
                level, expected_severity
            );
        }
    }

    #[test]
    fn test_map_basic_fields() {
        let mut entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc-01".to_string(),
            "System started".to_string(),
            "app.startup".to_string(),
            LogLevel::Info,
        );
        entry.task_name = "MainTask".to_string();
        entry.task_index = 42;
        entry.app_name = "MyApp".to_string();
        entry.project_name = "MyProject".to_string();

        let record = OtelMapping::log_entry_to_record(entry);

        // Check body
        assert_eq!(
            record.body,
            serde_json::Value::String("System started".to_string())
        );

        // Check severity
        assert_eq!(record.severity_number, 9);

        // Check resource attributes
        assert_eq!(
            record.resource_attributes.get("service.name").unwrap(),
            &serde_json::Value::String("MyProject".to_string())
        );
        assert_eq!(
            record
                .resource_attributes
                .get("service.instance.id")
                .unwrap(),
            &serde_json::Value::String("MyApp".to_string())
        );
        assert_eq!(
            record.resource_attributes.get("host.name").unwrap(),
            &serde_json::Value::String("plc-01".to_string())
        );
        assert_eq!(
            record.resource_attributes.get("process.pid").unwrap(),
            &serde_json::Value::Number(42.into())
        );
        assert_eq!(
            record
                .resource_attributes
                .get("process.command_line")
                .unwrap(),
            &serde_json::Value::String("MainTask".to_string())
        );

        // Check scope attributes
        assert_eq!(
            record.scope_attributes.get("logger.name").unwrap(),
            &serde_json::Value::String("app.startup".to_string())
        );
    }

    #[test]
    fn test_map_context_variables_to_attributes() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        entry
            .context
            .insert("user_id".to_string(), serde_json::json!("user123"));
        entry
            .context
            .insert("request_id".to_string(), serde_json::json!("req-456"));
        entry
            .context
            .insert("error_code".to_string(), serde_json::json!(500));

        let record = OtelMapping::log_entry_to_record(entry);

        // Check that context variables are in log_attributes
        assert_eq!(
            record.log_attributes.get("user_id").unwrap(),
            &serde_json::json!("user123")
        );
        assert_eq!(
            record.log_attributes.get("request_id").unwrap(),
            &serde_json::json!("req-456")
        );
        assert_eq!(
            record.log_attributes.get("error_code").unwrap(),
            &serde_json::json!(500)
        );
    }

    #[test]
    fn test_map_plc_specific_attributes() {
        let mut entry = LogEntry::new(
            "192.168.1.1:851".to_string(),
            "plc".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Warn,
        );
        entry.task_cycle_counter = 1234;
        entry.online_change_count = 5;

        let record = OtelMapping::log_entry_to_record(entry);

        // Check PLC-specific attributes
        assert!(record.log_attributes.contains_key("plc.timestamp"));
        assert_eq!(
            record.log_attributes.get("task.cycle").unwrap(),
            &serde_json::json!(1234)
        );
        assert_eq!(
            record.log_attributes.get("online.changes").unwrap(),
            &serde_json::json!(5)
        );
        assert_eq!(
            record.log_attributes.get("source.address").unwrap(),
            &serde_json::Value::String("192.168.1.1:851".to_string())
        );
    }

    #[test]
    fn test_map_arguments_to_attributes() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "User {0} logged in from {1}".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        entry.arguments.insert(0, serde_json::json!("alice"));
        entry
            .arguments
            .insert(1, serde_json::json!("192.168.1.100"));

        let record = OtelMapping::log_entry_to_record(entry);

        assert_eq!(
            record.log_attributes.get("arg.0").unwrap(),
            &serde_json::json!("alice")
        );
        assert_eq!(
            record.log_attributes.get("arg.1").unwrap(),
            &serde_json::json!("192.168.1.100")
        );
    }

    #[test]
    fn test_map_empty_message() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let record = OtelMapping::log_entry_to_record(entry);
        assert_eq!(record.body, serde_json::Value::String("".to_string()));
    }

    #[test]
    fn test_map_empty_optional_fields() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );
        // No context or arguments set

        let record = OtelMapping::log_entry_to_record(entry);

        // Should still have the default attributes
        assert!(record.log_attributes.contains_key("plc.timestamp"));
        assert!(record.log_attributes.contains_key("task.cycle"));
        assert!(record.log_attributes.contains_key("source.address"));
    }

    #[test]
    fn test_map_timestamp_preserved() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let expected_ts = entry.plc_timestamp;
        let record = OtelMapping::log_entry_to_record(entry);

        // The LogRecord timestamp should match the entry's plc_timestamp
        assert_eq!(record.timestamp, expected_ts);

        // The plc_timestamp should be in attributes as string
        let plc_ts_attr = record.log_attributes.get("plc.timestamp").unwrap();
        assert!(plc_ts_attr.is_string());
    }

    #[test]
    fn test_record_to_json() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "Test message".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let record = OtelMapping::log_entry_to_record(entry);
        let json_result = OtelMapping::record_to_json(&record);

        assert!(json_result.is_ok());
        let json_str = json_result.unwrap();
        assert!(json_str.contains("\"body\":\"Test message\""));
        assert!(json_str.contains("\"severity_number\":9"));
    }

    #[test]
    fn test_map_special_characters_in_message() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "Message with special chars: <tag> \"quoted\" 'apostrophe' & é ñ".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let record = OtelMapping::log_entry_to_record(entry);
        assert!(record.body.is_string());
        assert!(record.body.as_str().unwrap().contains("special chars"));
    }

    #[test]
    fn test_map_with_null_values_in_context() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        entry
            .context
            .insert("nullable_field".to_string(), serde_json::Value::Null);

        let record = OtelMapping::log_entry_to_record(entry);

        assert_eq!(
            record.log_attributes.get("nullable_field").unwrap(),
            &serde_json::Value::Null
        );
    }

    #[test]
    fn test_map_complex_context_objects() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let nested_obj = serde_json::json!({
            "user": {
                "id": "user123",
                "role": "admin"
            },
            "tags": ["important", "security"]
        });

        entry
            .context
            .insert("request_context".to_string(), nested_obj);

        let record = OtelMapping::log_entry_to_record(entry);

        assert!(record.log_attributes.contains_key("request_context"));
        let ctx = record.log_attributes.get("request_context").unwrap();
        assert!(ctx.is_object());
    }

    #[test]
    fn test_severity_text_format() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Error,
        );

        let record = OtelMapping::log_entry_to_record(entry);

        // severity_text should contain the log level name
        assert!(record.severity_text.contains("ERROR"));
    }
}

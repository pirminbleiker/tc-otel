//! Performance benchmarking utilities and fixtures for TC-OTel

use tc_otel_core::{LogEntry, LogLevel};

/// Create test log entries with various complexity levels
pub struct LogEntryFixtures;

impl LogEntryFixtures {
    /// Simple message with no arguments or context
    pub fn simple_message() -> LogEntry {
        LogEntry::new(
            "192.168.1.1:2702".to_string(),
            "plc-01".to_string(),
            "Simple log message".to_string(),
            "app.logger".to_string(),
            LogLevel::Info,
        )
    }

    /// Typical message with moderate arguments and context
    pub fn typical_message() -> LogEntry {
        let mut entry = LogEntry::new(
            "192.168.1.1:2702".to_string(),
            "plc-01".to_string(),
            "Motor {motorId} temperature is {temperature}°C at {timestamp}".to_string(),
            "motors.controller".to_string(),
            LogLevel::Warn,
        );

        entry.task_name = "MotorTask".to_string();
        entry.app_name = "MyApplication".to_string();
        entry.project_name = "PlcProject".to_string();
        entry.task_cycle_counter = 12345;

        // Arguments
        entry.arguments.insert(0, serde_json::json!("MOT-001"));
        entry.arguments.insert(1, serde_json::json!(85.5));
        entry
            .arguments
            .insert(2, serde_json::json!("2024-03-31T12:00:00Z"));

        // Context
        entry
            .context
            .insert("request_id".to_string(), serde_json::json!("req-12345"));
        entry
            .context
            .insert("trace_id".to_string(), serde_json::json!("trace-98765"));
        entry
            .context
            .insert("user".to_string(), serde_json::json!("admin"));

        entry
    }

    /// Complex message with many arguments and context properties
    pub fn complex_message() -> LogEntry {
        let mut entry = LogEntry::new(
            "192.168.1.1:2702".to_string(),
            "plc-01".to_string(),
            "Complex event: {action} on {resource} with params {param1}={value1}, {param2}={value2}, {param3}={value3}".to_string(),
            "system.events".to_string(),
            LogLevel::Error,
        );

        entry.task_name = "EventProcessorTask".to_string();
        entry.app_name = "ComplexApp".to_string();
        entry.project_name = "LargeProject".to_string();
        entry.task_cycle_counter = 98765;
        entry.task_index = 5;
        entry.online_change_count = 3;

        // Many arguments
        for i in 0..10 {
            let value = match i {
                0 => serde_json::json!("UPDATE"),
                1 => serde_json::json!("database"),
                2..=4 => serde_json::json!(format!("param{}", i)),
                5..=7 => serde_json::json!(format!("value{}", i)),
                _ => serde_json::json!(i),
            };
            entry.arguments.insert(i, value);
        }

        // Many context properties
        let context_keys = vec![
            "request_id",
            "trace_id",
            "span_id",
            "user_id",
            "session_id",
            "client_ip",
            "server_id",
            "region",
            "environment",
            "version",
            "request_time",
            "processing_stage",
            "retry_count",
            "circuit_breaker_state",
        ];

        for (i, key) in context_keys.iter().enumerate() {
            entry.context.insert(
                key.to_string(),
                serde_json::json!(format!("{}-value-{}", key, i)),
            );
        }

        entry
    }

    /// Create a message with specific argument and context counts
    pub fn with_counts(args: usize, context_items: usize) -> LogEntry {
        let mut entry = LogEntry::new(
            "192.168.1.1:2702".to_string(),
            "plc-01".to_string(),
            format!(
                "Message with {} args and {} context items",
                args, context_items
            ),
            "custom.logger".to_string(),
            LogLevel::Debug,
        );

        entry.task_name = "CustomTask".to_string();
        entry.app_name = "App".to_string();
        entry.project_name = "Project".to_string();

        for i in 0..args {
            entry
                .arguments
                .insert(i, serde_json::json!(format!("arg_{}", i)));
        }

        for i in 0..context_items {
            entry.context.insert(
                format!("ctx_{}", i),
                serde_json::json!(format!("value_{}", i)),
            );
        }

        entry
    }
}

/// ADS binary protocol test fixtures
pub struct AdsFixtures;

impl AdsFixtures {
    /// Create minimal valid ADS binary message (simple case)
    pub fn minimal_ads_message() -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0x01); // version

        // Message string
        Self::append_string(&mut data, "Test message");

        // Logger string
        Self::append_string(&mut data, "test");

        // Level
        data.push(0x02); // Info

        // Timestamps (8 bytes each)
        data.extend_from_slice(&[0; 8]); // plc_timestamp
        data.extend_from_slice(&[0; 8]); // clock_timestamp

        // Task metadata
        data.extend_from_slice(&(1i32).to_le_bytes()); // task_index
        Self::append_string(&mut data, "Task1"); // task_name
        data.extend_from_slice(&(100u32).to_le_bytes()); // task_cycle_counter

        // Application metadata
        Self::append_string(&mut data, "App1"); // app_name
        Self::append_string(&mut data, "Proj1"); // project_name
        data.extend_from_slice(&(0u32).to_le_bytes()); // online_change_count

        // End of arguments/context
        data.push(0x00);

        data
    }

    /// Create typical ADS binary message with arguments
    pub fn typical_ads_message() -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0x01); // version

        Self::append_string(&mut data, "Motor {m} temp is {t}");
        Self::append_string(&mut data, "motors");
        data.push(0x03); // Warning

        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&[0; 8]);

        data.extend_from_slice(&(2i32).to_le_bytes());
        Self::append_string(&mut data, "MotorTask");
        data.extend_from_slice(&(5000u32).to_le_bytes());

        Self::append_string(&mut data, "MotorControl");
        Self::append_string(&mut data, "IndustrialApp");
        data.extend_from_slice(&(1u32).to_le_bytes());

        // Argument 1: "MOT-001"
        data.push(0x01); // type = argument
        data.push(0x00); // index = 0
        data.push(0x03); // type = string
        Self::append_string(&mut data, "MOT-001");

        // Argument 2: 85.5 (float)
        data.push(0x01); // type = argument
        data.push(0x01); // index = 1
        data.push(0x02); // type = float
        data.extend_from_slice(&(85.5f64).to_le_bytes());

        // End
        data.push(0x00);

        data
    }

    fn append_string(data: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        data.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        data.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_fixture() {
        let entry = LogEntryFixtures::simple_message();
        assert_eq!(entry.message, "Simple log message");
        assert!(entry.arguments.is_empty());
        assert!(entry.context.is_empty());
    }

    #[test]
    fn test_typical_fixture() {
        let entry = LogEntryFixtures::typical_message();
        assert_eq!(entry.arguments.len(), 3);
        assert_eq!(entry.context.len(), 3);
    }

    #[test]
    fn test_ads_minimal_roundtrip() {
        let data = AdsFixtures::minimal_ads_message();
        assert!(!data.is_empty());
        assert_eq!(data[0], 0x01); // version
    }
}

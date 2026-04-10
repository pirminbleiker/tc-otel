//! Core data models for Log4TC

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ─── Span / Trace types ────────────────────────────────────────────

/// OpenTelemetry span kind
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum SpanKind {
    Internal = 0,
    Server = 1,
    Client = 2,
    Producer = 3,
    Consumer = 4,
}

impl SpanKind {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(SpanKind::Internal),
            1 => Some(SpanKind::Server),
            2 => Some(SpanKind::Client),
            3 => Some(SpanKind::Producer),
            4 => Some(SpanKind::Consumer),
            _ => None,
        }
    }

    pub fn as_u8(&self) -> u8 {
        *self as u8
    }

    /// Convert to OTEL SpanKind integer (matches opentelemetry-proto values)
    pub fn to_otel_kind(&self) -> i32 {
        match self {
            SpanKind::Internal => 1, // SPAN_KIND_INTERNAL
            SpanKind::Server => 2,   // SPAN_KIND_SERVER
            SpanKind::Client => 3,   // SPAN_KIND_CLIENT
            SpanKind::Producer => 4, // SPAN_KIND_PRODUCER
            SpanKind::Consumer => 5, // SPAN_KIND_CONSUMER
        }
    }
}

impl std::fmt::Display for SpanKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpanKind::Internal => write!(f, "Internal"),
            SpanKind::Server => write!(f, "Server"),
            SpanKind::Client => write!(f, "Client"),
            SpanKind::Producer => write!(f, "Producer"),
            SpanKind::Consumer => write!(f, "Consumer"),
        }
    }
}

/// OpenTelemetry span status code
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum SpanStatusCode {
    Unset = 0,
    Ok = 1,
    Error = 2,
}

impl SpanStatusCode {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(SpanStatusCode::Unset),
            1 => Some(SpanStatusCode::Ok),
            2 => Some(SpanStatusCode::Error),
            _ => None,
        }
    }

    pub fn as_u8(&self) -> u8 {
        *self as u8
    }

    /// Convert to OTEL StatusCode integer
    pub fn to_otel_status(&self) -> i32 {
        match self {
            SpanStatusCode::Unset => 0,
            SpanStatusCode::Ok => 1,
            SpanStatusCode::Error => 2,
        }
    }
}

impl std::fmt::Display for SpanStatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpanStatusCode::Unset => write!(f, "Unset"),
            SpanStatusCode::Ok => write!(f, "Ok"),
            SpanStatusCode::Error => write!(f, "Error"),
        }
    }
}

/// An event within a span (e.g. "axis reached target position")
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEvent {
    pub timestamp: DateTime<Utc>,
    pub name: String,
    pub attributes: HashMap<String, serde_json::Value>,
}

/// A completed span entry from the ADS protocol or internal creation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEntry {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: [u8; 8],

    pub name: String,
    pub kind: SpanKind,
    pub status_code: SpanStatusCode,
    pub status_message: String,

    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,

    // Source identification (mirrored from LogEntry pattern)
    pub source: String,
    pub hostname: String,
    pub ams_net_id: String,
    pub ams_source_port: u16,

    // Task metadata
    pub task_index: i32,
    pub task_name: String,
    pub task_cycle_counter: u32,

    // Application metadata
    pub app_name: String,
    pub project_name: String,

    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<SpanEvent>,
}

impl SpanEntry {
    /// Create a new span with the given trace/span IDs and name
    pub fn new(trace_id: [u8; 16], span_id: [u8; 8], name: String) -> Self {
        Self {
            trace_id,
            span_id,
            parent_span_id: [0u8; 8],
            name,
            kind: SpanKind::Internal,
            status_code: SpanStatusCode::Unset,
            status_message: String::new(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            source: String::new(),
            hostname: String::new(),
            ams_net_id: String::new(),
            ams_source_port: 0,
            task_index: 0,
            task_name: String::new(),
            task_cycle_counter: 0,
            app_name: String::new(),
            project_name: String::new(),
            attributes: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Format trace_id as lowercase hex string (32 chars)
    pub fn trace_id_hex(&self) -> String {
        self.trace_id.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Format span_id as lowercase hex string (16 chars)
    pub fn span_id_hex(&self) -> String {
        self.span_id.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Format parent_span_id as lowercase hex string (16 chars)
    pub fn parent_span_id_hex(&self) -> String {
        self.parent_span_id
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Check if this span has a parent
    pub fn has_parent(&self) -> bool {
        self.parent_span_id != [0u8; 8]
    }
}

/// Log severity level, mapped from ADS binary protocol
/// Values match the .NET Log4Tc.Model.LogLevel enumeration
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Fatal = 5,
}

impl LogLevel {
    pub fn as_u8(&self) -> u8 {
        *self as u8
    }

    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(LogLevel::Trace),
            1 => Some(LogLevel::Debug),
            2 => Some(LogLevel::Info),
            3 => Some(LogLevel::Warn),
            4 => Some(LogLevel::Error),
            5 => Some(LogLevel::Fatal),
            _ => None,
        }
    }

    /// Convert LogLevel to OpenTelemetry SeverityNumber
    /// Mapping: Trace->1, Debug->5, Info->9, Warn->13, Error->17, Fatal->21
    pub fn to_otel_severity_number(&self) -> i32 {
        match self {
            LogLevel::Trace => 1,
            LogLevel::Debug => 5,
            LogLevel::Info => 9,
            LogLevel::Warn => 13,
            LogLevel::Error => 17,
            LogLevel::Fatal => 21,
        }
    }

    /// Convert LogLevel to OpenTelemetry SeverityText
    pub fn to_otel_severity_text(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Fatal => "FATAL",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Trace => write!(f, "Trace"),
            LogLevel::Debug => write!(f, "Debug"),
            LogLevel::Info => write!(f, "Info"),
            LogLevel::Warn => write!(f, "Warn"),
            LogLevel::Error => write!(f, "Error"),
            LogLevel::Fatal => write!(f, "Fatal"),
        }
    }
}

/// A log entry from the ADS protocol or OTEL receiver
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,

    // Source identification
    pub source: String,       // AMS address or source identifier
    pub hostname: String,     // PLC hostname
    pub ams_net_id: String,   // AMS Net ID from AMS header
    pub ams_source_port: u16, // AMS Source Port from AMS header

    // Message content
    pub message: String, // Template string or formatted message
    pub logger: String,  // Logger name

    pub level: LogLevel, // Severity level

    // Timestamps
    pub plc_timestamp: DateTime<Utc>,   // PLC-side time
    pub clock_timestamp: DateTime<Utc>, // System clock time

    // Task metadata
    pub task_index: i32,         // Task ID
    pub task_name: String,       // Task name
    pub task_cycle_counter: u32, // Cycle count

    // Application metadata
    pub app_name: String,         // Application name
    pub project_name: String,     // Project name
    pub online_change_count: u32, // Online changes

    // Trace context (for log-trace correlation)
    pub trace_id: [u8; 16], // W3C trace ID (all zeros = no trace context)
    pub span_id: [u8; 8],   // W3C span ID (all zeros = no span context)

    // Variable data
    pub arguments: HashMap<usize, serde_json::Value>, // Positional arguments
    pub context: HashMap<String, serde_json::Value>,  // Context properties
}

impl LogEntry {
    pub fn new(
        source: String,
        hostname: String,
        message: String,
        logger: String,
        level: LogLevel,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            source,
            hostname,
            ams_net_id: String::new(),
            ams_source_port: 0,
            message,
            logger,
            level,
            plc_timestamp: Utc::now(),
            clock_timestamp: Utc::now(),
            task_index: 0,
            task_name: String::new(),
            task_cycle_counter: 0,
            app_name: String::new(),
            project_name: String::new(),
            online_change_count: 0,
            trace_id: [0u8; 16],
            span_id: [0u8; 8],
            arguments: HashMap::new(),
            context: HashMap::new(),
        }
    }

    /// Check if this log entry has trace context (non-zero trace_id)
    pub fn has_trace_context(&self) -> bool {
        self.trace_id != [0u8; 16]
    }

    /// Format trace_id as lowercase hex string (32 chars)
    pub fn trace_id_hex(&self) -> String {
        self.trace_id.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Format span_id as lowercase hex string (16 chars)
    pub fn span_id_hex(&self) -> String {
        self.span_id.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

/// OpenTelemetry LogRecord representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub body: serde_json::Value,
    pub severity_number: i32,
    pub severity_text: String,
    pub trace_id: String, // Hex-encoded trace ID (empty = no trace context)
    pub span_id: String,  // Hex-encoded span ID (empty = no span context)
    pub resource_attributes: HashMap<String, serde_json::Value>,
    pub scope_attributes: HashMap<String, serde_json::Value>,
    pub log_attributes: HashMap<String, serde_json::Value>,
}

impl LogRecord {
    pub fn from_log_entry(entry: LogEntry) -> Self {
        let severity_number = entry.level.to_otel_severity_number();
        let severity_text = entry.level.to_otel_severity_text().to_string();

        // Trace context: capture hex IDs before entry fields are moved
        let (trace_id, span_id) = if entry.has_trace_context() {
            (entry.trace_id_hex(), entry.span_id_hex())
        } else {
            (String::new(), String::new())
        };

        // Pre-allocate resource attributes with expected capacity
        let mut resource_attributes = HashMap::with_capacity(5);
        resource_attributes.insert(
            "service.name".to_string(),
            serde_json::Value::String(entry.project_name),
        );
        resource_attributes.insert(
            "service.instance.id".to_string(),
            serde_json::Value::String(entry.app_name),
        );
        resource_attributes.insert(
            "host.name".to_string(),
            serde_json::Value::String(entry.hostname),
        );
        resource_attributes.insert(
            "process.pid".to_string(),
            serde_json::Value::Number(entry.task_index.into()),
        );
        resource_attributes.insert(
            "process.command_line".to_string(),
            serde_json::Value::String(entry.task_name),
        );

        let mut scope_attributes = HashMap::with_capacity(1);
        scope_attributes.insert(
            "logger.name".to_string(),
            serde_json::Value::String(entry.logger),
        );

        // Pre-allocate log_attributes: context items + 4 standard keys + arguments
        let expected_capacity = entry.context.len() + entry.arguments.len() + 4;
        let mut log_attributes = HashMap::with_capacity(expected_capacity);

        // Merge context items without cloning the entire map
        log_attributes.extend(entry.context);

        // Add standard OTEL attributes
        log_attributes.insert(
            "plc.timestamp".to_string(),
            serde_json::Value::String(entry.plc_timestamp.to_rfc3339()),
        );
        log_attributes.insert(
            "task.cycle".to_string(),
            serde_json::Value::Number(entry.task_cycle_counter.into()),
        );
        log_attributes.insert(
            "online.changes".to_string(),
            serde_json::Value::Number(entry.online_change_count.into()),
        );
        log_attributes.insert(
            "source.address".to_string(),
            serde_json::Value::String(entry.source),
        );
        if !entry.ams_net_id.is_empty() {
            log_attributes.insert(
                "plc.ams_net_id".to_string(),
                serde_json::Value::String(entry.ams_net_id),
            );
        }
        if entry.ams_source_port > 0 {
            log_attributes.insert(
                "plc.ams_source_port".to_string(),
                serde_json::Value::Number(entry.ams_source_port.into()),
            );
        }

        // Merge in positional arguments with pre-formatted keys
        for (idx, val) in entry.arguments {
            log_attributes.insert(format!("arg.{}", idx), val);
        }

        Self {
            timestamp: entry.plc_timestamp,
            body: serde_json::Value::String(entry.message),
            severity_number,
            severity_text,
            trace_id,
            span_id,
            resource_attributes,
            scope_attributes,
            log_attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_conversion() {
        assert_eq!(LogLevel::from_u8(0), Some(LogLevel::Trace));
        assert_eq!(LogLevel::from_u8(2), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_u8(4), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_u8(255), None);
    }

    #[test]
    fn test_log_level_display() {
        assert_eq!(LogLevel::Trace.to_string(), "Trace");
        assert_eq!(LogLevel::Debug.to_string(), "Debug");
        assert_eq!(LogLevel::Info.to_string(), "Info");
        assert_eq!(LogLevel::Warn.to_string(), "Warn");
        assert_eq!(LogLevel::Error.to_string(), "Error");
        assert_eq!(LogLevel::Fatal.to_string(), "Fatal");
    }

    #[test]
    fn test_log_level_otel_severity() {
        assert_eq!(LogLevel::Trace.to_otel_severity_number(), 1);
        assert_eq!(LogLevel::Info.to_otel_severity_number(), 9);
        assert_eq!(LogLevel::Warn.to_otel_severity_number(), 13);
        assert_eq!(LogLevel::Fatal.to_otel_severity_number(), 21);

        assert_eq!(LogLevel::Trace.to_otel_severity_text(), "TRACE");
        assert_eq!(LogLevel::Fatal.to_otel_severity_text(), "FATAL");
    }

    #[test]
    fn test_log_entry_creation() {
        let entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc-01".to_string(),
            "Test message".to_string(),
            "test.logger".to_string(),
            LogLevel::Info,
        );

        assert_eq!(entry.source, "192.168.1.1");
        assert_eq!(entry.level, LogLevel::Info);
        assert!(!entry.id.is_empty());
    }

    #[test]
    fn test_log_record_from_entry() {
        let mut entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc-01".to_string(),
            "Test message".to_string(),
            "test.logger".to_string(),
            LogLevel::Warn,
        );
        entry.project_name = "TestProject".to_string();
        entry.app_name = "TestApp".to_string();

        let record = LogRecord::from_log_entry(entry);

        // Warn (3) maps to OTEL severity 13
        assert_eq!(record.severity_number, 13);
        assert_eq!(
            record.resource_attributes.get("service.name"),
            Some(&serde_json::Value::String("TestProject".to_string()))
        );
    }

    #[test]
    fn test_log_level_as_u8() {
        assert_eq!(LogLevel::Trace.as_u8(), 0);
        assert_eq!(LogLevel::Debug.as_u8(), 1);
        assert_eq!(LogLevel::Info.as_u8(), 2);
        assert_eq!(LogLevel::Warn.as_u8(), 3);
        assert_eq!(LogLevel::Error.as_u8(), 4);
        assert_eq!(LogLevel::Fatal.as_u8(), 5);
    }

    #[test]
    fn test_log_level_from_u8() {
        assert_eq!(LogLevel::from_u8(0), Some(LogLevel::Trace));
        assert_eq!(LogLevel::from_u8(1), Some(LogLevel::Debug));
        assert_eq!(LogLevel::from_u8(2), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_u8(3), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_u8(4), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_u8(5), Some(LogLevel::Fatal));
        assert_eq!(LogLevel::from_u8(255), None);
        assert_eq!(LogLevel::from_u8(100), None);
    }

    #[test]
    fn test_log_level_comparison() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Fatal);
        assert_eq!(LogLevel::Info, LogLevel::Info);
    }

    #[test]
    fn test_log_entry_with_metadata() {
        let mut entry = LogEntry::new(
            "192.168.1.1:851".to_string(),
            "plc-hub".to_string(),
            "Motor started".to_string(),
            "motion.logger".to_string(),
            LogLevel::Info,
        );

        entry.task_name = "MotorControl".to_string();
        entry.task_index = 2;
        entry.task_cycle_counter = 1000;
        entry.app_name = "HydraulicSystem".to_string();
        entry.project_name = "ProductionLine".to_string();
        entry.online_change_count = 0;

        assert_eq!(entry.task_name, "MotorControl");
        assert_eq!(entry.task_index, 2);
        assert_eq!(entry.task_cycle_counter, 1000);
        assert_eq!(entry.app_name, "HydraulicSystem");
        assert_eq!(entry.project_name, "ProductionLine");
    }

    #[test]
    fn test_log_entry_with_arguments() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "Error code: {0}".to_string(),
            "error.logger".to_string(),
            LogLevel::Error,
        );

        entry.arguments.insert(0, serde_json::json!(1234));
        entry.arguments.insert(1, serde_json::json!("timeout"));

        assert_eq!(entry.arguments.len(), 2);
        assert_eq!(entry.arguments[&0], serde_json::json!(1234));
        assert_eq!(entry.arguments[&1], serde_json::json!("timeout"));
    }

    #[test]
    fn test_log_entry_with_context() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Debug,
        );

        entry
            .context
            .insert("user_id".to_string(), serde_json::json!("user_123"));
        entry
            .context
            .insert("session_id".to_string(), serde_json::json!("sess_456"));
        entry
            .context
            .insert("request_count".to_string(), serde_json::json!(42));

        assert_eq!(entry.context.len(), 3);
        assert_eq!(entry.context["user_id"], serde_json::json!("user_123"));
    }

    #[test]
    fn test_log_entry_unique_ids() {
        let entry1 = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg1".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let entry2 = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg2".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        assert_ne!(entry1.id, entry2.id);
    }

    #[test]
    fn test_log_entry_timestamps() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        // Timestamps should be set to approximately now
        let now = chrono::Utc::now();
        let diff_plc = (now - entry.plc_timestamp).num_seconds().abs();
        let diff_clock = (now - entry.clock_timestamp).num_seconds().abs();

        assert!(diff_plc < 2);
        assert!(diff_clock < 2);
    }

    #[test]
    fn test_log_record_all_attributes() {
        let mut entry = LogEntry::new(
            "192.168.1.1".to_string(),
            "plc".to_string(),
            "Test".to_string(),
            "app.module".to_string(),
            LogLevel::Error,
        );

        entry.task_name = "Task1".to_string();
        entry.task_index = 10;
        entry.task_cycle_counter = 500;
        entry.app_name = "App1".to_string();
        entry.project_name = "Project1".to_string();
        entry.online_change_count = 3;
        entry
            .context
            .insert("key1".to_string(), serde_json::json!("value1"));
        entry.arguments.insert(0, serde_json::json!(123));

        let record = LogRecord::from_log_entry(entry);

        // Check all attribute categories are present
        assert_eq!(record.resource_attributes.len(), 5);
        assert_eq!(record.scope_attributes.len(), 1);
        assert!(record.log_attributes.len() >= 5); // context + 4 standard + args

        assert_eq!(record.severity_number, 17); // Error = 17
        assert_eq!(record.severity_text, "ERROR");
    }

    #[test]
    fn test_log_record_empty_optional_fields() {
        let entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        let record = LogRecord::from_log_entry(entry);

        // Should still have standard attributes
        assert!(record.resource_attributes.contains_key("service.name"));
        assert!(record.resource_attributes.contains_key("host.name"));
        assert!(record.log_attributes.contains_key("plc.timestamp"));
        assert!(record.log_attributes.contains_key("task.cycle"));
    }

    #[test]
    fn test_log_record_body_preservation() {
        let messages = vec![
            "Simple message",
            "Message with numbers 123",
            "Message with special chars: !@#$%",
            "Message with\nmultiple\nlines",
            "",
        ];

        for msg in messages {
            let entry = LogEntry::new(
                "src".to_string(),
                "host".to_string(),
                msg.to_string(),
                "logger".to_string(),
                LogLevel::Info,
            );

            let record = LogRecord::from_log_entry(entry);
            assert_eq!(record.body, serde_json::Value::String(msg.to_string()));
        }
    }

    #[test]
    fn test_log_record_resource_attributes_structure() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        entry.project_name = "MyProject".to_string();
        entry.app_name = "MyApp".to_string();
        entry.task_name = "MainTask".to_string();
        entry.task_index = 5;

        let record = LogRecord::from_log_entry(entry);

        assert_eq!(
            record.resource_attributes["service.name"],
            serde_json::Value::String("MyProject".to_string())
        );
        assert_eq!(
            record.resource_attributes["service.instance.id"],
            serde_json::Value::String("MyApp".to_string())
        );
        assert_eq!(
            record.resource_attributes["process.command_line"],
            serde_json::Value::String("MainTask".to_string())
        );
        assert_eq!(
            record.resource_attributes["process.pid"],
            serde_json::Value::Number(5.into())
        );
    }

    #[test]
    fn test_log_level_otel_all_levels() {
        let all_levels = [
            (LogLevel::Trace, 1, "TRACE"),
            (LogLevel::Debug, 5, "DEBUG"),
            (LogLevel::Info, 9, "INFO"),
            (LogLevel::Warn, 13, "WARN"),
            (LogLevel::Error, 17, "ERROR"),
            (LogLevel::Fatal, 21, "FATAL"),
        ];

        for (level, expected_num, expected_text) in all_levels {
            assert_eq!(level.to_otel_severity_number(), expected_num);
            assert_eq!(level.to_otel_severity_text(), expected_text);
        }
    }

    #[test]
    fn test_log_entry_clone() {
        let mut entry = LogEntry::new(
            "src".to_string(),
            "host".to_string(),
            "msg".to_string(),
            "logger".to_string(),
            LogLevel::Info,
        );

        entry.arguments.insert(0, serde_json::json!("arg1"));
        let cloned = entry.clone();

        assert_eq!(cloned.message, entry.message);
        assert_eq!(cloned.arguments, entry.arguments);
    }

    // ─── Span type tests ────────────────────────────────────────────

    #[test]
    fn test_span_kind_from_u8() {
        assert_eq!(SpanKind::from_u8(0), Some(SpanKind::Internal));
        assert_eq!(SpanKind::from_u8(1), Some(SpanKind::Server));
        assert_eq!(SpanKind::from_u8(2), Some(SpanKind::Client));
        assert_eq!(SpanKind::from_u8(3), Some(SpanKind::Producer));
        assert_eq!(SpanKind::from_u8(4), Some(SpanKind::Consumer));
        assert_eq!(SpanKind::from_u8(5), None);
        assert_eq!(SpanKind::from_u8(255), None);
    }

    #[test]
    fn test_span_kind_roundtrip() {
        for val in 0..5u8 {
            let kind = SpanKind::from_u8(val).unwrap();
            assert_eq!(kind.as_u8(), val);
        }
    }

    #[test]
    fn test_span_kind_otel_mapping() {
        assert_eq!(SpanKind::Internal.to_otel_kind(), 1);
        assert_eq!(SpanKind::Server.to_otel_kind(), 2);
        assert_eq!(SpanKind::Client.to_otel_kind(), 3);
        assert_eq!(SpanKind::Producer.to_otel_kind(), 4);
        assert_eq!(SpanKind::Consumer.to_otel_kind(), 5);
    }

    #[test]
    fn test_span_kind_display() {
        assert_eq!(SpanKind::Internal.to_string(), "Internal");
        assert_eq!(SpanKind::Server.to_string(), "Server");
        assert_eq!(SpanKind::Client.to_string(), "Client");
        assert_eq!(SpanKind::Producer.to_string(), "Producer");
        assert_eq!(SpanKind::Consumer.to_string(), "Consumer");
    }

    #[test]
    fn test_span_status_code_from_u8() {
        assert_eq!(SpanStatusCode::from_u8(0), Some(SpanStatusCode::Unset));
        assert_eq!(SpanStatusCode::from_u8(1), Some(SpanStatusCode::Ok));
        assert_eq!(SpanStatusCode::from_u8(2), Some(SpanStatusCode::Error));
        assert_eq!(SpanStatusCode::from_u8(3), None);
    }

    #[test]
    fn test_span_status_code_roundtrip() {
        for val in 0..3u8 {
            let code = SpanStatusCode::from_u8(val).unwrap();
            assert_eq!(code.as_u8(), val);
        }
    }

    #[test]
    fn test_span_status_code_otel_mapping() {
        assert_eq!(SpanStatusCode::Unset.to_otel_status(), 0);
        assert_eq!(SpanStatusCode::Ok.to_otel_status(), 1);
        assert_eq!(SpanStatusCode::Error.to_otel_status(), 2);
    }

    #[test]
    fn test_span_entry_creation() {
        let trace_id = [1u8; 16];
        let span_id = [2u8; 8];
        let entry = SpanEntry::new(trace_id, span_id, "axis.move".to_string());

        assert_eq!(entry.trace_id, trace_id);
        assert_eq!(entry.span_id, span_id);
        assert_eq!(entry.parent_span_id, [0u8; 8]);
        assert_eq!(entry.name, "axis.move");
        assert_eq!(entry.kind, SpanKind::Internal);
        assert_eq!(entry.status_code, SpanStatusCode::Unset);
        assert!(!entry.has_parent());
    }

    #[test]
    fn test_span_entry_has_parent() {
        let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "test".to_string());
        assert!(!entry.has_parent());

        entry.parent_span_id = [3u8; 8];
        assert!(entry.has_parent());
    }

    #[test]
    fn test_span_entry_hex_ids() {
        let trace_id: [u8; 16] = [
            0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45,
            0x67, 0x89,
        ];
        let span_id: [u8; 8] = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];

        let entry = SpanEntry::new(trace_id, span_id, "test".to_string());

        assert_eq!(entry.trace_id_hex(), "abcdef0123456789abcdef0123456789");
        assert_eq!(entry.span_id_hex(), "fedcba9876543210");
        assert_eq!(entry.parent_span_id_hex(), "0000000000000000");
    }

    #[test]
    fn test_span_event_creation() {
        let event = SpanEvent {
            timestamp: Utc::now(),
            name: "axis.target_reached".to_string(),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert("axis.position".to_string(), serde_json::json!(150.5));
                attrs
            },
        };

        assert_eq!(event.name, "axis.target_reached");
        assert_eq!(event.attributes.len(), 1);
        assert_eq!(event.attributes["axis.position"], serde_json::json!(150.5));
    }

    #[test]
    fn test_span_entry_with_motion_attributes() {
        let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "motion.axis_move".to_string());
        entry.kind = SpanKind::Internal;
        entry.status_code = SpanStatusCode::Ok;
        entry
            .attributes
            .insert("motion.axis_id".to_string(), serde_json::json!(1));
        entry.attributes.insert(
            "motion.target_position".to_string(),
            serde_json::json!(250.0),
        );
        entry
            .attributes
            .insert("motion.velocity".to_string(), serde_json::json!(100.0));

        assert_eq!(entry.attributes.len(), 3);
        assert_eq!(entry.name, "motion.axis_move");
        assert_eq!(entry.kind, SpanKind::Internal);
        assert_eq!(entry.status_code, SpanStatusCode::Ok);
    }

    #[test]
    fn test_span_entry_clone() {
        let mut entry = SpanEntry::new([1u8; 16], [2u8; 8], "test".to_string());
        entry
            .attributes
            .insert("key".to_string(), serde_json::json!("value"));
        entry.events.push(SpanEvent {
            timestamp: Utc::now(),
            name: "event1".to_string(),
            attributes: HashMap::new(),
        });

        let cloned = entry.clone();
        assert_eq!(cloned.name, entry.name);
        assert_eq!(cloned.attributes, entry.attributes);
        assert_eq!(cloned.events.len(), entry.events.len());
    }
}

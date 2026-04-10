//! ADS protocol data structures and constants

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tc_otel_core::{LogLevel, SpanKind, SpanStatusCode};

/// ADS protocol version currently supported
pub const ADS_PROTOCOL_VERSION: u8 = 1;

/// Default ADS server port
pub const ADS_DEFAULT_PORT: u16 = 16150;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdsProtocolVersion {
    V1 = 1,
    V2 = 2,
    Registration = 3,
}

impl AdsProtocolVersion {
    pub fn as_u8(&self) -> u8 {
        *self as u8
    }

    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(AdsProtocolVersion::V1),
            2 => Some(AdsProtocolVersion::V2),
            3 => Some(AdsProtocolVersion::Registration),
            _ => None,
        }
    }
}

/// Raw ADS log entry (as it comes over the wire)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsLogEntry {
    pub version: AdsProtocolVersion,
    pub message: String,
    pub logger: String,
    pub level: LogLevel,
    pub plc_timestamp: DateTime<Utc>,
    pub clock_timestamp: DateTime<Utc>,
    pub task_index: i32,
    pub task_name: String,
    pub task_cycle_counter: u32,
    pub app_name: String,
    pub project_name: String,
    pub online_change_count: u32,
    pub arguments: HashMap<usize, serde_json::Value>,
    pub context: HashMap<String, serde_json::Value>,
}

/// Represents a structured argument in the ADS protocol
#[derive(Debug, Clone)]
pub struct AdsArgument {
    pub type_id: u8,
    pub index: u8,
    pub value: serde_json::Value,
}

/// Represents a context property in the ADS protocol
#[derive(Debug, Clone)]
pub struct AdsContext {
    pub type_id: u8,
    pub scope: u8,
    pub name: String,
    pub value: serde_json::Value,
}

/// Registration message for static task metadata (protocol v2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationMessage {
    pub task_index: u8,
    pub task_name: String,
    pub app_name: String,
    pub project_name: String,
    pub online_change_count: u32,
}

/// Unique key for task registration: (AMS Net ID, AMS Source Port, Task Index)
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationKey {
    pub ams_net_id: String,
    pub ams_source_port: u16,
    pub task_index: u8,
}

/// Task metadata from registration message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMetadata {
    pub task_name: String,
    pub app_name: String,
    pub project_name: String,
    pub online_change_count: u32,
}

/// An event within an ADS span (wire-level representation)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsSpanEvent {
    pub name: String,
    pub timestamp: DateTime<Utc>,
    pub attributes: HashMap<String, serde_json::Value>,
}

/// Raw ADS span entry (as it comes over the wire, message type 0x05)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsSpanEntry {
    // Trace identity (fixed-size on wire: 16 bytes trace_id, 8 bytes span_id, 8 bytes parent)
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: [u8; 8],

    // Span metadata
    pub name: String,
    pub kind: SpanKind,
    pub status_code: SpanStatusCode,
    pub status_message: String,

    // Timestamps
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,

    // Task metadata (same as log entries)
    pub task_index: i32,
    pub task_cycle_counter: u32,

    // Events
    pub events: Vec<AdsSpanEvent>,

    // Attributes (key-value pairs)
    pub attributes: HashMap<String, serde_json::Value>,
}

impl AdsSpanEntry {
    /// Format trace_id as hex string (32 chars)
    pub fn trace_id_hex(&self) -> String {
        self.trace_id.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Format span_id as hex string (16 chars)
    pub fn span_id_hex(&self) -> String {
        self.span_id.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Format parent_span_id as hex string (16 chars), empty string if all zeros
    pub fn parent_span_id_hex(&self) -> String {
        if self.parent_span_id.iter().all(|&b| b == 0) {
            String::new()
        } else {
            self.parent_span_id
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ads_version_conversion() {
        assert_eq!(AdsProtocolVersion::from_u8(1), Some(AdsProtocolVersion::V1));
        assert_eq!(AdsProtocolVersion::from_u8(255), None);
    }

    #[test]
    fn test_ads_span_entry_trace_id_hex() {
        let entry = AdsSpanEntry {
            trace_id: [
                0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56,
                0x78, 0x90,
            ],
            span_id: [0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef],
            parent_span_id: [0; 8],
            name: "test".to_string(),
            kind: SpanKind::Internal,
            status_code: SpanStatusCode::Unset,
            status_message: String::new(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            task_index: 0,
            task_cycle_counter: 0,
            events: Vec::new(),
            attributes: HashMap::new(),
        };

        assert_eq!(entry.trace_id_hex(), "abcdef1234567890abcdef1234567890");
        assert_eq!(entry.span_id_hex(), "1234567890abcdef");
        assert_eq!(entry.parent_span_id_hex(), ""); // all zeros = empty
    }

    #[test]
    fn test_ads_span_entry_parent_span_id_hex() {
        let entry = AdsSpanEntry {
            trace_id: [0; 16],
            span_id: [0; 8],
            parent_span_id: [0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88],
            name: "test".to_string(),
            kind: SpanKind::Internal,
            status_code: SpanStatusCode::Unset,
            status_message: String::new(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            task_index: 0,
            task_cycle_counter: 0,
            events: Vec::new(),
            attributes: HashMap::new(),
        };

        assert_eq!(entry.parent_span_id_hex(), "ffeeddccbbaa9988");
    }

    #[test]
    fn test_ads_span_event_creation() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "state_machine.transition.old_state".to_string(),
            serde_json::json!("IDLE"),
        );

        let event = AdsSpanEvent {
            name: "transition".to_string(),
            timestamp: Utc::now(),
            attributes: attrs,
        };

        assert_eq!(event.name, "transition");
        assert_eq!(event.attributes.len(), 1);
    }
}

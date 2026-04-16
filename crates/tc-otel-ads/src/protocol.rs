//! ADS protocol data structures and constants

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tc_otel_core::{LogLevel, MetricKind, SpanKind, SpanStatusCode};

/// Attribute value types for spans (wire format)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttrValue {
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
}

/// Wire-format trace events (streaming span events)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceWireEvent {
    /// SPAN_BEGIN (event_type=1): starts a new span
    Begin {
        local_id: u8,
        task_index: u8,
        flags: u8,
        dc_time: i64,
        parent_local_id: u8,
        kind: u8,
        name: String,
        traceparent: Option<String>,
    },
    /// SPAN_ATTR (event_type=2): adds an attribute to a pending span
    Attr {
        local_id: u8,
        task_index: u8,
        flags: u8,
        dc_time: i64,
        key: String,
        value: AttrValue,
    },
    /// SPAN_EVENT (event_type=3): adds a timestamped event to a pending span
    Event {
        local_id: u8,
        task_index: u8,
        flags: u8,
        dc_time: i64,
        name: String,
        attrs: Vec<(String, AttrValue)>,
    },
    /// SPAN_END (event_type=4): completes a pending span
    End {
        local_id: u8,
        task_index: u8,
        flags: u8,
        dc_time: i64,
        status: u8,
        message: String,
    },
}

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
    pub trace_id: [u8; 16], // Trace context (all zeros = no trace)
    pub span_id: [u8; 8],   // Span context (all zeros = no span)
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

/// Wire-level metric entry (ADS message type 0x04)
///
/// Binary format:
/// ```text
/// [type: u8 = 0x04]
/// [entry_length: u16 LE]  -- total bytes after this field
/// [kind: u8]              -- 0=Gauge, 1=Sum, 2=Histogram
/// [timestamp: 8 bytes FILETIME]
/// [task_index: u8]
/// [cycle_counter: u32 LE]
/// [attr_count: u8]
/// [flags: u8]             -- bit 0: is_monotonic (for Sum)
/// [name: string]          -- 1-byte len + UTF-8
/// [description: string]
/// [unit: string]
/// [value: f64 LE]         -- metric value (Gauge/Sum)
/// // For Histogram (kind=2), additional fields:
/// [bucket_count: u8]      -- number of boundaries
/// [bounds: bucket_count × f64 LE]
/// [counts: (bucket_count+1) × u64 LE]
/// [histogram_count: u64 LE]
/// [histogram_sum: f64 LE]
/// [attributes: attr_count × (key: string, value_type: u8, value: typed)]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsMetricEntry {
    pub name: String,
    pub description: String,
    pub unit: String,
    pub kind: MetricKind,
    pub value: f64,
    pub timestamp: DateTime<Utc>,
    pub task_index: i32,
    pub task_cycle_counter: u32,
    pub is_monotonic: bool,
    pub attributes: HashMap<String, serde_json::Value>,
    // Histogram-specific
    pub histogram_bounds: Vec<f64>,
    pub histogram_counts: Vec<u64>,
    pub histogram_count: u64,
    pub histogram_sum: f64,
}

/// Wire-level span event (as received from ADS protocol, type 0x05)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsSpanEvent {
    pub timestamp: DateTime<Utc>,
    pub name: String,
    pub attributes: HashMap<String, serde_json::Value>,
}

/// Wire-level completed span entry (ADS message type 0x05)
///
/// Binary format:
/// ```text
/// [type: u8 = 0x05]
/// [entry_length: u16 LE]  -- total bytes after this field
/// [trace_id: 16 bytes]
/// [span_id: 8 bytes]
/// [parent_span_id: 8 bytes]
/// [kind: u8]
/// [status_code: u8]
/// [start_time: 8 bytes FILETIME]
/// [end_time: 8 bytes FILETIME]
/// [task_index: u8]
/// [cycle_counter: u32 LE]
/// [attr_count: u8]
/// [event_count: u8]
/// [name: string]
/// [status_message: string]
/// [attributes: attr_count × (key: string, value_type: u8, value: typed)]
/// [events: event_count × (timestamp: FILETIME, name: string, attr_count: u8, attrs...)]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdsSpanEntry {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: [u8; 8],
    pub name: String,
    pub kind: SpanKind,
    pub status_code: SpanStatusCode,
    pub status_message: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub task_index: i32,
    pub task_cycle_counter: u32,
    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<AdsSpanEvent>,
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
    fn test_ads_span_entry_creation() {
        let entry = AdsSpanEntry {
            trace_id: [1u8; 16],
            span_id: [2u8; 8],
            parent_span_id: [0u8; 8],
            name: "motion.axis_move".to_string(),
            kind: SpanKind::Internal,
            status_code: SpanStatusCode::Ok,
            status_message: String::new(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            task_index: 1,
            task_cycle_counter: 500,
            attributes: HashMap::new(),
            events: Vec::new(),
        };

        assert_eq!(entry.name, "motion.axis_move");
        assert_eq!(entry.kind, SpanKind::Internal);
    }

    #[test]
    fn test_ads_metric_entry_gauge() {
        let entry = AdsMetricEntry {
            name: "plc.motor.temperature".to_string(),
            description: "Motor temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKind::Gauge,
            value: 72.5,
            timestamp: Utc::now(),
            task_index: 1,
            task_cycle_counter: 500,
            is_monotonic: false,
            attributes: HashMap::new(),
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
        };

        assert_eq!(entry.name, "plc.motor.temperature");
        assert_eq!(entry.kind, MetricKind::Gauge);
        assert_eq!(entry.value, 72.5);
    }

    #[test]
    fn test_ads_metric_entry_sum() {
        let entry = AdsMetricEntry {
            name: "plc.parts_produced".to_string(),
            description: "Total parts produced".to_string(),
            unit: "{count}".to_string(),
            kind: MetricKind::Sum,
            value: 1234.0,
            timestamp: Utc::now(),
            task_index: 1,
            task_cycle_counter: 1000,
            is_monotonic: true,
            attributes: HashMap::new(),
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
        };

        assert_eq!(entry.kind, MetricKind::Sum);
        assert!(entry.is_monotonic);
        assert_eq!(entry.value, 1234.0);
    }

    #[test]
    fn test_ads_metric_entry_histogram() {
        let entry = AdsMetricEntry {
            name: "plc.cycle_time".to_string(),
            description: "PLC task cycle time".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Histogram,
            value: 0.0,
            timestamp: Utc::now(),
            task_index: 1,
            task_cycle_counter: 2000,
            is_monotonic: false,
            attributes: HashMap::new(),
            histogram_bounds: vec![1.0, 5.0, 10.0, 50.0],
            histogram_counts: vec![10, 25, 12, 3, 1],
            histogram_count: 51,
            histogram_sum: 320.5,
        };

        assert_eq!(entry.kind, MetricKind::Histogram);
        assert_eq!(entry.histogram_bounds.len(), 4);
        assert_eq!(entry.histogram_counts.len(), 5);
        assert_eq!(entry.histogram_count, 51);
    }

    #[test]
    fn test_ads_span_event_creation() {
        let event = AdsSpanEvent {
            timestamp: Utc::now(),
            name: "axis.target_reached".to_string(),
            attributes: {
                let mut m = HashMap::new();
                m.insert("position".to_string(), serde_json::json!(100.0));
                m
            },
        };

        assert_eq!(event.name, "axis.target_reached");
        assert_eq!(event.attributes.len(), 1);
    }
}

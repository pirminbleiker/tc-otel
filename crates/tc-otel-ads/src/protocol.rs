//! ADS protocol data structures and constants

use chrono::{DateTime, Utc};
use tc_otel_core::LogLevel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ads_version_conversion() {
        assert_eq!(AdsProtocolVersion::from_u8(1), Some(AdsProtocolVersion::V1));
        assert_eq!(AdsProtocolVersion::from_u8(255), None);
    }
}

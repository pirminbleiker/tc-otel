//! ADS binary protocol parser

use crate::error::*;
use crate::protocol::*;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tc_otel_core::{LogLevel, SpanKind, SpanStatusCode};

// Security limits for protocol parsing
/// Maximum length for individual strings (65 KB)
const MAX_STRING_LENGTH: usize = 65_536;
/// Maximum number of arguments allowed per message
const MAX_ARGUMENTS: usize = 32;
/// Maximum number of context variables allowed per message
const MAX_CONTEXT_VARS: usize = 64;
/// Maximum total message size (1 MB)
const MAX_MESSAGE_SIZE: usize = 1_048_576;
/// Maximum number of events allowed per span
const MAX_SPAN_EVENTS: usize = 128;
/// Maximum number of attributes allowed per span or event
const MAX_SPAN_ATTRIBUTES: usize = 64;

/// Result of parsing a buffer containing log entries, registrations, and spans
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub entries: Vec<AdsLogEntry>,
    pub registrations: Vec<RegistrationMessage>,
    pub spans: Vec<AdsSpanEntry>,
}

/// Parser for ADS binary protocol messages
pub struct AdsParser;

impl AdsParser {
    /// Parse ALL log entries and registrations from a buffer (buffer can contain multiple entries)
    /// Parse ALL log entries from a buffer (PLC sends multiple entries per ADS Write)
    pub fn parse_all(data: &[u8]) -> Result<ParseResult> {
        if data.len() > MAX_MESSAGE_SIZE {
            return Err(AdsError::ParseError(format!(
                "Message size {} exceeds maximum {}",
                data.len(),
                MAX_MESSAGE_SIZE
            )));
        }

        let mut entries = Vec::new();
        let mut registrations = Vec::new();
        let mut spans = Vec::new();
        let mut reader = BytesReader::new(data);

        while reader.remaining() > 0 {
            // Skip zero padding or legacy terminator
            match reader.peek() {
                Some(0) | Some(0xFF) | None => break,
                _ => {}
            }

            // Dispatch on message type (first byte)
            let message_type = reader.peek().unwrap();
            match message_type {
                1 => {
                    // v1 log entry
                    match Self::parse_v1_from_reader(&mut reader) {
                        Ok(entry) => entries.push(entry),
                        Err(e) => {
                            if entries.is_empty() && registrations.is_empty() {
                                return Err(e);
                            }
                            // Partial entry at end - ok, we got what we could
                            tracing::debug!(
                                "Partial entry at buffer end ({} bytes remaining): {}",
                                reader.remaining(),
                                e
                            );
                            break;
                        }
                    }
                }
                2 => {
                    // v2 log entry
                    let entry_pos = reader.pos;
                    let peek_len = std::cmp::min(40, reader.remaining());
                    let peek_bytes = &reader.data[entry_pos..entry_pos + peek_len];
                    tracing::debug!("Parsing v2 entry at offset {} (entries so far: {}), first {} bytes: {:02x?}", entry_pos, entries.len(), peek_len, peek_bytes);
                    match Self::parse_v2_from_reader(&mut reader) {
                        Ok(entry) => entries.push(entry),
                        Err(e) => {
                            if !entries.is_empty() || !registrations.is_empty() {
                                // Already parsed some entries — remaining buffer is likely padding/garbage
                                tracing::debug!(
                                    "Partial v2 entry at buffer end ({} bytes remaining): {}",
                                    reader.remaining(),
                                    e
                                );
                                break;
                            }
                            tracing::debug!("Error parsing v2 entry: {}", e);
                            // Try to skip using entry_length if available
                            let skip_pos = reader.pos;
                            if reader.remaining() >= 3 {
                                // Try to read entry_length to skip ahead
                                reader.pos += 1; // Skip type byte
                                if let Ok(entry_len) = reader.read_u16() {
                                    reader.pos = skip_pos + entry_len as usize + 3;
                                } else {
                                    // Can't skip, stop parsing
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
                3 => {
                    // Registration message
                    match Self::parse_registration_from_reader(&mut reader) {
                        Ok(reg) => registrations.push(reg),
                        Err(e) => {
                            tracing::debug!("Error parsing registration: {}", e);
                            break;
                        }
                    }
                }
                5 => {
                    // Span entry
                    match Self::parse_span_from_reader(&mut reader) {
                        Ok(span) => spans.push(span),
                        Err(e) => {
                            if !entries.is_empty()
                                || !registrations.is_empty()
                                || !spans.is_empty()
                            {
                                tracing::debug!(
                                    "Partial span at buffer end ({} bytes remaining): {}",
                                    reader.remaining(),
                                    e
                                );
                                break;
                            }
                            return Err(e);
                        }
                    }
                }
                _ => {
                    // Unknown message type, stop
                    tracing::warn!("Unknown message type: {}", message_type);
                    break;
                }
            }
        }

        Ok(ParseResult {
            entries,
            registrations,
            spans,
        })
    }

    /// Parse a single ADS log entry from bytes (v1 only for backward compatibility)
    pub fn parse(data: &[u8]) -> Result<AdsLogEntry> {
        if data.len() > MAX_MESSAGE_SIZE {
            return Err(AdsError::ParseError(format!(
                "Message size {} exceeds maximum {}",
                data.len(),
                MAX_MESSAGE_SIZE
            )));
        }

        let mut reader = BytesReader::new(data);
        Self::parse_v1_from_reader(&mut reader)
    }

    fn parse_v1_from_reader(reader: &mut BytesReader) -> Result<AdsLogEntry> {
        // Version (1 byte)
        let version_byte = reader.read_u8()?;
        let version = AdsProtocolVersion::from_u8(version_byte)
            .ok_or(AdsError::InvalidVersion(version_byte))?;

        // Message (string)
        let message = reader.read_string()?;

        // Logger (string)
        let logger = reader.read_string()?;

        // Level (2 bytes - UINT, PLC uses _WriteUInt for eLogLevel)
        let level_bytes = reader.read_bytes(2)?;
        let level_u16 = u16::from_le_bytes([level_bytes[0], level_bytes[1]]);
        let level = LogLevel::from_u8(level_u16 as u8).ok_or(AdsError::ParseError(format!(
            "Invalid log level: {}",
            level_u16
        )))?;

        // Timestamps (8 bytes each, FILETIME format)
        let plc_timestamp = reader.read_filetime()?;
        let clock_timestamp = reader.read_filetime()?;

        // Task metadata
        let task_index = reader.read_i32()?;
        let task_name = reader.read_string()?;
        let task_cycle_counter = reader.read_u32()?;

        // Application metadata
        let app_name = reader.read_string()?;
        let project_name = reader.read_string()?;
        let online_change_count = reader.read_u32()?;

        // Arguments and context (pre-allocate small capacity)
        let mut arguments = HashMap::with_capacity(8);
        let mut context = HashMap::with_capacity(4);

        loop {
            // Check if there's more data
            if reader.remaining() == 0 {
                break;
            }
            let type_id = reader.read_u8()?;
            if type_id == 0 || type_id == 255 {
                // 0 = legacy end marker, 255 = spec end marker
                break;
            }

            if type_id == 1 {
                // Argument - with security limit
                if arguments.len() >= MAX_ARGUMENTS {
                    return Err(AdsError::ParseError(format!(
                        "Too many arguments: {} exceeds maximum {}",
                        arguments.len() + 1,
                        MAX_ARGUMENTS
                    )));
                }
                let index = reader.read_u8()?;
                let value = reader.read_value()?;
                arguments.insert(index as usize, value);
            } else if type_id == 2 {
                // Context - with security limit
                if context.len() >= MAX_CONTEXT_VARS {
                    return Err(AdsError::ParseError(format!(
                        "Too many context variables: {} exceeds maximum {}",
                        context.len() + 1,
                        MAX_CONTEXT_VARS
                    )));
                }
                let scope = reader.read_u8()?;
                let name = reader.read_string()?;
                let value = reader.read_value()?;
                context.insert(format!("scope_{}_{}", scope, name), value);
            }
        }

        Ok(AdsLogEntry {
            version,
            message,
            logger,
            level,
            plc_timestamp,
            clock_timestamp,
            task_index,
            task_name,
            task_cycle_counter,
            app_name,
            project_name,
            online_change_count,
            arguments,
            context,
        })
    }

    fn parse_v2_from_reader(reader: &mut BytesReader) -> Result<AdsLogEntry> {
        // Type byte (should be 2)
        let version_byte = reader.read_u8()?;
        if version_byte != 2 {
            return Err(AdsError::InvalidVersion(version_byte));
        }

        // Entry length (2 bytes LE) - total length from after this field
        let entry_length = reader.read_u16()? as usize;
        let entry_start = reader.pos;

        // Fixed header (27 - 3 = 24 bytes after entry_length)
        let level_byte = reader.read_u8()?;
        let level = LogLevel::from_u8(level_byte).ok_or(AdsError::ParseError(format!(
            "Invalid log level: {}",
            level_byte
        )))?;

        let plc_timestamp = reader.read_filetime()?;
        let clock_timestamp = reader.read_filetime()?;

        let task_index = reader.read_u8()? as i32;
        let cycle_counter = reader.read_u32()?;
        let arg_count = reader.read_u8()? as usize;
        let context_count = reader.read_u8()? as usize;

        // Message string
        let message = reader.read_string()?;

        // Logger string (0 length = global/default logger)
        let logger = reader.read_string()?;

        // Arguments (1-based keys to match v1 format and formatter expectations)
        let mut arguments = HashMap::with_capacity(arg_count);
        for arg_idx in 0..arg_count {
            if arguments.len() >= MAX_ARGUMENTS {
                return Err(AdsError::ParseError("Too many arguments".to_string()));
            }
            let type_id = reader.read_u8()?;
            let type_id_i32 = Self::remap_v2_type_id(type_id as i32);
            let value = reader.read_value_with_type(type_id_i32)?;
            arguments.insert(arg_idx + 1, value);
        }

        // Context
        let mut context = HashMap::with_capacity(context_count * 2);
        let mut context_idx = 0;
        for _ in 0..context_count {
            if context_idx >= MAX_CONTEXT_VARS {
                return Err(AdsError::ParseError(
                    "Too many context variables".to_string(),
                ));
            }
            let scope = reader.read_u8()?;
            let prop_count = reader.read_u8()?;

            for _ in 0..prop_count {
                let name = reader.read_string()?;
                let type_id = reader.read_u8()?;
                let type_id_i32 = Self::remap_v2_type_id(type_id as i32);
                let value = reader.read_value_with_type(type_id_i32)?;
                context.insert(format!("scope_{}_{}", scope, name), value);
                context_idx += 1;
            }
        }

        // Sync reader position to entry boundary — the PLC may pad entries
        // or include fields we don't yet parse. entry_length is authoritative.
        reader.pos = entry_start + entry_length;

        Ok(AdsLogEntry {
            version: AdsProtocolVersion::V2,
            message,
            logger,
            level,
            plc_timestamp,
            clock_timestamp,
            task_index,
            task_name: String::new(), // Will be filled by registry lookup
            task_cycle_counter: cycle_counter,
            app_name: String::new(),     // Will be filled by registry lookup
            project_name: String::new(), // Will be filled by registry lookup
            online_change_count: 0,      // Will be filled by registry lookup
            arguments,
            context,
        })
    }

    fn parse_registration_from_reader(reader: &mut BytesReader) -> Result<RegistrationMessage> {
        // Type byte (should be 3)
        let type_byte = reader.read_u8()?;
        if type_byte != 3 {
            return Err(AdsError::ParseError(format!(
                "Invalid registration type: {}",
                type_byte
            )));
        }

        let task_index = reader.read_u8()?;
        let task_name = reader.read_string()?;
        let app_name = reader.read_string()?;
        let project_name = reader.read_string()?;
        let online_change_count = reader.read_u32()?;

        Ok(RegistrationMessage {
            task_index,
            task_name,
            app_name,
            project_name,
            online_change_count,
        })
    }

    /// Parse a span entry from the binary stream (wire type 0x05).
    ///
    /// Wire format:
    /// ```text
    /// [0x05]           - 1 byte: message type
    /// [entry_length]   - 2 bytes: u16 LE, total length after this field
    /// [trace_id]       - 16 bytes: trace ID
    /// [span_id]        - 8 bytes: span ID
    /// [parent_span_id] - 8 bytes: parent span ID (all zeros = root)
    /// [kind]           - 1 byte: SpanKind (0-4)
    /// [status_code]    - 1 byte: SpanStatusCode (0-2)
    /// [start_time]     - 8 bytes: FILETIME
    /// [end_time]       - 8 bytes: FILETIME
    /// [task_index]     - 1 byte: task index
    /// [cycle_counter]  - 4 bytes: u32 LE
    /// [event_count]    - 1 byte: number of events
    /// [attr_count]     - 1 byte: number of attributes
    /// [name]           - string: span name
    /// [status_message] - string: status message (empty if ok)
    /// [events...]      - variable: event_count events
    /// [attributes...]  - variable: attr_count attributes
    /// ```
    ///
    /// Each event:
    /// ```text
    /// [name]           - string: event name
    /// [timestamp]      - 8 bytes: FILETIME
    /// [attr_count]     - 1 byte: number of event attributes
    /// [attributes...]  - variable: key-value pairs
    /// ```
    ///
    /// Each attribute (span or event):
    /// ```text
    /// [key]            - string: attribute name
    /// [type_id]        - 1 byte: value type
    /// [value]          - variable: typed value
    /// ```
    fn parse_span_from_reader(reader: &mut BytesReader) -> Result<AdsSpanEntry> {
        // Type byte (should be 5)
        let type_byte = reader.read_u8()?;
        if type_byte != 5 {
            return Err(AdsError::ParseError(format!(
                "Invalid span type: {}",
                type_byte
            )));
        }

        // Entry length
        let entry_length = reader.read_u16()? as usize;
        let entry_start = reader.pos;

        // Trace identity
        let trace_id_bytes = reader.read_bytes(16)?;
        let mut trace_id = [0u8; 16];
        trace_id.copy_from_slice(trace_id_bytes);

        let span_id_bytes = reader.read_bytes(8)?;
        let mut span_id = [0u8; 8];
        span_id.copy_from_slice(span_id_bytes);

        let parent_span_id_bytes = reader.read_bytes(8)?;
        let mut parent_span_id = [0u8; 8];
        parent_span_id.copy_from_slice(parent_span_id_bytes);

        // Span metadata
        let kind_byte = reader.read_u8()?;
        let kind = SpanKind::from_u8(kind_byte).ok_or(AdsError::ParseError(format!(
            "Invalid span kind: {}",
            kind_byte
        )))?;

        let status_byte = reader.read_u8()?;
        let status_code =
            SpanStatusCode::from_u8(status_byte).ok_or(AdsError::ParseError(format!(
                "Invalid span status code: {}",
                status_byte
            )))?;

        // Timestamps
        let start_time = reader.read_filetime()?;
        let end_time = reader.read_filetime()?;

        // Task metadata
        let task_index = reader.read_u8()? as i32;
        let cycle_counter = reader.read_u32()?;

        // Counts
        let event_count = reader.read_u8()? as usize;
        let attr_count = reader.read_u8()? as usize;

        if event_count > MAX_SPAN_EVENTS {
            return Err(AdsError::ParseError(format!(
                "Too many span events: {} exceeds maximum {}",
                event_count, MAX_SPAN_EVENTS
            )));
        }
        if attr_count > MAX_SPAN_ATTRIBUTES {
            return Err(AdsError::ParseError(format!(
                "Too many span attributes: {} exceeds maximum {}",
                attr_count, MAX_SPAN_ATTRIBUTES
            )));
        }

        // Strings
        let name = reader.read_string()?;
        let status_message = reader.read_string()?;

        // Events
        let mut events = Vec::with_capacity(event_count);
        for _ in 0..event_count {
            let event_name = reader.read_string()?;
            let event_timestamp = reader.read_filetime()?;
            let event_attr_count = reader.read_u8()? as usize;

            if event_attr_count > MAX_SPAN_ATTRIBUTES {
                return Err(AdsError::ParseError(format!(
                    "Too many event attributes: {} exceeds maximum {}",
                    event_attr_count, MAX_SPAN_ATTRIBUTES
                )));
            }

            let mut event_attrs = HashMap::with_capacity(event_attr_count);
            for _ in 0..event_attr_count {
                let key = reader.read_string()?;
                let type_id = reader.read_u8()?;
                let type_id_i32 = Self::remap_v2_type_id(type_id as i32);
                let value = reader.read_value_with_type(type_id_i32)?;
                event_attrs.insert(key, value);
            }

            events.push(AdsSpanEvent {
                name: event_name,
                timestamp: event_timestamp,
                attributes: event_attrs,
            });
        }

        // Span attributes
        let mut attributes = HashMap::with_capacity(attr_count);
        for _ in 0..attr_count {
            let key = reader.read_string()?;
            let type_id = reader.read_u8()?;
            let type_id_i32 = Self::remap_v2_type_id(type_id as i32);
            let value = reader.read_value_with_type(type_id_i32)?;
            attributes.insert(key, value);
        }

        // Sync to entry boundary
        reader.pos = entry_start + entry_length;

        Ok(AdsSpanEntry {
            trace_id,
            span_id,
            parent_span_id,
            name,
            kind,
            status_code,
            status_message,
            start_time,
            end_time,
            task_index,
            task_cycle_counter: cycle_counter,
            events,
            attributes,
        })
    }

    /// Remap v2 type ID (1 byte) to internal format (for lookup in read_value_with_type)
    fn remap_v2_type_id(v2_type: i32) -> i32 {
        match v2_type {
            100 => 20000,   // TIME
            101 => 20001,   // LTIME
            102 => 20002,   // DATE
            103 => 20003,   // DATE_AND_TIME
            104 => 20004,   // TIME_OF_DAY
            105 => 20005,   // ENUM
            106 => 20006,   // WSTRING
            other => other, // Standard types 1-17 pass through unchanged
        }
    }
}

struct BytesReader<'a> {
    data: &'a [u8],
    pub pos: usize,
}

impl<'a> BytesReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn peek(&self) -> Option<u8> {
        if self.pos < self.data.len() {
            Some(self.data[self.pos])
        } else {
            None
        }
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(AdsError::IncompleteMessage {
                expected: n,
                got: self.remaining(),
            });
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_bytes(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_string(&mut self) -> Result<String> {
        // String format: [Length: u8] + [Data: UTF-8 bytes]
        // PLC FB_LogEntry._WriteString uses _WriteByte(len) - single byte length prefix
        let len = self.read_u8()? as usize;

        // Security: Enforce maximum string length
        if len > MAX_STRING_LENGTH {
            return Err(AdsError::ParseError(format!(
                "String length {} exceeds maximum {}",
                len, MAX_STRING_LENGTH
            )));
        }

        let str_bytes = self.read_bytes(len)?;

        // Validate UTF-8 first before allocating
        match std::str::from_utf8(str_bytes) {
            Ok(valid_str) => Ok(valid_str.to_string()),
            Err(e) => Err(AdsError::InvalidStringEncoding(e.to_string())),
        }
    }

    fn read_filetime(&mut self) -> Result<DateTime<Utc>> {
        let bytes = self.read_bytes(8)?;
        let filetime = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);

        // FILETIME is 100-nanosecond intervals since 1601-01-01
        // Convert to Unix timestamp (1970-01-01)
        const FILETIME_EPOCH_DIFF: u64 = 116444736000000000; // 100-nanosecond intervals
        if filetime < FILETIME_EPOCH_DIFF {
            // PLC may send 0 if RTC not synced - use current time as fallback
            return Ok(Utc::now());
        }

        let unix_time_100ns = filetime - FILETIME_EPOCH_DIFF;
        let secs = unix_time_100ns / 10_000_000;
        let nanos = ((unix_time_100ns % 10_000_000) * 100) as u32;

        DateTime::<Utc>::from_timestamp(secs as i64, nanos)
            .ok_or(AdsError::InvalidTimestamp("Invalid timestamp".to_string()))
    }

    fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i16(&mut self) -> Result<i16> {
        let bytes = self.read_bytes(2)?;
        Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Read a typed value per ADS protocol spec.
    /// Type IDs are INT16 (2 bytes), matching Tc2_Utilities.E_ArgType.
    fn read_value(&mut self) -> Result<serde_json::Value> {
        let val_type = self.read_i16()? as i32;

        match val_type {
            0 => Ok(serde_json::Value::Null),
            1 | 9 => {
                let v = self.read_u8()?;
                Ok(serde_json::json!(v))
            } // BYTE/USINT
            2 | 10 => {
                let v = self.read_u16()?;
                Ok(serde_json::json!(v))
            } // WORD/UINT
            3 | 11 => {
                let v = self.read_u32()?;
                Ok(serde_json::json!(v))
            } // DWORD/UDINT
            4 => {
                // REAL (f32)
                let b = self.read_bytes(4)?;
                Ok(serde_json::json!(f32::from_le_bytes([
                    b[0], b[1], b[2], b[3]
                ])))
            }
            5 => {
                // LREAL (f64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(f64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            6 => {
                let v = self.read_u8()? as i8;
                Ok(serde_json::json!(v))
            } // SINT
            7 => {
                let v = self.read_i16()?;
                Ok(serde_json::json!(v))
            } // INT
            8 => {
                let v = self.read_i32()?;
                Ok(serde_json::json!(v))
            } // DINT
            12 => {
                let s = self.read_string()?;
                Ok(serde_json::Value::String(s))
            } // STRING
            13 => {
                let b = self.read_u8()? != 0;
                Ok(serde_json::Value::Bool(b))
            } // BOOL
            15 => {
                // ULARGE (u64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(u64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            17 => {
                // LARGE (i64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(i64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            20000 => {
                // TIME (ms as u32)
                let total_ms = self.read_u32()?;
                let d = total_ms / 86_400_000;
                let h = (total_ms % 86_400_000) / 3_600_000;
                let m = (total_ms % 3_600_000) / 60_000;
                let s = (total_ms % 60_000) / 1000;
                let ms = total_ms % 1000;
                let mut parts = String::from("T#");
                if d > 0 {
                    parts.push_str(&format!("{}d", d));
                }
                if h > 0 {
                    parts.push_str(&format!("{}h", h));
                }
                if m > 0 {
                    parts.push_str(&format!("{}m", m));
                }
                if s > 0 || (d == 0 && h == 0 && m == 0 && ms == 0) {
                    parts.push_str(&format!("{}s", s));
                }
                if ms > 0 {
                    parts.push_str(&format!("{}ms", ms));
                }
                Ok(serde_json::Value::String(parts))
            }
            20001 => {
                // LTIME (100ns as u64)
                let b = self.read_bytes(8)?;
                let ns100 = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                let total_ns = ns100 * 100;
                let d = total_ns / 86_400_000_000_000;
                let h = (total_ns % 86_400_000_000_000) / 3_600_000_000_000;
                let m = (total_ns % 3_600_000_000_000) / 60_000_000_000;
                let s = (total_ns % 60_000_000_000) / 1_000_000_000;
                let ms = (total_ns % 1_000_000_000) / 1_000_000;
                let us = (total_ns % 1_000_000) / 1_000;
                let ns = total_ns % 1_000;
                let mut parts = String::from("LTIME#");
                if d > 0 {
                    parts.push_str(&format!("{}d", d));
                }
                if h > 0 {
                    parts.push_str(&format!("{}h", h));
                }
                if m > 0 {
                    parts.push_str(&format!("{}m", m));
                }
                if s > 0 {
                    parts.push_str(&format!("{}s", s));
                }
                if ms > 0 {
                    parts.push_str(&format!("{}ms", ms));
                }
                if us > 0 {
                    parts.push_str(&format!("{}us", us));
                }
                if ns > 0 {
                    parts.push_str(&format!("{}ns", ns));
                }
                if parts == "LTIME#" {
                    parts.push_str("0s");
                }
                Ok(serde_json::Value::String(parts))
            }
            20004 => {
                // TIME_OF_DAY (ms as u32)
                let total_ms = self.read_u32()?;
                let h = total_ms / 3_600_000;
                let m = (total_ms % 3_600_000) / 60_000;
                let s = (total_ms % 60_000) / 1000;
                let ms = total_ms % 1000;
                if ms > 0 {
                    Ok(serde_json::Value::String(format!(
                        "TOD#{:02}:{:02}:{:02}.{:03}",
                        h, m, s, ms
                    )))
                } else {
                    Ok(serde_json::Value::String(format!(
                        "TOD#{:02}:{:02}:{:02}",
                        h, m, s
                    )))
                }
            }
            20002 => {
                // DATE
                let unix_secs = self.read_u32()? as i64;
                let dt = chrono::DateTime::from_timestamp(unix_secs, 0).unwrap_or_default();
                Ok(serde_json::Value::String(format!(
                    "D#{}",
                    dt.format("%Y-%-m-%-d")
                )))
            }
            20003 => {
                // DATE_AND_TIME
                let unix_secs = self.read_u32()? as i64;
                let dt = chrono::DateTime::from_timestamp(unix_secs, 0).unwrap_or_default();
                Ok(serde_json::Value::String(format!(
                    "DT#{}",
                    dt.format("%Y-%-m-%-d-%H:%M:%S")
                )))
            }
            20005 => {
                // ENUM (recursive)
                // Read underlying type, then value
                let underlying = self.read_i16()? as i32;
                match underlying {
                    1 | 9 => {
                        let v = self.read_u8()?;
                        Ok(serde_json::json!(v))
                    }
                    2 | 10 => {
                        let v = self.read_u16()?;
                        Ok(serde_json::json!(v))
                    }
                    3 | 11 => {
                        let v = self.read_u32()?;
                        Ok(serde_json::json!(v))
                    }
                    15 => {
                        let b = self.read_bytes(8)?;
                        Ok(serde_json::json!(u64::from_le_bytes([
                            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                        ])))
                    }
                    _ => {
                        tracing::warn!("Unknown enum underlying type: {}", underlying);
                        Ok(serde_json::Value::Null)
                    }
                }
            }
            20006 => {
                // WSTRING (UTF-16LE)
                // Length in characters (1 byte), data is len*2 bytes UTF-16LE
                let char_count = self.read_u8()? as usize;
                let byte_count = char_count * 2;
                let raw = self.read_bytes(byte_count)?;
                // Decode UTF-16LE
                let u16_vals: Vec<u16> = raw
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let s = String::from_utf16_lossy(&u16_vals);
                Ok(serde_json::Value::String(s))
            }
            _ => {
                tracing::warn!("Unknown value type: {}", val_type);
                Ok(serde_json::Value::Null)
            }
        }
    }

    /// Read a typed value when the type is already known (v2 protocol)
    fn read_value_with_type(&mut self, val_type: i32) -> Result<serde_json::Value> {
        match val_type {
            0 => Ok(serde_json::Value::Null),
            1 | 9 => {
                let v = self.read_u8()?;
                Ok(serde_json::json!(v))
            } // BYTE/USINT
            2 | 10 => {
                let v = self.read_u16()?;
                Ok(serde_json::json!(v))
            } // WORD/UINT
            3 | 11 => {
                let v = self.read_u32()?;
                Ok(serde_json::json!(v))
            } // DWORD/UDINT
            4 => {
                // REAL (f32)
                let b = self.read_bytes(4)?;
                Ok(serde_json::json!(f32::from_le_bytes([
                    b[0], b[1], b[2], b[3]
                ])))
            }
            5 => {
                // LREAL (f64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(f64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            6 => {
                let v = self.read_u8()? as i8;
                Ok(serde_json::json!(v))
            } // SINT
            7 => {
                let v = self.read_i16()?;
                Ok(serde_json::json!(v))
            } // INT
            8 => {
                let v = self.read_i32()?;
                Ok(serde_json::json!(v))
            } // DINT
            12 => {
                let s = self.read_string()?;
                Ok(serde_json::Value::String(s))
            } // STRING
            13 => {
                let b = self.read_u8()? != 0;
                Ok(serde_json::Value::Bool(b))
            } // BOOL
            15 => {
                // ULARGE (u64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(u64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            17 => {
                // LARGE (i64)
                let b = self.read_bytes(8)?;
                Ok(serde_json::json!(i64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                ])))
            }
            20000 => {
                // TIME (ms as u32)
                let total_ms = self.read_u32()?;
                let d = total_ms / 86_400_000;
                let h = (total_ms % 86_400_000) / 3_600_000;
                let m = (total_ms % 3_600_000) / 60_000;
                let s = (total_ms % 60_000) / 1000;
                let ms = total_ms % 1000;
                let mut parts = String::from("T#");
                if d > 0 {
                    parts.push_str(&format!("{}d", d));
                }
                if h > 0 {
                    parts.push_str(&format!("{}h", h));
                }
                if m > 0 {
                    parts.push_str(&format!("{}m", m));
                }
                if s > 0 || (d == 0 && h == 0 && m == 0 && ms == 0) {
                    parts.push_str(&format!("{}s", s));
                }
                if ms > 0 {
                    parts.push_str(&format!("{}ms", ms));
                }
                Ok(serde_json::Value::String(parts))
            }
            20001 => {
                // LTIME (100ns as u64)
                let b = self.read_bytes(8)?;
                let ns100 = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                let total_ns = ns100 * 100;
                let d = total_ns / 86_400_000_000_000;
                let h = (total_ns % 86_400_000_000_000) / 3_600_000_000_000;
                let m = (total_ns % 3_600_000_000_000) / 60_000_000_000;
                let s = (total_ns % 60_000_000_000) / 1_000_000_000;
                let ms = (total_ns % 1_000_000_000) / 1_000_000;
                let us = (total_ns % 1_000_000) / 1_000;
                let ns = total_ns % 1_000;
                let mut parts = String::from("LTIME#");
                if d > 0 {
                    parts.push_str(&format!("{}d", d));
                }
                if h > 0 {
                    parts.push_str(&format!("{}h", h));
                }
                if m > 0 {
                    parts.push_str(&format!("{}m", m));
                }
                if s > 0 {
                    parts.push_str(&format!("{}s", s));
                }
                if ms > 0 {
                    parts.push_str(&format!("{}ms", ms));
                }
                if us > 0 {
                    parts.push_str(&format!("{}us", us));
                }
                if ns > 0 {
                    parts.push_str(&format!("{}ns", ns));
                }
                if parts == "LTIME#" {
                    parts.push_str("0s");
                }
                Ok(serde_json::Value::String(parts))
            }
            20004 => {
                // TIME_OF_DAY (ms as u32)
                let total_ms = self.read_u32()?;
                let h = total_ms / 3_600_000;
                let m = (total_ms % 3_600_000) / 60_000;
                let s = (total_ms % 60_000) / 1000;
                let ms = total_ms % 1000;
                if ms > 0 {
                    Ok(serde_json::Value::String(format!(
                        "TOD#{:02}:{:02}:{:02}.{:03}",
                        h, m, s, ms
                    )))
                } else {
                    Ok(serde_json::Value::String(format!(
                        "TOD#{:02}:{:02}:{:02}",
                        h, m, s
                    )))
                }
            }
            20002 => {
                // DATE
                let unix_secs = self.read_u32()? as i64;
                let dt = chrono::DateTime::from_timestamp(unix_secs, 0).unwrap_or_default();
                Ok(serde_json::Value::String(format!(
                    "D#{}",
                    dt.format("%Y-%-m-%-d")
                )))
            }
            20003 => {
                // DATE_AND_TIME
                let unix_secs = self.read_u32()? as i64;
                let dt = chrono::DateTime::from_timestamp(unix_secs, 0).unwrap_or_default();
                Ok(serde_json::Value::String(format!(
                    "DT#{}",
                    dt.format("%Y-%-m-%-d-%H:%M:%S")
                )))
            }
            20005 => {
                // ENUM (recursive)
                // Read underlying type, then value
                let underlying = self.read_u8()? as i32;
                match underlying {
                    1 | 9 => {
                        let v = self.read_u8()?;
                        Ok(serde_json::json!(v))
                    }
                    2 | 10 => {
                        let v = self.read_u16()?;
                        Ok(serde_json::json!(v))
                    }
                    3 | 11 => {
                        let v = self.read_u32()?;
                        Ok(serde_json::json!(v))
                    }
                    15 => {
                        let b = self.read_bytes(8)?;
                        Ok(serde_json::json!(u64::from_le_bytes([
                            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
                        ])))
                    }
                    _ => {
                        tracing::warn!("Unknown enum underlying type: {}", underlying);
                        Ok(serde_json::Value::Null)
                    }
                }
            }
            20006 => {
                // WSTRING (UTF-16LE)
                // Length in characters (1 byte), data is len*2 bytes UTF-16LE
                let char_count = self.read_u8()? as usize;
                let byte_count = char_count * 2;
                let raw = self.read_bytes(byte_count)?;
                // Decode UTF-16LE
                let u16_vals: Vec<u16> = raw
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let s = String::from_utf16_lossy(&u16_vals);
                Ok(serde_json::Value::String(s))
            }
            _ => {
                tracing::warn!("Unknown value type: {}", val_type);
                Ok(serde_json::Value::Null)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    // Helper function to build test payloads
    /// Build test payload matching real PLC FB_LogEntry format:
    /// - Strings: 1-byte length prefix (u8) + data
    /// - Level: 2 bytes (u16 LE, _WriteUInt)
    /// - Timestamps: 8 bytes each (FILETIME)
    /// - task_index: 4 bytes (i32, _WriteDInt)
    /// - cycle_counter: 4 bytes (u32, _WriteUDInt)
    /// - online_change_count: 4 bytes (u32, _WriteUDInt)
    fn build_test_payload(message: &str, logger: &str, level: u8) -> Vec<u8> {
        let mut payload = vec![1]; // version byte

        // Message (1-byte len + data)
        payload.push(message.len() as u8);
        payload.extend_from_slice(message.as_bytes());

        // Logger (1-byte len + data)
        payload.push(logger.len() as u8);
        payload.extend_from_slice(logger.as_bytes());

        // Level (2 bytes, u16 LE)
        payload.extend_from_slice(&(level as u16).to_le_bytes());

        // Timestamps (FILETIME: 100-ns intervals since 1601-01-01)
        let unix_now = Utc::now().timestamp() as u64;
        let filetime = (unix_now * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes()); // plc_timestamp
        payload.extend_from_slice(&filetime.to_le_bytes()); // clock_timestamp

        // Task metadata
        payload.extend_from_slice(&1i32.to_le_bytes()); // task_index (_WriteDInt)
        let task_name = "MainTask";
        payload.push(task_name.len() as u8); // 1-byte string len
        payload.extend_from_slice(task_name.as_bytes());
        payload.extend_from_slice(&100u32.to_le_bytes()); // cycle_counter (_WriteUDInt)

        // App metadata
        let app_name = "TestApp";
        payload.push(app_name.len() as u8);
        payload.extend_from_slice(app_name.as_bytes());

        let project_name = "TestProject";
        payload.push(project_name.len() as u8);
        payload.extend_from_slice(project_name.as_bytes());

        payload.extend_from_slice(&0u32.to_le_bytes()); // online_change_count

        // End marker
        payload.push(0);

        payload
    }

    #[test]
    fn test_bytes_reader() {
        let data = vec![1, 2, 3, 4, 5];
        let mut reader = BytesReader::new(&data);
        assert_eq!(reader.read_u8().unwrap(), 1);
        assert_eq!(reader.remaining(), 4);
    }

    #[test]
    fn test_parse_minimal_log_entry() {
        let payload = build_test_payload("Test message", "test.logger", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok());

        let entry = result.unwrap();
        assert_eq!(entry.message, "Test message");
        assert_eq!(entry.logger, "test.logger");
        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.version, AdsProtocolVersion::V1);
    }

    #[test]
    fn test_parse_with_all_log_levels() {
        let levels = vec![
            (0, LogLevel::Trace),
            (1, LogLevel::Debug),
            (2, LogLevel::Info),
            (3, LogLevel::Warn),
            (4, LogLevel::Error),
            (5, LogLevel::Fatal),
        ];

        for (level_byte, expected_level) in levels {
            let payload = build_test_payload("Test", "logger", level_byte);
            let entry = AdsParser::parse(&payload).unwrap();
            assert_eq!(
                entry.level, expected_level,
                "Level mismatch for byte {}",
                level_byte
            );
        }
    }

    #[test]
    fn test_parse_empty_strings() {
        let payload = build_test_payload("", "", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok());

        let entry = result.unwrap();
        assert_eq!(entry.message, "");
        assert_eq!(entry.logger, "");
    }

    #[test]
    fn test_parse_string_encoding_utf8() {
        let payload = build_test_payload("Hello 世界 🌍", "logger.café", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok());

        let entry = result.unwrap();
        assert_eq!(entry.message, "Hello 世界 🌍");
        assert_eq!(entry.logger, "logger.café");
    }

    #[test]
    fn test_parse_invalid_version() {
        let mut payload = vec![255]; // Invalid version
        payload.push(1); // message length (1 byte)
        payload.push(b'A');

        let result = AdsParser::parse(&payload);
        assert!(result.is_err());
        match result {
            Err(AdsError::InvalidVersion(v)) => assert_eq!(v, 255),
            _ => panic!("Expected InvalidVersion error"),
        }
    }

    #[test]
    fn test_parse_invalid_log_level() {
        let mut payload = vec![1]; // version
        payload.push(4); // message length (1 byte)
        payload.extend_from_slice(b"test");
        payload.push(6); // logger length (1 byte)
        payload.extend_from_slice(b"logger");
        payload.extend_from_slice(&99u16.to_le_bytes()); // Invalid level (2 bytes)

        let result = AdsParser::parse(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_incomplete_message() {
        let payload = vec![1, 0, 5]; // Version + incomplete string length
        let result = AdsParser::parse(&payload);
        assert!(result.is_err());
        match result {
            Err(AdsError::IncompleteMessage { .. }) => (),
            _ => panic!("Expected IncompleteMessage error"),
        }
    }

    #[test]
    fn test_parse_buffer_overflow_detection() {
        let mut payload = vec![1]; // version
        payload.push(255); // Claims 255 byte message (1 byte length)
        payload.extend_from_slice(b"short"); // But only provides 5 bytes

        let result = AdsParser::parse(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_filetime_conversion() {
        // Test conversion of FILETIME to Unix timestamp
        // FILETIME 132900000000000000 should convert to a valid Unix timestamp
        let payload = build_test_payload("Test", "logger", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok());

        let entry = result.unwrap();
        // Verify the timestamps are reasonable (within a few seconds of now)
        let now = Utc::now();
        let diff = (now - entry.plc_timestamp).num_seconds().abs();
        assert!(
            diff < 10,
            "Parsed timestamp should be close to now, diff: {} seconds",
            diff
        );
    }

    #[test]
    fn test_parse_large_message() {
        // Create a message up to 255 bytes (max for 1-byte length prefix)
        let large_message = "x".repeat(255);
        let payload = build_test_payload(&large_message, "logger", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok());

        let entry = result.unwrap();
        assert_eq!(entry.message.len(), 255);
    }

    #[test]
    fn test_bytes_reader_remaining() {
        let data = vec![1, 2, 3, 4, 5];
        let mut reader = BytesReader::new(&data);
        assert_eq!(reader.remaining(), 5);
        let _ = reader.read_u8();
        assert_eq!(reader.remaining(), 4);
        let _ = reader.read_bytes(2);
        assert_eq!(reader.remaining(), 2);
    }

    #[test]
    fn test_bytes_reader_read_i32() {
        let data: Vec<u8> = 42i32.to_le_bytes().to_vec();
        let mut reader = BytesReader::new(&data);
        assert_eq!(reader.read_i32().unwrap(), 42);
    }

    #[test]
    fn test_bytes_reader_read_u32() {
        let data: Vec<u8> = 1000u32.to_le_bytes().to_vec();
        let mut reader = BytesReader::new(&data);
        assert_eq!(reader.read_u32().unwrap(), 1000);
    }

    #[test]
    fn test_bytes_reader_read_string() {
        let text = "Hello";
        let mut data = vec![text.len() as u8]; // 1-byte length prefix
        data.extend_from_slice(text.as_bytes());

        let mut reader = BytesReader::new(&data);
        assert_eq!(reader.read_string().unwrap(), "Hello");
    }

    #[test]
    fn test_bytes_reader_invalid_utf8() {
        let mut data = vec![2u8]; // 1-byte length prefix
        data.push(0xFF);
        data.push(0xFF); // Invalid UTF-8 sequence

        let mut reader = BytesReader::new(&data);
        let result = reader.read_string();
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_positional_arguments() {
        let mut payload = build_test_payload("User {0} logged in", "auth.logger", 2);

        // Add positional argument (value type 8 = DINT)
        // Remove the end marker first
        payload.pop();
        payload.push(1); // type_id = argument
        payload.push(0); // index = 0
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
        payload.extend_from_slice(&123i32.to_le_bytes());
        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments.len(), 1);
        assert_eq!(entry.arguments[&0], serde_json::json!(123));
    }

    #[test]
    fn test_parse_with_context_variables() {
        let mut payload = build_test_payload("Test", "logger", 2);

        // Add context variable
        payload.pop(); // Remove end marker
        payload.push(2); // type_id = context
        payload.push(1); // scope = 1
        let ctx_name = "request_id";
        payload.push(ctx_name.len() as u8); // 1-byte length for string name
        payload.extend_from_slice(ctx_name.as_bytes());
        payload.extend_from_slice(&12i16.to_le_bytes()); // value type = STRING (2 bytes)
        let ctx_value = "req-12345";
        payload.push(ctx_value.len() as u8); // 1-byte length for string value
        payload.extend_from_slice(ctx_value.as_bytes());
        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.context.len(), 1);
        assert_eq!(
            entry.context["scope_1_request_id"],
            serde_json::json!("req-12345")
        );
    }

    #[test]
    fn test_parse_multiple_arguments() {
        let mut payload = build_test_payload("Test {0} {1} {2}", "logger", 2);

        // Add multiple arguments
        payload.pop(); // Remove end marker

        // Argument 0: DINT 42
        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
        payload.extend_from_slice(&42i32.to_le_bytes());

        // Argument 1: STRING "test"
        payload.push(1); // type_id = argument
        payload.push(1); // index
        payload.extend_from_slice(&12i16.to_le_bytes()); // value type = STRING (2 bytes)
        payload.push(4); // 1-byte length for "test"
        payload.extend_from_slice(b"test");

        // Argument 2: BOOL true
        payload.push(1); // type_id = argument
        payload.push(2); // index
        payload.extend_from_slice(&13i16.to_le_bytes()); // value type = BOOL (2 bytes)
        payload.push(1); // value = true

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments.len(), 3);
        assert_eq!(entry.arguments[&0], serde_json::json!(42));
        assert_eq!(entry.arguments[&1], serde_json::json!("test"));
        assert_eq!(entry.arguments[&2], serde_json::json!(true));
    }

    #[test]
    fn test_parse_value_types() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        // Test null
        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&0i16.to_le_bytes()); // type = null (2 bytes)

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments[&0], serde_json::Value::Null);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_parse_float_argument() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&5i16.to_le_bytes()); // value type = LREAL (2 bytes)
        payload.extend_from_slice(&3.14f64.to_le_bytes());

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        let value = &entry.arguments[&0];
        assert!(value.is_number());
    }

    // Additional edge case tests
    #[test]
    fn test_parse_max_u32_value() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
        payload.extend_from_slice(&(u32::MAX as i32).to_le_bytes());

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert!(entry.arguments.contains_key(&0));
    }

    #[test]
    fn test_parse_negative_numbers() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
        payload.extend_from_slice(&(-42i32).to_le_bytes());

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments[&0], serde_json::json!(-42));
    }

    #[test]
    fn test_parse_long_context_name() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        payload.push(2); // type_id = context
        payload.push(1); // scope
        let long_name = "x".repeat(1000);
        payload.push(long_name.len() as u8); // 1-byte length for context name
        payload.extend_from_slice(long_name.as_bytes());
        payload.extend_from_slice(&12i16.to_le_bytes()); // value type = STRING (2 bytes)
        let value = "test";
        payload.push(value.len() as u8); // 1-byte length for value
        payload.extend_from_slice(value.as_bytes());

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.context.len(), 1);
    }

    #[test]
    fn test_parse_many_arguments() {
        let mut payload = build_test_payload("Test", "logger", 2);
        payload.pop(); // Remove end marker

        // Add 32 arguments (the maximum allowed)
        for i in 0i32..32 {
            payload.push(1); // type_id = argument
            payload.push(i as u8); // index
            payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
            payload.extend_from_slice(&i.to_le_bytes());
        }

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments.len(), 32);
    }

    #[test]
    fn test_parse_roundtrip_utf8_emoji() {
        let emoji_message = "System event 🎉 alert ⚠️ error ❌";
        let payload = build_test_payload(emoji_message, "logger", 2);
        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.message, emoji_message);
    }

    #[test]
    fn test_parse_cjk_characters() {
        let cjk_message = "日本語メッセージ 中文信息 한국어 메시지";
        let payload = build_test_payload(cjk_message, "logger", 2);
        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.message, cjk_message);
    }

    #[test]
    fn test_parse_control_characters() {
        let msg = "Message\twith\ttabs\nand\nnewlines";
        let payload = build_test_payload(msg, "logger", 2);
        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.message, msg);
    }

    #[test]
    fn test_filetime_boundary_unix_epoch() {
        // Test timestamp conversion around Unix epoch boundaries
        let payload = build_test_payload("Test", "logger", 2);
        let entry = AdsParser::parse(&payload).unwrap();

        // Should have valid timestamps
        assert!(entry.plc_timestamp.timestamp() > 0);
        assert!(entry.clock_timestamp.timestamp() > 0);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_parse_mixed_argument_types() {
        let mut payload = build_test_payload("Test {0} {1} {2} {3} {4}", "logger", 2);
        payload.pop(); // Remove end marker

        // DINT 100
        payload.push(1); // type_id = argument
        payload.push(0); // index
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT
        payload.extend_from_slice(&100i32.to_le_bytes());

        // LREAL 2.71828
        payload.push(1); // type_id = argument
        payload.push(1); // index
        payload.extend_from_slice(&5i16.to_le_bytes()); // value type = LREAL
        payload.extend_from_slice(&2.71828f64.to_le_bytes());

        // STRING "hello"
        payload.push(1); // type_id = argument
        payload.push(2); // index
        payload.extend_from_slice(&12i16.to_le_bytes()); // value type = STRING
        payload.push(5); // 1-byte length
        payload.extend_from_slice(b"hello");

        // BOOL true
        payload.push(1); // type_id = argument
        payload.push(3); // index
        payload.extend_from_slice(&13i16.to_le_bytes()); // value type = BOOL
        payload.push(1); // true

        // BOOL false
        payload.push(1); // type_id = argument
        payload.push(4); // index
        payload.extend_from_slice(&13i16.to_le_bytes()); // value type = BOOL
        payload.push(0); // false

        payload.push(0); // end marker

        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.arguments.len(), 5);
        assert!(entry.arguments[&0].is_number());
        assert!(entry.arguments[&1].is_number());
        assert!(entry.arguments[&2].is_string());
        assert!(entry.arguments[&3].is_boolean());
        assert!(entry.arguments[&4].is_boolean());
    }

    // Tests for v2 protocol
    #[test]
    fn test_parse_v2_minimal() {
        let mut payload = Vec::new();
        payload.push(2); // type = v2

        // entry_length (placeholder, will be updated)
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());

        // Fixed header (27 - 3 = 24 bytes after entry_length)
        payload.push(2); // level = Info

        // Timestamps (FILETIME: 100-ns intervals)
        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes()); // plc_timestamp
        payload.extend_from_slice(&filetime.to_le_bytes()); // clock_timestamp

        payload.push(1); // task_index
        payload.extend_from_slice(&100u32.to_le_bytes()); // cycle_counter
        payload.push(0); // arg_count
        payload.push(0); // context_count

        // Message string
        let msg = "Test v2 message";
        payload.push(msg.len() as u8);
        payload.extend_from_slice(msg.as_bytes());

        // Logger string (empty = global)
        payload.push(0);

        // Update entry_length
        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.registrations.len(), 0);

        let entry = &result.entries[0];
        assert_eq!(entry.version, AdsProtocolVersion::V2);
        assert_eq!(entry.message, msg);
        assert_eq!(entry.level, LogLevel::Info);
    }

    #[test]
    fn test_parse_v2_with_arguments() {
        let mut payload = Vec::new();
        payload.push(2); // type = v2

        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());

        payload.push(2); // level = Info

        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes());
        payload.extend_from_slice(&filetime.to_le_bytes());

        payload.push(1); // task_index
        payload.extend_from_slice(&100u32.to_le_bytes());
        payload.push(1); // arg_count = 1
        payload.push(0); // context_count = 0

        let msg = b"Test: {0}";
        payload.push(msg.len() as u8); // message length
        payload.extend_from_slice(msg);
        payload.push(0); // logger empty

        // Argument: DINT value (type_id = 8, which is standard type, not remapped)
        payload.push(8); // type_id (1 byte in v2)
        payload.extend_from_slice(&42i32.to_le_bytes());

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 1);

        let entry = &result.entries[0];
        assert_eq!(entry.arguments.len(), 1);
        assert_eq!(entry.arguments[&1], serde_json::json!(42)); // V2: arg_idx+1
    }

    #[test]
    fn test_parse_registration() {
        let mut payload = Vec::new();
        payload.push(3); // type = registration
        payload.push(5); // task_index

        let task_name = "MainTask";
        payload.push(task_name.len() as u8);
        payload.extend_from_slice(task_name.as_bytes());

        let app_name = "MyApp";
        payload.push(app_name.len() as u8);
        payload.extend_from_slice(app_name.as_bytes());

        let project_name = "MyProject";
        payload.push(project_name.len() as u8);
        payload.extend_from_slice(project_name.as_bytes());

        payload.extend_from_slice(&123u32.to_le_bytes()); // online_change_count

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 0);
        assert_eq!(result.registrations.len(), 1);

        let reg = &result.registrations[0];
        assert_eq!(reg.task_index, 5);
        assert_eq!(reg.task_name, task_name);
        assert_eq!(reg.app_name, app_name);
        assert_eq!(reg.project_name, project_name);
        assert_eq!(reg.online_change_count, 123);
    }

    #[test]
    fn test_parse_mixed_v1_v2_registration() {
        let mut payload = Vec::new();

        // First: Registration (0x03)
        payload.push(3);
        payload.push(1); // task_index
        payload.push(4); // "Task"
        payload.extend_from_slice(b"Task");
        payload.push(3); // "App"
        payload.extend_from_slice(b"App");
        payload.push(4); // "Proj"
        payload.extend_from_slice(b"Proj");
        payload.extend_from_slice(&0u32.to_le_bytes());

        // Second: v1 entry
        let v1_payload = build_test_payload("V1 message", "v1.logger", 2);
        payload.extend_from_slice(&v1_payload);

        // Third: v2 entry
        payload.push(2); // type = v2
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.push(3); // level = Warn
        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes());
        payload.extend_from_slice(&filetime.to_le_bytes());
        payload.push(2); // task_index
        payload.extend_from_slice(&50u32.to_le_bytes());
        payload.push(0); // arg_count
        payload.push(0); // context_count
        payload.push(5); // "V2msg"
        payload.extend_from_slice(b"V2msg");
        payload.push(0); // logger empty

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.registrations.len(), 1);

        // Check registration
        assert_eq!(result.registrations[0].task_name, "Task");

        // Check v1 entry
        assert_eq!(result.entries[0].version, AdsProtocolVersion::V1);
        assert_eq!(result.entries[0].message, "V1 message");

        // Check v2 entry
        assert_eq!(result.entries[1].version, AdsProtocolVersion::V2);
        assert_eq!(result.entries[1].message, "V2msg");
        assert_eq!(result.entries[1].level, LogLevel::Warn);
    }

    #[test]
    fn test_parse_v2_with_context() {
        let mut payload = Vec::new();
        payload.push(2); // type = v2

        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());

        payload.push(2); // level
        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes());
        payload.extend_from_slice(&filetime.to_le_bytes());

        payload.push(1); // task_index
        payload.extend_from_slice(&100u32.to_le_bytes());
        payload.push(0); // arg_count
        payload.push(1); // context_count = 1 (one scope group)

        payload.push(5); // message
        payload.extend_from_slice(b"Test!");
        payload.push(0); // logger empty

        // Context scope group
        payload.push(1); // scope = 1
        payload.push(1); // prop_count = 1 property in this scope

        // Property: request_id
        let prop_name = "request_id";
        payload.push(prop_name.len() as u8);
        payload.extend_from_slice(prop_name.as_bytes());
        payload.push(12); // type_id = STRING
        let prop_val = "req-123";
        payload.push(prop_val.len() as u8);
        payload.extend_from_slice(prop_val.as_bytes());

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 1);

        let entry = &result.entries[0];
        assert_eq!(entry.context.len(), 1);
        assert!(entry.context.contains_key("scope_1_request_id"));
        assert_eq!(
            entry.context["scope_1_request_id"],
            serde_json::json!("req-123")
        );
    }

    #[test]
    fn test_parse_multiple_registrations() {
        let mut payload = Vec::new();

        // Two registrations for different tasks
        for task_idx in 0..2 {
            payload.push(3); // type = registration
            payload.push(task_idx); // task_index
            let task_name = format!("Task{}", task_idx);
            payload.push(task_name.len() as u8);
            payload.extend_from_slice(task_name.as_bytes());
            payload.push(3); // "App"
            payload.extend_from_slice(b"App");
            payload.push(4); // "Proj"
            payload.extend_from_slice(b"Proj");
            payload.extend_from_slice(&(task_idx as u32).to_le_bytes());
        }

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.registrations.len(), 2);
        assert_eq!(result.registrations[0].task_index, 0);
        assert_eq!(result.registrations[1].task_index, 1);
    }

    #[test]
    fn test_parse_v1_v1_entries_in_buffer() {
        // Two v1 entries in one buffer
        let mut payload = build_test_payload("First", "log1", 2);
        payload.pop(); // Remove end marker of first entry
        payload.push(0); // Add end marker between entries

        let second = build_test_payload("Second", "log2", 3);
        payload.extend_from_slice(&second);

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].message, "First");
        assert_eq!(result.entries[1].message, "Second");
    }

    // === Span parser tests ===

    /// Build a minimal span payload for wire type 0x05.
    /// Returns (payload, entry_length_offset) for tests that need to manipulate length.
    fn build_span_payload(
        name: &str,
        kind: u8,
        status_code: u8,
        events: &[(
            &str,
            &[(&str, u8, &[u8])], // (event_name, &[(attr_key, type_id, value_bytes)])
        )],
        attrs: &[(&str, u8, &[u8])], // (attr_key, type_id, value_bytes)
    ) -> Vec<u8> {
        let mut payload = vec![5u8]; // type = span

        // entry_length placeholder
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());

        // trace_id (16 bytes)
        let trace_id: [u8; 16] = [
            0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56,
            0x78, 0x90,
        ];
        payload.extend_from_slice(&trace_id);

        // span_id (8 bytes)
        let span_id: [u8; 8] = [0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef];
        payload.extend_from_slice(&span_id);

        // parent_span_id (8 bytes, zeros = root span)
        payload.extend_from_slice(&[0u8; 8]);

        // kind, status_code
        payload.push(kind);
        payload.push(status_code);

        // Timestamps (FILETIME)
        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes()); // start_time
        payload.extend_from_slice(&filetime.to_le_bytes()); // end_time

        // task_index, cycle_counter
        payload.push(1); // task_index
        payload.extend_from_slice(&500u32.to_le_bytes()); // cycle_counter

        // event_count, attr_count
        payload.push(events.len() as u8);
        payload.push(attrs.len() as u8);

        // name string
        payload.push(name.len() as u8);
        payload.extend_from_slice(name.as_bytes());

        // status_message (empty)
        payload.push(0);

        // Events
        for (event_name, event_attrs) in events {
            payload.push(event_name.len() as u8);
            payload.extend_from_slice(event_name.as_bytes());
            payload.extend_from_slice(&filetime.to_le_bytes()); // event timestamp
            payload.push(event_attrs.len() as u8);

            for (key, type_id, value_bytes) in *event_attrs {
                payload.push(key.len() as u8);
                payload.extend_from_slice(key.as_bytes());
                payload.push(*type_id);
                payload.extend_from_slice(value_bytes);
            }
        }

        // Span attributes
        for (key, type_id, value_bytes) in attrs {
            payload.push(key.len() as u8);
            payload.extend_from_slice(key.as_bytes());
            payload.push(*type_id);
            payload.extend_from_slice(value_bytes);
        }

        // Update entry_length
        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        payload
    }

    /// Helper to encode a string value for the test payload (type 12 = STRING)
    fn string_attr_bytes(s: &str) -> Vec<u8> {
        let mut bytes = vec![s.len() as u8];
        bytes.extend_from_slice(s.as_bytes());
        bytes
    }

    #[test]
    fn test_parse_span_minimal() {
        let payload = build_span_payload("motor_cycle", 0, 0, &[], &[]);

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 0);
        assert_eq!(result.registrations.len(), 0);
        assert_eq!(result.spans.len(), 1);

        let span = &result.spans[0];
        assert_eq!(span.name, "motor_cycle");
        assert_eq!(span.kind, SpanKind::Internal);
        assert_eq!(span.status_code, SpanStatusCode::Unset);
        assert_eq!(span.trace_id_hex(), "abcdef1234567890abcdef1234567890");
        assert_eq!(span.span_id_hex(), "1234567890abcdef");
        assert_eq!(span.parent_span_id_hex(), ""); // all zeros
        assert!(span.events.is_empty());
        assert!(span.attributes.is_empty());
        assert_eq!(span.task_index, 1);
        assert_eq!(span.task_cycle_counter, 500);
    }

    #[test]
    fn test_parse_span_all_kinds() {
        for (kind_byte, expected_kind) in [
            (0u8, SpanKind::Internal),
            (1, SpanKind::Server),
            (2, SpanKind::Client),
            (3, SpanKind::Producer),
            (4, SpanKind::Consumer),
        ] {
            let payload = build_span_payload("test", kind_byte, 0, &[], &[]);
            let result = AdsParser::parse_all(&payload).unwrap();
            assert_eq!(
                result.spans[0].kind, expected_kind,
                "Kind mismatch for byte {}",
                kind_byte
            );
        }
    }

    #[test]
    fn test_parse_span_all_status_codes() {
        for (status_byte, expected_status) in [
            (0u8, SpanStatusCode::Unset),
            (1, SpanStatusCode::Ok),
            (2, SpanStatusCode::Error),
        ] {
            let payload = build_span_payload("test", 0, status_byte, &[], &[]);
            let result = AdsParser::parse_all(&payload).unwrap();
            assert_eq!(
                result.spans[0].status_code, expected_status,
                "Status mismatch for byte {}",
                status_byte
            );
        }
    }

    #[test]
    fn test_parse_span_with_state_machine_attributes() {
        let sm_name = string_attr_bytes("MotorController");
        let old_state = string_attr_bytes("IDLE");
        let new_state = string_attr_bytes("RUNNING");

        let attrs: Vec<(&str, u8, &[u8])> = vec![
            ("state_machine.name", 12, &sm_name),
            ("state_machine.transition.old_state", 12, &old_state),
            ("state_machine.transition.new_state", 12, &new_state),
        ];

        let payload = build_span_payload("state_machine_cycle", 0, 1, &[], &attrs);

        let result = AdsParser::parse_all(&payload).unwrap();
        let span = &result.spans[0];

        assert_eq!(span.name, "state_machine_cycle");
        assert_eq!(span.status_code, SpanStatusCode::Ok);
        assert_eq!(span.attributes.len(), 3);
        assert_eq!(
            span.attributes["state_machine.name"],
            serde_json::json!("MotorController")
        );
        assert_eq!(
            span.attributes["state_machine.transition.old_state"],
            serde_json::json!("IDLE")
        );
        assert_eq!(
            span.attributes["state_machine.transition.new_state"],
            serde_json::json!("RUNNING")
        );
    }

    #[test]
    fn test_parse_span_with_event() {
        let old_state = string_attr_bytes("IDLE");
        let new_state = string_attr_bytes("RUNNING");

        let event_attrs: Vec<(&str, u8, &[u8])> = vec![
            ("state_machine.transition.old_state", 12, &old_state),
            ("state_machine.transition.new_state", 12, &new_state),
        ];

        let events: Vec<(&str, &[(&str, u8, &[u8])])> =
            vec![("state_transition", event_attrs.as_slice())];

        let payload = build_span_payload("pump_cycle", 0, 0, &events, &[]);

        let result = AdsParser::parse_all(&payload).unwrap();
        let span = &result.spans[0];

        assert_eq!(span.events.len(), 1);
        assert_eq!(span.events[0].name, "state_transition");
        assert_eq!(span.events[0].attributes.len(), 2);
        assert_eq!(
            span.events[0].attributes["state_machine.transition.old_state"],
            serde_json::json!("IDLE")
        );
        assert_eq!(
            span.events[0].attributes["state_machine.transition.new_state"],
            serde_json::json!("RUNNING")
        );
    }

    #[test]
    fn test_parse_span_with_multiple_events() {
        let old1 = string_attr_bytes("IDLE");
        let new1 = string_attr_bytes("STARTING");
        let old2 = string_attr_bytes("STARTING");
        let new2 = string_attr_bytes("RUNNING");

        let event1_attrs: Vec<(&str, u8, &[u8])> = vec![
            ("state_machine.transition.old_state", 12, &old1),
            ("state_machine.transition.new_state", 12, &new1),
        ];
        let event2_attrs: Vec<(&str, u8, &[u8])> = vec![
            ("state_machine.transition.old_state", 12, &old2),
            ("state_machine.transition.new_state", 12, &new2),
        ];

        let events: Vec<(&str, &[(&str, u8, &[u8])])> = vec![
            ("transition_1", event1_attrs.as_slice()),
            ("transition_2", event2_attrs.as_slice()),
        ];

        let payload = build_span_payload("motor_startup", 0, 0, &events, &[]);

        let result = AdsParser::parse_all(&payload).unwrap();
        let span = &result.spans[0];

        assert_eq!(span.events.len(), 2);
        assert_eq!(span.events[0].name, "transition_1");
        assert_eq!(span.events[1].name, "transition_2");
        assert_eq!(
            span.events[0].attributes["state_machine.transition.new_state"],
            serde_json::json!("STARTING")
        );
        assert_eq!(
            span.events[1].attributes["state_machine.transition.new_state"],
            serde_json::json!("RUNNING")
        );
    }

    #[test]
    fn test_parse_span_invalid_kind() {
        // Build manually with invalid kind byte
        let mut payload = vec![5u8]; // type = span
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&[0u8; 16]); // trace_id
        payload.extend_from_slice(&[0u8; 8]); // span_id
        payload.extend_from_slice(&[0u8; 8]); // parent_span_id
        payload.push(99); // invalid kind
        // Don't need more — should fail here

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_span_invalid_status_code() {
        let mut payload = vec![5u8];
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&[0u8; 16]); // trace_id
        payload.extend_from_slice(&[0u8; 8]); // span_id
        payload.extend_from_slice(&[0u8; 8]); // parent_span_id
        payload.push(0); // valid kind
        payload.push(99); // invalid status code

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mixed_logs_and_spans() {
        let mut payload = Vec::new();

        // First: v1 log entry
        let v1 = build_test_payload("Log message", "logger", 2);
        payload.extend_from_slice(&v1);

        // Second: span entry
        let span = build_span_payload("motor_cycle", 0, 1, &[], &[]);
        payload.extend_from_slice(&span);

        // Third: registration
        payload.push(3); // type = registration
        payload.push(1); // task_index
        payload.push(4);
        payload.extend_from_slice(b"Task");
        payload.push(3);
        payload.extend_from_slice(b"App");
        payload.push(4);
        payload.extend_from_slice(b"Proj");
        payload.extend_from_slice(&0u32.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.spans.len(), 1);
        assert_eq!(result.registrations.len(), 1);

        assert_eq!(result.entries[0].message, "Log message");
        assert_eq!(result.spans[0].name, "motor_cycle");
        assert_eq!(result.registrations[0].task_name, "Task");
    }

    #[test]
    fn test_parse_span_with_numeric_attribute() {
        let dint_bytes = 42i32.to_le_bytes();
        let attrs: Vec<(&str, u8, &[u8])> = vec![("retry_count", 8, &dint_bytes)];

        let payload = build_span_payload("operation", 0, 0, &[], &attrs);

        let result = AdsParser::parse_all(&payload).unwrap();
        let span = &result.spans[0];

        assert_eq!(span.attributes.len(), 1);
        assert_eq!(span.attributes["retry_count"], serde_json::json!(42));
    }

    #[test]
    fn test_parse_span_with_parent() {
        // Build manually to set a non-zero parent_span_id
        let mut payload = vec![5u8];
        let len_pos = payload.len();
        payload.extend_from_slice(&0u16.to_le_bytes());

        // trace_id
        payload.extend_from_slice(&[0xaa; 16]);
        // span_id
        payload.extend_from_slice(&[0xbb; 8]);
        // parent_span_id (non-zero)
        payload.extend_from_slice(&[0xcc; 8]);

        payload.push(0); // kind = Internal
        payload.push(0); // status = Unset

        let filetime = (Utc::now().timestamp() as u64 * 10_000_000) + 116444736000000000;
        payload.extend_from_slice(&filetime.to_le_bytes());
        payload.extend_from_slice(&filetime.to_le_bytes());

        payload.push(1); // task_index
        payload.extend_from_slice(&100u32.to_le_bytes());
        payload.push(0); // event_count
        payload.push(0); // attr_count

        let name = "child_span";
        payload.push(name.len() as u8);
        payload.extend_from_slice(name.as_bytes());
        payload.push(0); // status_message empty

        let entry_len = (payload.len() - len_pos - 2) as u16;
        payload[len_pos..len_pos + 2].copy_from_slice(&entry_len.to_le_bytes());

        let result = AdsParser::parse_all(&payload).unwrap();
        let span = &result.spans[0];

        assert_eq!(span.name, "child_span");
        assert_eq!(span.parent_span_id_hex(), "cccccccccccccccc");
        assert_eq!(span.span_id_hex(), "bbbbbbbbbbbbbbbb");
        assert_eq!(
            span.trace_id_hex(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn test_parse_multiple_spans() {
        let mut payload = Vec::new();

        let span1 = build_span_payload("span_one", 0, 1, &[], &[]);
        let span2 = build_span_payload("span_two", 1, 0, &[], &[]);
        payload.extend_from_slice(&span1);
        payload.extend_from_slice(&span2);

        let result = AdsParser::parse_all(&payload).unwrap();
        assert_eq!(result.spans.len(), 2);
        assert_eq!(result.spans[0].name, "span_one");
        assert_eq!(result.spans[0].kind, SpanKind::Internal);
        assert_eq!(result.spans[0].status_code, SpanStatusCode::Ok);
        assert_eq!(result.spans[1].name, "span_two");
        assert_eq!(result.spans[1].kind, SpanKind::Server);
        assert_eq!(result.spans[1].status_code, SpanStatusCode::Unset);
    }

    #[test]
    fn test_parse_span_incomplete_buffer() {
        // Just a type byte and partial length
        let payload = vec![5u8, 0xFF];
        let result = AdsParser::parse_all(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_result_spans_default_empty() {
        // Verify that parsing a buffer with only log entries
        // still produces an empty spans vector
        let payload = build_test_payload("Test", "logger", 2);
        let result = AdsParser::parse_all(&payload).unwrap();
        assert!(result.spans.is_empty());
    }
}

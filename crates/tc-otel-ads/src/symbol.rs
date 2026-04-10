//! ADS symbol table types and binary parsing
//!
//! Implements parsing of the ADS symbol table as returned by
//! ADSIGRP_SYM_UPLOADINFO (0xF00C) and ADSIGRP_SYM_UPLOAD (0xF00B).

use crate::error::{AdsError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ADS index group constants for symbol operations
pub const ADSIGRP_SYM_HNDBYNAME: u32 = 0xF003;
pub const ADSIGRP_SYM_VALBYNAME: u32 = 0xF004;
pub const ADSIGRP_SYM_VALBYHND: u32 = 0xF005;
pub const ADSIGRP_SYM_RELEASEHND: u32 = 0xF006;
pub const ADSIGRP_SYM_INFOBYNAME: u32 = 0xF007;
pub const ADSIGRP_SYM_VERSION: u32 = 0xF008;
pub const ADSIGRP_SYM_INFOBYNAMEEX: u32 = 0xF009;
pub const ADSIGRP_SYM_UPLOAD: u32 = 0xF00B;
pub const ADSIGRP_SYM_UPLOADINFO: u32 = 0xF00C;

/// Maximum allowed tag subscriptions per PLC (resource-intensive)
pub const MAX_SUBSCRIPTIONS_PER_PLC: usize = 500;

/// Response from ADSIGRP_SYM_UPLOADINFO (0xF00C)
///
/// Contains metadata about the symbol table: counts and sizes needed
/// to allocate the read buffer for ADSIGRP_SYM_UPLOAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolUploadInfo {
    pub symbol_count: u32,
    pub symbol_length: u32,
    pub data_type_count: u32,
    pub data_type_length: u32,
    pub extra_count: u32,
    pub extra_length: u32,
}

impl SymbolUploadInfo {
    /// Parse from 24-byte response data.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 24 {
            return Err(AdsError::IncompleteMessage {
                expected: 24,
                got: data.len(),
            });
        }

        Ok(SymbolUploadInfo {
            symbol_count: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            symbol_length: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            data_type_count: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
            data_type_length: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            extra_count: u32::from_le_bytes([data[16], data[17], data[18], data[19]]),
            extra_length: u32::from_le_bytes([data[20], data[21], data[22], data[23]]),
        })
    }

    /// Serialize to 24 bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&self.symbol_count.to_le_bytes());
        buf.extend_from_slice(&self.symbol_length.to_le_bytes());
        buf.extend_from_slice(&self.data_type_count.to_le_bytes());
        buf.extend_from_slice(&self.data_type_length.to_le_bytes());
        buf.extend_from_slice(&self.extra_count.to_le_bytes());
        buf.extend_from_slice(&self.extra_length.to_le_bytes());
        buf
    }
}

/// A single entry from the ADS symbol table.
///
/// Binary layout (little-endian):
/// - entry_length: u32 (total bytes for this entry)
/// - index_group: u32
/// - index_offset: u32
/// - size: u32 (data size in bytes)
/// - data_type: u32
/// - flags: u32
/// - name_length: u16 (including null terminator)
/// - type_length: u16 (including null terminator)
/// - comment_length: u16 (including null terminator)
/// - name: [u8; name_length]
/// - type_name: [u8; type_length]
/// - comment: [u8; comment_length]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdsSymbolEntry {
    pub index_group: u32,
    pub index_offset: u32,
    pub size: u32,
    pub data_type: u32,
    pub flags: u32,
    pub name: String,
    pub type_name: String,
    pub comment: String,
}

/// Fixed-size portion of a symbol entry (before the variable-length strings)
const SYMBOL_ENTRY_HEADER_SIZE: usize = 4 + 4 + 4 + 4 + 4 + 4 + 2 + 2 + 2; // 30 bytes

impl AdsSymbolEntry {
    /// Parse a single symbol entry from the binary data at the given offset.
    /// Returns the parsed entry and the number of bytes consumed.
    pub fn parse(data: &[u8]) -> Result<(Self, usize)> {
        if data.len() < SYMBOL_ENTRY_HEADER_SIZE {
            return Err(AdsError::IncompleteMessage {
                expected: SYMBOL_ENTRY_HEADER_SIZE,
                got: data.len(),
            });
        }

        let entry_length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() < entry_length {
            return Err(AdsError::IncompleteMessage {
                expected: entry_length,
                got: data.len(),
            });
        }

        let index_group = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let index_offset = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let size = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let data_type = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let flags = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let name_length = u16::from_le_bytes([data[24], data[25]]) as usize;
        let type_length = u16::from_le_bytes([data[26], data[27]]) as usize;
        let comment_length = u16::from_le_bytes([data[28], data[29]]) as usize;

        let strings_start = SYMBOL_ENTRY_HEADER_SIZE;
        let name_end = strings_start + name_length;
        let type_end = name_end + type_length;
        let comment_end = type_end + comment_length;

        if data.len() < comment_end {
            return Err(AdsError::IncompleteMessage {
                expected: comment_end,
                got: data.len(),
            });
        }

        let name = parse_null_terminated_string(&data[strings_start..name_end])?;
        let type_name = parse_null_terminated_string(&data[name_end..type_end])?;
        let comment = parse_null_terminated_string(&data[type_end..comment_end])?;

        Ok((
            AdsSymbolEntry {
                index_group,
                index_offset,
                size,
                data_type,
                flags,
                name,
                type_name,
                comment,
            },
            entry_length,
        ))
    }

    /// Serialize a symbol entry to binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let type_bytes = self.type_name.as_bytes();
        let comment_bytes = self.comment.as_bytes();

        // Lengths include null terminator
        let name_length = (name_bytes.len() + 1) as u16;
        let type_length = (type_bytes.len() + 1) as u16;
        let comment_length = (comment_bytes.len() + 1) as u16;

        let entry_length = SYMBOL_ENTRY_HEADER_SIZE
            + name_length as usize
            + type_length as usize
            + comment_length as usize;

        let mut buf = Vec::with_capacity(entry_length);
        buf.extend_from_slice(&(entry_length as u32).to_le_bytes());
        buf.extend_from_slice(&self.index_group.to_le_bytes());
        buf.extend_from_slice(&self.index_offset.to_le_bytes());
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.data_type.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&name_length.to_le_bytes());
        buf.extend_from_slice(&type_length.to_le_bytes());
        buf.extend_from_slice(&comment_length.to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.push(0); // null terminator
        buf.extend_from_slice(type_bytes);
        buf.push(0); // null terminator
        buf.extend_from_slice(comment_bytes);
        buf.push(0); // null terminator

        buf
    }
}

/// Parsed symbol table with indexed lookup.
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    entries: Vec<AdsSymbolEntry>,
    by_name: HashMap<String, usize>,
}

impl SymbolTable {
    /// Parse a complete symbol table from the ADSIGRP_SYM_UPLOAD response data.
    pub fn parse(data: &[u8], expected_count: u32) -> Result<Self> {
        let mut entries = Vec::with_capacity(expected_count as usize);
        let mut by_name = HashMap::with_capacity(expected_count as usize);
        let mut offset = 0;

        while offset < data.len() {
            let remaining = &data[offset..];
            if remaining.len() < SYMBOL_ENTRY_HEADER_SIZE {
                break;
            }

            let (entry, consumed) = AdsSymbolEntry::parse(remaining)?;
            let idx = entries.len();
            by_name.insert(entry.name.clone(), idx);
            entries.push(entry);
            offset += consumed;
        }

        if (entries.len() as u32) != expected_count {
            tracing::warn!(
                "Symbol table count mismatch: expected {}, parsed {}",
                expected_count,
                entries.len()
            );
        }

        Ok(SymbolTable { entries, by_name })
    }

    /// Get all symbol entries.
    pub fn entries(&self) -> &[AdsSymbolEntry] {
        &self.entries
    }

    /// Look up a symbol by name (case-sensitive).
    pub fn get(&self, name: &str) -> Option<&AdsSymbolEntry> {
        self.by_name.get(name).map(|&idx| &self.entries[idx])
    }

    /// Search symbols by name prefix (case-insensitive).
    pub fn search(&self, prefix: &str) -> Vec<&AdsSymbolEntry> {
        let lower = prefix.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.name.to_lowercase().starts_with(&lower))
            .collect()
    }

    /// Number of symbols in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// ADS Read request payload (for sending symbol browse requests).
///
/// Binary layout:
/// - index_group: u32
/// - index_offset: u32
/// - read_length: u32
#[derive(Debug, Clone)]
pub struct AdsReadRequest {
    pub index_group: u32,
    pub index_offset: u32,
    pub read_length: u32,
}

impl AdsReadRequest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&self.index_group.to_le_bytes());
        buf.extend_from_slice(&self.index_offset.to_le_bytes());
        buf.extend_from_slice(&self.read_length.to_le_bytes());
        buf
    }

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(AdsError::IncompleteMessage {
                expected: 12,
                got: data.len(),
            });
        }
        Ok(AdsReadRequest {
            index_group: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            index_offset: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            read_length: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        })
    }
}

/// ADS Read response payload.
///
/// Binary layout:
/// - result: u32 (0 = success)
/// - read_length: u32
/// - data: [u8; read_length]
#[derive(Debug, Clone)]
pub struct AdsReadResponse {
    pub result: u32,
    pub data: Vec<u8>,
}

impl AdsReadResponse {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(AdsError::IncompleteMessage {
                expected: 8,
                got: data.len(),
            });
        }
        let result = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let read_length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;

        if data.len() < 8 + read_length {
            return Err(AdsError::IncompleteMessage {
                expected: 8 + read_length,
                got: data.len(),
            });
        }

        Ok(AdsReadResponse {
            result,
            data: data[8..8 + read_length].to_vec(),
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data.len());
        buf.extend_from_slice(&self.result.to_le_bytes());
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }
}

/// Parse a null-terminated string from a byte slice.
fn parse_null_terminated_string(data: &[u8]) -> Result<String> {
    // Strip trailing null bytes
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8(data[..end].to_vec())
        .map_err(|e| AdsError::InvalidStringEncoding(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SymbolUploadInfo tests ---

    #[test]
    fn test_upload_info_parse_valid() {
        let info = SymbolUploadInfo {
            symbol_count: 42,
            symbol_length: 8192,
            data_type_count: 10,
            data_type_length: 2048,
            extra_count: 0,
            extra_length: 0,
        };
        let data = info.serialize();
        let parsed = SymbolUploadInfo::parse(&data).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn test_upload_info_parse_too_short() {
        let data = [0u8; 20]; // needs 24
        let err = SymbolUploadInfo::parse(&data).unwrap_err();
        match err {
            AdsError::IncompleteMessage { expected: 24, got: 20 } => {}
            other => panic!("Expected IncompleteMessage, got: {:?}", other),
        }
    }

    #[test]
    fn test_upload_info_roundtrip() {
        let info = SymbolUploadInfo {
            symbol_count: 1000,
            symbol_length: 500_000,
            data_type_count: 200,
            data_type_length: 100_000,
            extra_count: 5,
            extra_length: 1024,
        };
        let serialized = info.serialize();
        assert_eq!(serialized.len(), 24);
        let parsed = SymbolUploadInfo::parse(&serialized).unwrap();
        assert_eq!(parsed, info);
    }

    // --- AdsSymbolEntry tests ---

    fn make_test_entry(name: &str, type_name: &str, comment: &str) -> AdsSymbolEntry {
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0x1000,
            size: 4,
            data_type: 3, // Int32
            flags: 0x0008,
            name: name.to_string(),
            type_name: type_name.to_string(),
            comment: comment.to_string(),
        }
    }

    #[test]
    fn test_symbol_entry_roundtrip() {
        let entry = make_test_entry("MAIN.nCounter", "INT", "A counter variable");
        let serialized = entry.serialize();
        let (parsed, consumed) = AdsSymbolEntry::parse(&serialized).unwrap();
        assert_eq!(parsed, entry);
        assert_eq!(consumed, serialized.len());
    }

    #[test]
    fn test_symbol_entry_empty_comment() {
        let entry = make_test_entry("GVL.bRunning", "BOOL", "");
        let serialized = entry.serialize();
        let (parsed, _) = AdsSymbolEntry::parse(&serialized).unwrap();
        assert_eq!(parsed.comment, "");
        assert_eq!(parsed.name, "GVL.bRunning");
    }

    #[test]
    fn test_symbol_entry_parse_too_short() {
        let data = [0u8; 10]; // needs at least 30
        let err = AdsSymbolEntry::parse(&data).unwrap_err();
        match err {
            AdsError::IncompleteMessage { expected: 30, got: 10 } => {}
            other => panic!("Expected IncompleteMessage(30, 10), got: {:?}", other),
        }
    }

    #[test]
    fn test_symbol_entry_truncated_data() {
        let entry = make_test_entry("MAIN.x", "REAL", "temp");
        let mut serialized = entry.serialize();
        serialized.truncate(serialized.len() - 3); // cut off some string data
        let err = AdsSymbolEntry::parse(&serialized).unwrap_err();
        assert!(matches!(err, AdsError::IncompleteMessage { .. }));
    }

    #[test]
    fn test_symbol_entry_various_types() {
        let entries = vec![
            make_test_entry("MAIN.bFlag", "BOOL", "Boolean flag"),
            make_test_entry("MAIN.fTemp", "REAL", "Temperature"),
            make_test_entry("MAIN.sMessage", "STRING(255)", "Status message"),
            make_test_entry("MAIN.stData", "ST_ProcessData", "Struct type"),
        ];

        for entry in entries {
            let serialized = entry.serialize();
            let (parsed, consumed) = AdsSymbolEntry::parse(&serialized).unwrap();
            assert_eq!(parsed, entry);
            assert_eq!(consumed, serialized.len());
        }
    }

    #[test]
    fn test_symbol_entry_large_offset() {
        let entry = AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0xFFFFFFFF,
            size: 8,
            data_type: 5, // Real64
            flags: 0,
            name: "MAIN.dValue".to_string(),
            type_name: "LREAL".to_string(),
            comment: String::new(),
        };
        let serialized = entry.serialize();
        let (parsed, _) = AdsSymbolEntry::parse(&serialized).unwrap();
        assert_eq!(parsed.index_offset, 0xFFFFFFFF);
    }

    // --- SymbolTable tests ---

    fn make_test_table_data(entries: &[AdsSymbolEntry]) -> Vec<u8> {
        let mut data = Vec::new();
        for entry in entries {
            data.extend(entry.serialize());
        }
        data
    }

    #[test]
    fn test_symbol_table_parse_empty() {
        let table = SymbolTable::parse(&[], 0).unwrap();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_symbol_table_parse_single() {
        let entry = make_test_entry("MAIN.x", "INT", "");
        let data = make_test_table_data(&[entry.clone()]);
        let table = SymbolTable::parse(&data, 1).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get("MAIN.x"), Some(&entry));
    }

    #[test]
    fn test_symbol_table_parse_multiple() {
        let entries = vec![
            make_test_entry("MAIN.nCounter", "INT", "Counter"),
            make_test_entry("MAIN.fTemperature", "REAL", "Temp sensor"),
            make_test_entry("GVL.bEnabled", "BOOL", "System enable"),
        ];
        let data = make_test_table_data(&entries);
        let table = SymbolTable::parse(&data, 3).unwrap();

        assert_eq!(table.len(), 3);
        assert_eq!(table.get("MAIN.nCounter"), Some(&entries[0]));
        assert_eq!(table.get("MAIN.fTemperature"), Some(&entries[1]));
        assert_eq!(table.get("GVL.bEnabled"), Some(&entries[2]));
        assert_eq!(table.get("NONEXISTENT"), None);
    }

    #[test]
    fn test_symbol_table_search_prefix() {
        let entries = vec![
            make_test_entry("MAIN.nCounter", "INT", ""),
            make_test_entry("MAIN.fTemperature", "REAL", ""),
            make_test_entry("GVL.bEnabled", "BOOL", ""),
            make_test_entry("MAIN.bRunning", "BOOL", ""),
        ];
        let data = make_test_table_data(&entries);
        let table = SymbolTable::parse(&data, 4).unwrap();

        let main_symbols = table.search("MAIN.");
        assert_eq!(main_symbols.len(), 3);

        let gvl_symbols = table.search("GVL.");
        assert_eq!(gvl_symbols.len(), 1);
        assert_eq!(gvl_symbols[0].name, "GVL.bEnabled");
    }

    #[test]
    fn test_symbol_table_search_case_insensitive() {
        let entries = vec![
            make_test_entry("MAIN.nCounter", "INT", ""),
            make_test_entry("MAIN.fTemp", "REAL", ""),
        ];
        let data = make_test_table_data(&entries);
        let table = SymbolTable::parse(&data, 2).unwrap();

        let results = table.search("main.");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_symbol_table_entries_accessor() {
        let entries = vec![
            make_test_entry("A", "INT", ""),
            make_test_entry("B", "REAL", ""),
        ];
        let data = make_test_table_data(&entries);
        let table = SymbolTable::parse(&data, 2).unwrap();

        let all = table.entries();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "A");
        assert_eq!(all[1].name, "B");
    }

    // --- AdsReadRequest tests ---

    #[test]
    fn test_read_request_roundtrip() {
        let req = AdsReadRequest {
            index_group: ADSIGRP_SYM_UPLOADINFO,
            index_offset: 0,
            read_length: 24,
        };
        let serialized = req.serialize();
        assert_eq!(serialized.len(), 12);
        let parsed = AdsReadRequest::parse(&serialized).unwrap();
        assert_eq!(parsed.index_group, ADSIGRP_SYM_UPLOADINFO);
        assert_eq!(parsed.index_offset, 0);
        assert_eq!(parsed.read_length, 24);
    }

    #[test]
    fn test_read_request_parse_too_short() {
        let err = AdsReadRequest::parse(&[0u8; 8]).unwrap_err();
        assert!(matches!(err, AdsError::IncompleteMessage { .. }));
    }

    // --- AdsReadResponse tests ---

    #[test]
    fn test_read_response_success_roundtrip() {
        let resp = AdsReadResponse {
            result: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let serialized = resp.serialize();
        let parsed = AdsReadResponse::parse(&serialized).unwrap();
        assert_eq!(parsed.result, 0);
        assert_eq!(parsed.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_read_response_error_code() {
        let resp = AdsReadResponse {
            result: 0x0706, // ADS error: target port not found
            data: vec![],
        };
        let serialized = resp.serialize();
        let parsed = AdsReadResponse::parse(&serialized).unwrap();
        assert_eq!(parsed.result, 0x0706);
        assert!(parsed.data.is_empty());
    }

    #[test]
    fn test_read_response_parse_too_short() {
        let err = AdsReadResponse::parse(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, AdsError::IncompleteMessage { .. }));
    }

    #[test]
    fn test_read_response_truncated_data() {
        // Header says 100 bytes of data but only 8 available
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // result
        buf.extend_from_slice(&100u32.to_le_bytes()); // read_length claims 100
        buf.extend_from_slice(&[0u8; 8]); // only 8 bytes of data
        let err = AdsReadResponse::parse(&buf).unwrap_err();
        assert!(matches!(err, AdsError::IncompleteMessage { .. }));
    }

    // --- parse_null_terminated_string tests ---

    #[test]
    fn test_parse_null_terminated_normal() {
        let data = b"Hello\0";
        let s = parse_null_terminated_string(data).unwrap();
        assert_eq!(s, "Hello");
    }

    #[test]
    fn test_parse_null_terminated_no_null() {
        let data = b"NoNull";
        let s = parse_null_terminated_string(data).unwrap();
        assert_eq!(s, "NoNull");
    }

    #[test]
    fn test_parse_null_terminated_empty() {
        let data = b"\0";
        let s = parse_null_terminated_string(data).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn test_max_subscriptions_constant() {
        assert_eq!(MAX_SUBSCRIPTIONS_PER_PLC, 500);
    }

    // --- Index group constant tests ---

    #[test]
    fn test_index_group_constants() {
        assert_eq!(ADSIGRP_SYM_UPLOAD, 0xF00B);
        assert_eq!(ADSIGRP_SYM_UPLOADINFO, 0xF00C);
        assert_eq!(ADSIGRP_SYM_HNDBYNAME, 0xF003);
        assert_eq!(ADSIGRP_SYM_VALBYHND, 0xF005);
    }
}

//! ADS symbol table types and binary parsing
//!
//! Implements parsing of ADS symbol discovery responses:
//! - Index group 0xF00C: ADSIGRP_SYM_UPLOADINFO (symbol count + total size)
//! - Index group 0xF00B: ADSIGRP_SYM_UPLOAD (full symbol table)
//!
//! Symbol entries are variable-length binary structs with null-terminated
//! name, type, and comment strings.

use crate::error::{AdsError, Result};
use serde::{Deserialize, Serialize};

/// ADS index group for reading symbol upload info (count + total size)
pub const ADSIGRP_SYM_UPLOADINFO: u32 = 0xF00C;

/// ADS index group for reading the full symbol table
pub const ADSIGRP_SYM_UPLOAD: u32 = 0xF00B;

/// Response from ADSIGRP_SYM_UPLOADINFO: summary of the symbol table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdsSymbolUploadInfo {
    /// Number of symbols in the PLC
    pub symbol_count: u32,
    /// Total byte size of the symbol table
    pub symbol_size: u32,
}

impl AdsSymbolUploadInfo {
    /// Parse from binary response data (8 bytes: count u32 LE + size u32 LE)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(AdsError::IncompleteMessage {
                expected: 8,
                got: data.len(),
            });
        }

        let symbol_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let symbol_size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        Ok(AdsSymbolUploadInfo {
            symbol_count,
            symbol_size,
        })
    }

    /// Serialize to binary (8 bytes)
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(&self.symbol_count.to_le_bytes());
        buf.extend_from_slice(&self.symbol_size.to_le_bytes());
        buf
    }
}

/// A single entry from the ADS symbol table (ADSIGRP_SYM_UPLOAD response)
///
/// Binary layout:
/// ```text
/// [entry_length: u32 LE]     -- total entry size in bytes
/// [index_group: u32 LE]      -- index group for read/write access
/// [index_offset: u32 LE]     -- index offset for read/write access
/// [size: u32 LE]             -- data type size in bytes
/// [data_type: u32 LE]        -- ADST_* data type identifier
/// [flags: u32 LE]            -- ADSSYMBOLFLAG_* flags
/// [name_length: u16 LE]      -- symbol name length (without null terminator)
/// [type_length: u16 LE]      -- type name length (without null terminator)
/// [comment_length: u16 LE]   -- comment length (without null terminator)
/// [name: u8 * (name_length + 1)]   -- null-terminated name
/// [type_name: u8 * (type_length + 1)]  -- null-terminated type
/// [comment: u8 * (comment_length + 1)] -- null-terminated comment
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdsSymbolEntry {
    /// Index group for read/write access to this symbol
    pub index_group: u32,
    /// Index offset for read/write access to this symbol
    pub index_offset: u32,
    /// Data type size in bytes
    pub size: u32,
    /// ADS data type identifier (ADST_*)
    pub data_type: u32,
    /// Symbol flags (ADSSYMBOLFLAG_*)
    pub flags: u32,
    /// Symbol name (e.g. "MAIN.bMotorRunning")
    pub name: String,
    /// Type name (e.g. "BOOL", "LREAL", "INT")
    pub type_name: String,
    /// Comment associated with the symbol
    pub comment: String,
}

/// Fixed-size header portion of a symbol entry (before variable-length strings)
const SYMBOL_ENTRY_HEADER_SIZE: usize = 4 + 4 + 4 + 4 + 4 + 4 + 2 + 2 + 2; // 30 bytes

impl AdsSymbolEntry {
    /// Parse a single symbol entry from the start of a buffer.
    /// Returns the parsed entry and the number of bytes consumed.
    pub fn parse(data: &[u8]) -> Result<(Self, usize)> {
        if data.len() < SYMBOL_ENTRY_HEADER_SIZE {
            return Err(AdsError::IncompleteMessage {
                expected: SYMBOL_ENTRY_HEADER_SIZE,
                got: data.len(),
            });
        }

        let entry_length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let index_group = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let index_offset = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let size = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let data_type = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let flags = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let name_length = u16::from_le_bytes([data[24], data[25]]) as usize;
        let type_length = u16::from_le_bytes([data[26], data[27]]) as usize;
        let comment_length = u16::from_le_bytes([data[28], data[29]]) as usize;

        // Strings are null-terminated, so +1 for each
        let strings_size = (name_length + 1) + (type_length + 1) + (comment_length + 1);
        let total_needed = SYMBOL_ENTRY_HEADER_SIZE + strings_size;

        if data.len() < total_needed {
            return Err(AdsError::IncompleteMessage {
                expected: total_needed,
                got: data.len(),
            });
        }

        let mut offset = SYMBOL_ENTRY_HEADER_SIZE;

        let name = parse_null_terminated_string(&data[offset..offset + name_length + 1])?;
        offset += name_length + 1;

        let type_name = parse_null_terminated_string(&data[offset..offset + type_length + 1])?;
        offset += type_length + 1;

        let comment = parse_null_terminated_string(&data[offset..offset + comment_length + 1])?;
        // Don't advance offset — use entry_length for consumed bytes

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

    /// Serialize a symbol entry to binary
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let type_bytes = self.type_name.as_bytes();
        let comment_bytes = self.comment.as_bytes();

        let entry_length =
            SYMBOL_ENTRY_HEADER_SIZE + (name_bytes.len() + 1) + (type_bytes.len() + 1) + (comment_bytes.len() + 1);

        let mut buf = Vec::with_capacity(entry_length);
        buf.extend_from_slice(&(entry_length as u32).to_le_bytes());
        buf.extend_from_slice(&self.index_group.to_le_bytes());
        buf.extend_from_slice(&self.index_offset.to_le_bytes());
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.data_type.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(type_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(comment_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.push(0); // null terminator
        buf.extend_from_slice(type_bytes);
        buf.push(0);
        buf.extend_from_slice(comment_bytes);
        buf.push(0);
        buf
    }
}

/// Parse the full symbol table from ADSIGRP_SYM_UPLOAD response data.
/// Returns all symbol entries found in the buffer.
pub fn parse_symbol_table(data: &[u8]) -> Result<Vec<AdsSymbolEntry>> {
    let mut symbols = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        // Need at least 4 bytes for entry_length
        if offset + 4 > data.len() {
            break;
        }

        let (entry, consumed) = AdsSymbolEntry::parse(&data[offset..])?;
        if consumed == 0 {
            return Err(AdsError::ParseError(
                "symbol entry with zero length".to_string(),
            ));
        }
        symbols.push(entry);
        offset += consumed;
    }

    Ok(symbols)
}

/// Parse a null-terminated string from a byte slice
fn parse_null_terminated_string(data: &[u8]) -> Result<String> {
    // Find the null terminator or use all but the last byte
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8(data[..end].to_vec())
        .map_err(|e| AdsError::InvalidStringEncoding(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upload_info_parse() {
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&42u32.to_le_bytes()); // symbol_count
        data[4..8].copy_from_slice(&1024u32.to_le_bytes()); // symbol_size

        let info = AdsSymbolUploadInfo::parse(&data).unwrap();
        assert_eq!(info.symbol_count, 42);
        assert_eq!(info.symbol_size, 1024);
    }

    #[test]
    fn test_upload_info_roundtrip() {
        let info = AdsSymbolUploadInfo {
            symbol_count: 256,
            symbol_size: 65536,
        };
        let bytes = info.serialize();
        let parsed = AdsSymbolUploadInfo::parse(&bytes).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn test_upload_info_too_short() {
        let data = [0u8; 4]; // only 4 bytes, need 8
        let result = AdsSymbolUploadInfo::parse(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_symbol_entry_parse_basic() {
        let entry = AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0x0000,
            size: 1,
            data_type: 33, // ADST_BIT
            flags: 0x0008,
            name: "MAIN.bFlag".to_string(),
            type_name: "BOOL".to_string(),
            comment: "A test flag".to_string(),
        };

        let bytes = entry.serialize();
        let (parsed, consumed) = AdsSymbolEntry::parse(&bytes).unwrap();

        assert_eq!(parsed.index_group, 0x4020);
        assert_eq!(parsed.index_offset, 0x0000);
        assert_eq!(parsed.size, 1);
        assert_eq!(parsed.data_type, 33);
        assert_eq!(parsed.flags, 0x0008);
        assert_eq!(parsed.name, "MAIN.bFlag");
        assert_eq!(parsed.type_name, "BOOL");
        assert_eq!(parsed.comment, "A test flag");
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_symbol_entry_roundtrip_empty_comment() {
        let entry = AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 256,
            size: 8,
            data_type: 5, // ADST_REAL64
            flags: 0,
            name: "GVL.fTemperature".to_string(),
            type_name: "LREAL".to_string(),
            comment: String::new(),
        };

        let bytes = entry.serialize();
        let (parsed, _) = AdsSymbolEntry::parse(&bytes).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_symbol_entry_header_too_short() {
        let data = [0u8; 10]; // less than 30 bytes
        let result = AdsSymbolEntry::parse(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_symbol_entry_data_truncated() {
        // Valid header but not enough data for strings
        let entry = AdsSymbolEntry {
            index_group: 1,
            index_offset: 2,
            size: 4,
            data_type: 3,
            flags: 0,
            name: "MAIN.longVariableName".to_string(),
            type_name: "INT".to_string(),
            comment: String::new(),
        };

        let bytes = entry.serialize();
        // Truncate the data
        let truncated = &bytes[..bytes.len() - 5];
        let result = AdsSymbolEntry::parse(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_symbol_table_multiple_entries() {
        let entries = vec![
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size: 1,
                data_type: 33,
                flags: 0x0008,
                name: "MAIN.bMotorRunning".to_string(),
                type_name: "BOOL".to_string(),
                comment: "Motor status".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 8,
                size: 8,
                data_type: 5,
                flags: 0x0008,
                name: "MAIN.fSpeed".to_string(),
                type_name: "LREAL".to_string(),
                comment: "Motor speed".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 16,
                size: 2,
                data_type: 3,
                flags: 0x0008,
                name: "GVL.nCycleCount".to_string(),
                type_name: "INT".to_string(),
                comment: String::new(),
            },
        ];

        // Concatenate serialized entries
        let mut data = Vec::new();
        for e in &entries {
            data.extend(e.serialize());
        }

        let parsed = parse_symbol_table(&data).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "MAIN.bMotorRunning");
        assert_eq!(parsed[1].name, "MAIN.fSpeed");
        assert_eq!(parsed[2].name, "GVL.nCycleCount");
    }

    #[test]
    fn test_parse_symbol_table_empty() {
        let parsed = parse_symbol_table(&[]).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_index_group_constants() {
        assert_eq!(ADSIGRP_SYM_UPLOADINFO, 0xF00C);
        assert_eq!(ADSIGRP_SYM_UPLOAD, 0xF00B);
    }

    #[test]
    fn test_symbol_entry_various_data_types() {
        let test_cases = vec![
            ("MAIN.bVal", "BOOL", 1, 33),
            ("MAIN.nVal", "INT", 2, 3),
            ("MAIN.fVal", "REAL", 4, 4),
            ("MAIN.fBigVal", "LREAL", 8, 5),
            ("MAIN.sVal", "STRING(80)", 81, 30),
        ];

        for (name, type_name, size, data_type) in test_cases {
            let entry = AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size,
                data_type,
                flags: 0,
                name: name.to_string(),
                type_name: type_name.to_string(),
                comment: String::new(),
            };

            let bytes = entry.serialize();
            let (parsed, _) = AdsSymbolEntry::parse(&bytes).unwrap();
            assert_eq!(parsed.name, name);
            assert_eq!(parsed.type_name, type_name);
            assert_eq!(parsed.size, size);
            assert_eq!(parsed.data_type, data_type);
        }
    }
}

//! Pure parser for ADS symbol-upload bytes → [`SymbolTree`].
//!
//! The TwinCAT ADS protocol exposes two index groups for enumerating symbols:
//!
//! | Index Group | Purpose | Response |
//! |---|---|---|
//! | `ADSIGRP_SYM_UPLOADINFO2` (0xF00F) | Query table sizes | 24-byte header |
//! | `ADSIGRP_SYM_UPLOAD` (0xF00B) | Dump symbol table | Packed entries |
//!
//! Each entry in the upload blob has a fixed-size header (30 B) followed by three
//! null-terminated strings (name, type, comment) and padding to `entry_length`.
//!
//! This module does **no I/O** — it takes bytes returned by `ads::Device::read`
//! (or a fixture file) and returns parsed structs. The active-client side lives
//! in `crate::client`.

use crate::error::{ClientError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const ADSIGRP_SYM_UPLOADINFO2: u32 = 0xF00F;
pub const ADSIGRP_SYM_UPLOAD: u32 = 0xF00B;
pub const ADSIGRP_SYM_DT_UPLOAD: u32 = 0xF00E;

/// Fixed-size 24-byte response to `SYM_UPLOADINFO2` (Beckhoff).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadInfo {
    pub symbol_count: u32,
    pub symbol_length: u32,
    pub datatype_count: u32,
    pub datatype_length: u32,
    pub extra_count: u32,
    pub extra_length: u32,
}

impl UploadInfo {
    pub const BYTE_LEN: usize = 24;
}

/// Fixed-size header prefix of an `AdsSymbolEntry`.
///
/// Total header width is 30 bytes; the three name/type/comment strings follow
/// and each has `*_length + 1` bytes (null terminator included) on the wire.
#[derive(Debug, Clone, Copy)]
struct EntryHeader {
    entry_length: u32,
    igroup: u32,
    ioffset: u32,
    size: u32,
    datatype: u32,
    flags: u32,
    name_length: u16,
    type_length: u16,
    comment_length: u16,
}

impl EntryHeader {
    const BYTE_LEN: usize = 30;
}

/// A single PLC symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolNode {
    pub name: String,
    pub type_name: String,
    pub comment: String,
    pub igroup: u32,
    pub ioffset: u32,
    pub size: u32,
    pub datatype: u32,
    pub flags: u32,
}

/// Parsed symbol table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolTree {
    pub nodes: Vec<SymbolNode>,
    /// Index of node position by symbol name (case-preserving, case-sensitive lookup).
    index_by_name: HashMap<String, usize>,
}

impl SymbolTree {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&SymbolNode> {
        self.index_by_name
            .get(name)
            .and_then(|&i| self.nodes.get(i))
    }

    /// Build a [`SymbolTree`] from the `ads` crate's already-decoded symbols.
    ///
    /// Used by the service bridge when it leverages `ads::symbol::get_symbol_info`
    /// (which does the full upload + type-map decode in one call) rather than
    /// running our own byte-level parser.
    pub fn from_ads_symbols(symbols: Vec<ads::symbol::Symbol>) -> Self {
        let mut nodes = Vec::with_capacity(symbols.len());
        let mut index_by_name = HashMap::with_capacity(symbols.len());
        for (i, s) in symbols.into_iter().enumerate() {
            index_by_name.insert(s.name.clone(), i);
            nodes.push(SymbolNode {
                name: s.name,
                type_name: s.typ,
                comment: String::new(),
                igroup: s.ix_group,
                ioffset: s.ix_offset,
                size: s.size as u32,
                datatype: s.base_type,
                flags: s.flags,
            });
        }
        Self {
            nodes,
            index_by_name,
        }
    }

    /// Iterate names with a prefix (case-insensitive). Useful for UI filters.
    pub fn iter_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a SymbolNode> + 'a {
        let needle = prefix.to_ascii_lowercase();
        self.nodes
            .iter()
            .filter(move |n| n.name.to_ascii_lowercase().starts_with(&needle))
    }
}

/// Decode a 24-byte `SYM_UPLOADINFO2` response.
pub fn parse_upload_info(bytes: &[u8]) -> Result<UploadInfo> {
    if bytes.len() < UploadInfo::BYTE_LEN {
        return Err(ClientError::Decode(format!(
            "upload_info: need {} bytes, got {}",
            UploadInfo::BYTE_LEN,
            bytes.len()
        )));
    }
    Ok(UploadInfo {
        symbol_count: read_u32(bytes, 0),
        symbol_length: read_u32(bytes, 4),
        datatype_count: read_u32(bytes, 8),
        datatype_length: read_u32(bytes, 12),
        extra_count: read_u32(bytes, 16),
        extra_length: read_u32(bytes, 20),
    })
}

/// Decode the packed symbol table returned by `SYM_UPLOAD`.
///
/// `info` is used as a sanity bound: we stop either at `info.symbol_count`
/// entries or when the byte cursor runs out, whichever comes first. A malformed
/// stream (entry_length pointing past the end) returns `ClientError::Decode`
/// rather than panicking.
pub fn parse_upload(bytes: &[u8], info: &UploadInfo) -> Result<SymbolTree> {
    let expected_bytes = info.symbol_length as usize;
    if bytes.len() < expected_bytes {
        return Err(ClientError::Decode(format!(
            "symbol upload: info says {} bytes, buffer has {}",
            expected_bytes,
            bytes.len()
        )));
    }

    let mut nodes = Vec::with_capacity(info.symbol_count as usize);
    let mut index_by_name = HashMap::with_capacity(info.symbol_count as usize);
    let mut cursor = 0usize;
    let limit = expected_bytes.max(bytes.len());

    while cursor + EntryHeader::BYTE_LEN <= limit && nodes.len() < info.symbol_count as usize {
        let hdr = parse_entry_header(&bytes[cursor..])?;
        let entry_end = cursor
            .checked_add(hdr.entry_length as usize)
            .ok_or_else(|| ClientError::Decode("entry_length overflow".into()))?;
        if entry_end > limit {
            return Err(ClientError::Decode(format!(
                "entry at {} overruns buffer (ends at {}, limit {})",
                cursor, entry_end, limit
            )));
        }

        let strings_start = cursor + EntryHeader::BYTE_LEN;
        // Each string is null-terminated; on-wire length = declared + 1.
        let name_end = strings_start + hdr.name_length as usize;
        let type_end = name_end + 1 + hdr.type_length as usize;
        let comment_end = type_end + 1 + hdr.comment_length as usize;
        if comment_end > entry_end {
            return Err(ClientError::Decode(format!(
                "strings overrun entry at cursor {}",
                cursor
            )));
        }

        let name = read_ascii(&bytes[strings_start..name_end])?;
        let type_name = read_ascii(&bytes[name_end + 1..type_end])?;
        let comment = read_ascii(&bytes[type_end + 1..comment_end])?;

        let idx = nodes.len();
        index_by_name.insert(name.clone(), idx);
        nodes.push(SymbolNode {
            name,
            type_name,
            comment,
            igroup: hdr.igroup,
            ioffset: hdr.ioffset,
            size: hdr.size,
            datatype: hdr.datatype,
            flags: hdr.flags,
        });

        cursor = entry_end;
    }

    Ok(SymbolTree {
        nodes,
        index_by_name,
    })
}

fn parse_entry_header(bytes: &[u8]) -> Result<EntryHeader> {
    if bytes.len() < EntryHeader::BYTE_LEN {
        return Err(ClientError::Decode(format!(
            "entry header: need {} bytes, got {}",
            EntryHeader::BYTE_LEN,
            bytes.len()
        )));
    }
    Ok(EntryHeader {
        entry_length: read_u32(bytes, 0),
        igroup: read_u32(bytes, 4),
        ioffset: read_u32(bytes, 8),
        size: read_u32(bytes, 12),
        datatype: read_u32(bytes, 16),
        flags: read_u32(bytes, 20),
        name_length: read_u16(bytes, 24),
        type_length: read_u16(bytes, 26),
        comment_length: read_u16(bytes, 28),
    })
}

fn read_u16(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn read_ascii(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(|s| s.to_owned())
        .map_err(|e| ClientError::Decode(format!("non-utf8 in symbol string: {e}")))
}

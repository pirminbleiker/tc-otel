//! Tests for `browse::parse_upload_info` / `browse::parse_upload`.
//!
//! The fixture builder below emits byte streams identical to what an XAE
//! capture would yield for the same symbols. Captured-from-real-PLC fixtures
//! can be dropped into `tests/fixtures/` later and exercised with the same
//! assertions.

use tc_otel_client::browse::{parse_upload, parse_upload_info, UploadInfo};

/// Build the 30-byte header + name + type + comment + null terminators
/// for one `AdsSymbolEntry`, and return the packed bytes.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    name: &str,
    type_name: &str,
    comment: &str,
    igroup: u32,
    ioffset: u32,
    size: u32,
    datatype: u32,
    flags: u32,
) -> Vec<u8> {
    let name_b = name.as_bytes();
    let type_b = type_name.as_bytes();
    let com_b = comment.as_bytes();
    // header (30) + name + \0 + type + \0 + comment + \0
    let entry_len = 30 + name_b.len() + 1 + type_b.len() + 1 + com_b.len() + 1;
    let mut out = Vec::with_capacity(entry_len);
    out.extend_from_slice(&(entry_len as u32).to_le_bytes());
    out.extend_from_slice(&igroup.to_le_bytes());
    out.extend_from_slice(&ioffset.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&datatype.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&(type_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&(com_b.len() as u16).to_le_bytes());
    out.extend_from_slice(name_b);
    out.push(0);
    out.extend_from_slice(type_b);
    out.push(0);
    out.extend_from_slice(com_b);
    out.push(0);
    assert_eq!(out.len(), entry_len);
    out
}

fn build_upload_info_bytes(info: &UploadInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&info.symbol_count.to_le_bytes());
    out.extend_from_slice(&info.symbol_length.to_le_bytes());
    out.extend_from_slice(&info.datatype_count.to_le_bytes());
    out.extend_from_slice(&info.datatype_length.to_le_bytes());
    out.extend_from_slice(&info.extra_count.to_le_bytes());
    out.extend_from_slice(&info.extra_length.to_le_bytes());
    out
}

#[test]
fn parses_upload_info_header() {
    let info = UploadInfo {
        symbol_count: 3,
        symbol_length: 200,
        datatype_count: 42,
        datatype_length: 1024,
        extra_count: 0,
        extra_length: 0,
    };
    let bytes = build_upload_info_bytes(&info);
    let parsed = parse_upload_info(&bytes).unwrap();
    assert_eq!(parsed, info);
}

#[test]
fn upload_info_rejects_short_buffer() {
    let too_short = vec![0u8; 16];
    let err = parse_upload_info(&too_short).unwrap_err();
    assert!(err.to_string().contains("need 24 bytes"));
}

#[test]
fn parses_three_scalar_symbols() {
    let e1 = build_entry(
        "MAIN.fTemp",
        "REAL",
        "Kessel-Temperatur",
        0x4040,
        0,
        4,
        4,
        0,
    );
    let e2 = build_entry("MAIN.bRun", "BOOL", "", 0x4040, 4, 1, 33, 0);
    let e3 = build_entry("GVL.nCount", "DINT", "count", 0x4041, 0, 4, 18, 1);

    let body_len = e1.len() + e2.len() + e3.len();
    let mut body = Vec::with_capacity(body_len);
    body.extend(&e1);
    body.extend(&e2);
    body.extend(&e3);

    let info = UploadInfo {
        symbol_count: 3,
        symbol_length: body_len as u32,
        datatype_count: 0,
        datatype_length: 0,
        extra_count: 0,
        extra_length: 0,
    };

    let tree = parse_upload(&body, &info).unwrap();
    assert_eq!(tree.len(), 3);

    let n = tree.get("MAIN.fTemp").unwrap();
    assert_eq!(n.type_name, "REAL");
    assert_eq!(n.comment, "Kessel-Temperatur");
    assert_eq!(n.igroup, 0x4040);
    assert_eq!(n.size, 4);
    assert_eq!(n.datatype, 4);

    let n = tree.get("MAIN.bRun").unwrap();
    assert_eq!(n.type_name, "BOOL");
    assert_eq!(n.comment, "");
    assert_eq!(n.ioffset, 4);
    assert_eq!(n.size, 1);

    let n = tree.get("GVL.nCount").unwrap();
    assert_eq!(n.igroup, 0x4041);
    assert_eq!(n.flags, 1);
}

#[test]
fn prefix_iter_filters_case_insensitive() {
    let e1 = build_entry("MAIN.fTemp", "REAL", "", 0, 0, 4, 4, 0);
    let e2 = build_entry("main.fSetpoint", "REAL", "", 0, 4, 4, 4, 0);
    let e3 = build_entry("GVL.nCount", "DINT", "", 0, 0, 4, 18, 0);
    let body: Vec<u8> = [e1.clone(), e2.clone(), e3.clone()].concat();

    let info = UploadInfo {
        symbol_count: 3,
        symbol_length: body.len() as u32,
        datatype_count: 0,
        datatype_length: 0,
        extra_count: 0,
        extra_length: 0,
    };
    let tree = parse_upload(&body, &info).unwrap();

    let names: Vec<_> = tree.iter_prefix("main").map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["MAIN.fTemp", "main.fSetpoint"]);
}

#[test]
fn rejects_entry_that_overruns_buffer() {
    let mut entry = build_entry("X", "BOOL", "", 0, 0, 1, 33, 0);
    // Corrupt entry_length to point past end.
    let bogus = (entry.len() as u32 + 100).to_le_bytes();
    entry[..4].copy_from_slice(&bogus);

    let info = UploadInfo {
        symbol_count: 1,
        symbol_length: entry.len() as u32,
        datatype_count: 0,
        datatype_length: 0,
        extra_count: 0,
        extra_length: 0,
    };

    let err = parse_upload(&entry, &info).unwrap_err();
    assert!(err.to_string().contains("overruns buffer"));
}

#[test]
fn stops_at_declared_symbol_count_even_with_more_bytes() {
    // Build 3 entries but lie in info: symbol_count=2.
    let e1 = build_entry("A", "BOOL", "", 0, 0, 1, 33, 0);
    let e2 = build_entry("B", "BOOL", "", 0, 1, 1, 33, 0);
    let e3 = build_entry("C", "BOOL", "", 0, 2, 1, 33, 0);
    let body: Vec<u8> = [e1.clone(), e2.clone(), e3.clone()].concat();

    let info = UploadInfo {
        symbol_count: 2,
        symbol_length: body.len() as u32,
        datatype_count: 0,
        datatype_length: 0,
        extra_count: 0,
        extra_length: 0,
    };
    let tree = parse_upload(&body, &info).unwrap();
    assert_eq!(tree.len(), 2);
    assert!(tree.get("A").is_some());
    assert!(tree.get("B").is_some());
    assert!(tree.get("C").is_none());
}

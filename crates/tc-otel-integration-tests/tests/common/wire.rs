//! Byte encoders for the PLC-to-service wire format.
//!
//! The reference builders live as private helpers in other test files, which
//! Cargo compiles as separate crates, so they cannot be imported. They are
//! restated here once; `wire_encoders_parse` in `e2e_socket_otlp.rs`
//! round-trips each encoder through `AdsParser::parse_all` so layout drift in
//! the parser fails there instead of as a timeout in a scenario.

#![allow(clippy::vec_init_then_push)]

fn append_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
}

/// v2 log entry (type 0x02): `[0x02][len: u16 LE][payload]`.
///
/// Mirrors `build_v2_log_bytes` in `log_trace_correlation.rs`.
pub fn v2_log(level: u8, task_index: u8, cycle: u32, message: &str, logger: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(level);
    // plc_timestamp and clock_timestamp as FILETIME
    let filetime: u64 = 116_444_736_000_000_000 + 1_000_000_000;
    payload.extend_from_slice(&filetime.to_le_bytes());
    payload.extend_from_slice(&filetime.to_le_bytes());
    payload.push(task_index);
    payload.extend_from_slice(&cycle.to_le_bytes());
    payload.push(0); // arg_count
    payload.push(0); // context_count
    append_string(&mut payload, message);
    append_string(&mut payload, logger);

    let mut data = Vec::new();
    data.push(0x02);
    data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    data.extend_from_slice(&payload);
    data
}

/// Root span BEGIN (event type 5) with PLC-minted trace and span ids.
///
/// Mirrors `build_begin_with_local_ids` in `trace_wire_format.rs`
/// (parent_span_id(8) + kind(1) + name_len(1) + reserved(2) +
/// trace_id(16) + span_id(8) + name).
pub fn span_begin(
    local_id: u8,
    task_index: u8,
    trace_id: [u8; 16],
    span_id: [u8; 8],
    name: &str,
    dc_time: i64,
) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(5);
    data.push(local_id);
    data.push(task_index);
    data.push(0); // flags: no external parent
    data.extend_from_slice(&dc_time.to_le_bytes());
    data.extend_from_slice(&[0u8; 8]); // parent_span_id: all-zero = root
    data.push(0); // kind
    data.push(name.len() as u8);
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved
    data.extend_from_slice(&trace_id);
    data.extend_from_slice(&span_id);
    data.extend_from_slice(name.as_bytes());
    data
}

/// Span END (event type 8) with status 1 (ok) and an empty message.
///
/// Mirrors the END block in `test_round_trip_begin_attr_event_end` in
/// `trace_wire_format.rs`: the first span id byte sits in the header and the
/// remaining seven follow `dc_time`.
pub fn span_end(task_index: u8, span_id: [u8; 8], dc_time: i64) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(8);
    data.push(span_id[0]);
    data.push(task_index);
    data.push(0); // flags
    data.extend_from_slice(&dc_time.to_le_bytes());
    data.extend_from_slice(&span_id[1..8]);
    data.push(1); // status
    data.push(0); // message_len
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved
    data
}

/// Concatenate entries into one ADS WRITE data buffer.
pub fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

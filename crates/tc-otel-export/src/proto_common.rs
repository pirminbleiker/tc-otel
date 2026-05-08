//! Shared helpers used by every OTLP-Protobuf encoder.
//!
//! Hex-decoding for trace/span IDs, mapping from `serde_json::Value`
//! attributes onto OTLP `AnyValue`s, and resource construction. The
//! mapping rules mirror what the OTLP-JSON path does in
//! [`crate::exporter::build_otel_*_payload`] so both wire formats
//! emit semantically identical batches.

use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as AnyValueOneof, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use std::collections::HashMap;

/// Decode a hex-encoded W3C trace ID (32 hex characters → 16 bytes).
/// Returns an empty `Vec<u8>` when the input is empty or malformed,
/// matching the OTLP-JSON exporter's "skip the field if no context"
/// semantics.
pub fn decode_trace_id_hex(s: &str) -> Vec<u8> {
    decode_fixed_hex(s, 16)
}

/// Decode a hex-encoded span ID (16 hex characters → 8 bytes). Empty
/// input or malformed strings produce an empty `Vec<u8>`.
pub fn decode_span_id_hex(s: &str) -> Vec<u8> {
    decode_fixed_hex(s, 8)
}

fn decode_fixed_hex(s: &str, expected_bytes: usize) -> Vec<u8> {
    if s.is_empty() || s.len() != expected_bytes * 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(expected_bytes);
    for i in 0..expected_bytes {
        let pair = &s[i * 2..i * 2 + 2];
        match u8::from_str_radix(pair, 16) {
            Ok(b) => out.push(b),
            Err(_) => return Vec::new(),
        }
    }
    out
}

/// Convert one `serde_json::Value` into the OTLP `AnyValue` shape.
///
/// Matches the JSON-path mapping rules:
/// - `String` → `StringValue`
/// - `Bool` → `BoolValue`
/// - `Number` → `IntValue` (when integral) or `DoubleValue`
/// - `Array` → `ArrayValue` of recursively-converted entries
/// - `Object` → `KvlistValue`
/// - `Null` → returns `None` so callers can skip the attribute entirely
pub fn json_to_any_value(value: &serde_json::Value) -> Option<AnyValue> {
    use serde_json::Value;
    Some(AnyValue {
        value: Some(match value {
            Value::Null => return None,
            Value::Bool(b) => AnyValueOneof::BoolValue(*b),
            Value::String(s) => AnyValueOneof::StringValue(s.clone()),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    AnyValueOneof::IntValue(i)
                } else if let Some(u) = n.as_u64() {
                    // OTLP IntValue is i64; clamp via cast — values up to i64::MAX fit.
                    AnyValueOneof::IntValue(u as i64)
                } else if let Some(f) = n.as_f64() {
                    AnyValueOneof::DoubleValue(f)
                } else {
                    AnyValueOneof::StringValue(n.to_string())
                }
            }
            Value::Array(items) => {
                let values = items.iter().filter_map(json_to_any_value).collect();
                AnyValueOneof::ArrayValue(ArrayValue { values })
            }
            Value::Object(map) => {
                let kvs = map
                    .iter()
                    .filter_map(|(k, v)| {
                        json_to_any_value(v).map(|av| KeyValue {
                            key: k.clone(),
                            value: Some(av),
                        })
                    })
                    .collect();
                AnyValueOneof::KvlistValue(KeyValueList { values: kvs })
            }
        }),
    })
}

/// Convert a tc-otel attribute map into the OTLP `Vec<KeyValue>` shape,
/// dropping `Null` values.
pub fn build_attributes(attrs: &HashMap<String, serde_json::Value>) -> Vec<KeyValue> {
    attrs
        .iter()
        .filter_map(|(k, v)| {
            json_to_any_value(v).map(|av| KeyValue {
                key: k.clone(),
                value: Some(av),
            })
        })
        .collect()
}

/// Build an OTLP `Resource` from a tc-otel resource attribute map.
/// `dropped_attributes_count` is always 0 — tc-otel never drops
/// resource attributes upstream.
pub fn build_resource(resource_attrs: &HashMap<String, serde_json::Value>) -> Resource {
    Resource {
        attributes: build_attributes(resource_attrs),
        dropped_attributes_count: 0,
        // entity_refs is a 0.31 addition; default empty Vec is fine.
        entity_refs: Vec::new(),
    }
}

/// Convert a `chrono::DateTime<Utc>` into the OTLP `time_unix_nano`
/// shape (`u64`, nanoseconds since epoch). Returns 0 on overflow which
/// is well-defined OTLP "no timestamp" rather than panicking.
pub fn datetime_to_unix_nano(ts: chrono::DateTime<chrono::Utc>) -> u64 {
    ts.timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hex_decode_trace_id_valid() {
        let bytes = decode_trace_id_hex("0123456789abcdef0123456789abcdef");
        assert_eq!(
            bytes,
            vec![
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef,
            ]
        );
    }

    #[test]
    fn hex_decode_trace_id_empty_or_bad() {
        assert!(decode_trace_id_hex("").is_empty());
        assert!(decode_trace_id_hex("nothex").is_empty());
        assert!(decode_trace_id_hex("0123").is_empty(), "wrong length");
    }

    #[test]
    fn hex_decode_span_id_valid() {
        let bytes = decode_span_id_hex("0011223344556677");
        assert_eq!(bytes, vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
    }

    #[test]
    fn json_value_int() {
        let v = json_to_any_value(&json!(42)).unwrap();
        match v.value.unwrap() {
            AnyValueOneof::IntValue(i) => assert_eq!(i, 42),
            other => panic!("expected IntValue, got {other:?}"),
        }
    }

    #[test]
    fn json_value_float() {
        let v = json_to_any_value(&json!(3.14)).unwrap();
        match v.value.unwrap() {
            AnyValueOneof::DoubleValue(f) => assert!((f - 3.14).abs() < 1e-9),
            other => panic!("expected DoubleValue, got {other:?}"),
        }
    }

    #[test]
    fn json_value_string() {
        let v = json_to_any_value(&json!("hello")).unwrap();
        match v.value.unwrap() {
            AnyValueOneof::StringValue(s) => assert_eq!(s, "hello"),
            other => panic!("expected StringValue, got {other:?}"),
        }
    }

    #[test]
    fn json_value_bool() {
        let v = json_to_any_value(&json!(true)).unwrap();
        match v.value.unwrap() {
            AnyValueOneof::BoolValue(b) => assert!(b),
            other => panic!("expected BoolValue, got {other:?}"),
        }
    }

    #[test]
    fn json_value_null_dropped() {
        assert!(json_to_any_value(&json!(null)).is_none());
    }

    #[test]
    fn build_attributes_drops_null() {
        let mut h = HashMap::new();
        h.insert("a".into(), json!("x"));
        h.insert("b".into(), json!(null));
        h.insert("c".into(), json!(7));
        let kvs = build_attributes(&h);
        assert_eq!(kvs.len(), 2);
        let keys: Vec<_> = kvs.iter().map(|k| k.key.as_str()).collect();
        assert!(keys.contains(&"a"));
        assert!(keys.contains(&"c"));
        assert!(!keys.contains(&"b"));
    }
}

//! Shared helpers used by every OTLP-Protobuf encoder.
//!
//! Hex-decoding for trace/span IDs, mapping from `serde_json::Value`
//! attributes onto OTLP `AnyValue`s, and resource construction. The
//! mapping rules mirror what the OTLP-JSON path does in
//! `crate::exporter::build_otel_*_payload` so both wire formats
//! emit semantically identical batches.

use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as AnyValueOneof, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use std::collections::{BTreeMap, HashMap};

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

/// Build a stable, order-independent fingerprint of a resource attribute
/// map. Two records with semantically equal resource attribute sets must
/// hash to the same key — this is what lets the encoders bucket records
/// into one `Resource*` block per resource.
///
/// Implementation: copy entries into a `BTreeMap` (deterministic key
/// order) and serialise the values via `serde_json::to_string`. The
/// JSON form distinguishes `42` (int), `"42"` (string), `42.0` (float),
/// nested objects, etc., so semantically distinct values that happen to
/// stringify the same don't collide.
pub fn resource_fingerprint(attrs: &HashMap<String, serde_json::Value>) -> String {
    let sorted: BTreeMap<&str, &serde_json::Value> =
        attrs.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut out = String::with_capacity(attrs.len() * 32);
    for (k, v) in sorted {
        out.push_str(k);
        out.push('=');
        // Best-effort: every serde_json::Value serialises successfully.
        // Use to_string (Display) which produces canonical JSON for our
        // value shapes — falling back to a sentinel on the unreachable
        // error path keeps the function infallible.
        match serde_json::to_string(v) {
            Ok(s) => out.push_str(&s),
            Err(_) => out.push_str("<unserialisable>"),
        }
        out.push('\u{1f}'); // unit separator — illegal inside our keys
    }
    out
}

/// Group records by their resource attribute fingerprint, preserving
/// the original order within each bucket. Returns the buckets in
/// fingerprint-sorted order so the encoder output is deterministic
/// for fixed input — important for tests and content-hash caching.
///
/// Each bucket is `(resource_attrs_ref, Vec<&Record>)`. The encoder
/// then emits one `ResourceMetrics` / `ResourceSpans` / `ResourceLogs`
/// block per bucket. Single-resource batches (the common case for one
/// PLC over local-router) hit the fast path: a single bucket containing
/// every record, behaving identically to the pre-grouping code.
pub fn group_records_by_resource<R, F>(
    records: &[R],
    resource_of: F,
) -> Vec<(&HashMap<String, serde_json::Value>, Vec<&R>)>
where
    F: Fn(&R) -> &HashMap<String, serde_json::Value>,
{
    if records.is_empty() {
        return Vec::new();
    }

    let mut buckets: BTreeMap<String, (&HashMap<String, serde_json::Value>, Vec<&R>)> =
        BTreeMap::new();
    for record in records {
        let resource = resource_of(record);
        let fp = resource_fingerprint(resource);
        buckets
            .entry(fp)
            .or_insert_with(|| (resource, Vec::new()))
            .1
            .push(record);
    }
    buckets.into_values().collect()
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
        // 2.5 chosen to avoid clippy::approx_constant (PI/E/etc.)
        let v = json_to_any_value(&json!(2.5)).unwrap();
        match v.value.unwrap() {
            AnyValueOneof::DoubleValue(f) => assert!((f - 2.5).abs() < 1e-9),
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

    #[test]
    fn fingerprint_independent_of_insertion_order() {
        let mut a = HashMap::new();
        a.insert("service.name".into(), json!("svc"));
        a.insert("plc.ams_net_id".into(), json!("172.28.41.37.1.1"));

        let mut b = HashMap::new();
        b.insert("plc.ams_net_id".into(), json!("172.28.41.37.1.1"));
        b.insert("service.name".into(), json!("svc"));

        assert_eq!(resource_fingerprint(&a), resource_fingerprint(&b));
    }

    #[test]
    fn fingerprint_distinguishes_different_resources() {
        let mut a = HashMap::new();
        a.insert("plc.ams_net_id".into(), json!("172.28.41.37.1.1"));
        let mut b = HashMap::new();
        b.insert("plc.ams_net_id".into(), json!("10.0.0.1.1.1"));
        assert_ne!(resource_fingerprint(&a), resource_fingerprint(&b));
    }

    #[test]
    fn fingerprint_distinguishes_int_vs_string() {
        let mut a = HashMap::new();
        a.insert("port".into(), json!(851));
        let mut b = HashMap::new();
        b.insert("port".into(), json!("851"));
        assert_ne!(resource_fingerprint(&a), resource_fingerprint(&b));
    }

    #[test]
    fn group_records_buckets_by_resource() {
        struct R {
            res: HashMap<String, serde_json::Value>,
            id: u8,
        }
        let mk = |net: &str, id: u8| -> R {
            let mut m = HashMap::new();
            m.insert("plc.ams_net_id".into(), json!(net));
            R { res: m, id }
        };
        let records = vec![
            mk("172.28.41.37.1.1", 0),
            mk("10.0.0.1.1.1", 1),
            mk("172.28.41.37.1.1", 2),
            mk("10.0.0.1.1.1", 3),
        ];
        let groups = group_records_by_resource(&records, |r| &r.res);
        assert_eq!(groups.len(), 2);
        let mut total = 0;
        for (_res, bucket) in &groups {
            // Within each bucket, original order is preserved.
            assert!(bucket.windows(2).all(|w| w[0].id < w[1].id));
            total += bucket.len();
        }
        assert_eq!(total, 4);
    }

    #[test]
    fn group_records_empty_returns_empty() {
        let records: Vec<u8> = Vec::new();
        let groups = group_records_by_resource(&records, |_| {
            // Never called when input is empty.
            unreachable!()
        });
        assert!(groups.is_empty());
    }

    #[test]
    fn group_records_single_resource_single_bucket() {
        struct R {
            res: HashMap<String, serde_json::Value>,
        }
        let mut common = HashMap::new();
        common.insert("service.name".into(), json!("svc"));
        let records = vec![
            R {
                res: common.clone(),
            },
            R {
                res: common.clone(),
            },
            R {
                res: common.clone(),
            },
        ];
        let groups = group_records_by_resource(&records, |r| &r.res);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 3);
    }
}

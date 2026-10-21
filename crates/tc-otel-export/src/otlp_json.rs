//! OTLP/JSON decoding for inbound logs
//!
//! Parses the JSON encoding of `ExportLogsServiceRequest` (the body of an
//! OTLP/HTTP `POST /v1/logs` with `Content-Type: application/json`) into the
//! protobuf types from [`crate::grpc`], so HTTP and gRPC share one conversion
//! path to `LogEntry`.
//!
//! Follows the OTLP/JSON rules from the OTLP specification:
//! - field names are lowerCamelCase (snake_case is also accepted, as any
//!   proto3 JSON parser would)
//! - `traceId` / `spanId` are hex strings, not base64
//! - 64-bit integers (`timeUnixNano`, `intValue`, ...) may be strings or numbers
//! - enums (`severityNumber`) are integers; enum names are also accepted
//! - `bytesValue` is base64

use base64::Engine;
use serde_json::{Map, Value};

use crate::grpc::{
    any_value, AnyValue, ArrayValue, ExportLogsServiceRequest, InstrumentationScope, KeyValue,
    KeyValueList, LogRecord, Resource, ResourceLogs, ScopeLogs,
};

type ParseResult<T> = std::result::Result<T, String>;

/// Parse an OTLP/JSON `ExportLogsServiceRequest` document.
pub fn parse_logs_request(json: &Value) -> ParseResult<ExportLogsServiceRequest> {
    let obj = as_object(json, "request")?;
    Ok(ExportLogsServiceRequest {
        resource_logs: parse_array(obj, "resourceLogs", "resource_logs", parse_resource_logs)?,
    })
}

fn parse_resource_logs(v: &Value) -> ParseResult<ResourceLogs> {
    let obj = as_object(v, "resourceLogs[]")?;
    Ok(ResourceLogs {
        resource: field(obj, "resource", "resource")
            .map(parse_resource)
            .transpose()?,
        scope_logs: parse_array(obj, "scopeLogs", "scope_logs", parse_scope_logs)?,
        schema_url: parse_string(obj, "schemaUrl", "schema_url")?,
    })
}

fn parse_resource(v: &Value) -> ParseResult<Resource> {
    let obj = as_object(v, "resource")?;
    Ok(Resource {
        attributes: parse_array(obj, "attributes", "attributes", parse_key_value)?,
        dropped_attributes_count: parse_u32(
            obj,
            "droppedAttributesCount",
            "dropped_attributes_count",
        )?,
    })
}

fn parse_scope_logs(v: &Value) -> ParseResult<ScopeLogs> {
    let obj = as_object(v, "scopeLogs[]")?;
    Ok(ScopeLogs {
        scope: field(obj, "scope", "scope").map(parse_scope).transpose()?,
        log_records: parse_array(obj, "logRecords", "log_records", parse_log_record)?,
        schema_url: parse_string(obj, "schemaUrl", "schema_url")?,
    })
}

fn parse_scope(v: &Value) -> ParseResult<InstrumentationScope> {
    let obj = as_object(v, "scope")?;
    Ok(InstrumentationScope {
        name: parse_string(obj, "name", "name")?,
        version: parse_string(obj, "version", "version")?,
        attributes: parse_array(obj, "attributes", "attributes", parse_key_value)?,
        dropped_attributes_count: parse_u32(
            obj,
            "droppedAttributesCount",
            "dropped_attributes_count",
        )?,
    })
}

fn parse_log_record(v: &Value) -> ParseResult<LogRecord> {
    let obj = as_object(v, "logRecords[]")?;
    Ok(LogRecord {
        time_unix_nano: parse_u64(obj, "timeUnixNano", "time_unix_nano")?,
        observed_time_unix_nano: parse_u64(obj, "observedTimeUnixNano", "observed_time_unix_nano")?,
        severity_number: parse_severity_number(obj)?,
        severity_text: parse_string(obj, "severityText", "severity_text")?,
        body: field(obj, "body", "body")
            .map(parse_any_value)
            .transpose()?,
        attributes: parse_array(obj, "attributes", "attributes", parse_key_value)?,
        dropped_attributes_count: parse_u32(
            obj,
            "droppedAttributesCount",
            "dropped_attributes_count",
        )?,
        flags: parse_u32(obj, "flags", "flags")?,
        trace_id: parse_hex_id(obj, "traceId", "trace_id", 16)?,
        span_id: parse_hex_id(obj, "spanId", "span_id", 8)?,
    })
}

fn parse_key_value(v: &Value) -> ParseResult<KeyValue> {
    let obj = as_object(v, "attribute")?;
    Ok(KeyValue {
        key: parse_string(obj, "key", "key")?,
        value: field(obj, "value", "value")
            .map(parse_any_value)
            .transpose()?,
    })
}

fn parse_any_value(v: &Value) -> ParseResult<AnyValue> {
    let obj = as_object(v, "AnyValue")?;
    let value = if let Some(s) = field(obj, "stringValue", "string_value") {
        let s = s.as_str().ok_or("stringValue must be a string")?;
        Some(any_value::Value::StringValue(s.to_string()))
    } else if let Some(b) = field(obj, "boolValue", "bool_value") {
        let b = b.as_bool().ok_or("boolValue must be a boolean")?;
        Some(any_value::Value::BoolValue(b))
    } else if let Some(i) = field(obj, "intValue", "int_value") {
        Some(any_value::Value::IntValue(json_to_i64(i, "intValue")?))
    } else if let Some(d) = field(obj, "doubleValue", "double_value") {
        Some(any_value::Value::DoubleValue(json_to_f64(d)?))
    } else if let Some(a) = field(obj, "arrayValue", "array_value") {
        let a = as_object(a, "arrayValue")?;
        Some(any_value::Value::ArrayValue(ArrayValue {
            values: parse_array(a, "values", "values", parse_any_value)?,
        }))
    } else if let Some(k) = field(obj, "kvlistValue", "kvlist_value") {
        let k = as_object(k, "kvlistValue")?;
        Some(any_value::Value::KvlistValue(KeyValueList {
            values: parse_array(k, "values", "values", parse_key_value)?,
        }))
    } else if let Some(b) = field(obj, "bytesValue", "bytes_value") {
        let b = b.as_str().ok_or("bytesValue must be a base64 string")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b)
            .map_err(|e| format!("bytesValue is not valid base64: {e}"))?;
        Some(any_value::Value::BytesValue(bytes))
    } else {
        None
    };
    Ok(AnyValue { value })
}

// ─── Field helpers ───

/// Look up a field by its lowerCamelCase name, falling back to snake_case.
/// JSON `null` is treated as absent.
fn field<'a>(obj: &'a Map<String, Value>, camel: &str, snake: &str) -> Option<&'a Value> {
    obj.get(camel)
        .or_else(|| obj.get(snake))
        .filter(|v| !v.is_null())
}

fn as_object<'a>(v: &'a Value, what: &str) -> ParseResult<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| format!("{what} must be a JSON object"))
}

fn parse_array<T>(
    obj: &Map<String, Value>,
    camel: &str,
    snake: &str,
    parse: fn(&Value) -> ParseResult<T>,
) -> ParseResult<Vec<T>> {
    match field(obj, camel, snake) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(parse).collect(),
        Some(_) => Err(format!("{camel} must be an array")),
    }
}

fn parse_string(obj: &Map<String, Value>, camel: &str, snake: &str) -> ParseResult<String> {
    match field(obj, camel, snake) {
        None => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("{camel} must be a string")),
    }
}

fn parse_u64(obj: &Map<String, Value>, camel: &str, snake: &str) -> ParseResult<u64> {
    match field(obj, camel, snake) {
        None => Ok(0),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map_err(|_| format!("{camel} must be an unsigned integer")),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| format!("{camel} must be an unsigned integer")),
        Some(_) => Err(format!("{camel} must be an unsigned integer")),
    }
}

fn parse_u32(obj: &Map<String, Value>, camel: &str, snake: &str) -> ParseResult<u32> {
    let v = parse_u64(obj, camel, snake)?;
    u32::try_from(v).map_err(|_| format!("{camel} is out of range"))
}

fn json_to_i64(v: &Value, what: &str) -> ParseResult<i64> {
    match v {
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| format!("{what} must be an integer")),
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| format!("{what} must be an integer")),
        _ => Err(format!("{what} must be an integer")),
    }
}

fn json_to_f64(v: &Value) -> ParseResult<f64> {
    match v {
        Value::Number(n) => n.as_f64().ok_or_else(|| "invalid doubleValue".to_string()),
        Value::String(s) => match s.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            other => other
                .parse::<f64>()
                .map_err(|_| "doubleValue must be a number".to_string()),
        },
        _ => Err("doubleValue must be a number".to_string()),
    }
}

/// `severityNumber` is an integer per OTLP/JSON; proto3 JSON also allows the
/// enum name (e.g. `"SEVERITY_NUMBER_WARN"`), which some exporters emit.
fn parse_severity_number(obj: &Map<String, Value>) -> ParseResult<i32> {
    match field(obj, "severityNumber", "severity_number") {
        None => Ok(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|i| i32::try_from(i).ok())
            .ok_or_else(|| "severityNumber must be an integer".to_string()),
        Some(Value::String(s)) => severity_from_name(s)
            .or_else(|| s.parse::<i32>().ok())
            .ok_or_else(|| format!("unknown severityNumber: {s}")),
        Some(_) => Err("severityNumber must be an integer".to_string()),
    }
}

fn severity_from_name(name: &str) -> Option<i32> {
    const LEVELS: [&str; 6] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL"];
    let rest = name.strip_prefix("SEVERITY_NUMBER_")?;
    if rest == "UNSPECIFIED" {
        return Some(0);
    }
    // TRACE=1, TRACE2=2, ... FATAL4=24
    LEVELS.iter().enumerate().find_map(|(i, level)| {
        let suffix = rest.strip_prefix(level)?;
        let step = match suffix {
            "" => 1,
            "2" => 2,
            "3" => 3,
            "4" => 4,
            _ => return None,
        };
        Some(i as i32 * 4 + step)
    })
}

/// Decode a hex-encoded trace/span id. Empty or absent means "not set".
fn parse_hex_id(
    obj: &Map<String, Value>,
    camel: &str,
    snake: &str,
    len: usize,
) -> ParseResult<Vec<u8>> {
    let s = parse_string(obj, camel, snake)?;
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if s.len() != len * 2 {
        return Err(format!("{camel} must be {} hex characters", len * 2));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("{camel} must be hex-encoded"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Example payload from the OTLP spec (opentelemetry-proto/examples/logs.json).
    fn spec_example() -> Value {
        json!({
          "resourceLogs": [{
            "resource": {
              "attributes": [
                { "key": "service.name", "value": { "stringValue": "my.service" } }
              ]
            },
            "scopeLogs": [{
              "scope": {
                "name": "my.library",
                "version": "1.0.0",
                "attributes": [
                  { "key": "my.scope.attribute", "value": { "stringValue": "some scope attribute" } }
                ]
              },
              "logRecords": [{
                "timeUnixNano": "1544712660300000000",
                "observedTimeUnixNano": "1544712660300000000",
                "severityNumber": 10,
                "severityText": "Information",
                "traceId": "5b8efff798038103d269b633813fc60c",
                "spanId": "eee19b7ec3c1b174",
                "body": { "stringValue": "Example log record" },
                "attributes": [
                  { "key": "string.attribute", "value": { "stringValue": "some string" } },
                  { "key": "boolean.attribute", "value": { "boolValue": true } },
                  { "key": "int.attribute", "value": { "intValue": "10" } },
                  { "key": "double.attribute", "value": { "doubleValue": 637.704 } },
                  { "key": "array.attribute", "value": { "arrayValue": { "values": [
                    { "stringValue": "many" }, { "stringValue": "values" }
                  ] } } },
                  { "key": "map.attribute", "value": { "kvlistValue": { "values": [
                    { "key": "some.map.key", "value": { "stringValue": "some value" } }
                  ] } } }
                ]
              }]
            }]
          }]
        })
    }

    #[test]
    fn test_parse_spec_example() {
        let req = parse_logs_request(&spec_example()).unwrap();
        assert_eq!(req.resource_logs.len(), 1);
        let rl = &req.resource_logs[0];
        assert_eq!(
            rl.resource.as_ref().unwrap().attributes[0].key,
            "service.name"
        );

        let sl = &rl.scope_logs[0];
        let scope = sl.scope.as_ref().unwrap();
        assert_eq!(scope.name, "my.library");
        assert_eq!(scope.version, "1.0.0");

        let rec = &sl.log_records[0];
        assert_eq!(rec.time_unix_nano, 1_544_712_660_300_000_000);
        assert_eq!(rec.observed_time_unix_nano, 1_544_712_660_300_000_000);
        assert_eq!(rec.severity_number, 10);
        assert_eq!(rec.severity_text, "Information");
        assert_eq!(
            rec.trace_id,
            vec![
                0x5b, 0x8e, 0xff, 0xf7, 0x98, 0x03, 0x81, 0x03, 0xd2, 0x69, 0xb6, 0x33, 0x81, 0x3f,
                0xc6, 0x0c
            ]
        );
        assert_eq!(
            rec.span_id,
            vec![0xee, 0xe1, 0x9b, 0x7e, 0xc3, 0xc1, 0xb1, 0x74]
        );
        assert_eq!(
            rec.body.as_ref().unwrap().value,
            Some(any_value::Value::StringValue("Example log record".into()))
        );
        assert_eq!(rec.attributes.len(), 6);
        assert_eq!(
            rec.attributes[2].value.as_ref().unwrap().value,
            Some(any_value::Value::IntValue(10))
        );
        assert_eq!(
            rec.attributes[3].value.as_ref().unwrap().value,
            Some(any_value::Value::DoubleValue(637.704))
        );
    }

    #[test]
    fn test_parse_empty_request() {
        let req = parse_logs_request(&json!({})).unwrap();
        assert!(req.resource_logs.is_empty());
    }

    #[test]
    fn test_parse_accepts_snake_case_and_numeric_int64() {
        let req = parse_logs_request(&json!({
            "resource_logs": [{
                "scope_logs": [{
                    "log_records": [{
                        "time_unix_nano": 1_700_000_000_000_000_000u64,
                        "severity_number": 17,
                        "body": { "int_value": 42 }
                    }]
                }]
            }]
        }))
        .unwrap();
        let rec = &req.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(rec.time_unix_nano, 1_700_000_000_000_000_000);
        assert_eq!(rec.severity_number, 17);
        assert_eq!(
            rec.body.as_ref().unwrap().value,
            Some(any_value::Value::IntValue(42))
        );
    }

    #[test]
    fn test_parse_severity_enum_names() {
        let parse = |s: &str| {
            let req = parse_logs_request(&json!({"resourceLogs": [{"scopeLogs": [{
                "logRecords": [{ "severityNumber": s }]
            }]}]}))
            .unwrap();
            req.resource_logs[0].scope_logs[0].log_records[0].severity_number
        };
        assert_eq!(parse("SEVERITY_NUMBER_UNSPECIFIED"), 0);
        assert_eq!(parse("SEVERITY_NUMBER_TRACE"), 1);
        assert_eq!(parse("SEVERITY_NUMBER_INFO"), 9);
        assert_eq!(parse("SEVERITY_NUMBER_WARN3"), 15);
        assert_eq!(parse("SEVERITY_NUMBER_FATAL4"), 24);
        assert!(parse_logs_request(&json!({"resourceLogs": [{"scopeLogs": [{
            "logRecords": [{ "severityNumber": "SEVERITY_NUMBER_LOUD" }]
        }]}]}))
        .is_err());
    }

    #[test]
    fn test_parse_bytes_and_special_doubles() {
        let req = parse_logs_request(&json!({"resourceLogs": [{"scopeLogs": [{
            "logRecords": [{
                "body": { "bytesValue": "3q2+7w==" },
                "attributes": [
                    { "key": "nan", "value": { "doubleValue": "NaN" } },
                    { "key": "inf", "value": { "doubleValue": "-Infinity" } },
                    { "key": "empty" }
                ]
            }]
        }]}]}))
        .unwrap();
        let rec = &req.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(
            rec.body.as_ref().unwrap().value,
            Some(any_value::Value::BytesValue(vec![0xde, 0xad, 0xbe, 0xef]))
        );
        match rec.attributes[0].value.as_ref().unwrap().value {
            Some(any_value::Value::DoubleValue(d)) => assert!(d.is_nan()),
            ref other => panic!("expected NaN, got {other:?}"),
        }
        assert_eq!(
            rec.attributes[1].value.as_ref().unwrap().value,
            Some(any_value::Value::DoubleValue(f64::NEG_INFINITY))
        );
        assert!(rec.attributes[2].value.is_none());
    }

    #[test]
    fn test_parse_empty_ids_are_unset() {
        let req = parse_logs_request(&json!({"resourceLogs": [{"scopeLogs": [{
            "logRecords": [{ "traceId": "", "spanId": "" }]
        }]}]}))
        .unwrap();
        let rec = &req.resource_logs[0].scope_logs[0].log_records[0];
        assert!(rec.trace_id.is_empty());
        assert!(rec.span_id.is_empty());
    }

    #[test]
    fn test_parse_rejects_malformed_input() {
        let bad = [
            json!([]),
            json!({ "resourceLogs": {} }),
            json!({ "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "traceId": "abc" }] }] }] }),
            json!({ "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "spanId": "zzzzzzzzzzzzzzzz" }] }] }] }),
            json!({ "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "timeUnixNano": "-1" }] }] }] }),
            json!({ "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "body": { "intValue": "1.5" } }] }] }] }),
            json!({ "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "body": { "bytesValue": "!!" } }] }] }] }),
        ];
        for doc in bad {
            assert!(
                parse_logs_request(&doc).is_err(),
                "expected error for {doc}"
            );
        }
    }
}

//! OTLP-Protobuf encoder for log batches.
//!
//! Builds an `ExportLogsServiceRequest` from a slice of
//! [`tc_otel_core::LogRecord`] and serialises it via
//! `prost::Message::encode_to_vec`. Used by
//! [`crate::exporter::OtelExporter`] when `WireFormat::Protobuf`
//! is selected on the logs endpoint.
//!
//! Note: the project keeps a separate set of hand-rolled OTLP-Logs
//! prost types in [`crate::grpc`] for the inbound gRPC server.
//! Those stay where they are — switching the server to use
//! `opentelemetry-proto` is a separate refactor. For the *outbound*
//! HTTP-Protobuf path we use the canonical types from the crate so
//! we get histogram-spec compliance, schema_url, entity_refs etc.
//! for free as the spec evolves.

use crate::proto_common::{
    build_attributes, build_resource, datetime_to_unix_nano, decode_span_id_hex,
    decode_trace_id_hex, group_records_by_resource,
};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as AnyValueOneof, AnyValue, InstrumentationScope,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord as ProtoLogRecord, ResourceLogs, ScopeLogs};
use prost::Message;
use tc_otel_core::LogRecord;

const SCOPE_NAME: &str = "tc-otel";

/// Encode a batch of log records into OTLP-Protobuf bytes.
pub fn build(records: &[LogRecord]) -> Vec<u8> {
    build_request(records).encode_to_vec()
}

/// Build the OTLP request without serialising.
///
/// Two-level bucketing per the OTel Logs data model:
/// 1. Outer: by resource attribute fingerprint → one `ResourceLogs`
///    block per distinct resource.
/// 2. Inner: by `record.scope_name` (the PLC logger that produced the
///    record) → one `ScopeLogs` block per logger, with
///    `InstrumentationScope.name` set to the logger name. Records with
///    an empty `scope_name` (e.g. older entries that didn't carry the
///    field) fall back to the crate-default `tc-otel` scope.
///
/// Backend effect: VictoriaLogs / Grafana Loki / Tempo show the
/// logger as the canonical "scope" / "instrumentation" attribute
/// rather than a free-form per-record label, so log queries can
/// filter `scope.name="MyPlcLogger"` directly.
pub fn build_request(records: &[LogRecord]) -> ExportLogsServiceRequest {
    if records.is_empty() {
        return ExportLogsServiceRequest {
            resource_logs: Vec::new(),
        };
    }

    let crate_version = env!("CARGO_PKG_VERSION").to_string();

    let resource_logs = group_records_by_resource(records, |r| &r.resource_attributes)
        .into_iter()
        .map(|(resource_attrs, resource_bucket)| {
            // Inner: bucket by logger (scope_name). Preserves first-seen
            // order so encoder output is deterministic for fixed input.
            let mut scope_buckets: Vec<(String, Vec<&LogRecord>)> = Vec::new();
            for rec in resource_bucket {
                let key = if rec.scope_name.is_empty() {
                    SCOPE_NAME
                } else {
                    rec.scope_name.as_str()
                };
                if let Some((_, b)) = scope_buckets.iter_mut().find(|(n, _)| n == key) {
                    b.push(rec);
                } else {
                    scope_buckets.push((key.to_string(), vec![rec]));
                }
            }

            let scope_logs = scope_buckets
                .into_iter()
                .map(|(scope_name, bucket)| {
                    let scope = InstrumentationScope {
                        name: scope_name,
                        // `version` is the *instrumentation library*'s
                        // version. tc-otel is that library; per-logger
                        // versioning would be misleading.
                        version: crate_version.clone(),
                        attributes: build_attributes(&bucket[0].scope_attributes),
                        dropped_attributes_count: 0,
                    };
                    ScopeLogs {
                        scope: Some(scope),
                        log_records: bucket.into_iter().map(build_log_record).collect(),
                        schema_url: String::new(),
                    }
                })
                .collect();

            ResourceLogs {
                resource: Some(build_resource(resource_attrs)),
                scope_logs,
                schema_url: String::new(),
            }
        })
        .collect();

    ExportLogsServiceRequest { resource_logs }
}

fn build_log_record(record: &LogRecord) -> ProtoLogRecord {
    let ts = datetime_to_unix_nano(record.timestamp);
    let body = match &record.body {
        // tc-otel always emits a String body in practice, but the
        // serde_json::Value type is flexible — fall through to the JSON
        // representation when it's anything else.
        serde_json::Value::String(s) => Some(AnyValue {
            value: Some(AnyValueOneof::StringValue(s.clone())),
        }),
        serde_json::Value::Null => None,
        other => Some(AnyValue {
            value: Some(AnyValueOneof::StringValue(other.to_string())),
        }),
    };
    ProtoLogRecord {
        time_unix_nano: ts,
        observed_time_unix_nano: ts,
        severity_number: record.severity_number,
        severity_text: record.severity_text.clone(),
        body,
        attributes: build_attributes(&record.log_attributes),
        dropped_attributes_count: 0,
        flags: 0,
        trace_id: decode_trace_id_hex(&record.trace_id),
        span_id: decode_span_id_hex(&record.span_id),
        event_name: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn ts() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn log_record() -> LogRecord {
        let mut resource = HashMap::new();
        resource.insert(
            "service.name".to_string(),
            serde_json::json!("tc-otel-test"),
        );
        let mut log_attrs = HashMap::new();
        log_attrs.insert("axis".to_string(), serde_json::json!("x"));
        LogRecord {
            timestamp: ts(),
            body: serde_json::json!("axis stalled"),
            severity_number: 17, // ERROR
            severity_text: "ERROR".to_string(),
            trace_id: String::new(),
            span_id: String::new(),
            resource_attributes: resource,
            scope_name: "MotionAxis".to_string(),
            scope_attributes: HashMap::new(),
            log_attributes: log_attrs,
        }
    }

    #[test]
    fn empty_batch_produces_empty_request() {
        assert!(build_request(&[]).resource_logs.is_empty());
        assert!(build(&[]).is_empty());
    }

    #[test]
    fn roundtrip_basic_log() {
        let r = log_record();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let log = &decoded.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log.severity_number, 17);
        assert_eq!(log.severity_text, "ERROR");
        match log.body.as_ref().unwrap().value.as_ref().unwrap() {
            AnyValueOneof::StringValue(s) => assert_eq!(s, "axis stalled"),
            _ => panic!("expected StringValue body"),
        }
        assert_eq!(
            log.time_unix_nano,
            ts().timestamp_nanos_opt().unwrap() as u64
        );
        assert_eq!(log.observed_time_unix_nano, log.time_unix_nano);
        assert!(
            log.trace_id.is_empty(),
            "no trace context => empty trace_id"
        );
        assert!(log.span_id.is_empty());
    }

    #[test]
    fn trace_context_decoded_to_bytes() {
        let mut r = log_record();
        r.trace_id = "0123456789abcdef0123456789abcdef".to_string();
        r.span_id = "fedcba9876543210".to_string();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let log = &decoded.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(log.trace_id.len(), 16);
        assert_eq!(log.span_id.len(), 8);
        assert_eq!(
            log.trace_id,
            vec![
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
    }

    #[test]
    fn scope_name_is_logger_name() {
        // Per OTel Logs data model: the logger that produced the
        // record IS the InstrumentationScope.name — not a per-record
        // attribute. tc-otel maps `entry.logger` (`F_Log(...).
        // WithLogger("MotionAxis")` on the PLC side) onto
        // `LogRecord.scope_name`, and the encoder uses it directly.
        let r = log_record();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let scope = decoded.resource_logs[0].scope_logs[0]
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.name, "MotionAxis");
        // No per-record `logger.name` scope attribute — the scope name
        // already carries that information.
        assert!(scope.attributes.iter().all(|kv| kv.key != "logger.name"));
    }

    #[test]
    fn empty_scope_name_falls_back_to_crate_default() {
        let mut r = log_record();
        r.scope_name = String::new();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let scope = decoded.resource_logs[0].scope_logs[0]
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.name, "tc-otel");
    }

    #[test]
    fn distinct_loggers_become_separate_scope_logs() {
        let mut a = log_record();
        a.scope_name = "MotionAxis".to_string();
        let mut b = log_record();
        b.scope_name = "Hydraulics".to_string();
        let bytes = build(&[a, b]);
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        // Same resource → one ResourceLogs, but two ScopeLogs blocks.
        assert_eq!(decoded.resource_logs.len(), 1);
        let scopes: Vec<&str> = decoded.resource_logs[0]
            .scope_logs
            .iter()
            .map(|s| s.scope.as_ref().unwrap().name.as_str())
            .collect();
        assert!(scopes.contains(&"MotionAxis"));
        assert!(scopes.contains(&"Hydraulics"));
        assert_eq!(scopes.len(), 2);
    }

    #[test]
    fn body_null_omits_field() {
        let mut r = log_record();
        r.body = serde_json::Value::Null;
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let log = &decoded.resource_logs[0].scope_logs[0].log_records[0];
        assert!(log.body.is_none());
    }

    #[test]
    fn multi_resource_batch_emits_separate_blocks() {
        let mut a = log_record();
        a.resource_attributes.insert(
            "plc.ams_net_id".to_string(),
            serde_json::json!("172.28.41.37.1.1"),
        );
        a.body = serde_json::json!("from PLC A");
        let mut b = log_record();
        b.resource_attributes.insert(
            "plc.ams_net_id".to_string(),
            serde_json::json!("10.0.0.1.1.1"),
        );
        b.body = serde_json::json!("from PLC B");

        let bytes = build(&[a, b]);
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        assert_eq!(
            decoded.resource_logs.len(),
            2,
            "expected one ResourceLogs block per distinct resource"
        );

        let mut by_net_id: HashMap<String, Vec<String>> = HashMap::new();
        for rl in &decoded.resource_logs {
            let res = rl.resource.as_ref().unwrap();
            let net_id = res
                .attributes
                .iter()
                .find(|kv| kv.key == "plc.ams_net_id")
                .map(
                    |kv| match kv.value.as_ref().unwrap().value.as_ref().unwrap() {
                        AnyValueOneof::StringValue(s) => s.clone(),
                        _ => panic!("expected StringValue"),
                    },
                )
                .unwrap();
            let bodies: Vec<String> = rl.scope_logs[0]
                .log_records
                .iter()
                .map(
                    |lr| match lr.body.as_ref().unwrap().value.as_ref().unwrap() {
                        AnyValueOneof::StringValue(s) => s.clone(),
                        _ => panic!("expected StringValue body"),
                    },
                )
                .collect();
            by_net_id.insert(net_id, bodies);
        }
        assert_eq!(
            by_net_id.remove("172.28.41.37.1.1").unwrap(),
            vec!["from PLC A"]
        );
        assert_eq!(
            by_net_id.remove("10.0.0.1.1.1").unwrap(),
            vec!["from PLC B"]
        );
    }

    #[test]
    fn single_resource_batch_emits_one_block() {
        let r1 = log_record();
        let mut r2 = log_record();
        r2.body = serde_json::json!("second log");
        let bytes = build(&[r1, r2]);
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        assert_eq!(decoded.resource_logs.len(), 1);
        assert_eq!(decoded.resource_logs[0].scope_logs[0].log_records.len(), 2);
    }
}

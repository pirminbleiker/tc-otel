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
    decode_trace_id_hex,
};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as AnyValueOneof, AnyValue, InstrumentationScope,
};
use opentelemetry_proto::tonic::logs::v1::{
    LogRecord as ProtoLogRecord, ResourceLogs, ScopeLogs,
};
use prost::Message;
use tc_otel_core::LogRecord;

const SCOPE_NAME: &str = "tc-otel";

/// Encode a batch of log records into OTLP-Protobuf bytes.
pub fn build(records: &[LogRecord]) -> Vec<u8> {
    build_request(records).encode_to_vec()
}

/// Build the OTLP request without serialising.
pub fn build_request(records: &[LogRecord]) -> ExportLogsServiceRequest {
    if records.is_empty() {
        return ExportLogsServiceRequest {
            resource_logs: Vec::new(),
        };
    }

    let resource = build_resource(&records[0].resource_attributes);
    // tc-otel populates scope_attributes (e.g. logger.name) per record;
    // we attach the first record's scope to the single ScopeLogs block.
    // If logs in a batch span multiple loggers, downstream backends
    // already disambiguate via the per-record `attributes`.
    let scope = InstrumentationScope {
        name: SCOPE_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        attributes: build_attributes(&records[0].scope_attributes),
        dropped_attributes_count: 0,
    };

    let log_records = records.iter().map(build_log_record).collect();

    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource),
            scope_logs: vec![ScopeLogs {
                scope: Some(scope),
                log_records,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
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
    use prost::Message as _;
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
        let mut scope_attrs = HashMap::new();
        scope_attrs.insert(
            "logger.name".to_string(),
            serde_json::json!("MotionAxis"),
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
            scope_attributes: scope_attrs,
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
        assert_eq!(log.time_unix_nano, ts().timestamp_nanos_opt().unwrap() as u64);
        assert_eq!(log.observed_time_unix_nano, log.time_unix_nano);
        assert!(log.trace_id.is_empty(), "no trace context => empty trace_id");
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
    fn scope_carries_logger_name() {
        let r = log_record();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportLogsServiceRequest::decode(&bytes[..]).unwrap();
        let scope = decoded.resource_logs[0].scope_logs[0]
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.name, "tc-otel");
        let logger = scope
            .attributes
            .iter()
            .find(|kv| kv.key == "logger.name")
            .unwrap();
        match logger.value.as_ref().unwrap().value.as_ref().unwrap() {
            AnyValueOneof::StringValue(s) => assert_eq!(s, "MotionAxis"),
            _ => panic!("expected StringValue"),
        }
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
}

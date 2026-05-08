//! OTLP-Protobuf encoder for trace batches.
//!
//! Builds an `ExportTraceServiceRequest` from a slice of
//! [`tc_otel_core::TraceRecord`] and serialises it via
//! `prost::Message::encode_to_vec`. Used by
//! [`crate::exporter::OtelExporter`] when `WireFormat::Protobuf`
//! is selected on the traces endpoint.
//!
//! Mapping rules mirror the JSON path in
//! `exporter::build_otel_traces_payload`:
//!
//! - `trace_id` / `span_id` / `parent_span_id` are stored as raw bytes
//!   (16/8/8) decoded from the hex strings carried on `TraceRecord`.
//!   An empty hex string maps to an empty `Vec<u8>` per OTLP spec,
//!   never zero-filled bytes.
//! - `kind` and `status_code` are passed through as-is (`i32`); the
//!   tc-otel domain enums already align with the OTLP wire values via
//!   `SpanKind::to_otel_kind()` and `SpanStatusCode::to_otel_status()`.
//! - Each `TraceEventRecord` becomes one `Span::Event`.

use crate::proto_common::{
    build_attributes, build_resource, datetime_to_unix_nano, decode_span_id_hex,
    decode_trace_id_hex, group_records_by_resource,
};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::trace::v1::{
    span::Event as SpanEvent, ResourceSpans, ScopeSpans, Span, Status,
};
use prost::Message;
use tc_otel_core::TraceRecord;

const SCOPE_NAME: &str = "tc-otel";

/// Encode a batch of trace records into OTLP-Protobuf bytes.
pub fn build(records: &[TraceRecord]) -> Vec<u8> {
    build_request(records).encode_to_vec()
}

/// Build the OTLP request without serialising — used by tests for
/// roundtrip inspection.
///
/// Records are bucketed by resource attribute set, producing one
/// `ResourceSpans` block per distinct resource. Single-resource
/// batches collapse to a single block; multi-PLC batches preserve
/// per-resource fidelity instead of merging into the first record's
/// resource.
pub fn build_request(records: &[TraceRecord]) -> ExportTraceServiceRequest {
    if records.is_empty() {
        return ExportTraceServiceRequest {
            resource_spans: Vec::new(),
        };
    }

    let scope = InstrumentationScope {
        name: SCOPE_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
    };

    let resource_spans = group_records_by_resource(records, |r| &r.resource_attributes)
        .into_iter()
        .map(|(resource_attrs, bucket)| {
            let spans = bucket.into_iter().map(build_span).collect();
            ResourceSpans {
                resource: Some(build_resource(resource_attrs)),
                scope_spans: vec![ScopeSpans {
                    scope: Some(scope.clone()),
                    spans,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }
        })
        .collect();

    ExportTraceServiceRequest { resource_spans }
}

fn build_span(record: &TraceRecord) -> Span {
    Span {
        trace_id: decode_trace_id_hex(&record.trace_id),
        span_id: decode_span_id_hex(&record.span_id),
        trace_state: String::new(),
        parent_span_id: decode_span_id_hex(&record.parent_span_id),
        flags: 0,
        name: record.name.clone(),
        kind: record.kind,
        start_time_unix_nano: datetime_to_unix_nano(record.start_time),
        end_time_unix_nano: datetime_to_unix_nano(record.end_time),
        attributes: build_attributes(&record.span_attributes),
        dropped_attributes_count: 0,
        events: record
            .events
            .iter()
            .map(|ev| SpanEvent {
                time_unix_nano: datetime_to_unix_nano(ev.timestamp),
                name: ev.name.clone(),
                attributes: build_attributes(&ev.attributes),
                dropped_attributes_count: 0,
            })
            .collect(),
        dropped_events_count: 0,
        // tc-otel doesn't carry span links today.
        links: Vec::new(),
        dropped_links_count: 0,
        status: Some(Status {
            message: record.status_message.clone(),
            code: record.status_code,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use prost::Message as _;
    use std::collections::HashMap;
    use tc_otel_core::TraceEventRecord;

    fn ts(secs: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn span_record() -> TraceRecord {
        let mut resource = HashMap::new();
        resource.insert(
            "service.name".to_string(),
            serde_json::json!("tc-otel-test"),
        );
        let mut attrs = HashMap::new();
        attrs.insert("axis".to_string(), serde_json::json!(1));
        TraceRecord {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
            parent_span_id: String::new(),
            name: "motion.move".to_string(),
            kind: 2, // SERVER
            start_time: ts(1_700_000_000),
            end_time: ts(1_700_000_001),
            status_code: 1, // OK
            status_message: String::new(),
            resource_attributes: resource,
            scope_attributes: HashMap::new(),
            span_attributes: attrs,
            events: vec![TraceEventRecord {
                timestamp: ts(1_700_000_000),
                name: "axis.contacted_limit".to_string(),
                attributes: {
                    let mut m = HashMap::new();
                    m.insert("limit_id".to_string(), serde_json::json!(7));
                    m
                },
            }],
        }
    }

    #[test]
    fn empty_batch_produces_empty_request() {
        assert!(build_request(&[]).resource_spans.is_empty());
        assert!(build(&[]).is_empty());
    }

    #[test]
    fn roundtrip_basic_span() {
        let r = span_record();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        assert_eq!(span.name, "motion.move");
        assert_eq!(span.kind, 2);
        assert_eq!(span.trace_id.len(), 16);
        assert_eq!(span.span_id.len(), 8);
        assert!(span.parent_span_id.is_empty(), "root span has empty parent");
        assert_eq!(
            span.start_time_unix_nano,
            ts(1_700_000_000).timestamp_nanos_opt().unwrap() as u64
        );
        let status = span.status.as_ref().unwrap();
        assert_eq!(status.code, 1);
        assert!(status.message.is_empty());
    }

    #[test]
    fn parent_span_id_present_when_set() {
        let mut r = span_record();
        r.parent_span_id = "1122334455667788".to_string();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        assert_eq!(
            span.parent_span_id,
            vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn events_roundtrip() {
        let r = span_record();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        let events = &decoded.resource_spans[0].scope_spans[0].spans[0].events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "axis.contacted_limit");
        let limit_attr = events[0]
            .attributes
            .iter()
            .find(|kv| kv.key == "limit_id")
            .unwrap();
        match limit_attr.value.as_ref().unwrap().value.as_ref().unwrap() {
            opentelemetry_proto::tonic::common::v1::any_value::Value::IntValue(i) => {
                assert_eq!(*i, 7)
            }
            _ => panic!("expected IntValue"),
        }
    }

    #[test]
    fn status_error_with_message() {
        let mut r = span_record();
        r.status_code = 2; // ERROR
        r.status_message = "axis stalled".to_string();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        let status = decoded.resource_spans[0].scope_spans[0].spans[0]
            .status
            .as_ref()
            .unwrap();
        assert_eq!(status.code, 2);
        assert_eq!(status.message, "axis stalled");
    }

    #[test]
    fn multi_resource_batch_emits_separate_blocks() {
        let mut a = span_record();
        a.resource_attributes.insert(
            "plc.ams_net_id".to_string(),
            serde_json::json!("172.28.41.37.1.1"),
        );
        a.name = "axis.move.a".to_string();
        let mut b = span_record();
        b.resource_attributes.insert(
            "plc.ams_net_id".to_string(),
            serde_json::json!("10.0.0.1.1.1"),
        );
        b.name = "axis.move.b".to_string();

        let bytes = build(&[a, b]);
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        assert_eq!(
            decoded.resource_spans.len(),
            2,
            "expected one ResourceSpans block per distinct resource"
        );

        let mut by_net_id: HashMap<String, Vec<String>> = HashMap::new();
        for rs in &decoded.resource_spans {
            let res = rs.resource.as_ref().unwrap();
            let net_id = res
                .attributes
                .iter()
                .find(|kv| kv.key == "plc.ams_net_id")
                .map(
                    |kv| match kv.value.as_ref().unwrap().value.as_ref().unwrap() {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => s.clone(),
                        _ => panic!("expected StringValue"),
                    },
                )
                .unwrap();
            let names: Vec<String> = rs.scope_spans[0]
                .spans
                .iter()
                .map(|s| s.name.clone())
                .collect();
            by_net_id.insert(net_id, names);
        }
        assert_eq!(
            by_net_id.remove("172.28.41.37.1.1").unwrap(),
            vec!["axis.move.a"]
        );
        assert_eq!(
            by_net_id.remove("10.0.0.1.1.1").unwrap(),
            vec!["axis.move.b"]
        );
    }

    #[test]
    fn single_resource_batch_emits_one_block() {
        let r1 = span_record();
        let mut r2 = span_record();
        r2.name = "second".to_string();
        let bytes = build(&[r1, r2]);
        let decoded = ExportTraceServiceRequest::decode(&bytes[..]).unwrap();
        assert_eq!(decoded.resource_spans.len(), 1);
        assert_eq!(decoded.resource_spans[0].scope_spans[0].spans.len(), 2);
    }
}

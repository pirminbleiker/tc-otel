//! OTLP-Protobuf encoder for metric batches.
//!
//! Builds an `ExportMetricsServiceRequest` from a slice of
//! [`tc_otel_core::MetricRecord`] and serialises it via
//! `prost::Message::encode_to_vec`. Used by
//! [`crate::exporter::OtelExporter`] when
//! `WireFormat::Protobuf` is selected on the metrics endpoint.
//!
//! Mapping rules mirror the OTLP-JSON path in
//! `exporter::build_otel_metrics_payload`:
//!
//! - `MetricKind::Gauge` → `Metric.data = Gauge { … }` with one
//!   `NumberDataPoint(AsDouble)`.
//! - `MetricKind::Sum`   → `Metric.data = Sum { temporality = CUMULATIVE,
//!                                              is_monotonic, … }`.
//! - `MetricKind::Histogram` → `Metric.data = Histogram { temporality =
//!                                              CUMULATIVE, … }` with
//!   `bucket_counts.len() == explicit_bounds.len() + 1`
//!   (a `debug_assert!` enforces the invariant).
//!
//! When `MetricRecord.trace_id` and `span_id` are both non-empty
//! 32/16-char hex strings, a single OTLP `Exemplar` is attached to the
//! data point so backends can correlate a numeric sample with the trace
//! that produced it.

use crate::proto_common::{
    build_attributes, build_resource, datetime_to_unix_nano, decode_span_id_hex,
    decode_trace_id_hex,
};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::metrics::v1::{
    exemplar, metric, number_data_point, AggregationTemporality, Exemplar, Gauge, Histogram,
    HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
};
use prost::Message;
use tc_otel_core::{MetricKind, MetricRecord};

const SCOPE_NAME: &str = "tc-otel";

/// Encode a batch of metric records into an OTLP-Protobuf payload.
///
/// Returns the serialised bytes ready to be POSTed with
/// `Content-Type: application/x-protobuf`. An empty batch produces an
/// empty `ExportMetricsServiceRequest`, which is a 0-byte payload —
/// callers should generally check for empty input before calling.
pub fn build(records: &[MetricRecord]) -> Vec<u8> {
    let request = build_request(records);
    request.encode_to_vec()
}

/// Build the OTLP `ExportMetricsServiceRequest` without serialising it.
/// Useful for round-trip tests that want to inspect the structure.
pub fn build_request(records: &[MetricRecord]) -> ExportMetricsServiceRequest {
    if records.is_empty() {
        return ExportMetricsServiceRequest {
            resource_metrics: Vec::new(),
        };
    }

    // Group all records under one ResourceMetrics block. Every record
    // emitted by tc-otel for one process carries identical resource
    // attributes (service.name, host.name, plc.ams_net_id …), so a
    // single block is correct and far smaller on the wire than a
    // per-record block.
    let resource = build_resource(&records[0].resource_attributes);
    let scope = InstrumentationScope {
        name: SCOPE_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
    };

    let metrics = records.iter().map(build_metric).collect();

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(scope),
                metrics,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn build_metric(record: &MetricRecord) -> Metric {
    let data = match record.kind {
        MetricKind::Gauge => metric::Data::Gauge(Gauge {
            data_points: vec![build_number_data_point(record)],
        }),
        MetricKind::Sum => metric::Data::Sum(Sum {
            data_points: vec![build_number_data_point(record)],
            aggregation_temporality: AggregationTemporality::Cumulative as i32,
            is_monotonic: record.is_monotonic,
        }),
        MetricKind::Histogram => {
            // OTLP requires bucket_counts.len() == explicit_bounds.len() + 1.
            // tc-otel's MetricRecord already emits aligned vectors but
            // assert in debug builds to catch any future drift.
            debug_assert_eq!(
                record.histogram_counts.len(),
                record.histogram_bounds.len() + 1,
                "OTLP histogram invariant: bucket_counts must be one longer than explicit_bounds",
            );
            metric::Data::Histogram(Histogram {
                data_points: vec![build_histogram_data_point(record)],
                aggregation_temporality: AggregationTemporality::Cumulative as i32,
            })
        }
    };

    Metric {
        name: record.name.clone(),
        description: record.description.clone(),
        unit: record.unit.clone(),
        metadata: Vec::new(),
        data: Some(data),
    }
}

fn build_number_data_point(record: &MetricRecord) -> NumberDataPoint {
    let ts = datetime_to_unix_nano(record.timestamp);
    NumberDataPoint {
        attributes: build_attributes(&record.attributes),
        // start_time_unix_nano is OPTIONAL per spec — leaving 0 is fine
        // for Gauge and accepted by VM/VL/VT/Tempo. Sum monotonic
        // counters technically prefer a real value but tc-otel doesn't
        // track per-stream start; matching the JSON-path behaviour.
        start_time_unix_nano: 0,
        time_unix_nano: ts,
        exemplars: build_exemplars(record, ts),
        flags: 0,
        value: Some(number_data_point::Value::AsDouble(record.value)),
    }
}

fn build_histogram_data_point(record: &MetricRecord) -> HistogramDataPoint {
    let ts = datetime_to_unix_nano(record.timestamp);
    HistogramDataPoint {
        attributes: build_attributes(&record.attributes),
        start_time_unix_nano: 0,
        time_unix_nano: ts,
        count: record.histogram_count,
        sum: Some(record.histogram_sum),
        bucket_counts: record.histogram_counts.clone(),
        explicit_bounds: record.histogram_bounds.clone(),
        exemplars: build_exemplars(record, ts),
        flags: 0,
        min: None,
        max: None,
    }
}

fn build_exemplars(record: &MetricRecord, ts_unix_nano: u64) -> Vec<Exemplar> {
    if record.trace_id.is_empty() && record.span_id.is_empty() {
        return Vec::new();
    }
    let trace_id = decode_trace_id_hex(&record.trace_id);
    let span_id = decode_span_id_hex(&record.span_id);
    if trace_id.is_empty() && span_id.is_empty() {
        // Both decoded to empty — bad input. Match the JSON path which
        // ALSO emits exemplar in this case (its hex strings just sit
        // there). Keep semantically identical: emit if the source said
        // there was context, even if hex was malformed.
        // But: the JSON path serialises the original (possibly bad) hex
        // string. Protobuf takes bytes, so empty is what the recipient
        // would see. Pragmatic: skip the exemplar entirely on bad hex.
        return Vec::new();
    }
    vec![Exemplar {
        filtered_attributes: Vec::new(),
        time_unix_nano: ts_unix_nano,
        span_id,
        trace_id,
        value: Some(exemplar::Value::AsDouble(record.value)),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use prost::Message as _;
    use std::collections::HashMap;

    fn ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0).unwrap()
    }

    fn gauge_record(name: &str, v: f64) -> MetricRecord {
        let mut resource = HashMap::new();
        resource.insert(
            "service.name".to_string(),
            serde_json::json!("tc-otel-test"),
        );
        let mut attrs = HashMap::new();
        attrs.insert("axis".to_string(), serde_json::json!("x"));
        MetricRecord {
            name: name.to_string(),
            description: "test gauge".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKind::Gauge,
            timestamp: ts(),
            value: v,
            is_monotonic: false,
            resource_attributes: resource,
            attributes: attrs,
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
            trace_id: String::new(),
            span_id: String::new(),
        }
    }

    #[test]
    fn empty_batch_produces_empty_request() {
        let req = build_request(&[]);
        assert!(req.resource_metrics.is_empty());
        let bytes = build(&[]);
        assert!(bytes.is_empty());
    }

    #[test]
    fn roundtrip_gauge() {
        let r = gauge_record("motor.temperature", 23.5);
        let bytes = build(std::slice::from_ref(&r));
        assert!(!bytes.is_empty());
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        let metric = &decoded.resource_metrics[0].scope_metrics[0].metrics[0];
        assert_eq!(metric.name, "motor.temperature");
        assert_eq!(metric.unit, "Cel");
        match metric.data.as_ref().unwrap() {
            metric::Data::Gauge(g) => {
                let dp = &g.data_points[0];
                match dp.value.unwrap() {
                    number_data_point::Value::AsDouble(d) => {
                        assert!((d - 23.5).abs() < 1e-9);
                    }
                    other => panic!("expected AsDouble, got {other:?}"),
                }
                let axis = dp.attributes.iter().find(|kv| kv.key == "axis").unwrap();
                match axis.value.as_ref().unwrap().value.as_ref().unwrap() {
                    opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) => {
                        assert_eq!(s, "x");
                    }
                    _ => panic!("expected StringValue"),
                }
                assert!(dp.exemplars.is_empty(), "no trace context => no exemplar");
            }
            other => panic!("expected Gauge, got {other:?}"),
        }
        // Resource carries service.name
        let res = decoded.resource_metrics[0].resource.as_ref().unwrap();
        let svc = res
            .attributes
            .iter()
            .find(|kv| kv.key == "service.name")
            .unwrap();
        match svc.value.as_ref().unwrap().value.as_ref().unwrap() {
            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) => {
                assert_eq!(s, "tc-otel-test");
            }
            _ => panic!("expected StringValue"),
        }
    }

    #[test]
    fn roundtrip_sum_monotonic() {
        let mut r = gauge_record("requests.total", 42.0);
        r.kind = MetricKind::Sum;
        r.is_monotonic = true;
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        match decoded.resource_metrics[0].scope_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Sum(s) => {
                assert!(s.is_monotonic);
                assert_eq!(
                    s.aggregation_temporality,
                    AggregationTemporality::Cumulative as i32
                );
            }
            other => panic!("expected Sum, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_histogram_bucket_alignment() {
        let mut r = gauge_record("latency.ms", 0.0);
        r.kind = MetricKind::Histogram;
        r.histogram_bounds = vec![5.0, 10.0, 25.0];
        r.histogram_counts = vec![3, 7, 4, 1]; // 4 buckets for 3 bounds
        r.histogram_count = 15;
        r.histogram_sum = 145.0;
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        match decoded.resource_metrics[0].scope_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Histogram(h) => {
                let dp = &h.data_points[0];
                assert_eq!(dp.bucket_counts, vec![3, 7, 4, 1]);
                assert_eq!(dp.explicit_bounds, vec![5.0, 10.0, 25.0]);
                assert_eq!(dp.count, 15);
                assert!((dp.sum.unwrap() - 145.0).abs() < 1e-9);
            }
            other => panic!("expected Histogram, got {other:?}"),
        }
    }

    #[test]
    fn exemplar_emitted_when_trace_context_present() {
        let mut r = gauge_record("motor.temperature", 24.0);
        r.trace_id = "0123456789abcdef0123456789abcdef".to_string();
        r.span_id = "fedcba9876543210".to_string();
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        let dp = match decoded.resource_metrics[0].scope_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Gauge(g) => &g.data_points[0],
            _ => unreachable!(),
        };
        assert_eq!(dp.exemplars.len(), 1);
        let ex = &dp.exemplars[0];
        assert_eq!(ex.trace_id.len(), 16);
        assert_eq!(ex.span_id.len(), 8);
        assert_eq!(
            ex.trace_id,
            vec![
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
        match ex.value.unwrap() {
            exemplar::Value::AsDouble(v) => assert!((v - 24.0).abs() < 1e-9),
            _ => panic!("expected AsDouble exemplar"),
        }
    }

    #[test]
    fn exemplar_skipped_when_trace_id_empty() {
        let r = gauge_record("motor.temperature", 25.0);
        // both trace_id and span_id are empty by default
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        let dp = match decoded.resource_metrics[0].scope_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Gauge(g) => &g.data_points[0],
            _ => unreachable!(),
        };
        assert!(dp.exemplars.is_empty());
    }

    #[test]
    fn timestamp_in_nanoseconds() {
        let r = gauge_record("x", 1.0);
        let bytes = build(std::slice::from_ref(&r));
        let decoded = ExportMetricsServiceRequest::decode(&bytes[..]).unwrap();
        let dp = match decoded.resource_metrics[0].scope_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Gauge(g) => &g.data_points[0],
            _ => unreachable!(),
        };
        // 2026-05-07 12:00:00 UTC in nanos
        let expected = ts().timestamp_nanos_opt().unwrap() as u64;
        assert_eq!(dp.time_unix_nano, expected);
    }
}

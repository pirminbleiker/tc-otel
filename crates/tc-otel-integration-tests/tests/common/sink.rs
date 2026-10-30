//! In-process OTLP/HTTP sink that records what the service exports and decodes
//! it into a format-neutral model (protobuf and JSON bodies normalise to the
//! same records).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::Router;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value, AnyValue, KeyValue};
use prost::Message;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// One raw HTTP request received by the sink.
#[derive(Debug, Clone)]
pub struct Captured {
    pub path: String,
    pub content_type: String,
    pub body: Bytes,
}

/// A log record in a form that does not depend on the wire format.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportedLog {
    pub resource: BTreeMap<String, String>,
    pub scope: String,
    pub body: String,
    pub severity_number: i32,
    pub severity_text: String,
    pub attributes: BTreeMap<String, String>,
}

/// A span in a form that does not depend on the wire format.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportedSpan {
    pub resource: BTreeMap<String, String>,
    pub name: String,
    pub trace_id_hex: String,
    pub span_id_hex: String,
    pub parent_span_id_hex: String,
}

struct Shared {
    captured: Mutex<Vec<Captured>>,
    count: watch::Sender<usize>,
}

pub struct OtlpSink {
    addr: SocketAddr,
    shared: Arc<Shared>,
    rx: watch::Receiver<usize>,
    task: JoinHandle<()>,
}

async fn record(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut captured = shared.captured.lock().unwrap();
    captured.push(Captured {
        path: uri.path().to_string(),
        content_type,
        body,
    });
    shared.count.send_replace(captured.len());
    StatusCode::OK
}

impl OtlpSink {
    pub async fn start() -> OtlpSink {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OTLP sink");
        let addr = listener.local_addr().expect("sink local addr");
        let (tx, rx) = watch::channel(0usize);
        let shared = Arc::new(Shared {
            captured: Mutex::new(Vec::new()),
            count: tx,
        });
        let app = Router::new().fallback(record).with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        OtlpSink {
            addr,
            shared,
            rx,
            task,
        }
    }

    pub fn logs_url(&self) -> String {
        format!("http://{}/v1/logs", self.addr)
    }

    pub fn traces_url(&self) -> String {
        format!("http://{}/v1/traces", self.addr)
    }

    /// Raw requests received so far, oldest first.
    pub fn captured(&self) -> Vec<Captured> {
        self.shared.captured.lock().unwrap().clone()
    }

    /// Wait until at least `count` log records have been exported; panics with
    /// the received detail on timeout. Returns every log record received.
    pub async fn wait_for_logs(&self, count: usize, timeout: Duration) -> Vec<ExportedLog> {
        self.wait_for("logs", count, timeout, |s| s.decode_logs())
            .await
    }

    /// Wait until at least `count` spans have been exported; panics with the
    /// received detail on timeout. Returns every span received.
    pub async fn wait_for_spans(&self, count: usize, timeout: Duration) -> Vec<ExportedSpan> {
        self.wait_for("spans", count, timeout, |s| s.decode_spans())
            .await
    }

    async fn wait_for<T>(
        &self,
        what: &str,
        count: usize,
        timeout: Duration,
        decode: impl Fn(&OtlpSink) -> Vec<T>,
    ) -> Vec<T> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.rx.clone();
        loop {
            let items = decode(self);
            if items.len() >= count {
                return items;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => {
                    let items = decode(self);
                    if items.len() >= count {
                        return items;
                    }
                    panic!(
                        "timed out after {timeout:?} waiting for {count} {what}, got {}; \
                         received requests: {:#?}",
                        items.len(),
                        self.captured()
                            .iter()
                            .map(|c| (
                                c.path.clone(),
                                c.content_type.clone(),
                                String::from_utf8_lossy(&c.body).into_owned()
                            ))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    pub fn decode_logs(&self) -> Vec<ExportedLog> {
        let mut out = Vec::new();
        for c in self.captured() {
            if !c.path.ends_with("/v1/logs") {
                continue;
            }
            if is_json(&c) {
                out.extend(decode_logs_json(&c.body));
            } else {
                out.extend(decode_logs_proto(&c.body));
            }
        }
        out
    }

    pub fn decode_spans(&self) -> Vec<ExportedSpan> {
        let mut out = Vec::new();
        for c in self.captured() {
            if !c.path.ends_with("/v1/traces") {
                continue;
            }
            if is_json(&c) {
                out.extend(decode_spans_json(&c.body));
            } else {
                out.extend(decode_spans_proto(&c.body));
            }
        }
        out
    }
}

impl Drop for OtlpSink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn is_json(c: &Captured) -> bool {
    c.content_type.starts_with("application/json")
}

// ---------------------------------------------------------------------------
// protobuf
// ---------------------------------------------------------------------------

fn proto_value(v: &Option<AnyValue>) -> String {
    match v.as_ref().and_then(|a| a.value.as_ref()) {
        Some(Value::StringValue(s)) => s.clone(),
        Some(Value::IntValue(i)) => i.to_string(),
        Some(Value::DoubleValue(d)) => d.to_string(),
        Some(Value::BoolValue(b)) => b.to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}

fn proto_attrs(attrs: &[KeyValue]) -> BTreeMap<String, String> {
    attrs
        .iter()
        .map(|kv| (kv.key.clone(), proto_value(&kv.value)))
        .collect()
}

fn decode_logs_proto(body: &[u8]) -> Vec<ExportedLog> {
    let req = ExportLogsServiceRequest::decode(body).expect("decode protobuf logs request");
    let mut out = Vec::new();
    for rl in req.resource_logs {
        let resource = rl
            .resource
            .map(|r| proto_attrs(&r.attributes))
            .unwrap_or_default();
        for sl in rl.scope_logs {
            let scope = sl.scope.map(|s| s.name).unwrap_or_default();
            for lr in sl.log_records {
                out.push(ExportedLog {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    body: proto_value(&lr.body),
                    severity_number: lr.severity_number,
                    severity_text: lr.severity_text,
                    attributes: proto_attrs(&lr.attributes),
                });
            }
        }
    }
    out
}

fn decode_spans_proto(body: &[u8]) -> Vec<ExportedSpan> {
    let req = ExportTraceServiceRequest::decode(body).expect("decode protobuf traces request");
    let mut out = Vec::new();
    for rs in req.resource_spans {
        let resource = rs
            .resource
            .map(|r| proto_attrs(&r.attributes))
            .unwrap_or_default();
        for ss in rs.scope_spans {
            for sp in ss.spans {
                out.push(ExportedSpan {
                    resource: resource.clone(),
                    name: sp.name,
                    trace_id_hex: hex::encode(&sp.trace_id),
                    span_id_hex: hex::encode(&sp.span_id),
                    parent_span_id_hex: hex::encode(&sp.parent_span_id),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON
//
// The exporter's JSON builders (`OtelExporter::build_otel_payload` /
// `build_otel_traces_payload`) emit ids as hex strings and integers as
// `{"intValue": "<decimal string>"}`; severityNumber and kind are plain JSON
// numbers. Everything is normalised here.
// ---------------------------------------------------------------------------

fn json_value(v: &serde_json::Value) -> String {
    let Some(obj) = v.as_object() else {
        return v.to_string();
    };
    if let Some(s) = obj.get("stringValue").and_then(|x| x.as_str()) {
        return s.to_string();
    }
    if let Some(i) = obj.get("intValue") {
        return match i {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(d) = obj.get("doubleValue") {
        return d.to_string();
    }
    if let Some(b) = obj.get("boolValue") {
        return b.to_string();
    }
    v.to_string()
}

fn json_attrs(v: &serde_json::Value) -> BTreeMap<String, String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|kv| {
                    let key = kv.get("key")?.as_str()?.to_string();
                    Some((key, json_value(kv.get("value")?)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn decode_logs_json(body: &[u8]) -> Vec<ExportedLog> {
    let doc: serde_json::Value = serde_json::from_slice(body).expect("parse JSON logs request");
    let mut out = Vec::new();
    for rl in doc["resourceLogs"].as_array().into_iter().flatten() {
        let resource = json_attrs(&rl["resource"]["attributes"]);
        for sl in rl["scopeLogs"].as_array().into_iter().flatten() {
            let scope = json_str(&sl["scope"], "name");
            for lr in sl["logRecords"].as_array().into_iter().flatten() {
                out.push(ExportedLog {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    body: lr.get("body").map(json_value).unwrap_or_default(),
                    severity_number: lr["severityNumber"].as_i64().unwrap_or(0) as i32,
                    severity_text: json_str(lr, "severityText"),
                    attributes: json_attrs(&lr["attributes"]),
                });
            }
        }
    }
    out
}

fn decode_spans_json(body: &[u8]) -> Vec<ExportedSpan> {
    let doc: serde_json::Value = serde_json::from_slice(body).expect("parse JSON traces request");
    let mut out = Vec::new();
    for rs in doc["resourceSpans"].as_array().into_iter().flatten() {
        let resource = json_attrs(&rs["resource"]["attributes"]);
        for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
            for sp in ss["spans"].as_array().into_iter().flatten() {
                out.push(ExportedSpan {
                    resource: resource.clone(),
                    name: json_str(sp, "name"),
                    trace_id_hex: json_str(sp, "traceId"),
                    span_id_hex: json_str(sp, "spanId"),
                    parent_span_id_hex: json_str(sp, "parentSpanId"),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_int_value_string_and_number_normalise() {
        assert_eq!(json_value(&serde_json::json!({"intValue": "42"})), "42");
        assert_eq!(json_value(&serde_json::json!({"intValue": 42})), "42");
        assert_eq!(json_value(&serde_json::json!({"stringValue": "x"})), "x");
    }
}

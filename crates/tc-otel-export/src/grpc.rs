//! gRPC OTLP Logs service implementation
//!
//! Defines the OTLP protobuf types (matching opentelemetry-proto spec) and
//! implements a tonic gRPC server for receiving logs via OTLP/gRPC.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use tc_otel_core::{LogEntry, LogLevel};
use tokio::sync::mpsc;
use tonic::codegen::http;

// ─── OTLP Protobuf types (matching opentelemetry/proto/collector/logs/v1/) ───

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExportLogsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExportLogsServiceResponse {
    #[prost(message, optional, tag = "1")]
    pub partial_success: Option<ExportLogsPartialSuccess>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExportLogsPartialSuccess {
    #[prost(int64, tag = "1")]
    pub rejected_log_records: i64,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

// ─── Resource / Scope / LogRecord types ───

#[derive(Clone, PartialEq, prost::Message)]
pub struct ResourceLogs {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_logs: Vec<ScopeLogs>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ScopeLogs {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub log_records: Vec<LogRecord>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LogRecord {
    #[prost(fixed64, tag = "1")]
    pub time_unix_nano: u64,
    #[prost(fixed64, tag = "11")]
    pub observed_time_unix_nano: u64,
    #[prost(int32, tag = "2")]
    pub severity_number: i32,
    #[prost(string, tag = "3")]
    pub severity_text: String,
    #[prost(message, optional, tag = "5")]
    pub body: Option<AnyValue>,
    #[prost(message, repeated, tag = "6")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "7")]
    pub dropped_attributes_count: u32,
    #[prost(fixed32, tag = "8")]
    pub flags: u32,
    #[prost(bytes = "vec", tag = "9")]
    pub trace_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "10")]
    pub span_id: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "2")]
    pub dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
    #[prost(message, repeated, tag = "3")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "4")]
    pub dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4, 5, 6, 7")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
        #[prost(message, tag = "5")]
        ArrayValue(super::ArrayValue),
        #[prost(message, tag = "6")]
        KvlistValue(super::KeyValueList),
        #[prost(bytes, tag = "7")]
        BytesValue(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ArrayValue {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<AnyValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyValueList {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<KeyValue>,
}

// ─── Conversion: AnyValue → serde_json::Value ───

pub fn any_value_to_json(av: &AnyValue) -> serde_json::Value {
    match &av.value {
        Some(any_value::Value::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(any_value::Value::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(any_value::Value::IntValue(i)) => serde_json::json!(*i),
        Some(any_value::Value::DoubleValue(d)) => serde_json::Number::from_f64(*d)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(any_value::Value::ArrayValue(arr)) => {
            serde_json::Value::Array(arr.values.iter().map(any_value_to_json).collect())
        }
        Some(any_value::Value::KvlistValue(kvl)) => {
            let map: serde_json::Map<String, serde_json::Value> = kvl
                .values
                .iter()
                .map(|kv| {
                    let v = kv
                        .value
                        .as_ref()
                        .map(any_value_to_json)
                        .unwrap_or(serde_json::Value::Null);
                    (kv.key.clone(), v)
                })
                .collect();
            serde_json::Value::Object(map)
        }
        Some(any_value::Value::BytesValue(b)) => {
            // Encode bytes as hex string
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            serde_json::Value::String(hex)
        }
        None => serde_json::Value::Null,
    }
}

/// Extract a string value from a KeyValue slice by key name.
fn extract_string_attr(attrs: &[KeyValue], key: &str) -> String {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|av| match &av.value {
            Some(any_value::Value::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Extract an i32 value from a KeyValue slice by key name.
fn extract_int_attr(attrs: &[KeyValue], key: &str) -> i32 {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|av| match &av.value {
            Some(any_value::Value::IntValue(i)) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(0)
}

// ─── Conversion: OTEL severity → LogLevel ───

/// Map OTEL SeverityNumber to internal LogLevel.
/// OTEL ranges: 1-4=TRACE, 5-8=DEBUG, 9-12=INFO, 13-16=WARN, 17-20=ERROR, 21-24=FATAL
pub fn severity_to_log_level(severity_number: i32) -> LogLevel {
    match severity_number {
        1..=4 => LogLevel::Trace,
        5..=8 => LogLevel::Debug,
        9..=12 => LogLevel::Info,
        13..=16 => LogLevel::Warn,
        17..=20 => LogLevel::Error,
        21..=24 => LogLevel::Fatal,
        _ => LogLevel::Info, // UNSPECIFIED or out-of-range defaults to Info
    }
}

// ─── Conversion: OTLP request → Vec<LogEntry> ───

/// Convert an OTLP ExportLogsServiceRequest into a vector of LogEntry.
pub fn convert_request_to_entries(request: &ExportLogsServiceRequest) -> Vec<LogEntry> {
    let mut entries = Vec::new();

    for resource_logs in &request.resource_logs {
        let resource_attrs = resource_logs
            .resource
            .as_ref()
            .map(|r| &r.attributes[..])
            .unwrap_or(&[]);

        // Extract resource-level fields
        let hostname = extract_string_attr(resource_attrs, "host.name");
        let project_name = extract_string_attr(resource_attrs, "service.name");
        let app_name = extract_string_attr(resource_attrs, "service.instance.id");
        let task_index = extract_int_attr(resource_attrs, "process.pid");
        let task_name = extract_string_attr(resource_attrs, "process.command_line");

        for scope_logs in &resource_logs.scope_logs {
            let logger = scope_logs
                .scope
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default();

            for log_record in &scope_logs.log_records {
                let message = log_record
                    .body
                    .as_ref()
                    .map(|av| match &av.value {
                        Some(any_value::Value::StringValue(s)) => s.clone(),
                        Some(other) => {
                            // For non-string bodies, serialize to JSON string
                            let temp = AnyValue {
                                value: Some(other.clone()),
                            };
                            any_value_to_json(&temp).to_string()
                        }
                        None => String::new(),
                    })
                    .unwrap_or_default();

                let level = severity_to_log_level(log_record.severity_number);

                let source = extract_string_attr(&log_record.attributes, "source.address");

                let mut entry = LogEntry::new(
                    if source.is_empty() {
                        "otel-grpc".to_string()
                    } else {
                        source
                    },
                    hostname.clone(),
                    message,
                    logger.clone(),
                    level,
                );

                // Set resource-level metadata
                entry.project_name = project_name.clone();
                entry.app_name = app_name.clone();
                entry.task_index = task_index;
                entry.task_name = task_name.clone();

                // Set timestamp from proto
                if log_record.time_unix_nano > 0 {
                    let secs = (log_record.time_unix_nano / 1_000_000_000) as i64;
                    let nsecs = (log_record.time_unix_nano % 1_000_000_000) as u32;
                    if let Some(ts) = Utc.timestamp_opt(secs, nsecs).single() {
                        entry.plc_timestamp = ts;
                    }
                }

                // Propagate trace context from OTLP log record
                if log_record.trace_id.len() == 16 {
                    let mut trace_id = [0u8; 16];
                    trace_id.copy_from_slice(&log_record.trace_id);
                    entry.trace_id = trace_id;
                }
                if log_record.span_id.len() == 8 {
                    let mut span_id = [0u8; 8];
                    span_id.copy_from_slice(&log_record.span_id);
                    entry.span_id = span_id;
                }

                // Convert log attributes to context
                let mut context = HashMap::new();
                for kv in &log_record.attributes {
                    // Skip source.address since it's already used
                    if kv.key == "source.address" {
                        continue;
                    }
                    let value = kv
                        .value
                        .as_ref()
                        .map(any_value_to_json)
                        .unwrap_or(serde_json::Value::Null);
                    context.insert(kv.key.clone(), value);
                }
                entry.context = context;

                entries.push(entry);
            }
        }
    }

    entries
}

// ─── Tonic gRPC Service ───

/// The LogsService trait — handler for OTLP log export RPC.
#[tonic::async_trait]
pub trait LogsService: Send + Sync + 'static {
    async fn export(
        &self,
        request: tonic::Request<ExportLogsServiceRequest>,
    ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status>;
}

/// Implementation of the OTLP LogsService that forwards entries to a channel.
#[derive(Clone)]
pub struct LogsServiceImpl {
    log_tx: mpsc::Sender<LogEntry>,
}

impl LogsServiceImpl {
    pub fn new(log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self { log_tx }
    }
}

#[tonic::async_trait]
impl LogsService for LogsServiceImpl {
    async fn export(
        &self,
        request: tonic::Request<ExportLogsServiceRequest>,
    ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
        let req = request.into_inner();
        let entries = convert_request_to_entries(&req);

        let total = entries.len() as i64;
        let mut rejected = 0i64;

        for entry in entries {
            if self.log_tx.try_send(entry).is_err() {
                rejected += 1;
            }
        }

        let response = ExportLogsServiceResponse {
            partial_success: if rejected > 0 {
                Some(ExportLogsPartialSuccess {
                    rejected_log_records: rejected,
                    error_message: format!(
                        "Channel full: {}/{} log records rejected",
                        rejected, total
                    ),
                })
            } else {
                None
            },
        };

        Ok(tonic::Response::new(response))
    }
}

// ─── Tonic Service Server (manual, equivalent to tonic-build generated code) ───

/// Wrapper that implements tower::Service for the LogsService trait, enabling
/// it to be registered with tonic::transport::Server.
#[derive(Clone)]
pub struct LogsServiceServer<T> {
    inner: Arc<T>,
}

impl<T: LogsService> LogsServiceServer<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl<T: LogsService> tonic::server::NamedService for LogsServiceServer<T> {
    const NAME: &'static str = "opentelemetry.proto.collector.logs.v1.LogsService";
}

impl<T, B> tower::Service<http::Request<B>> for LogsServiceServer<T>
where
    T: LogsService,
    B: http_body::Body + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = self.inner.clone();

        match req.uri().path() {
            "/opentelemetry.proto.collector.logs.v1.LogsService/Export" => {
                struct ExportSvc<T: LogsService>(Arc<T>);

                impl<T: LogsService> tonic::server::UnaryService<ExportLogsServiceRequest> for ExportSvc<T> {
                    type Response = ExportLogsServiceResponse;
                    type Future = std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = Result<tonic::Response<Self::Response>, tonic::Status>,
                                > + Send
                                + 'static,
                        >,
                    >;

                    fn call(
                        &mut self,
                        request: tonic::Request<ExportLogsServiceRequest>,
                    ) -> Self::Future {
                        let inner = self.0.clone();
                        Box::pin(async move { inner.export(request).await })
                    }
                }

                let fut = async move {
                    let mut grpc = tonic::server::Grpc::new(tonic::codec::ProstCodec::default());
                    Ok(grpc.unary(ExportSvc(inner), req).await)
                };

                Box::pin(fut)
            }
            _ => Box::pin(async move {
                Ok(http::Response::builder()
                    .status(200)
                    .header("grpc-status", "12")
                    .header("content-type", "application/grpc")
                    .body(tonic::body::empty_body())
                    .unwrap())
            }),
        }
    }
}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    // ── Helper: build a KeyValue ──

    fn kv_string(key: &str, val: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(val.to_string())),
            }),
        }
    }

    fn kv_int(key: &str, val: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::IntValue(val)),
            }),
        }
    }

    // ── Helper: build a minimal OTLP request ──

    fn make_request(
        resource_attrs: Vec<KeyValue>,
        scope_name: &str,
        records: Vec<LogRecord>,
    ) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: resource_attrs,
                    dropped_attributes_count: 0,
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: scope_name.to_string(),
                        version: String::new(),
                        attributes: vec![],
                        dropped_attributes_count: 0,
                    }),
                    log_records: records,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn make_log_record(severity: i32, body: &str, attrs: Vec<KeyValue>, time_ns: u64) -> LogRecord {
        LogRecord {
            time_unix_nano: time_ns,
            observed_time_unix_nano: 0,
            severity_number: severity,
            severity_text: String::new(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body.to_string())),
            }),
            attributes: attrs,
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: vec![],
            span_id: vec![],
        }
    }

    // ── Severity mapping tests ──

    #[test]
    fn test_severity_trace() {
        assert_eq!(severity_to_log_level(1), LogLevel::Trace);
        assert_eq!(severity_to_log_level(4), LogLevel::Trace);
    }

    #[test]
    fn test_severity_debug() {
        assert_eq!(severity_to_log_level(5), LogLevel::Debug);
        assert_eq!(severity_to_log_level(8), LogLevel::Debug);
    }

    #[test]
    fn test_severity_info() {
        assert_eq!(severity_to_log_level(9), LogLevel::Info);
        assert_eq!(severity_to_log_level(12), LogLevel::Info);
    }

    #[test]
    fn test_severity_warn() {
        assert_eq!(severity_to_log_level(13), LogLevel::Warn);
        assert_eq!(severity_to_log_level(16), LogLevel::Warn);
    }

    #[test]
    fn test_severity_error() {
        assert_eq!(severity_to_log_level(17), LogLevel::Error);
        assert_eq!(severity_to_log_level(20), LogLevel::Error);
    }

    #[test]
    fn test_severity_fatal() {
        assert_eq!(severity_to_log_level(21), LogLevel::Fatal);
        assert_eq!(severity_to_log_level(24), LogLevel::Fatal);
    }

    #[test]
    fn test_severity_unspecified_defaults_to_info() {
        assert_eq!(severity_to_log_level(0), LogLevel::Info);
    }

    #[test]
    fn test_severity_out_of_range_defaults_to_info() {
        assert_eq!(severity_to_log_level(99), LogLevel::Info);
        assert_eq!(severity_to_log_level(-1), LogLevel::Info);
    }

    // ── AnyValue conversion tests ──

    #[test]
    fn test_any_value_string() {
        let av = AnyValue {
            value: Some(any_value::Value::StringValue("hello".to_string())),
        };
        assert_eq!(any_value_to_json(&av), serde_json::json!("hello"));
    }

    #[test]
    fn test_any_value_bool() {
        let av = AnyValue {
            value: Some(any_value::Value::BoolValue(true)),
        };
        assert_eq!(any_value_to_json(&av), serde_json::json!(true));
    }

    #[test]
    fn test_any_value_int() {
        let av = AnyValue {
            value: Some(any_value::Value::IntValue(42)),
        };
        assert_eq!(any_value_to_json(&av), serde_json::json!(42));
    }

    #[test]
    fn test_any_value_double() {
        let av = AnyValue {
            value: Some(any_value::Value::DoubleValue(42.5)),
        };
        assert_eq!(any_value_to_json(&av), serde_json::json!(42.5));
    }

    #[test]
    fn test_any_value_array() {
        let av = AnyValue {
            value: Some(any_value::Value::ArrayValue(ArrayValue {
                values: vec![
                    AnyValue {
                        value: Some(any_value::Value::StringValue("a".to_string())),
                    },
                    AnyValue {
                        value: Some(any_value::Value::IntValue(1)),
                    },
                ],
            })),
        };
        assert_eq!(any_value_to_json(&av), serde_json::json!(["a", 1]));
    }

    #[test]
    fn test_any_value_kvlist() {
        let av = AnyValue {
            value: Some(any_value::Value::KvlistValue(KeyValueList {
                values: vec![kv_string("name", "alice"), kv_int("age", 30)],
            })),
        };
        assert_eq!(
            any_value_to_json(&av),
            serde_json::json!({"name": "alice", "age": 30})
        );
    }

    #[test]
    fn test_any_value_none() {
        let av = AnyValue { value: None };
        assert_eq!(any_value_to_json(&av), serde_json::Value::Null);
    }

    // ── Request conversion tests ──

    #[test]
    fn test_convert_empty_request() {
        let req = ExportLogsServiceRequest {
            resource_logs: vec![],
        };
        let entries = convert_request_to_entries(&req);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_convert_simple_log_record() {
        let req = make_request(
            vec![kv_string("host.name", "plc-01")],
            "test.logger",
            vec![make_log_record(9, "Hello world", vec![], 0)],
        );

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.message, "Hello world");
        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.hostname, "plc-01");
        assert_eq!(entry.logger, "test.logger");
    }

    #[test]
    fn test_convert_resource_attributes() {
        let req = make_request(
            vec![
                kv_string("host.name", "plc-hub"),
                kv_string("service.name", "MyProject"),
                kv_string("service.instance.id", "MyApp"),
                kv_int("process.pid", 42),
                kv_string("process.command_line", "MainTask"),
            ],
            "app.logger",
            vec![make_log_record(17, "Error occurred", vec![], 0)],
        );

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.hostname, "plc-hub");
        assert_eq!(entry.project_name, "MyProject");
        assert_eq!(entry.app_name, "MyApp");
        assert_eq!(entry.task_index, 42);
        assert_eq!(entry.task_name, "MainTask");
        assert_eq!(entry.level, LogLevel::Error);
    }

    #[test]
    fn test_convert_log_attributes_to_context() {
        let req = make_request(
            vec![],
            "logger",
            vec![make_log_record(
                9,
                "msg",
                vec![kv_string("user_id", "user123"), kv_int("error_code", 500)],
                0,
            )],
        );

        let entries = convert_request_to_entries(&req);
        let entry = &entries[0];
        assert_eq!(
            entry.context.get("user_id"),
            Some(&serde_json::json!("user123"))
        );
        assert_eq!(
            entry.context.get("error_code"),
            Some(&serde_json::json!(500))
        );
    }

    #[test]
    fn test_convert_timestamp() {
        // 2024-01-15T10:30:00Z in nanoseconds
        let ts_nanos: u64 = 1_705_312_200_000_000_000;
        let req = make_request(
            vec![],
            "logger",
            vec![make_log_record(9, "msg", vec![], ts_nanos)],
        );

        let entries = convert_request_to_entries(&req);
        let entry = &entries[0];

        let expected: DateTime<Utc> = Utc.timestamp_opt(1705312200, 0).single().unwrap();
        assert_eq!(entry.plc_timestamp, expected);
    }

    #[test]
    fn test_convert_zero_timestamp_uses_now() {
        let req = make_request(vec![], "logger", vec![make_log_record(9, "msg", vec![], 0)]);

        let before = Utc::now();
        let entries = convert_request_to_entries(&req);
        let after = Utc::now();

        let ts = entries[0].plc_timestamp;
        assert!(ts >= before && ts <= after);
    }

    #[test]
    fn test_convert_multiple_records() {
        let req = make_request(
            vec![kv_string("host.name", "plc-01")],
            "logger",
            vec![
                make_log_record(9, "msg1", vec![], 0),
                make_log_record(17, "msg2", vec![], 0),
                make_log_record(5, "msg3", vec![], 0),
            ],
        );

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message, "msg1");
        assert_eq!(entries[0].level, LogLevel::Info);
        assert_eq!(entries[1].message, "msg2");
        assert_eq!(entries[1].level, LogLevel::Error);
        assert_eq!(entries[2].message, "msg3");
        assert_eq!(entries[2].level, LogLevel::Debug);
    }

    #[test]
    fn test_convert_multiple_resource_logs() {
        let req = ExportLogsServiceRequest {
            resource_logs: vec![
                ResourceLogs {
                    resource: Some(Resource {
                        attributes: vec![kv_string("host.name", "plc-01")],
                        dropped_attributes_count: 0,
                    }),
                    scope_logs: vec![ScopeLogs {
                        scope: None,
                        log_records: vec![make_log_record(9, "from-plc-01", vec![], 0)],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                },
                ResourceLogs {
                    resource: Some(Resource {
                        attributes: vec![kv_string("host.name", "plc-02")],
                        dropped_attributes_count: 0,
                    }),
                    scope_logs: vec![ScopeLogs {
                        scope: None,
                        log_records: vec![make_log_record(17, "from-plc-02", vec![], 0)],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                },
            ],
        };

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hostname, "plc-01");
        assert_eq!(entries[0].message, "from-plc-01");
        assert_eq!(entries[1].hostname, "plc-02");
        assert_eq!(entries[1].message, "from-plc-02");
    }

    #[test]
    fn test_convert_source_address_from_attributes() {
        let req = make_request(
            vec![],
            "logger",
            vec![make_log_record(
                9,
                "msg",
                vec![kv_string("source.address", "192.168.1.1:851")],
                0,
            )],
        );

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries[0].source, "192.168.1.1:851");
        // source.address should not appear in context since it's used for source
        assert!(!entries[0].context.contains_key("source.address"));
    }

    #[test]
    fn test_convert_default_source_when_no_address() {
        let req = make_request(vec![], "logger", vec![make_log_record(9, "msg", vec![], 0)]);

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries[0].source, "otel-grpc");
    }

    #[test]
    fn test_convert_no_resource() {
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: None,
                scope_logs: vec![ScopeLogs {
                    scope: None,
                    log_records: vec![make_log_record(9, "msg", vec![], 0)],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hostname, "");
        assert_eq!(entries[0].logger, "");
    }

    #[test]
    fn test_convert_no_body() {
        let req = make_request(
            vec![],
            "logger",
            vec![LogRecord {
                time_unix_nano: 0,
                observed_time_unix_nano: 0,
                severity_number: 9,
                severity_text: String::new(),
                body: None,
                attributes: vec![],
                dropped_attributes_count: 0,
                flags: 0,
                trace_id: vec![],
                span_id: vec![],
            }],
        );

        let entries = convert_request_to_entries(&req);
        assert_eq!(entries[0].message, "");
    }

    // ── gRPC service tests ──

    #[tokio::test]
    async fn test_service_export_success() {
        let (tx, mut rx) = mpsc::channel(100);
        let svc = LogsServiceImpl::new(tx);

        let req = tonic::Request::new(make_request(
            vec![kv_string("host.name", "plc-01")],
            "test.logger",
            vec![make_log_record(9, "Hello", vec![], 0)],
        ));

        let resp = svc.export(req).await.unwrap();
        let resp = resp.into_inner();
        assert!(resp.partial_success.is_none());

        // Verify entry was sent to channel
        let entry = rx.try_recv().unwrap();
        assert_eq!(entry.message, "Hello");
        assert_eq!(entry.hostname, "plc-01");
        assert_eq!(entry.logger, "test.logger");
        assert_eq!(entry.level, LogLevel::Info);
    }

    #[tokio::test]
    async fn test_service_export_multiple_entries() {
        let (tx, mut rx) = mpsc::channel(100);
        let svc = LogsServiceImpl::new(tx);

        let req = tonic::Request::new(make_request(
            vec![],
            "logger",
            vec![
                make_log_record(9, "msg1", vec![], 0),
                make_log_record(17, "msg2", vec![], 0),
            ],
        ));

        let resp = svc.export(req).await.unwrap();
        assert!(resp.into_inner().partial_success.is_none());

        assert_eq!(rx.try_recv().unwrap().message, "msg1");
        assert_eq!(rx.try_recv().unwrap().message, "msg2");
    }

    #[tokio::test]
    async fn test_service_export_channel_full() {
        // Channel with capacity 1
        let (tx, _rx) = mpsc::channel(1);
        let svc = LogsServiceImpl::new(tx);

        // Send 3 records — first should succeed, rest should be rejected
        let req = tonic::Request::new(make_request(
            vec![],
            "logger",
            vec![
                make_log_record(9, "msg1", vec![], 0),
                make_log_record(9, "msg2", vec![], 0),
                make_log_record(9, "msg3", vec![], 0),
            ],
        ));

        let resp = svc.export(req).await.unwrap();
        let inner = resp.into_inner();
        let ps = inner.partial_success.unwrap();
        assert!(ps.rejected_log_records >= 1);
        assert!(ps.error_message.contains("Channel full"));
    }

    #[tokio::test]
    async fn test_service_export_empty_request() {
        let (tx, _rx) = mpsc::channel(100);
        let svc = LogsServiceImpl::new(tx);

        let req = tonic::Request::new(ExportLogsServiceRequest {
            resource_logs: vec![],
        });

        let resp = svc.export(req).await.unwrap();
        assert!(resp.into_inner().partial_success.is_none());
    }
}

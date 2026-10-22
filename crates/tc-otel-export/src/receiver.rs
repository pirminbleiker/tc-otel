//! OTEL receiver endpoints for HTTP and gRPC

use crate::error::*;
use crate::grpc::{
    convert_request_to_entries_with_source, ExportLogsPartialSuccess, ExportLogsServiceRequest,
    ExportLogsServiceResponse,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use prost::Message;
use tc_otel_core::LogEntry;
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;

/// OTEL HTTP Receiver for HTTP/JSON OTLP endpoint
pub struct OtelHttpReceiver {
    host: String,
    port: u16,
    log_tx: mpsc::Sender<LogEntry>,
}

impl OtelHttpReceiver {
    pub fn new(host: String, port: u16, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self { host, port, log_tx }
    }

    /// Start the HTTP receiver
    pub async fn start(&self) -> Result<()> {
        let router = router(self.log_tx.clone());

        let addr = format!("{}:{}", self.host, self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        tracing::info!("OTEL HTTP receiver listening on {}", addr);

        axum::serve(listener, router)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        Ok(())
    }
}

/// OTEL gRPC Receiver for OTLP/gRPC endpoint
pub struct OtelGrpcReceiver {
    host: String,
    port: u16,
    log_tx: mpsc::Sender<LogEntry>,
}

impl OtelGrpcReceiver {
    pub fn new(host: String, port: u16, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self { host, port, log_tx }
    }

    /// Start the gRPC receiver
    pub async fn start(&self) -> Result<()> {
        let svc = crate::grpc::LogsServiceImpl::new(self.log_tx.clone());
        let server = crate::grpc::LogsServiceServer::new(svc);

        let addr = format!("{}:{}", self.host, self.port)
            .parse()
            .map_err(|e| OtelError::ReceiverError(format!("Invalid address: {e}")))?;

        tracing::info!("OTEL gRPC receiver listening on {}", addr);

        tonic::transport::Server::builder()
            .add_service(server)
            .serve(addr)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        Ok(())
    }
}

/// Build the OTLP/HTTP router (`POST /v1/logs`).
fn router(log_tx: mpsc::Sender<LogEntry>) -> Router {
    Router::new()
        .route("/v1/logs", post(handle_logs_request))
        .layer(CorsLayer::permissive())
        .with_state(log_tx)
}

const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";
const CONTENT_TYPE_JSON: &str = "application/json";

/// Wire encoding of an OTLP/HTTP request; the response uses the same one.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OtlpEncoding {
    Protobuf,
    Json,
}

impl OtlpEncoding {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(CONTENT_TYPE_JSON);
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match mime.as_str() {
            CONTENT_TYPE_PROTOBUF => Some(Self::Protobuf),
            CONTENT_TYPE_JSON => Some(Self::Json),
            _ => None,
        }
    }

    fn decode(self, body: &[u8]) -> std::result::Result<ExportLogsServiceRequest, String> {
        match self {
            Self::Protobuf => ExportLogsServiceRequest::decode(body)
                .map_err(|e| format!("invalid OTLP protobuf payload: {e}")),
            Self::Json => {
                let json: serde_json::Value = serde_json::from_slice(body)
                    .map_err(|e| format!("invalid JSON payload: {e}"))?;
                crate::otlp_json::parse_logs_request(&json)
                    .map_err(|e| format!("invalid OTLP/JSON payload: {e}"))
            }
        }
    }

    fn encode_response(self, response: &ExportLogsServiceResponse) -> Response {
        match self {
            Self::Protobuf => (
                [(header::CONTENT_TYPE, CONTENT_TYPE_PROTOBUF)],
                response.encode_to_vec(),
            )
                .into_response(),
            Self::Json => {
                // OTLP/JSON: lowerCamelCase keys, int64 encoded as string.
                let body = match &response.partial_success {
                    Some(ps) => serde_json::json!({
                        "partialSuccess": {
                            "rejectedLogRecords": ps.rejected_log_records.to_string(),
                            "errorMessage": ps.error_message,
                        }
                    }),
                    None => serde_json::json!({}),
                };
                (
                    [(header::CONTENT_TYPE, CONTENT_TYPE_JSON)],
                    body.to_string(),
                )
                    .into_response()
            }
        }
    }
}

/// Handle an OTLP/HTTP logs export (`ExportLogsServiceRequest`, protobuf or JSON).
///
/// Records that do not fit into the channel are reported via `partialSuccess`,
/// matching the gRPC receiver.
async fn handle_logs_request(
    State(log_tx): State<mpsc::Sender<LogEntry>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(encoding) = OtlpEncoding::from_headers(&headers) else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Content-Type must be {CONTENT_TYPE_PROTOBUF} or {CONTENT_TYPE_JSON}"),
        )
            .into_response();
    };

    let request = match encoding.decode(&body) {
        Ok(request) => request,
        Err(e) => {
            tracing::debug!("Rejected OTLP/HTTP logs request: {}", e);
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
    };

    let entries = convert_request_to_entries_with_source(&request, "otel-http");
    let total = entries.len();
    let mut rejected = 0usize;
    for entry in entries {
        if log_tx.try_send(entry).is_err() {
            rejected += 1;
        }
    }

    let response = ExportLogsServiceResponse {
        partial_success: (rejected > 0).then(|| ExportLogsPartialSuccess {
            rejected_log_records: rejected as i64,
            error_message: format!("Channel full: {rejected}/{total} log records rejected"),
        }),
    };

    encoding.encode_response(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::{
        any_value, AnyValue, KeyValue, LogRecord, Resource, ResourceLogs, ScopeLogs,
    };
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tc_otel_core::LogLevel;
    use tower::ServiceExt;

    async fn post_logs(
        log_tx: mpsc::Sender<LogEntry>,
        content_type: Option<&str>,
        body: impl Into<Body>,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let mut req = Request::post("/v1/logs");
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        let response = router(log_tx)
            .oneshot(req.body(body.into()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, ct, body.to_vec())
    }

    fn json_payload() -> serde_json::Value {
        serde_json::json!({
            "resourceLogs": [{
                "resource": { "attributes": [
                    { "key": "service.name", "value": { "stringValue": "Line1" } },
                    { "key": "host.name", "value": { "stringValue": "plc-01" } },
                    { "key": "tc.task.index", "value": { "intValue": "2" } }
                ]},
                "scopeLogs": [{
                    "scope": { "name": "MAIN.fbMotor" },
                    "logRecords": [
                        {
                            "timeUnixNano": "1700000000123456789",
                            "severityNumber": 13,
                            "body": { "stringValue": "Motor overheating" },
                            "traceId": "0102030405060708090a0b0c0d0e0f10",
                            "spanId": "a1a2a3a4a5a6a7a8",
                            "attributes": [
                                { "key": "temperature", "value": { "doubleValue": 92.5 } }
                            ]
                        },
                        { "severityNumber": 17, "body": { "stringValue": "Second" } }
                    ]
                }]
            }]
        })
    }

    #[tokio::test]
    async fn test_http_json_logs_are_parsed() {
        let (tx, mut rx) = mpsc::channel(10);
        let (status, ct, body) =
            post_logs(tx, Some("application/json"), json_payload().to_string()).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("application/json"));
        assert_eq!(body, b"{}");

        let first = rx.try_recv().unwrap();
        assert_eq!(first.source, "otel-http");
        assert_eq!(first.hostname, "plc-01");
        assert_eq!(first.project_name, "Line1");
        assert_eq!(first.task_index, 2);
        assert_eq!(first.logger, "MAIN.fbMotor");
        assert_eq!(first.message, "Motor overheating");
        assert_eq!(first.level, LogLevel::Warn);
        assert_eq!(first.plc_timestamp.timestamp(), 1_700_000_000);
        assert_eq!(first.plc_timestamp.timestamp_subsec_nanos(), 123_456_789);
        assert_eq!(
            first.trace_id,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(
            first.span_id,
            [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8]
        );
        assert_eq!(first.context["temperature"], serde_json::json!(92.5));

        let second = rx.try_recv().unwrap();
        assert_eq!(second.message, "Second");
        assert_eq!(second.level, LogLevel::Error);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_http_json_with_charset_and_missing_content_type() {
        let (tx, mut rx) = mpsc::channel(10);
        let (status, _, _) = post_logs(
            tx.clone(),
            Some("application/json; charset=utf-8"),
            json_payload().to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _, _) = post_logs(tx, None, json_payload().to_string()).await;
        assert_eq!(status, StatusCode::OK);

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 4);
    }

    #[tokio::test]
    async fn test_http_protobuf_logs_are_parsed() {
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "host.name".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("plc-02".into())),
                        }),
                    }],
                    dropped_attributes_count: 0,
                }),
                scope_logs: vec![ScopeLogs {
                    scope: None,
                    log_records: vec![LogRecord {
                        severity_number: 21,
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("E-Stop".into())),
                        }),
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let (tx, mut rx) = mpsc::channel(10);
        let (status, ct, body) =
            post_logs(tx, Some("application/x-protobuf"), request.encode_to_vec()).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("application/x-protobuf"));
        let response = ExportLogsServiceResponse::decode(body.as_slice()).unwrap();
        assert!(response.partial_success.is_none());

        let entry = rx.try_recv().unwrap();
        assert_eq!(entry.source, "otel-http");
        assert_eq!(entry.hostname, "plc-02");
        assert_eq!(entry.message, "E-Stop");
        assert_eq!(entry.level, LogLevel::Fatal);
    }

    #[tokio::test]
    async fn test_http_empty_request_is_accepted() {
        let (tx, mut rx) = mpsc::channel(10);
        let (status, _, body) = post_logs(tx.clone(), Some("application/json"), "{}").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"{}");

        let (status, _, _) = post_logs(tx, Some("application/x-protobuf"), Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_http_invalid_payloads_are_bad_request() {
        let (tx, mut rx) = mpsc::channel(10);
        let (status, _, _) = post_logs(tx.clone(), Some("application/json"), "not json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _, _) = post_logs(
            tx.clone(),
            Some("application/json"),
            r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"traceId":"xyz"}]}]}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _, _) =
            post_logs(tx, Some("application/x-protobuf"), vec![0xff, 0xff, 0xff]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_http_unsupported_content_type() {
        let (tx, _rx) = mpsc::channel(10);
        let (status, _, _) = post_logs(tx, Some("text/plain"), "hello").await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn test_http_channel_full_reports_partial_success() {
        let (tx, mut rx) = mpsc::channel(1);
        let (status, _, body) =
            post_logs(tx, Some("application/json"), json_payload().to_string()).await;

        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["partialSuccess"]["rejectedLogRecords"], "1");
        assert_eq!(
            body["partialSuccess"]["errorMessage"],
            "Channel full: 1/2 log records rejected"
        );
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn test_otel_http_receiver_creation() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelHttpReceiver::new("127.0.0.1".to_string(), 4318, tx);
        assert_eq!(receiver.port, 4318);
        assert_eq!(receiver.host, "127.0.0.1");
    }

    #[test]
    fn test_otel_http_receiver_custom_host() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelHttpReceiver::new("0.0.0.0".to_string(), 8080, tx);
        assert_eq!(receiver.host, "0.0.0.0");
        assert_eq!(receiver.port, 8080);
    }

    #[test]
    fn test_otel_grpc_receiver_creation() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelGrpcReceiver::new("127.0.0.1".to_string(), 4317, tx);
        assert_eq!(receiver.port, 4317);
        assert_eq!(receiver.host, "127.0.0.1");
    }

    #[test]
    fn test_otel_grpc_receiver_config() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelGrpcReceiver::new("localhost".to_string(), 9090, tx);
        assert_eq!(receiver.host, "localhost");
        assert_eq!(receiver.port, 9090);
    }

    #[test]
    fn test_multiple_receivers_same_channel() {
        let (tx, _rx) = mpsc::channel(100);

        let http_receiver = OtelHttpReceiver::new("127.0.0.1".to_string(), 4318, tx.clone());
        let grpc_receiver = OtelGrpcReceiver::new("127.0.0.1".to_string(), 4317, tx.clone());

        assert_eq!(http_receiver.port, 4318);
        assert_eq!(grpc_receiver.port, 4317);
    }
}

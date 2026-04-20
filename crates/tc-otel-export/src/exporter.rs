//! OTEL exporter for sending logs to collectors

use crate::error::*;
use regex::Regex;
use serde_json::json;
use std::time::Duration;
use tc_otel_core::{LogRecord, MetricKind, MetricRecord, TraceRecord};

/// Helper function to expand environment variables in strings
/// Supports ${VAR_NAME} syntax, e.g., "Bearer ${API_KEY}"
fn expand_env_vars(template: &str) -> String {
    // Pattern: ${VARIABLE_NAME}
    let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("regex should compile");

    re.replace_all(template, |caps: &regex::Captures| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!("Environment variable not found: {}", var_name);
            format!("${{{}}}", var_name)
        })
    })
    .to_string()
}

/// Configuration for OTEL export
#[derive(Clone, Debug)]
pub struct ExportConfig {
    /// Collector endpoint URL
    pub endpoint: String,
    /// Maximum number of records per batch
    pub batch_size: usize,
    /// Maximum number of retry attempts
    pub max_retries: usize,
    /// Delay between retries
    pub retry_delay_ms: u64,
    /// HTTP request timeout
    pub timeout_secs: u64,
    /// Optional auth header with environment variable expansion (e.g., "Bearer ${OTEL_AUTH_TOKEN}")
    pub auth_header: Option<String>,
}

impl Default for ExportConfig {
    fn default() -> Self {
        // Support OTEL standard environment variables
        let auth_header = std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok();

        Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "https://localhost:4318/v1/logs".to_string()),
            batch_size: 100,
            max_retries: 3,
            retry_delay_ms: 100,
            timeout_secs: 30,
            auth_header,
        }
    }
}

/// OTEL Exporter handles sending logs to OTEL collectors
pub struct OtelExporter {
    config: ExportConfig,
    http_client: reqwest::Client,
}

impl OtelExporter {
    /// Create a new exporter with default config
    pub fn new(endpoint: String, batch_size: usize, max_retries: usize) -> Self {
        let config = ExportConfig {
            endpoint,
            batch_size,
            max_retries,
            ..Default::default()
        };
        Self::with_config(config)
    }

    /// Create a new exporter with custom config
    pub fn with_config(config: ExportConfig) -> Self {
        // Build HTTP client - allow HTTP for internal Docker networking
        let is_local = config.endpoint.contains("localhost")
            || config.endpoint.contains("127.0.0.1")
            || config.endpoint.contains("otel-collector")
            || config.endpoint.starts_with("http://");

        let mut builder =
            reqwest::ClientBuilder::new().timeout(Duration::from_secs(config.timeout_secs));

        if !is_local {
            builder = builder.https_only(true);
        }

        let http_client = builder.build().unwrap_or_else(|_| {
            tracing::warn!("Failed to build HTTP client, falling back to default");
            reqwest::Client::new()
        });

        Self {
            config,
            http_client,
        }
    }

    /// Export a single log record
    pub async fn export(&self, record: LogRecord) -> Result<()> {
        self.export_batch(vec![record]).await
    }

    /// Export a batch of log records with retry logic
    pub async fn export_batch(&self, records: Vec<LogRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let payload = self.build_otel_payload(&records)?;

        self.send_with_retry(&payload).await
    }

    /// Send payload to collector with exponential backoff retry
    /// Only retries on transient errors (5xx), fails immediately on permanent errors (4xx)
    async fn send_with_retry(&self, payload: &str) -> Result<()> {
        let mut retry_count = 0;
        let mut delay_ms = self.config.retry_delay_ms;

        loop {
            match self.send_payload(payload).await {
                Ok(_) => {
                    tracing::debug!("Successfully exported logs to {}", self.config.endpoint);
                    return Ok(());
                }
                Err(e) => {
                    // Check if error is retryable
                    let is_retryable = Self::is_retryable_error(&e);

                    // Fail fast on permanent errors (4xx, auth failures, etc)
                    if !is_retryable {
                        tracing::error!("Permanent export error, not retrying: {}", e);
                        return Err(e);
                    }

                    retry_count += 1;

                    if retry_count > self.config.max_retries {
                        tracing::error!(
                            "Failed to export logs after {} retries: {}",
                            self.config.max_retries,
                            e
                        );
                        return Err(e);
                    }

                    tracing::warn!(
                        "Transient export error (attempt {}/{}), retrying in {}ms: {}",
                        retry_count,
                        self.config.max_retries + 1,
                        delay_ms,
                        e
                    );

                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                    // Exponential backoff: double the delay for next attempt
                    delay_ms = (delay_ms * 2).min(5000); // Cap at 5 seconds
                }
            }
        }
    }

    /// Determine if an error is retryable (transient) or permanent
    fn is_retryable_error(err: &OtelError) -> bool {
        match err {
            OtelError::ExportFailed(msg) => {
                // Only retry on 5xx server errors (connection timeout, temporary unavailable)
                // Fail fast on 4xx client errors (auth, bad request, etc)
                !msg.contains("HTTP 4") // 4xx errors are permanent
            }
            OtelError::HttpError(_) => true, // Network errors are retryable
            OtelError::SerializationError(_) => false, // Serialization errors are permanent
            OtelError::ReceiverError(_) => false, // Receiver setup errors are permanent
            _ => false,                      // All other errors are permanent
        }
    }

    /// Send the actual HTTP request to the collector
    async fn send_payload(&self, payload: &str) -> Result<()> {
        // Special sink: print to stdout instead of HTTP POST.
        // Useful for local testing / dry-run exporter.
        if self.config.endpoint.eq_ignore_ascii_case("stdout") {
            println!("{}", payload);
            return Ok(());
        }

        let mut request = self
            .http_client
            .post(&self.config.endpoint)
            .header("Content-Type", "application/json");

        // Add authentication header if configured (with environment variable expansion)
        if let Some(auth) = &self.config.auth_header {
            let expanded_auth = expand_env_vars(auth);
            request = request.header("Authorization", expanded_auth);
        }

        let response = request
            .body(payload.to_string())
            .timeout(Duration::from_secs(self.config.timeout_secs))
            .send()
            .await
            .map_err(|e| OtelError::HttpError(format!("Request failed: {}", e)))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(OtelError::ExportFailed(format!(
                "HTTP {} from {}",
                response.status(),
                self.config.endpoint
            )))
        }
    }

    /// Build OTEL LogsData JSON payload
    /// Convert a serde_json::Value to OTLP AnyValue format
    fn to_otlp_value(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) => json!({"stringValue": s}),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    json!({"intValue": i.to_string()})
                } else {
                    json!({"doubleValue": n.as_f64().unwrap_or(0.0)})
                }
            }
            serde_json::Value::Bool(b) => json!({"boolValue": b}),
            _ => json!({"stringValue": v.to_string()}),
        }
    }

    /// Convert HashMap to OTLP attributes array format
    fn to_otlp_attributes(
        attrs: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        attrs
            .iter()
            .map(|(k, v)| json!({"key": k, "value": Self::to_otlp_value(v)}))
            .collect()
    }

    fn build_otel_payload(&self, records: &[LogRecord]) -> Result<String> {
        let resource_logs = records
            .iter()
            .map(|record| {
                let mut log_record = json!({
                    "timeUnixNano": format!("{}", record.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64),
                    "body": {
                        "stringValue": record.body.as_str().unwrap_or("")
                    },
                    "severityNumber": record.severity_number,
                    "severityText": record.severity_text.clone(),
                    "attributes": Self::to_otlp_attributes(&record.log_attributes)
                });

                // Include trace context in OTLP log record when present
                if !record.trace_id.is_empty() {
                    log_record["traceId"] = json!(record.trace_id);
                }
                if !record.span_id.is_empty() {
                    log_record["spanId"] = json!(record.span_id);
                }

                json!({
                    "resource": {
                        "attributes": Self::to_otlp_attributes(&record.resource_attributes)
                    },
                    "scopeLogs": [
                        {
                            "scope": {
                                "name": "tc-otel",
                                "attributes": Self::to_otlp_attributes(&record.scope_attributes)
                            },
                            "logRecords": [log_record]
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "resourceLogs": resource_logs
        });

        serde_json::to_string(&payload).map_err(|e| {
            OtelError::SerializationError(format!("Failed to serialize OTEL payload: {}", e))
        })
    }

    /// Export a batch of metric records with retry logic
    pub async fn export_metrics_batch(&self, records: Vec<MetricRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let payload = self.build_otel_metrics_payload(&records)?;

        // Metrics use /v1/metrics endpoint
        let endpoint = self
            .config
            .endpoint
            .replace("/v1/logs", "/v1/metrics")
            .replace("/insert/jsonline", "/v1/metrics");

        self.send_metrics_with_retry(&payload, &endpoint).await
    }

    /// Send metrics payload with retry
    async fn send_metrics_with_retry(&self, payload: &str, endpoint: &str) -> Result<()> {
        let mut retry_count = 0;
        let mut delay_ms = self.config.retry_delay_ms;

        loop {
            let mut request = self
                .http_client
                .post(endpoint)
                .header("Content-Type", "application/json");

            if let Some(auth) = &self.config.auth_header {
                let expanded_auth = expand_env_vars(auth);
                request = request.header("Authorization", expanded_auth);
            }

            match request
                .body(payload.to_string())
                .timeout(Duration::from_secs(self.config.timeout_secs))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    tracing::debug!("Successfully exported metrics to {}", endpoint);
                    return Ok(());
                }
                Ok(response) => {
                    let err = OtelError::ExportFailed(format!(
                        "HTTP {} from {}",
                        response.status(),
                        endpoint
                    ));
                    if !Self::is_retryable_error(&err) {
                        return Err(err);
                    }
                    retry_count += 1;
                    if retry_count > self.config.max_retries {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(5000);
                }
                Err(e) => {
                    let err = OtelError::HttpError(format!("Request failed: {}", e));
                    retry_count += 1;
                    if retry_count > self.config.max_retries {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(5000);
                }
            }
        }
    }

    /// Build OTLP MetricsData JSON payload
    pub fn build_otel_metrics_payload(&self, records: &[MetricRecord]) -> Result<String> {
        let resource_metrics = records
            .iter()
            .map(|record| {
                let data_point = self.build_metric_data_point(record);
                let metric = match record.kind {
                    MetricKind::Gauge => json!({
                        "name": record.name,
                        "description": record.description,
                        "unit": record.unit,
                        "gauge": {
                            "dataPoints": [data_point]
                        }
                    }),
                    MetricKind::Sum => json!({
                        "name": record.name,
                        "description": record.description,
                        "unit": record.unit,
                        "sum": {
                            "dataPoints": [data_point],
                            "aggregationTemporality": 2, // CUMULATIVE
                            "isMonotonic": record.is_monotonic
                        }
                    }),
                    MetricKind::Histogram => {
                        let hist_point = json!({
                            "timeUnixNano": format!("{}", record.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64),
                            "attributes": Self::to_otlp_attributes(&record.attributes),
                            "count": format!("{}", record.histogram_count),
                            "sum": record.histogram_sum,
                            "bucketCounts": record.histogram_counts.iter().map(|c| format!("{}", c)).collect::<Vec<_>>(),
                            "explicitBounds": record.histogram_bounds
                        });
                        json!({
                            "name": record.name,
                            "description": record.description,
                            "unit": record.unit,
                            "histogram": {
                                "dataPoints": [hist_point],
                                "aggregationTemporality": 2
                            }
                        })
                    }
                };

                json!({
                    "resource": {
                        "attributes": Self::to_otlp_attributes(&record.resource_attributes)
                    },
                    "scopeMetrics": [
                        {
                            "scope": {
                                "name": "tc-otel"
                            },
                            "metrics": [metric]
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "resourceMetrics": resource_metrics
        });

        serde_json::to_string(&payload).map_err(|e| {
            OtelError::SerializationError(format!(
                "Failed to serialize OTEL metrics payload: {}",
                e
            ))
        })
    }

    /// Build a single OTLP metric data point (for Gauge and Sum).
    ///
    /// When the record carries a W3C trace_id / span_id we attach an OTel
    /// Exemplar on the data point rather than the plain `trace_id` /
    /// `span_id` attribute set. Exemplars are the spec-native way to
    /// correlate a metric datum with the span that produced it, and
    /// Grafana Tempo / Prometheus render "View trace" links directly off
    /// this field. OTLP JSON field names are proto3-lowerCamelCase.
    fn build_metric_data_point(&self, record: &MetricRecord) -> serde_json::Value {
        let time = format!(
            "{}",
            record.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64
        );
        let mut dp = json!({
            "timeUnixNano": time,
            "asDouble": record.value,
            "attributes": Self::to_otlp_attributes(&record.attributes),
        });

        if !record.trace_id.is_empty() || !record.span_id.is_empty() {
            dp["exemplars"] = json!([{
                "timeUnixNano": format!("{}", record.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64),
                "asDouble": record.value,
                "traceId": record.trace_id,
                "spanId": record.span_id,
            }]);
        }

        dp
    }

    /// Export a batch of trace records with retry logic
    pub async fn export_traces_batch(&self, records: Vec<TraceRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let payload = self.build_otel_traces_payload(&records)?;

        // Traces use /v1/traces endpoint
        let endpoint = self
            .config
            .endpoint
            .replace("/v1/logs", "/v1/traces")
            .replace("/v1/metrics", "/v1/traces")
            .replace("/insert/jsonline", "/v1/traces");

        self.send_traces_with_retry(&payload, &endpoint).await
    }

    /// Send traces payload with retry
    async fn send_traces_with_retry(&self, payload: &str, endpoint: &str) -> Result<()> {
        let mut retry_count = 0;
        let mut delay_ms = self.config.retry_delay_ms;

        loop {
            let mut request = self
                .http_client
                .post(endpoint)
                .header("Content-Type", "application/json");

            if let Some(auth) = &self.config.auth_header {
                let expanded_auth = expand_env_vars(auth);
                request = request.header("Authorization", expanded_auth);
            }

            match request
                .body(payload.to_string())
                .timeout(Duration::from_secs(self.config.timeout_secs))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    tracing::debug!("Successfully exported traces to {}", endpoint);
                    return Ok(());
                }
                Ok(response) => {
                    let err = OtelError::ExportFailed(format!(
                        "HTTP {} from {}",
                        response.status(),
                        endpoint
                    ));
                    if !Self::is_retryable_error(&err) {
                        return Err(err);
                    }
                    retry_count += 1;
                    if retry_count > self.config.max_retries {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(5000);
                }
                Err(e) => {
                    let err = OtelError::HttpError(format!("Request failed: {}", e));
                    retry_count += 1;
                    if retry_count > self.config.max_retries {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(5000);
                }
            }
        }
    }

    /// Build OTLP TracesData JSON payload
    pub fn build_otel_traces_payload(&self, records: &[TraceRecord]) -> Result<String> {
        let resource_spans = records
            .iter()
            .map(|record| {
                let events: Vec<serde_json::Value> = record
                    .events
                    .iter()
                    .map(|event| {
                        json!({
                            "timeUnixNano": format!("{}", event.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64),
                            "name": event.name,
                            "attributes": Self::to_otlp_attributes(&event.attributes)
                        })
                    })
                    .collect();

                let span = json!({
                    "traceId": record.trace_id,
                    "spanId": record.span_id,
                    "parentSpanId": record.parent_span_id,
                    "name": record.name,
                    "kind": record.kind,
                    "startTimeUnixNano": format!("{}", record.start_time.timestamp_nanos_opt().unwrap_or(0) as u64),
                    "endTimeUnixNano": format!("{}", record.end_time.timestamp_nanos_opt().unwrap_or(0) as u64),
                    "attributes": Self::to_otlp_attributes(&record.span_attributes),
                    "status": {
                        "code": record.status_code,
                        "message": record.status_message
                    },
                    "events": events
                });

                json!({
                    "resource": {
                        "attributes": Self::to_otlp_attributes(&record.resource_attributes)
                    },
                    "scopeSpans": [
                        {
                            "scope": {
                                "name": "tc-otel",
                                "attributes": Self::to_otlp_attributes(&record.scope_attributes)
                            },
                            "spans": [span]
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "resourceSpans": resource_spans
        });

        serde_json::to_string(&payload).map_err(|e| {
            OtelError::SerializationError(format!("Failed to serialize OTEL traces payload: {}", e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_otel_exporter_creation() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);
        assert_eq!(exporter.config.batch_size, 100);
        assert_eq!(exporter.config.max_retries, 3);
    }

    #[test]
    fn test_export_config_defaults() {
        let config = ExportConfig::default();
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn test_build_otel_payload() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = LogRecord {
            timestamp: chrono::Utc::now(),
            body: serde_json::json!("Test message"),
            severity_number: 2,
            severity_text: "Information".to_string(),
            resource_attributes: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "service.name".to_string(),
                    serde_json::json!("test-service"),
                );
                map
            },
            trace_id: String::new(),
            span_id: String::new(),
            scope_attributes: Default::default(),
            log_attributes: Default::default(),
        };

        let payload = exporter.build_otel_payload(&[record]).unwrap();
        assert!(payload.contains("test-service"));
        assert!(payload.contains("Test message"));
    }

    #[test]
    fn test_export_config_custom() {
        let config = ExportConfig {
            endpoint: "http://collector.example.com:4318".to_string(),
            batch_size: 500,
            max_retries: 5,
            retry_delay_ms: 200,
            timeout_secs: 60,
            auth_header: None,
        };

        assert_eq!(config.batch_size, 500);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.retry_delay_ms, 200);
        assert_eq!(config.timeout_secs, 60);
    }

    #[test]
    fn test_otel_exporter_with_config() {
        let config = ExportConfig {
            endpoint: "https://secure.collector.local:4317".to_string(),
            batch_size: 256,
            max_retries: 10,
            retry_delay_ms: 50,
            timeout_secs: 45,
            auth_header: None,
        };

        let exporter = OtelExporter::with_config(config.clone());
        assert_eq!(
            exporter.config.endpoint,
            "https://secure.collector.local:4317"
        );
        assert_eq!(exporter.config.batch_size, 256);
    }

    #[test]
    fn test_build_payload_empty_records() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);
        let payload = exporter.build_otel_payload(&[]).unwrap();

        // Should still be valid JSON with empty resourceLogs
        assert!(payload.contains("resourceLogs"));
    }

    #[test]
    fn test_build_payload_multiple_records() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let records = vec![
            LogRecord {
                timestamp: chrono::Utc::now(),
                body: serde_json::json!("Message 1"),
                severity_number: 9,
                severity_text: "INFO".to_string(),
                trace_id: String::new(),
                span_id: String::new(),
                resource_attributes: std::collections::HashMap::new(),
                scope_attributes: std::collections::HashMap::new(),
                log_attributes: std::collections::HashMap::new(),
            },
            LogRecord {
                timestamp: chrono::Utc::now(),
                body: serde_json::json!("Message 2"),
                severity_number: 17,
                severity_text: "ERROR".to_string(),
                trace_id: String::new(),
                span_id: String::new(),
                resource_attributes: std::collections::HashMap::new(),
                scope_attributes: std::collections::HashMap::new(),
                log_attributes: std::collections::HashMap::new(),
            },
        ];

        let payload = exporter.build_otel_payload(&records).unwrap();
        assert!(payload.contains("Message 1"));
        assert!(payload.contains("Message 2"));
    }

    #[test]
    fn test_build_payload_with_attributes() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let mut resource_attrs = std::collections::HashMap::new();
        resource_attrs.insert("service.name".to_string(), serde_json::json!("my-service"));
        resource_attrs.insert("host.name".to_string(), serde_json::json!("plc-01"));

        let mut log_attrs = std::collections::HashMap::new();
        log_attrs.insert("user_id".to_string(), serde_json::json!("user_123"));

        let record = LogRecord {
            timestamp: chrono::Utc::now(),
            body: serde_json::json!("Log message"),
            severity_number: 9,
            severity_text: "INFO".to_string(),
            trace_id: String::new(),
            span_id: String::new(),
            resource_attributes: resource_attrs,
            scope_attributes: std::collections::HashMap::new(),
            log_attributes: log_attrs,
        };

        let payload = exporter.build_otel_payload(&[record]).unwrap();
        assert!(payload.contains("my-service"));
        assert!(payload.contains("plc-01"));
        assert!(payload.contains("user_123"));
    }

    #[test]
    fn test_build_payload_structure() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = LogRecord {
            timestamp: chrono::Utc::now(),
            body: serde_json::json!("Test"),
            severity_number: 9,
            severity_text: "INFO".to_string(),
            trace_id: String::new(),
            span_id: String::new(),
            resource_attributes: std::collections::HashMap::new(),
            scope_attributes: std::collections::HashMap::new(),
            log_attributes: std::collections::HashMap::new(),
        };

        let payload_str = exporter.build_otel_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        // Verify OTEL structure
        assert!(payload.get("resourceLogs").is_some());
        assert!(payload["resourceLogs"].is_array());
    }

    #[test]
    fn test_export_config_batch_size_boundary() {
        let configs = vec![
            ExportConfig {
                batch_size: 1,
                ..Default::default()
            },
            ExportConfig {
                batch_size: 1000,
                ..Default::default()
            },
            ExportConfig {
                batch_size: 10000,
                ..Default::default()
            },
        ];

        for config in configs {
            let exporter = OtelExporter::with_config(config.clone());
            assert!(exporter.config.batch_size > 0);
        }
    }

    #[test]
    fn test_export_config_retry_backoff() {
        let config = ExportConfig {
            retry_delay_ms: 100,
            ..Default::default()
        };

        assert_eq!(config.retry_delay_ms, 100);
        // The actual backoff calculation happens during retry attempts
    }

    #[test]
    fn test_otel_payload_serialization() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = LogRecord {
            timestamp: chrono::Utc::now(),
            body: serde_json::json!("Message with special chars: <>&\"'"),
            severity_number: 9,
            severity_text: "INFO".to_string(),
            trace_id: String::new(),
            span_id: String::new(),
            resource_attributes: std::collections::HashMap::new(),
            scope_attributes: std::collections::HashMap::new(),
            log_attributes: std::collections::HashMap::new(),
        };

        let payload = exporter.build_otel_payload(&[record]).unwrap();

        // Verify JSON is valid and serializable
        let _parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    }

    // ─── Metric payload tests ─────────────────────────────────────

    #[test]
    fn test_build_gauge_metrics_payload() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = MetricRecord {
            name: "plc.motor.temperature".to_string(),
            description: "Motor temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKind::Gauge,
            timestamp: chrono::Utc::now(),
            value: 72.5,
            is_monotonic: false,
            resource_attributes: {
                let mut m = std::collections::HashMap::new();
                m.insert("service.name".to_string(), serde_json::json!("TestProject"));
                m
            },
            attributes: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "plc.symbol".to_string(),
                    serde_json::json!("GVL.motor.temp"),
                );
                m
            },
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
            trace_id: String::new(),
            span_id: String::new(),
        };

        let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        // Verify OTLP metrics structure
        assert!(payload.get("resourceMetrics").is_some());
        let rm = &payload["resourceMetrics"][0];
        assert!(rm.get("resource").is_some());
        assert!(rm.get("scopeMetrics").is_some());

        let metric = &rm["scopeMetrics"][0]["metrics"][0];
        assert_eq!(metric["name"], "plc.motor.temperature");
        assert_eq!(metric["unit"], "Cel");
        assert!(metric.get("gauge").is_some());
        assert_eq!(metric["gauge"]["dataPoints"][0]["asDouble"], 72.5);
        // No trace context → no exemplars on the data point
        assert!(
            metric["gauge"]["dataPoints"][0].get("exemplars").is_none(),
            "exemplars must be absent when trace_id / span_id are empty"
        );
    }

    #[test]
    fn test_build_gauge_metrics_payload_with_exemplar() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = MetricRecord {
            name: "demo.sine".to_string(),
            description: String::new(),
            unit: "unit".to_string(),
            kind: MetricKind::Gauge,
            timestamp: chrono::Utc::now(),
            value: 0.42,
            is_monotonic: false,
            resource_attributes: std::collections::HashMap::new(),
            attributes: std::collections::HashMap::new(),
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
            trace_id: "abcdef0123456789abcdef0123456789".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };

        let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let dp = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]
            ["gauge"]["dataPoints"][0];
        let exemplars = dp["exemplars"]
            .as_array()
            .expect("exemplars array present when trace context is set");
        assert_eq!(exemplars.len(), 1);
        assert_eq!(exemplars[0]["asDouble"], 0.42);
        assert_eq!(
            exemplars[0]["traceId"],
            "abcdef0123456789abcdef0123456789"
        );
        assert_eq!(exemplars[0]["spanId"], "fedcba9876543210");
        // Exemplar timestamp should match the data point
        assert_eq!(exemplars[0]["timeUnixNano"], dp["timeUnixNano"]);
    }

    #[test]
    fn test_build_sum_metrics_payload() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = MetricRecord {
            name: "plc.errors.total".to_string(),
            description: "Total error count".to_string(),
            unit: "{count}".to_string(),
            kind: MetricKind::Sum,
            timestamp: chrono::Utc::now(),
            value: 42.0,
            is_monotonic: true,
            resource_attributes: std::collections::HashMap::new(),
            attributes: std::collections::HashMap::new(),
            histogram_bounds: Vec::new(),
            histogram_counts: Vec::new(),
            histogram_count: 0,
            histogram_sum: 0.0,
            trace_id: String::new(),
            span_id: String::new(),
        };

        let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
        assert!(metric.get("sum").is_some());
        assert_eq!(metric["sum"]["isMonotonic"], true);
        assert_eq!(metric["sum"]["aggregationTemporality"], 2);
        assert_eq!(metric["sum"]["dataPoints"][0]["asDouble"], 42.0);
    }

    #[test]
    fn test_build_histogram_metrics_payload() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);

        let record = MetricRecord {
            name: "plc.cycle_time".to_string(),
            description: "PLC cycle time".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Histogram,
            timestamp: chrono::Utc::now(),
            value: 0.0,
            is_monotonic: false,
            resource_attributes: std::collections::HashMap::new(),
            attributes: std::collections::HashMap::new(),
            histogram_bounds: vec![1.0, 5.0, 10.0],
            histogram_counts: vec![10, 25, 5, 1],
            histogram_count: 41,
            histogram_sum: 230.5,
            trace_id: String::new(),
            span_id: String::new(),
        };

        let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
        assert!(metric.get("histogram").is_some());
        assert_eq!(metric["histogram"]["dataPoints"][0]["sum"], 230.5);
        assert_eq!(
            metric["histogram"]["dataPoints"][0]["explicitBounds"],
            serde_json::json!([1.0, 5.0, 10.0])
        );
    }

    #[test]
    fn test_build_metrics_payload_empty() {
        let exporter = OtelExporter::new("http://localhost:4317".to_string(), 100, 3);
        let payload_str = exporter.build_otel_metrics_payload(&[]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        assert!(payload["resourceMetrics"].is_array());
        assert_eq!(payload["resourceMetrics"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_export_config_timeout_values() {
        let configs = vec![
            ExportConfig {
                timeout_secs: 5,
                ..Default::default()
            },
            ExportConfig {
                timeout_secs: 30,
                ..Default::default()
            },
            ExportConfig {
                timeout_secs: 120,
                ..Default::default()
            },
        ];

        for config in configs {
            assert!(config.timeout_secs > 0);
        }
    }

    // ─── Trace payload tests ──────────────────────────────────────

    fn make_trace_record() -> tc_otel_core::TraceRecord {
        let mut entry = tc_otel_core::SpanEntry::new(
            [
                0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45,
                0x67, 0x89,
            ],
            [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10],
            "motion.axis_move".to_string(),
        );
        entry.kind = tc_otel_core::SpanKind::Server;
        entry.status_code = tc_otel_core::SpanStatusCode::Ok;
        entry.status_message = "Success".to_string();
        entry.hostname = "plc-01".to_string();
        entry.project_name = "ProductionLine".to_string();
        entry.app_name = "HydraulicPress".to_string();
        entry
            .attributes
            .insert("motion.axis_id".to_string(), serde_json::json!(1));
        tc_otel_core::TraceRecord::from_span_entry(entry)
    }

    #[test]
    fn test_build_traces_payload_structure() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let record = make_trace_record();

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        // Verify top-level OTLP traces structure
        assert!(payload.get("resourceSpans").is_some());
        assert!(payload["resourceSpans"].is_array());
        assert_eq!(payload["resourceSpans"].as_array().unwrap().len(), 1);

        let rs = &payload["resourceSpans"][0];
        assert!(rs.get("resource").is_some());
        assert!(rs.get("scopeSpans").is_some());
    }

    #[test]
    fn test_build_traces_payload_span_fields() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let record = make_trace_record();

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

        assert_eq!(span["traceId"], "abcdef0123456789abcdef0123456789");
        assert_eq!(span["spanId"], "fedcba9876543210");
        assert_eq!(span["parentSpanId"], "0000000000000000");
        assert_eq!(span["name"], "motion.axis_move");
        assert_eq!(span["kind"], 2); // SPAN_KIND_SERVER
        assert_eq!(span["status"]["code"], 1); // STATUS_CODE_OK
        assert_eq!(span["status"]["message"], "Success");

        // Check startTimeUnixNano and endTimeUnixNano are present
        assert!(span["startTimeUnixNano"].is_string());
        assert!(span["endTimeUnixNano"].is_string());
    }

    #[test]
    fn test_build_traces_payload_resource_attributes() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let record = make_trace_record();

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let resource_attrs = &payload["resourceSpans"][0]["resource"]["attributes"];
        let attrs_vec = resource_attrs.as_array().unwrap();

        // Should contain service.name = ProductionLine
        let service_name = attrs_vec
            .iter()
            .find(|a| a["key"] == "service.name")
            .unwrap();
        assert_eq!(service_name["value"]["stringValue"], "ProductionLine");

        // Should contain host.name = plc-01
        let host_name = attrs_vec.iter().find(|a| a["key"] == "host.name").unwrap();
        assert_eq!(host_name["value"]["stringValue"], "plc-01");
    }

    #[test]
    fn test_build_traces_payload_span_attributes() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let record = make_trace_record();

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let span_attrs = span["attributes"].as_array().unwrap();

        // Should contain motion.axis_id = 1
        let axis_id = span_attrs
            .iter()
            .find(|a| a["key"] == "motion.axis_id")
            .unwrap();
        assert_eq!(axis_id["value"]["intValue"], "1");
    }

    #[test]
    fn test_build_traces_payload_with_events() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);

        let mut entry = tc_otel_core::SpanEntry::new([1u8; 16], [2u8; 8], "test".to_string());
        entry.events.push(tc_otel_core::SpanEvent {
            timestamp: chrono::Utc::now(),
            name: "axis.target_reached".to_string(),
            attributes: {
                let mut attrs = std::collections::HashMap::new();
                attrs.insert("axis.position".to_string(), serde_json::json!(150.5));
                attrs
            },
        });
        let record = tc_otel_core::TraceRecord::from_span_entry(entry);

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let events = span["events"].as_array().unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "axis.target_reached");

        let event_attrs = events[0]["attributes"].as_array().unwrap();
        let pos_attr = event_attrs
            .iter()
            .find(|a| a["key"] == "axis.position")
            .unwrap();
        assert_eq!(pos_attr["value"]["doubleValue"], 150.5);
    }

    #[test]
    fn test_build_traces_payload_empty() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let payload_str = exporter.build_otel_traces_payload(&[]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        assert!(payload["resourceSpans"].is_array());
        assert_eq!(payload["resourceSpans"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_build_traces_payload_multiple_spans() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);

        let entry1 = tc_otel_core::SpanEntry::new([1u8; 16], [1u8; 8], "span_1".to_string());
        let entry2 = tc_otel_core::SpanEntry::new([1u8; 16], [2u8; 8], "span_2".to_string());

        let records = vec![
            tc_otel_core::TraceRecord::from_span_entry(entry1),
            tc_otel_core::TraceRecord::from_span_entry(entry2),
        ];

        let payload_str = exporter.build_otel_traces_payload(&records).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        assert_eq!(payload["resourceSpans"].as_array().unwrap().len(), 2);

        let span1 = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let span2 = &payload["resourceSpans"][1]["scopeSpans"][0]["spans"][0];

        assert_eq!(span1["name"], "span_1");
        assert_eq!(span2["name"], "span_2");
    }

    #[test]
    fn test_build_traces_payload_scope_name() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);
        let record = make_trace_record();

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let scope = &payload["resourceSpans"][0]["scopeSpans"][0]["scope"];
        assert_eq!(scope["name"], "tc-otel");
    }

    #[test]
    fn test_build_traces_payload_error_status() {
        let exporter = OtelExporter::new("http://localhost:4318/v1/logs".to_string(), 100, 3);

        let mut entry = tc_otel_core::SpanEntry::new([1u8; 16], [2u8; 8], "failing_op".to_string());
        entry.status_code = tc_otel_core::SpanStatusCode::Error;
        entry.status_message = "axis fault detected".to_string();
        let record = tc_otel_core::TraceRecord::from_span_entry(entry);

        let payload_str = exporter.build_otel_traces_payload(&[record]).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["status"]["code"], 2); // STATUS_CODE_ERROR
        assert_eq!(span["status"]["message"], "axis fault detected");
    }
}

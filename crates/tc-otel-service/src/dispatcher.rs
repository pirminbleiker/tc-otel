//! Log and metric dispatchers with batched async export
//!
//! Records are collected in a batch buffer and flushed either when the batch
//! is full or after a timeout - whichever comes first. This minimizes
//! HTTP overhead and CPU usage.

use anyhow::Result;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tc_otel_core::{
    AppSettings, LogEntry, LogRecord, MessageFormatter, MetricEntry, MetricMapper, MetricRecord,
};
use tc_otel_export::OtelExporter;
use tokio::sync::{mpsc, watch};

/// Log dispatcher - converts LogEntries and sends them to a batched export worker
#[derive(Clone)]
pub struct LogDispatcher {
    export_tx: mpsc::Sender<LogRecord>,
}

impl LogDispatcher {
    pub async fn new(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) -> Result<Self> {
        // ENV override for endpoint, otherwise use config
        let endpoint = std::env::var("LOG4TC_EXPORT_ENDPOINT")
            .unwrap_or_else(|_| settings.export.endpoint.clone());

        let batch_size = settings.export.batch_size;
        let flush_interval = Duration::from_millis(settings.export.flush_interval_ms);

        // Bounded channel for backpressure
        let (export_tx, export_rx) = mpsc::channel::<LogRecord>(settings.service.channel_capacity);

        // Spawn background batch worker
        tokio::spawn(Self::batch_worker(
            export_rx,
            endpoint,
            batch_size,
            flush_interval,
            config_rx,
        ));

        tracing::info!(
            "Dispatcher ready (batch={}, flush={}ms)",
            batch_size,
            flush_interval.as_millis()
        );

        Ok(Self { export_tx })
    }

    /// Dispatch a log entry - formats and sends to export worker (non-blocking)
    pub async fn dispatch(&self, entry: LogEntry) -> Result<()> {
        // Format message only if template has placeholders
        let body = if entry.message.contains('{') {
            MessageFormatter::format_with_context(&entry.message, &entry.arguments, &entry.context)
        } else {
            entry.message.clone()
        };

        let mut record = LogRecord::from_log_entry(entry);
        // Preserve the trace suffix that from_log_entry appended, if any,
        // so Grafana's derivedFields regex still finds trace_id in the body.
        let body = if !record.trace_id.is_empty() {
            format!(
                "{} [trace_id={} span_id={}]",
                body, record.trace_id, record.span_id
            )
        } else {
            body
        };
        record.body = serde_json::Value::String(body);

        // Non-blocking send - drops if channel full (backpressure)
        if self.export_tx.try_send(record).is_err() {
            tracing::warn!("Export channel full, dropping log");
        }

        Ok(())
    }

    /// True if the endpoint is an OTLP HTTP logs endpoint
    /// (OTel collector / Tempo / etc., not VictoriaLogs).
    fn is_otlp_endpoint(endpoint: &str) -> bool {
        endpoint.contains("/v1/logs")
    }

    /// Background worker that batches records and flushes to endpoint.
    /// Supports hot-reload of export config (endpoint, batch_size, flush_interval)
    /// via an optional `watch::Receiver<AppSettings>`.
    async fn batch_worker(
        mut rx: mpsc::Receiver<LogRecord>,
        initial_endpoint: String,
        initial_batch_size: usize,
        initial_flush_interval: Duration,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut endpoint = initial_endpoint;
        let mut batch_size = initial_batch_size;
        let mut flush_interval = initial_flush_interval;
        let mut config_rx = config_rx;

        let mut otlp_exporter: Option<OtelExporter> = if Self::is_otlp_endpoint(&endpoint) {
            Some(OtelExporter::new(endpoint.clone(), batch_size, 3))
        } else {
            None
        };

        let mut batch: Vec<LogRecord> = Vec::with_capacity(batch_size);
        let mut payload_buf = String::with_capacity(batch_size * 256);
        let mut interval = tokio::time::interval(flush_interval);
        let mut total_sent: u64 = 0;
        let mut total_errors: u64 = 0;

        loop {
            tokio::select! {
                // Receive new record
                Some(record) = rx.recv() => {
                    batch.push(record);
                    if batch.len() >= batch_size {
                        match Self::flush_batch(&client, &endpoint, otlp_exporter.as_ref(), &mut batch, &mut payload_buf).await {
                            Ok(n) => total_sent += n as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Batch export error: {}", e);
                            }
                        }
                        batch.clear();
                    }
                }
                // Periodic flush
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        match Self::flush_batch(&client, &endpoint, otlp_exporter.as_ref(), &mut batch, &mut payload_buf).await {
                            Ok(n) => total_sent += n as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Batch export error: {}", e);
                            }
                        }
                        batch.clear();
                    }
                }
                // Config change notification
                result = async {
                    match config_rx.as_mut() {
                        Some(crx) => crx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if result.is_err() {
                        // Watch channel closed, disable config reload
                        config_rx = None;
                        continue;
                    }
                    let new_export = config_rx.as_ref().unwrap().borrow().export.clone();
                    let new_endpoint = std::env::var("LOG4TC_EXPORT_ENDPOINT")
                        .unwrap_or(new_export.endpoint);
                    let new_batch_size = new_export.batch_size;
                    let new_flush_interval = Duration::from_millis(new_export.flush_interval_ms);

                    if new_endpoint != endpoint {
                        tracing::info!("Hot-reload: export endpoint changed to {}", new_endpoint);
                        endpoint = new_endpoint;
                        otlp_exporter = if Self::is_otlp_endpoint(&endpoint) {
                            Some(OtelExporter::new(endpoint.clone(), batch_size, 3))
                        } else {
                            None
                        };
                    }
                    if new_batch_size != batch_size {
                        tracing::info!("Hot-reload: batch_size changed from {} to {}", batch_size, new_batch_size);
                        batch_size = new_batch_size;
                        batch.reserve(new_batch_size.saturating_sub(batch.capacity()));
                    }
                    if new_flush_interval != flush_interval {
                        tracing::info!("Hot-reload: flush_interval changed to {}ms", new_flush_interval.as_millis());
                        flush_interval = new_flush_interval;
                        interval = tokio::time::interval(flush_interval);
                    }
                }
                // Channel closed
                else => {
                    // Flush remaining
                    if !batch.is_empty() {
                        let _ = Self::flush_batch(&client, &endpoint, otlp_exporter.as_ref(), &mut batch, &mut payload_buf).await;
                    }
                    tracing::info!("Export worker stopped. Total sent: {}, errors: {}", total_sent, total_errors);
                    break;
                }
            }
        }
    }

    /// Flush a batch of records to the endpoint.
    /// OTLP endpoints (`/v1/logs`) → delegate to `OtelExporter` (OTLP JSON over HTTP).
    /// Everything else → VictoriaLogs JSONL with `application/stream+json`.
    async fn flush_batch(
        client: &reqwest::Client,
        endpoint: &str,
        otlp_exporter: Option<&OtelExporter>,
        batch: &mut Vec<LogRecord>,
        payload: &mut String,
    ) -> Result<usize> {
        if let Some(exporter) = otlp_exporter {
            let count = batch.len();
            let records = std::mem::take(batch);
            exporter
                .export_batch(records)
                .await
                .map_err(|e| anyhow::anyhow!("OTLP export failed: {}", e))?;
            return Ok(count);
        }
        let count = batch.len();
        payload.clear();

        // Build JSONL directly without intermediate serde_json::Map
        for record in batch.iter() {
            payload.push('{');
            // _msg
            payload.push_str("\"_msg\":");
            push_json_value(payload, &record.body);
            // _time
            payload.push_str(",\"_time\":\"");
            payload.push_str(&record.timestamp.to_rfc3339());
            payload.push('"');
            // level
            payload.push_str(",\"level\":\"");
            push_json_escaped(payload, &record.severity_text.to_lowercase());
            payload.push('"');
            // severity_number
            payload.push_str(",\"severity_number\":");
            {
                use std::fmt::Write;
                let _ = write!(payload, "{}", record.severity_number);
            }

            // Scope attributes (logger name)
            for (k, v) in &record.scope_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            // Resource attributes (service, host, task)
            for (k, v) in &record.resource_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            // Log attributes (context, args, plc metadata)
            for (k, v) in &record.log_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            payload.push_str("}\n");
        }

        // Special sink: print to stdout instead of HTTP POST.
        if endpoint.eq_ignore_ascii_case("stdout") {
            print!("{}", payload);
            return Ok(count);
        }

        let response = client
            .post(endpoint)
            .header("Content-Type", "application/stream+json")
            .body(payload.clone())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP error: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Export failed: HTTP {} - {}", status, body));
        }

        Ok(count)
    }
}

/// Metric dispatcher - converts MetricEntries, batches, and exports via OTLP HTTP
#[derive(Clone)]
pub struct MetricDispatcher {
    export_tx: mpsc::Sender<MetricRecord>,
    mapper: Arc<RwLock<MetricMapper>>,
}

impl MetricDispatcher {
    pub async fn new(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) -> Result<Self> {
        let endpoint = Self::resolve_endpoint(settings);
        let batch_size = settings.metrics.export_batch_size;
        let flush_interval = Duration::from_millis(settings.metrics.export_flush_interval_ms);

        let (export_tx, export_rx) =
            mpsc::channel::<MetricRecord>(settings.service.channel_capacity);

        let mapper = Arc::new(RwLock::new(MetricMapper::from_config(&settings.metrics)));

        tokio::spawn(Self::batch_worker(
            export_rx,
            endpoint,
            batch_size,
            flush_interval,
            settings.export.max_retries,
            settings.export.timeout_secs,
            config_rx,
            mapper.clone(),
        ));

        tracing::info!(
            "MetricDispatcher ready (batch={}, flush={}ms, custom_metrics={})",
            batch_size,
            flush_interval.as_millis(),
            mapper.read().unwrap().len()
        );

        Ok(Self { export_tx, mapper })
    }

    /// Resolve the metrics export endpoint from config.
    /// Uses metrics.export_endpoint if set, otherwise derives from the main
    /// export endpoint by replacing /v1/logs with /v1/metrics.
    fn resolve_endpoint(settings: &AppSettings) -> String {
        if let Some(ref ep) = settings.metrics.export_endpoint {
            return ep.clone();
        }
        // Derive from the main export endpoint
        settings
            .export
            .endpoint
            .replace("/v1/logs", "/v1/metrics")
            .replace("/insert/jsonline", "/v1/metrics")
    }

    /// Dispatch a metric entry - applies custom-metric mapping, converts to
    /// MetricRecord, and sends to the export worker.
    pub async fn dispatch(&self, mut entry: MetricEntry) -> Result<()> {
        self.mapper.read().unwrap().apply(&mut entry);
        let record = MetricRecord::from_metric_entry(entry);

        if self.export_tx.try_send(record).is_err() {
            tracing::warn!("Metric export channel full, dropping metric");
        }

        Ok(())
    }

    /// Background worker that batches metric records and flushes to the OTLP endpoint.
    #[allow(clippy::too_many_arguments)]
    async fn batch_worker(
        mut rx: mpsc::Receiver<MetricRecord>,
        initial_endpoint: String,
        initial_batch_size: usize,
        initial_flush_interval: Duration,
        max_retries: usize,
        timeout_secs: u64,
        config_rx: Option<watch::Receiver<AppSettings>>,
        mapper: Arc<RwLock<MetricMapper>>,
    ) {
        let exporter = OtelExporter::new(initial_endpoint.clone(), initial_batch_size, max_retries);
        // Keep exporter config in sync — for now we rebuild on endpoint change
        let mut current_endpoint = initial_endpoint;
        let mut batch_size = initial_batch_size;
        let mut flush_interval = initial_flush_interval;
        let mut config_rx = config_rx;
        let mut current_max_retries = max_retries;
        let mut current_timeout_secs = timeout_secs;
        let mut exporter = exporter;

        let mut batch: Vec<MetricRecord> = Vec::with_capacity(batch_size);
        let mut interval = tokio::time::interval(flush_interval);
        let mut total_sent: u64 = 0;
        let mut total_errors: u64 = 0;

        loop {
            tokio::select! {
                Some(record) = rx.recv() => {
                    batch.push(record);
                    if batch.len() >= batch_size {
                        match exporter.export_metrics_batch(std::mem::take(&mut batch)).await {
                            Ok(()) => total_sent += batch_size as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Metric batch export error: {}", e);
                            }
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        let count = batch.len();
                        match exporter.export_metrics_batch(std::mem::take(&mut batch)).await {
                            Ok(()) => total_sent += count as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Metric batch export error: {}", e);
                            }
                        }
                    }
                }
                result = async {
                    match config_rx.as_mut() {
                        Some(crx) => crx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if result.is_err() {
                        config_rx = None;
                        continue;
                    }
                    let new_settings = config_rx.as_ref().unwrap().borrow().clone();
                    let new_endpoint = Self::resolve_endpoint_from(&new_settings);
                    let new_batch_size = new_settings.metrics.export_batch_size;
                    let new_flush_interval = Duration::from_millis(new_settings.metrics.export_flush_interval_ms);

                    if new_endpoint != current_endpoint || new_settings.export.max_retries != current_max_retries || new_settings.export.timeout_secs != current_timeout_secs {
                        tracing::info!("Hot-reload: metrics export endpoint changed to {}", new_endpoint);
                        current_endpoint = new_endpoint.clone();
                        current_max_retries = new_settings.export.max_retries;
                        current_timeout_secs = new_settings.export.timeout_secs;
                        exporter = OtelExporter::new(current_endpoint.clone(), new_batch_size, current_max_retries);
                    }
                    if new_batch_size != batch_size {
                        tracing::info!("Hot-reload: metrics batch_size changed from {} to {}", batch_size, new_batch_size);
                        batch_size = new_batch_size;
                        batch.reserve(new_batch_size.saturating_sub(batch.capacity()));
                    }
                    if new_flush_interval != flush_interval {
                        tracing::info!("Hot-reload: metrics flush_interval changed to {}ms", new_flush_interval.as_millis());
                        flush_interval = new_flush_interval;
                        interval = tokio::time::interval(flush_interval);
                    }
                    let new_mapper = MetricMapper::from_config(&new_settings.metrics);
                    let len = new_mapper.len();
                    *mapper.write().unwrap() = new_mapper;
                    tracing::info!("Hot-reload: custom_metrics mapper rebuilt ({} symbols)", len);
                }
                else => {
                    if !batch.is_empty() {
                        let _ = exporter.export_metrics_batch(std::mem::take(&mut batch)).await;
                    }
                    tracing::info!("Metric export worker stopped. Total sent: {}, errors: {}", total_sent, total_errors);
                    break;
                }
            }
        }
    }

    /// Resolve endpoint from settings (used inside batch_worker for hot-reload)
    fn resolve_endpoint_from(settings: &AppSettings) -> String {
        if let Some(ref ep) = settings.metrics.export_endpoint {
            return ep.clone();
        }
        settings
            .export
            .endpoint
            .replace("/v1/logs", "/v1/metrics")
            .replace("/insert/jsonline", "/v1/metrics")
    }
}

/// Write a JSON key:value pair directly to buffer
#[inline]
fn push_json_key_value(buf: &mut String, key: &str, value: &serde_json::Value) {
    buf.push('"');
    push_json_escaped(buf, key);
    buf.push_str("\":");
    push_json_value(buf, value);
}

/// Write a JSON value directly to buffer
fn push_json_value(buf: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => buf.push_str("null"),
        serde_json::Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => {
            use std::fmt::Write;
            let _ = write!(buf, "{}", n);
        }
        serde_json::Value::String(s) => {
            buf.push('"');
            push_json_escaped(buf, s);
            buf.push('"');
        }
        // Fallback for complex types
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            if let Ok(s) = serde_json::to_string(value) {
                buf.push_str(&s);
            }
        }
    }
}

/// Escape a string for JSON (handles \n, \r, \t, \", \\)
#[inline]
fn push_json_escaped(buf: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(buf, "\\u{:04x}", c as u32);
            }
            c => buf.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tc_otel_core::config::*;
    use tc_otel_core::LogLevel;
    use tokio::sync::watch;

    fn test_settings_with_endpoint(endpoint: &str) -> AppSettings {
        AppSettings {
            logging: LoggingConfig {
                log_level: "info".to_string(),
                format: LogFormat::Text,
                output_path: None,
            },
            receiver: ReceiverConfig::default(),
            export: ExportConfig {
                endpoint: endpoint.to_string(),
                batch_size: 2,
                flush_interval_ms: 50,
                ..Default::default()
            },
            outputs: vec![],
            service: ServiceConfig::default(),
            web: WebConfig::default(),
            metrics: tc_otel_core::MetricsConfig::default(),
            diagnostics: tc_otel_core::DiagnosticsConfig::default(),
            traces: tc_otel_core::TracesConfig::default(),
        }
    }

    fn make_test_entry() -> LogEntry {
        LogEntry::new(
            "test-source".to_string(),
            "test-host".to_string(),
            "test message".to_string(),
            "test.logger".to_string(),
            LogLevel::Info,
        )
    }

    /// Start a test HTTP server that counts POST requests.
    /// Returns (address, request_counter).
    async fn start_test_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use axum::{routing::post, Router};

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let app = Router::new().route(
            "/insert",
            post(move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    ""
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (addr, counter)
    }

    #[tokio::test]
    async fn test_dispatcher_works_without_config_watch() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let dispatcher = LogDispatcher::new(&settings, None).await.unwrap();

        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();

        // Wait for batch flush (batch_size=2, should trigger)
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have flushed at least once"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_hot_reloads_endpoint() {
        let (addr1, counter1) = start_test_server().await;
        let (addr2, counter2) = start_test_server().await;

        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr1));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Send records - should go to server 1
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter1.load(Ordering::SeqCst) > 0,
            "initial endpoint should receive"
        );
        assert_eq!(
            counter2.load(Ordering::SeqCst),
            0,
            "new endpoint should not receive yet"
        );

        // Hot-reload: change endpoint to server 2
        let mut new_settings = settings.clone();
        new_settings.export.endpoint = format!("http://{}/insert", addr2);
        tx.send(new_settings).unwrap();

        // Allow config propagation
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Send more records - should go to server 2
        let c2_before = counter2.load(Ordering::SeqCst);
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter2.load(Ordering::SeqCst) > c2_before,
            "new endpoint should receive after hot-reload"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_hot_reloads_batch_size() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Initial batch_size is 2. Send 1 record - should NOT flush immediately
        dispatcher.dispatch(make_test_entry()).await.unwrap();

        // Wait for timer flush
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "timer should have flushed the single record"
        );

        // Hot-reload: change batch_size to 1
        let mut new_settings = settings.clone();
        new_settings.export.batch_size = 1;
        tx.send(new_settings).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now send 1 record - should flush immediately (batch_size=1)
        let count_before = counter.load(Ordering::SeqCst);
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            counter.load(Ordering::SeqCst) > count_before,
            "should flush immediately with batch_size=1"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_config_watch_channel_close_is_safe() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Drop the sender - should not crash the batch worker
        drop(tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Dispatcher should still work
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should still flush after watch channel closes"
        );
    }

    // ─── MetricDispatcher tests ──────────────────────────────────────

    fn test_settings_with_metrics(endpoint: &str) -> AppSettings {
        let mut settings = test_settings_with_endpoint(endpoint);
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = Some(endpoint.to_string());
        settings.metrics.export_batch_size = 2;
        settings.metrics.export_flush_interval_ms = 50;
        settings
    }

    fn make_test_metric_entry() -> MetricEntry {
        MetricEntry::gauge("test.metric".to_string(), 42.0)
    }

    /// Start a test HTTP server that accepts POST to /v1/metrics and counts requests.
    async fn start_metrics_test_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use axum::{routing::post, Router};

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let app = Router::new().route(
            "/v1/metrics",
            post(move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    ""
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (addr, counter)
    }

    #[tokio::test]
    async fn test_metric_dispatcher_batches_and_flushes() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Send enough entries to trigger batch flush (batch_size=2)
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have flushed metrics at least once"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_timer_flush() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Send 1 metric (below batch_size=2) — should still flush via timer
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "timer should flush even below batch size"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_endpoint_derivation() {
        // When no explicit metrics endpoint, derive from logs endpoint
        let mut settings = test_settings_with_endpoint("http://collector:4318/v1/logs");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = None;
        settings.metrics.export_batch_size = 100;
        settings.metrics.export_flush_interval_ms = 50;

        // Verify endpoint derivation logic
        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://collector:4318/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_endpoint_derivation_victoria() {
        // Victoria-logs style endpoint should also be derived
        let mut settings = test_settings_with_endpoint("http://victoria-logs:9428/insert/jsonline");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = None;

        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://victoria-logs:9428/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_explicit_endpoint() {
        let mut settings = test_settings_with_endpoint("http://collector:4318/v1/logs");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint =
            Some("http://prometheus-gateway:9091/v1/metrics".to_string());

        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://prometheus-gateway:9091/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_converts_entry_to_record() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Dispatch a metric with PLC metadata
        let mut entry = MetricEntry::gauge("plc.temp".to_string(), 72.5);
        entry.project_name = "TestProject".to_string();
        entry.hostname = "plc-01".to_string();
        entry.unit = "Cel".to_string();

        dispatcher.dispatch(entry).await.unwrap();

        // Trigger timer flush
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have exported the metric"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_config_watch_close_safe() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let (_tx, rx) = watch::channel(settings.clone());
        let dispatcher = MetricDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Drop the watch sender
        drop(_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Dispatcher should still work
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should still flush after watch channel closes"
        );
    }

    /// Capture exported request bodies in a shared buffer.
    async fn start_capturing_metrics_server(
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::{routing::post, Router};

        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = bodies.clone();

        let app = Router::new().route(
            "/v1/metrics",
            post(move |body: String| {
                let b = b.clone();
                async move {
                    b.lock().unwrap().push(body);
                    ""
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (addr, bodies)
    }

    #[tokio::test]
    async fn test_metric_dispatcher_applies_custom_metric_mapping() {
        let (addr, bodies) = start_capturing_metrics_server().await;
        let mut settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        settings.metrics.custom_metrics = vec![CustomMetricDef {
            symbol: "GVL.fMotorTemp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "Motor 1 bearing temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }];

        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        let mut entry = MetricEntry::gauge("raw.plc.symbol".to_string(), 72.5);
        entry.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.fMotorTemp".to_string()),
        );
        dispatcher.dispatch(entry).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let captured = bodies.lock().unwrap().join("");
        assert!(
            captured.contains("plc.motor.temperature"),
            "exported body should contain mapped metric name; got: {captured}"
        );
        assert!(
            captured.contains("\"Cel\""),
            "exported body should contain mapped unit; got: {captured}"
        );
        assert!(
            !captured.contains("raw.plc.symbol"),
            "exported body should not contain the pre-mapping name"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_hot_reload_rebuilds_mapper() {
        let (addr, bodies) = start_capturing_metrics_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let (tx, rx) = watch::channel(settings.clone());

        let dispatcher = MetricDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Before reload: no mapping, entry goes through unchanged.
        let mut e1 = MetricEntry::gauge("initial.name".to_string(), 1.0);
        e1.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.nCount".to_string()),
        );
        dispatcher.dispatch(e1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Push a new config with a mapping.
        let mut updated = settings.clone();
        updated.metrics.custom_metrics = vec![CustomMetricDef {
            symbol: "GVL.nCount".to_string(),
            metric_name: "plc.parts.produced".to_string(),
            description: "Parts produced".to_string(),
            unit: "{parts}".to_string(),
            kind: MetricKindConfig::Sum,
            is_monotonic: true,
            ..CustomMetricDef::default()
        }];
        tx.send(updated).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // After reload: same symbol now maps to new name/unit.
        let mut e2 = MetricEntry::gauge("initial.name".to_string(), 2.0);
        e2.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.nCount".to_string()),
        );
        dispatcher.dispatch(e2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let captured = bodies.lock().unwrap().join("");
        assert!(
            captured.contains("plc.parts.produced"),
            "body after hot-reload should contain remapped name; got: {captured}"
        );
        assert!(
            captured.contains("initial.name"),
            "body before hot-reload should still contain original (unmapped) name; got: {captured}"
        );
    }
}
